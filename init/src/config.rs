//! The process's configuration, read from `fw_cfg` (interface version 1).
//!
//! The config may carry secrets (environment values, keys), so no error
//! message here quotes a value: they name the field at most.

use serde::Deserialize;
use serde_json::Value;

/// The `fw_cfg` file the agent writes (`-fw_cfg name=opt/gigahost/config`).
pub const PATH: &str = "/sys/firmware/qemu_fw_cfg/by_name/opt/gigahost/config/raw";

/// The largest config accepted.
pub const MAX_BYTES: usize = 1 << 20;

/// The longest stop timeout, in the config and in a stop command.
pub const MAX_STOP_TIMEOUT_SECONDS: u32 = 300;

const DEFAULT_STOP_TIMEOUT_SECONDS: u32 = 10;

/// The config versions this init accepts.
pub const VERSIONS: &[u64] = &[1];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[allow(dead_code, reason = "checked before the typed parse")]
    pub version: u64,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    /// Docker's `user[:group]`; absent means root (uid 0, gid 0).
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default = "default_workdir")]
    pub workdir: String,
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_seconds: u32,
    #[serde(default)]
    pub authorized_keys: Option<AuthorizedKeys>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedKeys {
    pub user: String,
    pub keys: Vec<String>,
}

/// Shows the shape only: the config may hold secrets.
impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("argv", &self.argv.len())
            .field("env", &self.env.len())
            .field(
                "authorized_keys",
                &self.authorized_keys.as_ref().map(|keys| keys.keys.len()),
            )
            .finish_non_exhaustive()
    }
}

fn default_workdir() -> String {
    "/".into()
}

fn default_stop_timeout() -> u32 {
    DEFAULT_STOP_TIMEOUT_SECONDS
}

const FIELDS: &[&str] = &[
    "version",
    "argv",
    "env",
    "user",
    "workdir",
    "stop_timeout_seconds",
    "authorized_keys",
];

/// Names what in `value` keeps it from parsing, without quoting a value.
fn diagnose(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return "the config is not a JSON object".into();
    };
    for key in object.keys() {
        if !FIELDS.contains(&key.as_str()) {
            return format!("the config has an unknown field `{}`", printable(key));
        }
    }
    for field in ["argv", "env"] {
        if let Some(item) = object.get(field)
            && Vec::<String>::deserialize(item).is_err()
        {
            return format!("config field `{field}` is not a list of strings");
        }
    }
    if object.get("argv").is_none() {
        return "config field `argv` is missing".into();
    }
    if let Some(item) = object.get("user")
        && !item.is_string()
        && !item.is_null()
    {
        return "config field `user` is not a string".into();
    }
    if object.get("workdir").is_some_and(|item| !item.is_string()) {
        return "config field `workdir` is not a string".into();
    }
    if object
        .get("stop_timeout_seconds")
        .is_some_and(|item| u32::deserialize(item).is_err())
    {
        return "config field `stop_timeout_seconds` is not a whole number of seconds".into();
    }
    if let Some(keys) = object.get("authorized_keys") {
        if let Some(inner) = keys.as_object() {
            for key in inner.keys() {
                if key != "user" && key != "keys" {
                    return format!(
                        "config field `authorized_keys` has an unknown field `{}`",
                        printable(key)
                    );
                }
            }
        }
        return "config field `authorized_keys` is not {\"user\": string, \"keys\": [string]}"
            .into();
    }
    "the config does not match interface version 1".into()
}

/// A field name for a message: field names are the agent's, never secret,
/// but are cut short and kept to plain characters.
fn printable(key: &str) -> String {
    key.chars()
        .take(40)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

/// Parses and checks a config.
pub fn parse(bytes: &[u8]) -> Result<Config, String> {
    if bytes.len() > MAX_BYTES {
        return Err("the config is larger than 1 MiB".into());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        format!(
            "the config is not valid JSON (line {}, column {})",
            error.line(),
            error.column()
        )
    })?;
    match value.get("version").map(Value::as_u64) {
        None => return Err("config field `version` is missing".into()),
        Some(Some(version)) if VERSIONS.contains(&version) => {}
        Some(_) => return Err("the config's version is not one this init supports".into()),
    }
    let config = Config::deserialize(&value).map_err(|_| diagnose(&value))?;
    check(&config)?;
    Ok(config)
}

