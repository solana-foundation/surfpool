//! NO_DNA agent-mode detection.
//!
//! Implements the [NO_DNA](https://no-dna.org) informal standard for detecting
//! non-human operators (AI agents, automation). When `NO_DNA` is set to any
//! non-empty value the caller signals it is an agent and the CLI drops
//! interactive UX, hides spinners/progress bars, routes log output to stderr,
//! emits JSONL on the console branch, and uses RFC3339 UTC timestamps.
//!
//! Detection follows the spec literally: **set and non-empty**. Truthy parsing
//! (`1|true|yes|on`) is explicitly rejected — `NO_DNA=0` activates agent mode,
//! mirroring [`NO_COLOR`](https://no-color.org) semantics.
//!
//! Color handling honors `NO_COLOR` independently: either env var disables
//! ANSI escapes in console output.

use std::sync::OnceLock;

const ENV_VAR_NO_DNA: &str = "NO_DNA";
const ENV_VAR_NO_COLOR: &str = "NO_COLOR";

static AGENT_MODE: OnceLock<AgentMode> = OnceLock::new();

/// Output format selected by the agent-mode boundary.
///
/// `Json` emits JSONL records on the console branch:
/// `{"ts","level","target","msg"}`. `Human` keeps the legacy human-readable
/// format (used outside NO_DNA).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

/// Color choice for ANSI escapes in console output.
///
/// `Never` is selected when NO_DNA OR NO_COLOR is set. `Always` is reserved
/// for the legacy unconditional-color behavior; `Auto` is the default
/// outside agent mode (currently the same as `Always` per fern's behavior).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorChoice {
    Always,
    Never,
    #[default]
    Auto,
}

/// Timestamp format for log records.
///
/// `Rfc3339Utc` emits e.g. `2026-06-04T16:57:00.123Z` and is selected under
/// NO_DNA. `Local` is the legacy human-readable format used outside agent
/// mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimestampFmt {
    #[default]
    Local,
    Rfc3339Utc,
}

/// Snapshot of the caller-disclosed execution environment.
///
/// Populated once per process from `NO_DNA` + `NO_COLOR`. Consumers should
/// take `&AgentMode`, not bare bools, so future fields do not ripple
/// through call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentMode {
    /// `true` when the caller has signaled non-human operation via `NO_DNA`.
    pub active: bool,
    /// Output format for the console branch of the logger and direct emit sites.
    pub output_format: OutputFormat,
    /// ANSI color choice. `Never` if NO_DNA or NO_COLOR is set.
    pub color: ColorChoice,
    /// Timestamp format for log records.
    pub timestamp_format: TimestampFmt,
    /// Optional floor for log verbosity. `None`.
    pub log_verbosity_floor: Option<log::LevelFilter>,
}

impl Default for AgentMode {
    fn default() -> Self {
        Self::const_inactive()
    }
}

impl AgentMode {
    /// Const constructor for the active-agent-mode preset.
    ///
    /// Useful in tests where the struct must appear in a `const` context;
    /// production code should always call [`from_env`](Self::from_env).
    pub const fn const_active() -> Self {
        Self {
            active: true,
            output_format: OutputFormat::Json,
            color: ColorChoice::Never,
            timestamp_format: TimestampFmt::Rfc3339Utc,
            log_verbosity_floor: None,
        }
    }

    /// Const constructor for the inactive preset.
    pub const fn const_inactive() -> Self {
        Self {
            active: false,
            output_format: OutputFormat::Human,
            color: ColorChoice::Auto,
            timestamp_format: TimestampFmt::Local,
            log_verbosity_floor: None,
        }
    }

    /// Resolve agent mode from the process environment.
    ///
    /// Reads `NO_DNA` and `NO_COLOR` exactly once per process via
    /// [`OnceLock`]; later calls return the cached value. Tests
    /// should call [`from_raw`](Self::from_raw) directly to avoid env
    /// mutation.
    pub fn from_env() -> Self {
        *AGENT_MODE.get_or_init(|| {
            Self::from_raw(
                std::env::var(ENV_VAR_NO_DNA).ok().as_deref(),
                std::env::var(ENV_VAR_NO_COLOR).ok().as_deref(),
            )
        })
    }

