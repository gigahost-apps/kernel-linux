//! Resolving the process's `user[:group]` against the image's /etc/passwd
//! and /etc/group exactly as Docker's runtime does (runc's
//! `user.GetExecUser`, with uid 0, gid 0 and home `/` as the defaults).

/// One /etc/passwd line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passwd {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

/// One /etc/group line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// Who the process runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecUser {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups, set with `setgroups` (may be empty).
    pub groups: Vec<u32>,
    pub home: String,
}

/// Why a user spec does not resolve. The messages quote no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserError {
    UnknownUser,
    UnknownGroup,
    OutOfRange,
}

impl UserError {
    pub fn message(self) -> &'static str {
        match self {
            UserError::UnknownUser => "the user is not in the image's /etc/passwd",
            UserError::UnknownGroup => "the group is not in the image's /etc/group",
            UserError::OutOfRange => "the user or group ID is out of range",
        }
    }
}

/// The largest ID runc accepts (`math.MaxInt32`).
const MAX_ID: i64 = i32::MAX as i64;

fn parse_id(text: &str) -> Option<u32> {
    text.parse().ok()
}

/// Parses /etc/passwd as runc does: a missing or non-numeric field is
/// empty or 0; blank lines and comments are skipped.
pub fn parse_passwd(text: &str) -> Vec<Passwd> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split(':');
            let name = fields.next()?.to_owned();
            let _password = fields.next();
            let uid = parse_id(fields.next().unwrap_or("0")).unwrap_or(0);
            let gid = parse_id(fields.next().unwrap_or("0")).unwrap_or(0);
            let _gecos = fields.next();
            let home = fields.next().unwrap_or_default().to_owned();
            Some(Passwd {
                name,
                uid,
                gid,
                home,
            })
        })
        .collect()
}

/// Parses /etc/group as runc does (see [`parse_passwd`]).
pub fn parse_group(text: &str) -> Vec<Group> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split(':');
            let name = fields.next()?.to_owned();
            let _password = fields.next();
            let gid = parse_id(fields.next().unwrap_or("0")).unwrap_or(0);
            let members = fields
                .next()
                .unwrap_or_default()
                .split(',')
                .filter(|member| !member.is_empty())
                .map(str::to_owned)
                .collect();
            Some(Group { name, gid, members })
        })
        .collect()
}

fn numeric(text: &str) -> Option<Result<u32, UserError>> {
    // Go's strconv.Atoi: an optional sign and decimal digits.
    let value: i64 = text.parse().ok()?;
    Some(
        u32::try_from(value)
            .ok()
            .filter(|_| value <= MAX_ID)
            .ok_or(UserError::OutOfRange),
    )
}

