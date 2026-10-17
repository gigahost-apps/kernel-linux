//! The JSON lines between the init and the host (interface versions 1
//! and 2): the reports the init sends, the commands it accepts and its
//! answers.

use crate::config::MAX_STOP_TIMEOUT_SECONDS;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};

/// The interface versions whose lines this init speaks.
pub const VERSIONS: &[u32] = &[1, 2];

/// The boot's interface version (`gh.v`), which every line carries as
/// `v`; 1 until the command line names a supported one.
static VERSION: AtomicU32 = AtomicU32::new(1);

/// Sets the version every later line carries, when it is one of
/// [`VERSIONS`].
pub fn set_version(version: u32) {
    if VERSIONS.contains(&version) {
        VERSION.store(version, Ordering::SeqCst);
    }
}

/// The boot's interface version.
pub fn version() -> u32 {
    VERSION.load(Ordering::SeqCst)
}

/// Why the process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// It ended by itself.
    Exited,
    /// It ended after a stop command.
    Stopped,
    /// The init could not start it (or, in the unpack mode, could not
    /// unpack the layers).
    Error,
    /// The unpack mode (version 2) unpacked every layer.
    Unpacked,
}

/// One report, sent once per event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub v: u32,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    Started {
        pid: i32,
        monotonic_ms: u64,
    },
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        reason: Reason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl Report {
    pub fn started(pid: i32, monotonic_ms: u64) -> Report {
        Report {
            v: version(),
            event: Event::Started { pid, monotonic_ms },
        }
    }

    pub fn exited(code: Option<i32>, signal: Option<i32>, reason: Reason) -> Report {
        Report {
            v: version(),
            event: Event::Exited {
                code,
                signal,
                reason,
                message: None,
            },
        }
    }

    /// The unpack mode's end: every layer is on the root disk.
    pub fn unpacked() -> Report {
        Report::exited(Some(0), None, Reason::Unpacked)
    }

    pub fn error(message: &str) -> Report {
        Report {
            v: version(),
            event: Event::Exited {
                code: None,
                signal: None,
                reason: Reason::Error,
                message: Some(message.to_owned()),
            },
        }
    }

    /// The line, without its newline.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!(
                r#"{{"v":{},"event":"exited","code":null,"signal":null,"reason":"error","message":"report encoding failed"}}"#,
                self.v
            )
        })
    }
}