    /// Pure parser used by [`from_env`](Self::from_env) and tests.
    ///
    /// - `no_dna = None | Some("")` → inactive. Any other value → active.
    /// - `no_color = Some(non-empty) OR no_dna active` → `ColorChoice::Never`.
    /// - Active → JSON / RFC3339-UTC; inactive → Human / Local.
    pub fn from_raw(no_dna: Option<&str>, no_color: Option<&str>) -> Self {
        let active = no_dna.is_some_and(|v| !v.is_empty());
        let no_color_set = no_color.is_some_and(|v| !v.is_empty());
        Self {
            active,
            output_format: if active {
                OutputFormat::Json
            } else {
                OutputFormat::Human
            },
            color: if active || no_color_set {
                ColorChoice::Never
            } else {
                ColorChoice::Auto
            },
            timestamp_format: if active {
                TimestampFmt::Rfc3339Utc
            } else {
                TimestampFmt::Local
            },
            log_verbosity_floor: None,
        }
    }

    /// `true` when agent mode is engaged.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentMode, ColorChoice, OutputFormat, TimestampFmt};

    #[test]
    fn unset_is_inactive() {
        assert!(!AgentMode::from_raw(None, None).is_active());
    }

    #[test]
    fn empty_string_is_inactive() {
        assert!(!AgentMode::from_raw(Some(""), None).is_active());
    }

    #[test]
    fn one_is_active() {
        assert!(AgentMode::from_raw(Some("1"), None).is_active());
    }

    #[test]
    fn zero_is_active() {
        // NO_COLOR semantics: any non-empty value activates, including "0".
        assert!(AgentMode::from_raw(Some("0"), None).is_active());
    }

    #[test]
    fn arbitrary_value_is_active() {
        assert!(AgentMode::from_raw(Some("anything"), None).is_active());
        assert!(AgentMode::from_raw(Some("false"), None).is_active());
        assert!(AgentMode::from_raw(Some("off"), None).is_active());
    }

    #[test]
    fn whitespace_is_active() {
        // Whitespace counts as non-empty per the spec; we do not trim.
        assert!(AgentMode::from_raw(Some(" "), None).is_active());
        assert!(AgentMode::from_raw(Some("\t"), None).is_active());
    }

    #[test]
    fn default_is_inactive() {
        assert!(!AgentMode::default().is_active());
    }

    #[test]
    fn no_dna_active_populates_all_fields() {
        let m = AgentMode::from_raw(Some("1"), None);
        assert!(m.active);
        assert_eq!(m.output_format, OutputFormat::Json);
        assert_eq!(m.color, ColorChoice::Never);
        assert_eq!(m.timestamp_format, TimestampFmt::Rfc3339Utc);
        assert!(m.log_verbosity_floor.is_none());
    }

    #[test]
    fn no_dna_inactive_populates_defaults() {
        let m = AgentMode::from_raw(None, None);
        assert!(!m.active);
        assert_eq!(m.output_format, OutputFormat::Human);
        assert_eq!(m.color, ColorChoice::Auto);
        assert_eq!(m.timestamp_format, TimestampFmt::Local);
    }

    #[test]
    fn no_color_alone_forces_never() {
        let m = AgentMode::from_raw(None, Some("1"));
        assert!(!m.active, "NO_COLOR must not activate agent mode");
        assert_eq!(m.color, ColorChoice::Never);
        assert_eq!(
            m.output_format,
            OutputFormat::Human,
            "NO_COLOR alone keeps human output"
        );
    }

    #[test]
    fn no_color_empty_does_not_force_never() {
        let m = AgentMode::from_raw(None, Some(""));
        assert_eq!(m.color, ColorChoice::Auto);
    }

    #[test]
    fn no_dna_and_no_color_both_force_never() {
        let m = AgentMode::from_raw(Some("1"), Some("1"));
        assert!(m.active);
        assert_eq!(m.color, ColorChoice::Never);
    }

    #[test]
    fn const_active_matches_from_raw_active() {
        assert_eq!(
            AgentMode::const_active(),
            AgentMode::from_raw(Some("1"), None)
        );
    }

    #[test]
    fn const_inactive_matches_from_raw_unset() {
        assert_eq!(AgentMode::const_inactive(), AgentMode::from_raw(None, None));
    }
}