fn check(config: &Config) -> Result<(), String> {
    if config.argv.is_empty() {
        return Err("config field `argv` is empty".into());
    }
    if config.argv.iter().any(|arg| arg.contains('\0')) {
        return Err("config field `argv` holds a NUL byte".into());
    }
    for (index, entry) in config.env.iter().enumerate() {
        match entry.split_once('=') {
            Some((key, _)) if !key.is_empty() && !entry.contains('\0') => {}
            _ => return Err(format!("config field `env` entry {index} is not KEY=value")),
        }
    }
    if config
        .user
        .as_deref()
        .is_some_and(|user| user.contains('\0'))
    {
        return Err("config field `user` holds a NUL byte".into());
    }
    if !config.workdir.starts_with('/') || config.workdir.contains('\0') {
        return Err("config field `workdir` is not an absolute path".into());
    }
    if config.stop_timeout_seconds > MAX_STOP_TIMEOUT_SECONDS {
        return Err(format!(
            "config field `stop_timeout_seconds` is above {MAX_STOP_TIMEOUT_SECONDS}"
        ));
    }
    if let Some(keys) = &config.authorized_keys {
        if keys.user.is_empty() || keys.user.contains('\0') {
            return Err("config field `authorized_keys.user` is empty".into());
        }
        if keys
            .keys
            .iter()
            .any(|key| key.is_empty() || key.contains(['\n', '\r', '\0']))
        {
            return Err(
                "config field `authorized_keys.keys` holds an empty or multi-line key".into(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-value-never-shown";

    fn parse_text(text: &str) -> Result<Config, String> {
        parse(text.as_bytes())
    }

    #[test]
    fn defaults() {
        let config = parse_text(r#"{"version": 1, "argv": ["nginx"]}"#).unwrap();
        assert_eq!(config.argv, ["nginx"]);
        assert!(config.env.is_empty());
        assert_eq!(config.user, None);
        assert_eq!(config.workdir, "/");
        assert_eq!(config.stop_timeout_seconds, 10);
        assert!(config.authorized_keys.is_none());
    }

    #[test]
    fn every_field() {
        let config = parse_text(
            r#"{ "version": 1,
                 "argv": ["nginx", "-g", "daemon off;"],
                 "env": ["KEY=value", "EMPTY="],
                 "user": "101:101",
                 "workdir": "/srv",
                 "stop_timeout_seconds": 300,
                 "authorized_keys": { "user": "root", "keys": ["ssh-ed25519 AAAA x"] } }"#,
        )
        .unwrap();
        assert_eq!(config.argv.len(), 3);
        assert_eq!(config.env, ["KEY=value", "EMPTY="]);
        assert_eq!(config.user.as_deref(), Some("101:101"));
        assert_eq!(config.workdir, "/srv");
        assert_eq!(config.stop_timeout_seconds, 300);
        let keys = config.authorized_keys.unwrap();
        assert_eq!(keys.user, "root");
        assert_eq!(keys.keys, ["ssh-ed25519 AAAA x"]);
    }

    #[test]
    fn unknown_field_refused() {
        let error = parse_text(r#"{"version": 1, "argv": ["a"], "extra": true}"#).unwrap_err();
        assert_eq!(error, "the config has an unknown field `extra`");
        let error = parse_text(
            r#"{"version": 1, "argv": ["a"], "authorized_keys": {"user": "r", "keys": [], "x": 1}}"#,
        )
        .unwrap_err();
        assert!(error.contains("unknown field `x`"), "{error}");
    }

    #[test]
    fn other_versions_refused() {
        for text in [
            r#"{"version": 2, "argv": ["a"]}"#,
            r#"{"version": 0, "argv": ["a"]}"#,
            r#"{"version": "1", "argv": ["a"]}"#,
            r#"{"version": 2, "argv": ["a"], "new_field": 1}"#,
        ] {
            let error = parse_text(text).unwrap_err();
            assert!(error.contains("version"), "{text}: {error}");
        }
        assert_eq!(
            parse_text(r#"{"argv": ["a"]}"#).unwrap_err(),
            "config field `version` is missing"
        );
    }

    #[test]
    fn size_cap() {
        let head = r#"{"version": 1, "argv": ["a"], "env": [""#;
        let tail = r#""]}"#;
        let fill = |total: usize| {
            let mut text = String::from(head);
            text.push_str("K=");
            text.push_str(&"x".repeat(total - head.len() - tail.len() - 2));
            text.push_str(tail);
            assert_eq!(text.len(), total);
            text
        };
        assert!(parse_text(&fill(MAX_BYTES)).is_ok());
        assert_eq!(
            parse_text(&fill(MAX_BYTES + 1)).unwrap_err(),
            "the config is larger than 1 MiB"
        );
    }

    #[test]
    fn argv_checked() {
        assert_eq!(
            parse_text(r#"{"version": 1, "argv": []}"#).unwrap_err(),
            "config field `argv` is empty"
        );
        assert_eq!(
            parse_text(r#"{"version": 1}"#).unwrap_err(),
            "config field `argv` is missing"
        );
        assert!(parse_text(r#"{"version": 1, "argv": ["a\u0000b"]}"#).is_err());
    }

    #[test]
    fn stop_timeout_cap() {
        assert!(
            parse_text(r#"{"version": 1, "argv": ["a"], "stop_timeout_seconds": 300}"#).is_ok()
        );
        assert!(
            parse_text(r#"{"version": 1, "argv": ["a"], "stop_timeout_seconds": 301}"#).is_err()
        );
        assert!(
            parse_text(r#"{"version": 1, "argv": ["a"], "stop_timeout_seconds": -1}"#).is_err()
        );
    }

    #[test]
    fn other_checks() {
        for text in [
            r#"{"version": 1, "argv": ["a"], "env": ["NOEQUALS"]}"#,
            r#"{"version": 1, "argv": ["a"], "env": ["=v"]}"#,
            r#"{"version": 1, "argv": ["a"], "workdir": "relative"}"#,
            r#"{"version": 1, "argv": ["a"], "authorized_keys": {"user": "", "keys": []}}"#,
            r#"{"version": 1, "argv": ["a"], "authorized_keys": {"user": "r", "keys": ["a\nb"]}}"#,
            r#"{"version": 1, "argv": ["a"], "authorized_keys": {"user": "r"}}"#,
            r"[1]",
            r"not json",
        ] {
            assert!(parse_text(text).is_err(), "{text}");
        }
    }

    #[test]
    fn errors_never_quote_values() {
        for text in [
            format!(r#"{{"version": 1, "argv": ["a"], "env": ["A={SECRET}", 5]}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "env": ["{SECRET}"]}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "env": "{SECRET}"}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "user": ["{SECRET}"]}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "stop_timeout_seconds": "{SECRET}"}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "workdir": "{SECRET}"}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "authorized_keys": "{SECRET}"}}"#),
            format!(
                r#"{{"version": 1, "argv": ["a"], "authorized_keys": {{"user": "r", "keys": ["{SECRET}\n"]}}}}"#
            ),
            format!(r#"{{"version": "{SECRET}", "argv": ["a"]}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "env": ["A={SECRET}"], "x": 1}}"#),
            format!(r#"{{"version": 1, "argv": ["a"], "env": ["A={SECRET}"#),
        ] {
            let error = parse_text(&text).unwrap_err();
            assert!(!error.contains(SECRET), "{text}: {error}");
        }
    }
}