/// A command from the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Stop the process: SIGTERM, then everything is killed after the
    /// timeout (capped at [`MAX_STOP_TIMEOUT_SECONDS`]; the config's
    /// `stop_timeout_seconds` when absent).
    Stop { timeout_seconds: Option<u32> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopLine {
    #[allow(dead_code, reason = "checked before the typed parse")]
    v: u64,
    #[allow(dead_code, reason = "checked before the typed parse")]
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

/// Decodes one command line of the boot's interface `version`; the error
/// is the answer's `error`.
pub fn decode_command(line: &str, version: u32) -> Result<Command, &'static str> {
    let value: Value = serde_json::from_str(line).map_err(|_| "the line is not a JSON object")?;
    if !value.is_object() {
        return Err("the line is not a JSON object");
    }
    if value.get("v").and_then(Value::as_u64) != Some(u64::from(version)) {
        return Err("unsupported interface version");
    }
    match value.get("command").and_then(Value::as_str) {
        Some("stop") => {}
        _ => return Err("unknown command"),
    }
    let stop = StopLine::deserialize(&value).map_err(|_| "invalid stop command")?;
    let timeout_seconds = stop.timeout_seconds.map(|seconds| {
        u32::try_from(seconds)
            .unwrap_or(u32::MAX)
            .min(MAX_STOP_TIMEOUT_SECONDS)
    });
    Ok(Command::Stop { timeout_seconds })
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Answer {
    pub v: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The answer line to a command, in the boot's interface `version`,
/// without its newline.
pub fn encode_answer(result: Result<(), &str>, version: u32) -> String {
    let answer = Answer {
        v: version,
        ok: result.is_ok(),
        error: result.err().map(str::to_owned),
    };
    serde_json::to_string(&answer).unwrap_or_else(|_| format!(r#"{{"v":{version},"ok":false}}"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_encode_as_documented() {
        assert_eq!(
            Report::started(2, 871).encode(),
            r#"{"v":1,"event":"started","pid":2,"monotonic_ms":871}"#
        );
        assert_eq!(
            Report::exited(Some(3), None, Reason::Exited).encode(),
            r#"{"v":1,"event":"exited","code":3,"signal":null,"reason":"exited"}"#
        );
        assert_eq!(
            Report::exited(None, Some(9), Reason::Exited).encode(),
            r#"{"v":1,"event":"exited","code":null,"signal":9,"reason":"exited"}"#
        );
        assert_eq!(
            Report::exited(None, Some(15), Reason::Stopped).encode(),
            r#"{"v":1,"event":"exited","code":null,"signal":15,"reason":"stopped"}"#
        );
        assert_eq!(
            Report::error("no \"disk\"").encode(),
            r#"{"v":1,"event":"exited","code":null,"signal":null,"reason":"error","message":"no \"disk\""}"#
        );
    }

    #[test]
    fn reports_decode() {
        for report in [
            Report::started(2, 871),
            Report::exited(Some(0), None, Reason::Exited),
            Report::exited(None, Some(15), Reason::Stopped),
            Report::error("x"),
        ] {
            let decoded: Report = serde_json::from_str(&report.encode()).unwrap();
            assert_eq!(decoded, report);
        }
    }

    #[test]
    fn stop_commands() {
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop","timeout_seconds":20}"#, 1),
            Ok(Command::Stop {
                timeout_seconds: Some(20)
            })
        );
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop"}"#, 1),
            Ok(Command::Stop {
                timeout_seconds: None
            })
        );
        // The timeout is capped.
        assert_eq!(
            decode_command(
                r#"{"v":1,"command":"stop","timeout_seconds":99999999999}"#,
                1
            ),
            Ok(Command::Stop {
                timeout_seconds: Some(MAX_STOP_TIMEOUT_SECONDS)
            })
        );
    }

    #[test]
    fn bad_commands() {
        assert_eq!(
            decode_command(r#"{"v":2,"command":"stop"}"#, 1),
            Err("unsupported interface version")
        );
        assert_eq!(
            decode_command(r#"{"command":"stop"}"#, 1),
            Err("unsupported interface version")
        );
        assert_eq!(
            decode_command(r#"{"v":1,"command":"reboot"}"#, 1),
            Err("unknown command")
        );
        assert_eq!(decode_command(r#"{"v":1}"#, 1), Err("unknown command"));
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop","force":true}"#, 1),
            Err("invalid stop command")
        );
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop","timeout_seconds":-1}"#, 1),
            Err("invalid stop command")
        );
        assert_eq!(
            decode_command("stop", 1),
            Err("the line is not a JSON object")
        );
        assert_eq!(
            decode_command("[1]", 1),
            Err("the line is not a JSON object")
        );
    }

    #[test]
    fn version_two_lines() {
        let report = Report {
            v: 2,
            ..Report::unpacked()
        };
        assert_eq!(
            report.encode(),
            r#"{"v":2,"event":"exited","code":0,"signal":null,"reason":"unpacked"}"#
        );
        assert_eq!(
            decode_command(r#"{"v":2,"command":"stop"}"#, 2),
            Ok(Command::Stop {
                timeout_seconds: None
            })
        );
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop"}"#, 2),
            Err("unsupported interface version")
        );
        assert_eq!(encode_answer(Ok(()), 2), r#"{"v":2,"ok":true}"#);
    }

    #[test]
    fn answers() {
        assert_eq!(encode_answer(Ok(()), 1), r#"{"v":1,"ok":true}"#);
        assert_eq!(
            encode_answer(Err("unknown command"), 1),
            r#"{"v":1,"ok":false,"error":"unknown command"}"#
        );
        let decoded: Answer = serde_json::from_str(&encode_answer(Err("x"), 1)).unwrap();
        assert_eq!(decoded.error.as_deref(), Some("x"));
    }
}
