//! The JSON lines between the init and the host (interface version 1):
//! the reports the init sends, the commands it accepts and its answers.

use crate::config::MAX_STOP_TIMEOUT_SECONDS;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The interface version every line carries as `v`.
pub const VERSION: u32 = 1;

/// Why the process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// It ended by itself.
    Exited,
    /// It ended after a stop command.
    Stopped,
    /// The init could not start it.
    Error,
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
            v: VERSION,
            event: Event::Started { pid, monotonic_ms },
        }
    }

    pub fn exited(code: Option<i32>, signal: Option<i32>, reason: Reason) -> Report {
        Report {
            v: VERSION,
            event: Event::Exited {
                code,
                signal,
                reason,
                message: None,
            },
        }
    }

    pub fn error(message: &str) -> Report {
        Report {
            v: VERSION,
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
            String::from(r#"{"v":1,"event":"exited","code":null,"signal":null,"reason":"error","message":"report encoding failed"}"#)
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

/// Decodes one command line; the error is the answer's `error`.
pub fn decode_command(line: &str) -> Result<Command, &'static str> {
    let value: Value = serde_json::from_str(line).map_err(|_| "the line is not a JSON object")?;
    if !value.is_object() {
        return Err("the line is not a JSON object");
    }
    if value.get("v").and_then(Value::as_u64) != Some(u64::from(VERSION)) {
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

/// The answer line to a command, without its newline.
pub fn encode_answer(result: Result<(), &str>) -> String {
    let answer = Answer {
        v: VERSION,
        ok: result.is_ok(),
        error: result.err().map(str::to_owned),
    };
    serde_json::to_string(&answer).unwrap_or_else(|_| String::from(r#"{"v":1,"ok":false}"#))
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
            decode_command(r#"{"v":1,"command":"stop","timeout_seconds":20}"#),
            Ok(Command::Stop {
                timeout_seconds: Some(20)
            })
        );
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop"}"#),
            Ok(Command::Stop {
                timeout_seconds: None
            })
        );
        // The timeout is capped.
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop","timeout_seconds":99999999999}"#),
            Ok(Command::Stop {
                timeout_seconds: Some(MAX_STOP_TIMEOUT_SECONDS)
            })
        );
    }

    #[test]
    fn bad_commands() {
        assert_eq!(
            decode_command(r#"{"v":2,"command":"stop"}"#),
            Err("unsupported interface version")
        );
        assert_eq!(
            decode_command(r#"{"command":"stop"}"#),
            Err("unsupported interface version")
        );
        assert_eq!(
            decode_command(r#"{"v":1,"command":"reboot"}"#),
            Err("unknown command")
        );
        assert_eq!(decode_command(r#"{"v":1}"#), Err("unknown command"));
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop","force":true}"#),
            Err("invalid stop command")
        );
        assert_eq!(
            decode_command(r#"{"v":1,"command":"stop","timeout_seconds":-1}"#),
            Err("invalid stop command")
        );
        assert_eq!(decode_command("stop"), Err("the line is not a JSON object"));
        assert_eq!(decode_command("[1]"), Err("the line is not a JSON object"));
    }

    #[test]
    fn answers() {
        assert_eq!(encode_answer(Ok(())), r#"{"v":1,"ok":true}"#);
        assert_eq!(
            encode_answer(Err("unknown command")),
            r#"{"v":1,"ok":false,"error":"unknown command"}"#
        );
        let decoded: Answer = serde_json::from_str(&encode_answer(Err("x"))).unwrap();
        assert_eq!(decoded.error.as_deref(), Some("x"));
    }
}