/// Resolves `spec` (`""`, `user`, `uid`, `user:group`, `uid:gid`, …).
///
/// - A user name must be in passwd; a uid need not be (gid 0, home `/`).
/// - A matched passwd entry gives the uid, the primary gid and the home.
/// - A group, by name (must be in group) or gid (need not be), replaces
///   the primary gid; without a group, the matched user's memberships in
///   /etc/group are the supplementary groups.
pub fn resolve(spec: &str, passwd: &[Passwd], groups: &[Group]) -> Result<ExecUser, UserError> {
    let (user_part, group_part) = spec.split_once(':').unwrap_or((spec, ""));
    let mut result = ExecUser {
        uid: 0,
        gid: 0,
        groups: Vec::new(),
        home: "/".into(),
    };
    let uid_arg = numeric(user_part);
    let matched = passwd.iter().find(|entry| match (&uid_arg, user_part) {
        (_, "") => entry.uid == 0,
        (Some(Ok(uid)), _) => entry.uid == *uid,
        (Some(Err(_)), _) => false,
        (None, name) => entry.name == name,
    });
    if let Some(entry) = matched {
        result.uid = entry.uid;
        result.gid = entry.gid;
        result.home.clone_from(&entry.home);
    } else if !user_part.is_empty() {
        result.uid = uid_arg.ok_or(UserError::UnknownUser)??;
    }
    if !group_part.is_empty() {
        let gid_arg = numeric(group_part);
        let found = groups.iter().find(|group| match &gid_arg {
            Some(Ok(gid)) => group.gid == *gid,
            Some(Err(_)) => false,
            None => group.name == group_part,
        });
        result.gid = match found {
            Some(group) => group.gid,
            None => gid_arg.ok_or(UserError::UnknownGroup)??,
        };
    } else if let Some(entry) = matched {
        result.groups = groups
            .iter()
            .filter(|group| group.members.contains(&entry.name))
            .map(|group| group.gid)
            .collect();
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
www-data:x:33:33:www-data:/var/www:/usr/sbin/nologin
app:x:1000:1000::/home/app:/bin/sh
# a comment
broken
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
";

    const GROUP: &str = "\
root:x:0:
daemon:x:1:
www-data:x:33:
app:x:1000:
wheel:x:10:root,app
video:x:44:app
staff:x:50:
nogroup:x:65534:
";

    fn resolve_text(spec: &str) -> Result<ExecUser, UserError> {
        resolve(spec, &parse_passwd(PASSWD), &parse_group(GROUP))
    }

    fn user(uid: u32, gid: u32, groups: &[u32], home: &str) -> ExecUser {
        ExecUser {
            uid,
            gid,
            groups: groups.to_vec(),
            home: home.into(),
        }
    }

    #[test]
    fn empty_is_root() {
        assert_eq!(resolve_text(""), Ok(user(0, 0, &[10], "/root")));
        assert_eq!(resolve("", &[], &[]), Ok(user(0, 0, &[], "/")));
    }

    #[test]
    fn by_name() {
        assert_eq!(
            resolve_text("app"),
            Ok(user(1000, 1000, &[10, 44], "/home/app"))
        );
        assert_eq!(resolve_text("www-data"), Ok(user(33, 33, &[], "/var/www")));
    }

    #[test]
    fn by_uid() {
        assert_eq!(
            resolve_text("1000"),
            Ok(user(1000, 1000, &[10, 44], "/home/app"))
        );
        assert_eq!(resolve_text("0"), Ok(user(0, 0, &[10], "/root")));
    }

    #[test]
    fn uid_not_in_passwd() {
        assert_eq!(resolve_text("4242"), Ok(user(4242, 0, &[], "/")));
    }

    #[test]
    fn uid_and_gid() {
        assert_eq!(resolve_text("101:101"), Ok(user(101, 101, &[], "/")));
        assert_eq!(
            resolve_text("1000:50"),
            Ok(user(1000, 50, &[], "/home/app"))
        );
    }

    #[test]
    fn name_and_group() {
        assert_eq!(
            resolve_text("app:staff"),
            Ok(user(1000, 50, &[], "/home/app"))
        );
        assert_eq!(
            resolve_text("nobody:nogroup"),
            Ok(user(65534, 65534, &[], "/nonexistent"))
        );
        assert_eq!(
            resolve_text("app:777"),
            Ok(user(1000, 777, &[], "/home/app"))
        );
    }

    #[test]
    fn unknown_names_refused() {
        assert_eq!(resolve_text("ghost"), Err(UserError::UnknownUser));
        assert_eq!(
            resolve_text("app:nosuchgroup"),
            Err(UserError::UnknownGroup)
        );
        assert_eq!(resolve_text("app:staff:x"), Err(UserError::UnknownGroup));
        assert_eq!(resolve("app", &[], &[]), Err(UserError::UnknownUser));
    }

    #[test]
    fn out_of_range() {
        assert_eq!(resolve_text("-1"), Err(UserError::OutOfRange));
        assert_eq!(resolve_text("2147483648"), Err(UserError::OutOfRange));
        assert_eq!(resolve_text("1:-5"), Err(UserError::OutOfRange));
        assert_eq!(
            resolve_text("2147483647"),
            Ok(user(2_147_483_647, 0, &[], "/"))
        );
    }

    #[test]
    fn short_lines_kept_as_runc_keeps_them() {
        let entries = parse_passwd(PASSWD);
        assert_eq!(entries.len(), 6);
        assert_eq!(
            entries[4],
            Passwd {
                name: "broken".into(),
                uid: 0,
                gid: 0,
                home: String::new()
            }
        );
        assert_eq!(entries[5].name, "nobody");
    }
}
