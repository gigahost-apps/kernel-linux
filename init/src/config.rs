//! The machine's configuration, read from `fw_cfg` (interface versions 1,
//! 2 and 3): the process to run (from version 3, possibly the image's own
//! init system), or (version 2's unpack mode) the image layers to unpack
//! onto the root disk.
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

/// The signal a stop sends first when the config names none (SIGTERM).
const DEFAULT_STOP_SIGNAL: i32 = 15;

/// The highest signal number a config may name (`SIGRTMAX` on Linux).
pub const MAX_SIGNAL: i32 = 64;

/// The config versions this init accepts.
pub const VERSIONS: &[u64] = &[1, 2, 3];

/// The first config version with `init_system`.
const INIT_SYSTEM_VERSION: u64 = 3;

/// The most layers an unpack config may list.
pub const MAX_LAYERS: usize = 128;

/// How a layer's blob is compressed, from its media type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

/// The layer media types the unpack mode takes. Non-distributable
/// (foreign) layers are not among them: their blobs are not in the image.
const MEDIA_TYPES: &[(&str, Compression)] = &[
    ("application/vnd.oci.image.layer.v1.tar", Compression::None),
    (
        "application/vnd.oci.image.layer.v1.tar+gzip",
        Compression::Gzip,
    ),
    (
        "application/vnd.oci.image.layer.v1.tar+zstd",
        Compression::Zstd,
    ),
    (
        "application/vnd.docker.image.rootfs.diff.tar.gzip",
        Compression::Gzip,
    ),
];

/// What the boot does: run the image's process (versions 1 and 2), or
/// unpack the image's layers onto the empty root disk (version 2).
#[derive(Debug)]
pub enum Boot {
    Run(Config),
    Unpack(Unpack),
}

/// The unpack mode's config: the layers, in order, as they lie one after
/// the other on the layers disk.
#[derive(Debug)]
pub struct Unpack {
    pub layers: Vec<Layer>,
}

/// One layer of the layers disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub compression: Compression,
    /// The blob's SHA-256.
    pub digest: [u8; 32],
    /// The blob's length in bytes, at least 1.
    pub size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerFields {
    media_type: String,
    digest: String,
    size: u64,
}

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
    /// The image's `STOPSIGNAL`, as a number: what a stop sends the main
    /// process first.
    #[serde(default = "default_stop_signal")]
    pub stop_signal: i32,
    #[serde(default)]
    pub authorized_keys: Option<AuthorizedKeys>,
    /// Version 3: `argv` is the image's own init system (systemd, a
    /// SysV-style or busybox init), run as PID 1 of a PID namespace of its
    /// own.
    #[serde(default)]
    pub init_system: bool,
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
            .field("init_system", &self.init_system)
            .finish_non_exhaustive()
    }
}

fn default_workdir() -> String {
    "/".into()
}

fn default_stop_timeout() -> u32 {
    DEFAULT_STOP_TIMEOUT_SECONDS
}

fn default_stop_signal() -> i32 {
    DEFAULT_STOP_SIGNAL
}

const FIELDS: &[&str] = &[
    "version",
    "argv",
    "env",
    "user",
    "workdir",
    "stop_timeout_seconds",
    "stop_signal",
    "authorized_keys",
    "init_system",
];

/// Whether `key` is a run field of config `version`: `init_system` is
/// version 3's, unknown to versions 1 and 2.
fn known_field(key: &str, version: u64) -> bool {
    FIELDS.contains(&key) && (key != "init_system" || version >= INIT_SYSTEM_VERSION)
}

