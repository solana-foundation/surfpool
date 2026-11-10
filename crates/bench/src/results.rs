//! On-disk result format shared by every subcommand, plus the small statistics
//! helpers the workloads and the comparison use.

use serde::{Deserialize, Serialize};

use crate::workloads::LoadConfig;

/// Version of the JSON layout written by `run` and `ab`. Bump it when a field
/// changes meaning so `compare` can refuse to mix incompatible files.
pub const SCHEMA_VERSION: u32 = 1;

/// Everything one invocation of `run` measured against one surfpool binary.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RunReport {
    pub schema_version: u32,
    /// Free-form name for this side of a comparison, usually `base` or `head`.
    pub label: String,
    /// Output of `surfpool --version` for the binary under test.
    pub surfpool_version: String,
    /// Git commit the binary was built from, when the caller knows it.
    pub commit: Option<String>,
    /// RFC 3339 timestamp of when the run started.
    pub started_at: String,
    pub host: HostInfo,
    pub config: LoadConfig,
    pub rounds: Vec<Round>,
}

/// Machine the load driver and the surfpool process ran on.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HostInfo {
    pub os: String,
    pub arch: String,
    pub cpus: usize,
}

impl HostInfo {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
        }
    }
}

/// One pass over every selected scenario, each against a fresh surfpool process.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Round {
    pub index: u32,
    pub samples: Vec<Sample>,
}

/// Whether a larger value of a metric is an improvement.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    HigherIsBetter,
    LowerIsBetter,
}

/// A single measured value. `gate` marks the metrics whose regression fails CI;
/// the others are reported for diagnosis only.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sample {
    pub scenario: String,
    pub metric: String,
    pub unit: String,
    pub value: f64,
    pub direction: Direction,
    pub gate: bool,
}

impl Sample {
    pub fn gated(scenario: &str, metric: &str, unit: &str, value: f64) -> Self {
        Self {
            scenario: scenario.to_string(),
            metric: metric.to_string(),
            unit: unit.to_string(),
            value,
            direction: Direction::HigherIsBetter,
            gate: true,
        }
    }

    pub fn info(
        scenario: &str,
        metric: &str,
        unit: &str,
        value: f64,
        direction: Direction,
    ) -> Self {
        Self {
            scenario: scenario.to_string(),
            metric: metric.to_string(),
            unit: unit.to_string(),
            value,
            direction,
            gate: false,
        }
    }
}

/// Returns the `p` quantile (0.0..=1.0) of `values` by nearest-rank, or 0.0
/// when there are no values. Sorts a copy, so the input order does not matter.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Returns the median of `values`, averaging the two middle values for an even
/// count, or 0.0 when there are no values.
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

/// Converts a `Duration` sample list to milliseconds for percentile reporting.
pub fn to_millis(durations: &[std::time::Duration]) -> Vec<f64> {
    durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect()
}
