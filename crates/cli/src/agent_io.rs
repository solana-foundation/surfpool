//! Agent-mode I/O: JSONL emit for direct print sites under NO_DNA.
//!
//! Surfpool has two ways of producing user-visible output:
//!
//! 1. **Logger calls** (`info!`/`warn!`/`error!`) — flow through `fern` and
//!    are formatted by the dispatch's format closure (see [`crate::cli::setup_logger`]).
//! 2. **Direct prints** (`println!`/`eprintln!`) — bypass the logger entirely.
//!
//! Under NO_DNA, both paths must produce the same JSONL contract.
//! The logger path is handled by the format closure; this module
//! handles direct prints via [`cli_emit`] and the `cli_info!`/`cli_warn!`/
//! `cli_error!` macros.
//!
//! The JSON shape is `{"ts","level","target","msg"}`
//! - `ts` — RFC3339 UTC milliseconds
//! - `level` — lowercase string (`"trace"`/`"debug"`/`"info"`/`"warn"`/`"error"`)
//! - `target` — pseudo-event tag (caller-provided, e.g. `"surfpool.update.download"`)
//! - `msg` — `serde_json`-escaped string of the original human text
//!
//! Outside NO_DNA, `cli_emit` routes to `println!` (info/debug/trace) or
//! `eprintln!` (warn/error) — i.e. the legacy split human callers expect.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::no_dna::{AgentMode, OutputFormat};

/// Format an agent-mode JSON record at the given timestamp.
pub fn format_agent_json_at(ts: DateTime<Utc>, level: &str, target: &str, msg: &str) -> String {
    format_agent_json_with_data_at(ts, level, target, msg, None)
}

/// Like [`format_agent_json_at`] but optionally includes a `data` object for
/// structured payloads (used by `surfpool ls` to attach runbook metadata).
pub fn format_agent_json_with_data_at(
    ts: DateTime<Utc>,
    level: &str,
    target: &str,
    msg: &str,
    data: Option<serde_json::Value>,
) -> String {
    let mut obj = serde_json::Map::with_capacity(5);
    obj.insert(
        "ts".into(),
        serde_json::Value::String(ts.to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    obj.insert("level".into(), serde_json::Value::String(level.to_string()));
    obj.insert(
        "target".into(),
        serde_json::Value::String(target.to_string()),
    );
    obj.insert("msg".into(), serde_json::Value::String(msg.to_string()));
    if let Some(d) = data {
        obj.insert("data".into(), d);
    }
    serde_json::Value::Object(obj).to_string()
}

/// Write a single JSONL line to stderr in agent-mode shape.
///
/// Bypasses the `log` crate so this works even before `setup_logger` has
/// installed the fern dispatch.
pub fn agent_emit(level: &str, target: &str, msg: &str) {
    eprintln!("{}", format_agent_json_at(Utc::now(), level, target, msg));
}

/// Like [`agent_emit`] but attaches a `data` payload. Used by `surfpool ls`
/// to emit structured runbook records.
pub fn agent_emit_with_data(level: &str, target: &str, msg: &str, data: serde_json::Value) {
    eprintln!(
        "{}",
        format_agent_json_with_data_at(Utc::now(), level, target, msg, Some(data))
    );
}

/// Emit a CLI line, branching on agent mode.
///
/// Under NO_DNA: JSON to stderr via [`agent_emit`].
/// Outside NO_DNA: plain text — `stderr` for `warn`/`error`, `stdout` for
/// everything else (preserves the legacy split humans expect).
pub fn cli_emit(level: &str, target: &str, msg: &str) {
    let mode = AgentMode::from_env();
    if mode.output_format == OutputFormat::Json {
        agent_emit(level, target, msg);
    } else if matches!(level, "warn" | "error") {
        eprintln!("{msg}");
    } else {
        println!("{msg}");
    }
}

/// `trace`-level direct emit. See [`cli_emit`].
#[macro_export]
macro_rules! cli_trace {
    ($target:literal, $($arg:tt)*) => {
        $crate::agent_io::cli_emit("trace", $target, &format!($($arg)*))
    };
}

/// `debug`-level direct emit. See [`cli_emit`].
#[macro_export]
macro_rules! cli_debug {
    ($target:literal, $($arg:tt)*) => {
        $crate::agent_io::cli_emit("debug", $target, &format!($($arg)*))
    };
}

/// `info`-level direct emit. See [`cli_emit`].
#[macro_export]
macro_rules! cli_info {
    ($target:literal, $($arg:tt)*) => {
        $crate::agent_io::cli_emit("info", $target, &format!($($arg)*))
    };
}

/// `warn`-level direct emit. See [`cli_emit`].
#[macro_export]
macro_rules! cli_warn {
    ($target:literal, $($arg:tt)*) => {
        $crate::agent_io::cli_emit("warn", $target, &format!($($arg)*))
    };
}

/// `error`-level direct emit. See [`cli_emit`].
#[macro_export]
macro_rules! cli_error {
    ($target:literal, $($arg:tt)*) => {
        $crate::agent_io::cli_emit("error", $target, &format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    // agent_emit output parses as JSON via serde_json;
    // all required fields present; msg escaped.
    #[test]
    fn format_agent_json_is_valid_json() {
        let ts = Utc.with_ymd_and_hms(2026, 6, 4, 17, 30, 0).unwrap();
        let s = format_agent_json_at(ts, "info", "surfpool.test", "hello world");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        assert_eq!(parsed["level"], "info");
        assert_eq!(parsed["target"], "surfpool.test");
        assert_eq!(parsed["msg"], "hello world");
        assert!(parsed["ts"].is_string());
    }

    #[test]
    fn format_agent_json_escapes_msg() {
        let ts = Utc.with_ymd_and_hms(2026, 6, 4, 17, 30, 0).unwrap();
        let s = format_agent_json_at(ts, "error", "surfpool.test", "line1\nline2 \"quoted\"");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        assert_eq!(parsed["msg"], "line1\nline2 \"quoted\"");
        // Raw string must NOT contain bare newlines or unescaped quotes.
        assert!(!s.contains("line1\nline2"), "newlines must be escaped");
    }

    #[test]
    fn timestamp_is_rfc3339_utc_with_z_suffix() {
        let ts = Utc.with_ymd_and_hms(2026, 6, 4, 17, 30, 0).unwrap();
        let s = format_agent_json_at(ts, "info", "t", "m");
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        let ts_str = parsed["ts"].as_str().unwrap();
        assert!(
            ts_str.ends_with('Z'),
            "RFC3339 UTC must end with Z, got {ts_str}"
        );
        assert!(
            ts_str.starts_with("2026-06-04T17:30:00"),
            "expected fixed timestamp, got {ts_str}"
        );
        // Re-parse to confirm round-trip
        let _: DateTime<Utc> = ts_str.parse().expect("must round-trip parse");
    }

    #[test]
    fn agent_json_contains_no_ansi_escapes() {
        let ts = Utc.with_ymd_and_hms(2026, 6, 4, 17, 30, 0).unwrap();
        // Pass a message that COULD contain colors if we accidentally let them through.
        let s = format_agent_json_at(ts, "warn", "surfpool.test", "plain msg");
        assert!(
            !s.contains('\x1b'),
            "JSON output must contain no ANSI ESC byte (0x1b)"
        );
    }

    #[test]
    fn format_agent_json_handles_unicode_in_msg() {
        let ts = Utc.with_ymd_and_hms(2026, 6, 4, 17, 30, 0).unwrap();
        let s = format_agent_json_at(ts, "info", "t", "✓ done — 日本語");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        assert_eq!(parsed["msg"], "✓ done — 日本語");
    }
}
