//! The files the init puts over the image's /etc/hosts, /etc/resolv.conf
//! and /etc/hostname, as Docker writes them.

use crate::cmdline::KernelArgs;
use std::fmt::Write as _;
use std::path::Path;

/// `/etc/hosts`: Docker's default entries, then the instance's addresses.
pub fn hosts(args: &KernelArgs) -> String {
    let mut text = String::from(
        "127.0.0.1\tlocalhost\n\
         ::1\tlocalhost ip6-localhost ip6-loopback\n\
         fe00::\tip6-localnet\n\
         ff00::\tip6-mcastprefix\n\
         ff02::1\tip6-allnodes\n\
         ff02::2\tip6-allrouters\n",
    );
    let _ = writeln!(text, "{}\t{}", args.ip4.0, args.hostname);
    if let Some((address, _)) = args.ip6 {
        let _ = writeln!(text, "{address}\t{}", args.hostname);
    }
    text
}

/// `/etc/resolv.conf`: one nameserver line per address.
pub fn resolv_conf(args: &KernelArgs) -> String {
    args.dns.iter().fold(String::new(), |mut text, address| {
        let _ = writeln!(text, "nameserver {address}");
        text
    })
}

/// `/etc/hostname`.
pub fn hostname(args: &KernelArgs) -> String {
    format!("{}\n", args.hostname)
}

/// Docker's default `PATH`.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The process's environment as Docker builds it: `PATH` (when the config
/// has none) and `HOSTNAME`, then the config's entries (one replaces a
/// default or an earlier entry of the same key in place), then `HOME` from
/// the user's passwd entry when still absent.
pub fn process_env(config_env: &[String], hostname: &str, home: &str) -> Vec<String> {
    fn key(entry: &str) -> &str {
        entry.split_once('=').map_or(entry, |(key, _)| key)
    }
    let mut env = Vec::new();
    if !config_env.iter().any(|entry| key(entry) == "PATH") {
        env.push(format!("PATH={DEFAULT_PATH}"));
    }
    env.push(format!("HOSTNAME={hostname}"));
    for entry in config_env {
        match env.iter_mut().find(|existing| key(existing) == key(entry)) {
            Some(existing) => existing.clone_from(entry),
            None => env.push(entry.clone()),
        }
    }
    if !env.iter().any(|entry| key(entry) == "HOME") {
        env.push(format!("HOME={home}"));
    }
    env
}

/// The `PATH` an environment holds.
pub fn path_of(env: &[String]) -> &str {
    env.iter()
        .find_map(|entry| entry.strip_prefix("PATH="))
        .unwrap_or(DEFAULT_PATH)
}

/// Searches `PATH` for `program` as Go's `exec.LookPath` does (which
/// Docker's runtime uses): a name with a slash is taken as it is, an empty
/// `PATH` entry is the working directory, and a candidate must be a file
/// with an execute bit.
pub fn look_path(program: &str, path: &str, workdir: &Path) -> Option<String> {
    let executable = |candidate: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(candidate)
            .is_ok_and(|meta| !meta.is_dir() && meta.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let candidate = workdir.join(program);
        return executable(&candidate).then(|| program.to_owned());
    }
    path.split(':').find_map(|directory| {
        let directory = if directory.is_empty() { "." } else { directory };
        let joined = format!("{directory}/{program}");
        executable(&workdir.join(&joined)).then_some(joined)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmdline;
    use std::os::unix::fs::PermissionsExt;

    fn args(extra: &str) -> KernelArgs {
        cmdline::parse(&format!(
            "gh.v=1 gh.hostname=web gh.ip4=192.0.2.10/24 gh.gw4=192.0.2.1 gh.dns=192.0.2.53,2001:db8::53 gh.vsock=9 {extra}"
        ))
        .unwrap()
    }

    #[test]
    fn hosts_file() {
        let text = hosts(&args(""));
        assert!(
            text.starts_with("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n")
        );
        assert!(text.ends_with("ff02::2\tip6-allrouters\n192.0.2.10\tweb\n"));
        let text = hosts(&args("gh.ip6=2001:db8::10/64 gh.gw6=fe80::1"));
        assert!(text.ends_with("192.0.2.10\tweb\n2001:db8::10\tweb\n"));
    }

    #[test]
    fn resolv_and_hostname() {
        assert_eq!(
            resolv_conf(&args("")),
            "nameserver 192.0.2.53\nnameserver 2001:db8::53\n"
        );
        assert_eq!(hostname(&args("")), "web\n");
    }

    #[test]
    fn environment() {
        let given = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            process_env(&given(&["FOO=bar"]), "web", "/root"),
            given(&[
                &format!("PATH={DEFAULT_PATH}"),
                "HOSTNAME=web",
                "FOO=bar",
                "HOME=/root"
            ])
        );
        assert_eq!(
            process_env(
                &given(&["PATH=/opt", "HOME=/srv", "HOSTNAME=x", "A=1", "A=2"]),
                "web",
                "/root"
            ),
            given(&["HOSTNAME=x", "PATH=/opt", "HOME=/srv", "A=2"])
        );
        assert_eq!(path_of(&given(&["PATH=/opt"])), "/opt");
        assert_eq!(path_of(&[]), DEFAULT_PATH);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gigahost-init-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn path_search() {
        let root = temp_dir("path");
        let a = root.join("a");
        let b = root.join("b");
        touch(&a.join("tool"), 0o644);
        touch(&b.join("tool"), 0o755);
        touch(&root.join("w/local"), 0o755);
        std::fs::create_dir_all(a.join("dir")).unwrap();
        let path = format!("{}:{}", a.display(), b.display());
        let workdir = root.join("w");
        // The first match that is executable wins.
        assert_eq!(
            look_path("tool", &path, &workdir),
            Some(format!("{}/tool", b.display()))
        );
        assert_eq!(look_path("missing", &path, &workdir), None);
        // A directory is not a program.
        assert_eq!(look_path("dir", &a.display().to_string(), &workdir), None);
        // An empty entry is the working directory.
        assert_eq!(
            look_path("local", &format!("{}:", a.display()), &workdir),
            Some("./local".into())
        );
        // A name with a slash is not searched.
        assert_eq!(
            look_path("./local", &path, &workdir),
            Some("./local".into())
        );
        let absolute = b.join("tool").display().to_string();
        assert_eq!(look_path(&absolute, "", &workdir), Some(absolute.clone()));
        assert_eq!(look_path("b/tool", &path, &workdir), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