/// Names what in `value` keeps it from parsing, without quoting a value.
fn diagnose(value: &Value, version: u64) -> String {
    let Some(object) = value.as_object() else {
        return "the config is not a JSON object".into();
    };
    for key in object.keys() {
        if !known_field(key, version) {
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
    if object
        .get("stop_signal")
        .is_some_and(|item| i32::deserialize(item).is_err())
    {
        return "config field `stop_signal` is not a signal number".into();
    }
    if object
        .get("init_system")
        .is_some_and(|item| !item.is_boolean())
    {
        return "config field `init_system` is not true or false".into();
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
    format!("the config does not match interface version {version}")
}

/// A field name for a message: field names are the agent's, never secret,
/// but are cut short and kept to plain characters.
fn printable(key: &str) -> String {
    key.chars()
        .take(40)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

/// Parses and checks the config of a boot of interface `boot_version`
/// (`gh.v`), whose version the config's must be.
pub fn parse(bytes: &[u8], boot_version: u32) -> Result<Boot, String> {
    if bytes.len() > MAX_BYTES {
        return Err("the config is larger than 1 MiB".into());
    }
    let mut value: Value = serde_json::from_slice(bytes).map_err(|error| {
        format!(
            "the config is not valid JSON (line {}, column {})",
            error.line(),
            error.column()
        )
    })?;
    let version = match value.get("version").map(Value::as_u64) {
        None => return Err("config field `version` is missing".into()),
        Some(Some(version)) if version == u64::from(boot_version) => version,
        // A boot answers a later version as one it does not support, as it
        // did before that version existed (a version-1 boot answers so
        // for every other version).
        Some(Some(version)) if VERSIONS.contains(&version) && version < u64::from(boot_version) => {
            return Err("the config's version is not the boot's interface version (gh.v)".into());
        }
        Some(_) => return Err("the config's version is not one this init supports".into()),
    };
    if version == 1 {
        return run_config(&value, version).map(Boot::Run);
    }
    let Some(object) = value.as_object_mut() else {
        return Err("the config is not a JSON object".into());
    };
    match object.get("mode") {
        None => {}
        Some(Value::String(mode)) if mode == "run" => {}
        Some(Value::String(mode)) if mode == "unpack" => {
            return unpack_config(object, version).map(Boot::Unpack);
        }
        Some(_) => return Err("config field `mode` is not \"run\" or \"unpack\"".into()),
    }
    object.remove("mode");
    if object.contains_key("layers") {
        return Err("config field `layers` is taken in the unpack mode only".into());
    }
    run_config(&value, version).map(Boot::Run)
}

/// The run mode's config, as version 1 has it.
fn run_config(value: &Value, version: u64) -> Result<Config, String> {
    if version < INIT_SYSTEM_VERSION && value.get("init_system").is_some() {
        // Not a field of this version: refused as any unknown field.
        return Err(diagnose(value, version));
    }
    let config = Config::deserialize(value).map_err(|_| diagnose(value, version))?;
    check(&config)?;
    Ok(config)
}

/// The unpack mode's config: `version`, `mode` and `layers`, nothing else.
fn unpack_config(object: &serde_json::Map<String, Value>, version: u64) -> Result<Unpack, String> {
    for key in object.keys() {
        if key == "mode" || key == "layers" {
            continue;
        }
        if !known_field(key, version) {
            return Err(format!(
                "the config has an unknown field `{}`",
                printable(key)
            ));
        }
        if key != "version" {
            return Err(format!(
                "config field `{key}` is not taken in the unpack mode"
            ));
        }
    }
    let Some(layers) = object.get("layers") else {
        return Err("config field `layers` is missing".into());
    };
    let Some(entries) = layers.as_array() else {
        return Err("config field `layers` is not a list".into());
    };
    if entries.is_empty() || entries.len() > MAX_LAYERS {
        return Err(format!(
            "config field `layers` does not hold 1 to {MAX_LAYERS} layers"
        ));
    }
    let mut total: u64 = 0;
    let mut checked = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let layer = layer(index, entry)?;
        total = total
            .checked_add(layer.size)
            .ok_or_else(|| format!("config field `layers` entry {index} ends beyond any disk"))?;
        checked.push(layer);
    }
    Ok(Unpack { layers: checked })
}

fn layer(index: usize, entry: &Value) -> Result<Layer, String> {
    let what = |problem: &str| format!("config field `layers` entry {index} {problem}");
    let Some(object) = entry.as_object() else {
        return Err(what("is not an object"));
    };
    for key in object.keys() {
        if !["media_type", "digest", "size"].contains(&key.as_str()) {
            return Err(what(&format!("has an unknown field `{}`", printable(key))));
        }
    }
    for key in ["media_type", "digest", "size"] {
        if !object.contains_key(key) {
            return Err(what(&format!("lacks `{key}`")));
        }
    }
    let fields = LayerFields::deserialize(entry).map_err(|_| {
        what("is not {\"media_type\": string, \"digest\": string, \"size\": number}")
    })?;
    let compression = MEDIA_TYPES
        .iter()
        .find(|(name, _)| *name == fields.media_type)
        .map(|&(_, compression)| compression)
        .ok_or_else(|| what("has a media type this init does not unpack"))?;
    let digest = parse_digest(&fields.digest)
        .ok_or_else(|| what("has a digest that is not sha256: and 64 lowercase hex digits"))?;
    if fields.size == 0 {
        return Err(what("has a size of 0"));
    }
    Ok(Layer {
        compression,
        digest,
        size: fields.size,
    })
}

/// `sha256:` and 64 lowercase hex digits, as bytes.
fn parse_digest(text: &str) -> Option<[u8; 32]> {
    let hex = text.strip_prefix("sha256:")?.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut digest = [0u8; 32];
    for (out, pair) in digest.iter_mut().zip(hex.chunks_exact(2)) {
        let [high, low] = pair else { return None };
        *out = nibble(*high)? << 4 | nibble(*low)?;
    }
    Some(digest)
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
    if !(1..=MAX_SIGNAL).contains(&config.stop_signal) {
        return Err(format!(
            "config field `stop_signal` is not a signal number from 1 to {MAX_SIGNAL}"
        ));
    }
    if config.init_system {
        check_init_system(config)?;
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

/// The `user` values that name root (uid 0, gid 0) without looking at the
/// image: absent, or root by name or number, with or without its group.
const ROOT_USERS: &[&str] = &["", "root", "0", "root:root", "root:0", "0:root", "0:0"];

/// What `init_system` requires of the rest of the run config: an absolute
/// path to the image's init, root, and `/` as the working directory.
fn check_init_system(config: &Config) -> Result<(), String> {
    if !config
        .argv
        .first()
        .is_some_and(|program| program.starts_with('/'))
    {
        return Err(
            "config field `argv` does not start with an absolute path, which `init_system` requires"
                .into(),
        );
    }
    if !ROOT_USERS.contains(&config.user.as_deref().unwrap_or("")) {
        return Err("config field `user` is not root, which `init_system` requires".into());
    }
    if config.workdir != "/" {
        return Err("config field `workdir` is not /, which `init_system` requires".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-value-never-shown";

    /// A version-1 boot's run config.
    fn parse_text(text: &str) -> Result<Config, String> {
        parse_boot(text, 1).map(|boot| match boot {
            Boot::Run(config) => config,
            Boot::Unpack(_) => panic!("a version-1 config gave the unpack mode"),
        })
    }

    fn parse_boot(text: &str, boot_version: u32) -> Result<Boot, String> {
        parse(text.as_bytes(), boot_version)
    }

    fn unpack(text: &str) -> Result<Vec<Layer>, String> {
        parse_boot(text, 2).map(|boot| match boot {
            Boot::Unpack(unpack) => unpack.layers,
            Boot::Run(_) => panic!("not the unpack mode"),
        })
    }

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TAR: &str = "application/vnd.oci.image.layer.v1.tar";

    fn layer_json(media_type: &str, digest: &str, size: &str) -> String {
        format!(r#"{{"media_type": "{media_type}", "digest": "{digest}", "size": {size}}}"#)
    }

    fn unpack_json(layers: &[String]) -> String {
        format!(
            r#"{{"version": 2, "mode": "unpack", "layers": [{}]}}"#,
            layers.join(", ")
        )
    }

    #[test]
    fn version_two_run_mode() {
        for text in [
            r#"{"version": 2, "argv": ["nginx"]}"#,
            r#"{"version": 2, "mode": "run", "argv": ["nginx"], "stop_signal": 3}"#,
        ] {
            match parse_boot(text, 2).unwrap() {
                Boot::Run(config) => assert_eq!(config.argv, ["nginx"]),
                Boot::Unpack(_) => panic!("{text}"),
            }
        }
        assert_eq!(
            parse_boot(r#"{"version": 2, "mode": "run"}"#, 2).unwrap_err(),
            "config field `argv` is missing"
        );
        let with_layers = format!(
            r#"{{"version": 2, "argv": ["a"], "layers": [{}]}}"#,
            layer_json(TAR, DIGEST, "1")
        );
        assert_eq!(
            parse_boot(&with_layers, 2).unwrap_err(),
            "config field `layers` is taken in the unpack mode only"
        );
        for mode in [r#""Run""#, r#""""#, "1", "null", r#"["run"]"#] {
            let text = format!(r#"{{"version": 2, "mode": {mode}, "argv": ["a"]}}"#);
            assert_eq!(
                parse_boot(&text, 2).unwrap_err(),
                r#"config field `mode` is not "run" or "unpack""#,
                "{mode}"
            );
        }
        assert_eq!(
            parse_boot(r#"{"version": 2, "argv": ["a"], "x": 1}"#, 2).unwrap_err(),
            "the config has an unknown field `x`"
        );
        assert_eq!(
            parse_boot(r#"{"version": 2, "argv": 5}"#, 2).unwrap_err(),
            "config field `argv` is not a list of strings"
        );
    }

    #[test]
    fn versions_must_match_the_boot() {
        // A version-1 boot refuses version 2 as it refused any other.
        assert_eq!(
            parse_boot(r#"{"version": 2, "argv": ["a"]}"#, 1).unwrap_err(),
            "the config's version is not one this init supports"
        );
        assert_eq!(
            parse_boot(&unpack_json(&[layer_json(TAR, DIGEST, "1")]), 1).unwrap_err(),
            "the config's version is not one this init supports"
        );
        assert_eq!(
            parse_boot(r#"{"version": 1, "argv": ["a"]}"#, 2).unwrap_err(),
            "the config's version is not the boot's interface version (gh.v)"
        );
        assert_eq!(
            parse_boot(r#"{"version": 3, "argv": ["a"]}"#, 2).unwrap_err(),
            "the config's version is not one this init supports"
        );
        // Version 1 knows no mode and no layers.
        assert_eq!(
            parse_boot(r#"{"version": 1, "mode": "run", "argv": ["a"]}"#, 1).unwrap_err(),
            "the config has an unknown field `mode`"
        );
        assert_eq!(
            parse_boot(r#"{"version": 1, "argv": ["a"], "layers": []}"#, 1).unwrap_err(),
            "the config has an unknown field `layers`"
        );
    }

    fn run_v3(text: &str) -> Result<Config, String> {
        parse_boot(text, 3).map(|boot| match boot {
            Boot::Run(config) => config,
            Boot::Unpack(_) => panic!("not the run mode"),
        })
    }

    #[test]
    fn version_three_init_system() {
        let config = run_v3(r#"{"version": 3, "argv": ["nginx"]}"#).unwrap();
        assert!(!config.init_system);
        assert_eq!(config.stop_signal, 15);
        let config = run_v3(
            r#"{"version": 3, "mode": "run", "argv": ["/sbin/init"], "init_system": true, "stop_signal": 37}"#,
        )
        .unwrap();
        assert!(config.init_system);
        assert_eq!(config.argv, ["/sbin/init"]);
        assert_eq!(config.stop_signal, 37);
        assert!(
            !run_v3(r#"{"version": 3, "argv": ["a"], "init_system": false}"#)
                .unwrap()
                .init_system
        );
        for user in ["root", "0", "0:0", "root:root", "root:0", "0:root"] {
            let text = format!(
                r#"{{"version": 3, "argv": ["/sbin/init"], "init_system": true, "user": "{user}", "workdir": "/"}}"#
            );
            assert!(run_v3(&text).is_ok(), "{user}");
        }
        for user in ["nobody", "1000", "0:1", "root:adm", "1:0", "root "] {
            let text = format!(
                r#"{{"version": 3, "argv": ["/sbin/init"], "init_system": true, "user": "{user}"}}"#
            );
            assert_eq!(
                run_v3(&text).unwrap_err(),
                "config field `user` is not root, which `init_system` requires",
                "{user}"
            );
        }
        assert_eq!(
            run_v3(
                r#"{"version": 3, "argv": ["/sbin/init"], "init_system": true, "workdir": "/srv"}"#
            )
            .unwrap_err(),
            "config field `workdir` is not /, which `init_system` requires"
        );
        assert_eq!(
            run_v3(r#"{"version": 3, "argv": ["init"], "init_system": true}"#).unwrap_err(),
            "config field `argv` does not start with an absolute path, which `init_system` requires"
        );
        // Without init_system, the same fields are v2's run mode.
        assert!(
            run_v3(r#"{"version": 3, "argv": ["init"], "user": "nobody", "workdir": "/srv"}"#)
                .is_ok()
        );
        for value in ["1", r#""true""#, "null", "[]"] {
            let text =
                format!(r#"{{"version": 3, "argv": ["/sbin/init"], "init_system": {value}}}"#);
            assert_eq!(
                run_v3(&text).unwrap_err(),
                "config field `init_system` is not true or false",
                "{value}"
            );
        }
    }

    #[test]
    fn init_system_unknown_before_version_three() {
        assert_eq!(
            parse_text(r#"{"version": 1, "argv": ["/sbin/init"], "init_system": true}"#)
                .unwrap_err(),
            "the config has an unknown field `init_system`"
        );
        for text in [
            r#"{"version": 2, "argv": ["/sbin/init"], "init_system": true}"#,
            r#"{"version": 2, "mode": "run", "argv": ["/sbin/init"], "init_system": false}"#,
        ] {
            assert_eq!(
                parse_boot(text, 2).unwrap_err(),
                "the config has an unknown field `init_system`",
                "{text}"
            );
        }
        // The first unknown field is named, as before version 3.
        assert_eq!(
            parse_boot(
                r#"{"version": 2, "argv": ["a"], "init_system": true, "a": 1}"#,
                2
            )
            .unwrap_err(),
            "the config has an unknown field `a`"
        );
        let text = format!(
            r#"{{"version": 2, "mode": "unpack", "init_system": true, "layers": [{}]}}"#,
            layer_json(TAR, DIGEST, "1")
        );
        assert_eq!(
            parse_boot(&text, 2).unwrap_err(),
            "the config has an unknown field `init_system`"
        );
        let text = format!(
            r#"{{"version": 3, "mode": "unpack", "init_system": true, "layers": [{}]}}"#,
            layer_json(TAR, DIGEST, "1")
        );
        assert_eq!(
            parse_boot(&text, 3).unwrap_err(),
            "config field `init_system` is not taken in the unpack mode"
        );
        let text = format!(
            r#"{{"version": 3, "mode": "unpack", "layers": [{}]}}"#,
            layer_json(TAR, DIGEST, "1")
        );
        assert!(matches!(parse_boot(&text, 3), Ok(Boot::Unpack(_))));
    }

    #[test]
    fn version_three_boot() {
        for (config, boot, error) in [
            (
                2,
                3,
                "the config's version is not the boot's interface version (gh.v)",
            ),
            (
                1,
                3,
                "the config's version is not the boot's interface version (gh.v)",
            ),
            (4, 3, "the config's version is not one this init supports"),
            (3, 2, "the config's version is not one this init supports"),
            (3, 1, "the config's version is not one this init supports"),
        ] {
            let text = format!(r#"{{"version": {config}, "argv": ["a"]}}"#);
            assert_eq!(
                parse_boot(&text, boot).unwrap_err(),
                error,
                "{config} on {boot}"
            );
        }
    }

    #[test]
    fn unpack_mode() {
        let layers = unpack(&unpack_json(&[
            layer_json(TAR, DIGEST, "1"),
            layer_json(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                DIGEST,
                "1024",
            ),
            layer_json(
                "application/vnd.oci.image.layer.v1.tar+zstd",
                DIGEST,
                "1099511627776",
            ),
            layer_json(
                "application/vnd.docker.image.rootfs.diff.tar.gzip",
                DIGEST,
                "1",
            ),
        ]))
        .unwrap();
        let compressions: Vec<_> = layers.iter().map(|layer| layer.compression).collect();
        assert_eq!(
            compressions,
            [
                Compression::None,
                Compression::Gzip,
                Compression::Zstd,
                Compression::Gzip
            ]
        );
        assert_eq!(layers[1].size, 1024);
        assert_eq!(layers[0].digest[0], 0x01);
        assert_eq!(layers[0].digest[31], 0xef);
    }

    #[test]
    fn unpack_refuses_run_fields() {
        for field in [
            r#""argv": ["a"]"#,
            r#""env": []"#,
            r#""user": "root""#,
            r#""workdir": "/""#,
            r#""stop_timeout_seconds": 1"#,
            r#""stop_signal": 15"#,
            r#""authorized_keys": {"user": "root", "keys": []}"#,
        ] {
            let text = format!(
                r#"{{"version": 2, "mode": "unpack", {field}, "layers": [{}]}}"#,
                layer_json(TAR, DIGEST, "1")
            );
            let error = unpack(&text).unwrap_err();
            assert!(
                error.ends_with("is not taken in the unpack mode"),
                "{field}: {error}"
            );
        }
        assert_eq!(
            unpack(r#"{"version": 2, "mode": "unpack", "layers": [], "x": 1}"#).unwrap_err(),
            "the config has an unknown field `x`"
        );
        assert_eq!(
            unpack(r#"{"version": 2, "mode": "unpack"}"#).unwrap_err(),
            "config field `layers` is missing"
        );
    }

    #[test]
    fn layer_count_bounds() {
        let one = layer_json(TAR, DIGEST, "1");
        assert_eq!(
            unpack(&unpack_json(&[])).unwrap_err(),
            "config field `layers` does not hold 1 to 128 layers"
        );
        assert_eq!(
            unpack(&unpack_json(&vec![one.clone(); 128])).unwrap().len(),
            128
        );
        assert_eq!(
            unpack(&unpack_json(&vec![one; 129])).unwrap_err(),
            "config field `layers` does not hold 1 to 128 layers"
        );
        assert_eq!(
            unpack(r#"{"version": 2, "mode": "unpack", "layers": {}}"#).unwrap_err(),
            "config field `layers` is not a list"
        );
    }

    #[test]
    fn layer_fields_checked() {
        for digest in [
            "sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            "sha512:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "sha256:g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "",
        ] {
            assert_eq!(
                unpack(&unpack_json(&[layer_json(TAR, digest, "1")])).unwrap_err(),
                "config field `layers` entry 0 has a digest that is not sha256: and 64 lowercase hex digits",
                "{digest}"
            );
        }
        for media_type in [
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
            "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
            "application/vnd.oci.image.layer.v1.tar+bzip2",
            "application/vnd.oci.image.layer.v1.tar ",
            "",
        ] {
            let text = unpack_json(&[
                layer_json(TAR, DIGEST, "1"),
                layer_json(media_type, DIGEST, "1"),
            ]);
            assert_eq!(
                unpack(&text).unwrap_err(),
                "config field `layers` entry 1 has a media type this init does not unpack",
                "{media_type}"
            );
        }
        assert_eq!(
            unpack(&unpack_json(&[layer_json(TAR, DIGEST, "0")])).unwrap_err(),
            "config field `layers` entry 0 has a size of 0"
        );
        for size in ["-1", "1.5", r#""1""#, "null", "18446744073709551616"] {
            let error = unpack(&unpack_json(&[layer_json(TAR, DIGEST, size)])).unwrap_err();
            assert!(
                error.starts_with("config field `layers` entry 0 is not"),
                "{size}: {error}"
            );
        }
        let text = unpack_json(&[
            layer_json(TAR, DIGEST, "18446744073709551615"),
            layer_json(TAR, DIGEST, "1"),
        ]);
        assert_eq!(
            unpack(&text).unwrap_err(),
            "config field `layers` entry 1 ends beyond any disk"
        );
        let text = unpack_json(&[format!(
            r#"{{"media_type": "{TAR}", "digest": "{DIGEST}"}}"#
        )]);
        assert_eq!(
            unpack(&text).unwrap_err(),
            "config field `layers` entry 0 lacks `size`"
        );
        let text = unpack_json(&[format!(
            r#"{{"media_type": "{TAR}", "digest": "{DIGEST}", "size": 1, "urls": []}}"#
        )]);
        assert_eq!(
            unpack(&text).unwrap_err(),
            "config field `layers` entry 0 has an unknown field `urls`"
        );
        assert_eq!(
            unpack(&unpack_json(&["5".into()])).unwrap_err(),
            "config field `layers` entry 0 is not an object"
        );
    }

    #[test]
    fn defaults() {
        let config = parse_text(r#"{"version": 1, "argv": ["nginx"]}"#).unwrap();
        assert_eq!(config.argv, ["nginx"]);
        assert!(config.env.is_empty());
        assert_eq!(config.user, None);
        assert_eq!(config.workdir, "/");
        assert_eq!(config.stop_timeout_seconds, 10);
        assert_eq!(config.stop_signal, 15);
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
                 "stop_signal": 3,
                 "authorized_keys": { "user": "root", "keys": ["ssh-ed25519 AAAA x"] } }"#,
        )
        .unwrap();
        assert_eq!(config.argv.len(), 3);
        assert_eq!(config.env, ["KEY=value", "EMPTY="]);
        assert_eq!(config.user.as_deref(), Some("101:101"));
        assert_eq!(config.workdir, "/srv");
        assert_eq!(config.stop_timeout_seconds, 300);
        assert_eq!(config.stop_signal, 3);
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
    fn stop_signal_range() {
        let signal = |value: &str| {
            parse_text(&format!(
                r#"{{"version": 1, "argv": ["a"], "stop_signal": {value}}}"#
            ))
            .map(|config| config.stop_signal)
        };
        assert_eq!(signal("1"), Ok(1));
        assert_eq!(signal("3"), Ok(3));
        assert_eq!(signal("64"), Ok(64));
        for value in ["0", "65", "-1"] {
            assert_eq!(
                signal(value).unwrap_err(),
                "config field `stop_signal` is not a signal number from 1 to 64",
                "{value}"
            );
        }
        for value in [r#""SIGQUIT""#, "3.5", "null", "99999999999", "[3]"] {
            assert_eq!(
                signal(value).unwrap_err(),
                "config field `stop_signal` is not a signal number",
                "{value}"
            );
        }
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
            format!(r#"{{"version": 1, "argv": ["a"], "stop_signal": "{SECRET}"}}"#),
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
