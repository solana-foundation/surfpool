//! Compares the result files of two sides, usually the merge-base and the PR
//! head, and decides whether any gated metric regressed past the threshold.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::results::{Direction, RunReport, SCHEMA_VERSION, Sample, median};

/// One metric across both sides, with the per-round values collapsed to a
/// median and range.
#[derive(Debug, Clone)]
pub struct Row {
    pub scenario: String,
    pub metric: String,
    pub unit: String,
    pub base: Option<Stats>,
    pub head: Option<Stats>,
    pub gate: bool,
    pub status: Status,
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub n: usize,
}

impl Stats {
    fn from_values(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        Some(Self {
            median: median(values),
            min: values.iter().copied().fold(f64::INFINITY, f64::min),
            max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            n: values.len(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Gated metric within the threshold.
    Pass,
    /// Gated metric regressed past the threshold.
    Regression,
    /// Reported for diagnosis, never fails the run.
    Info,
    /// Present on only one side, so there is nothing to compare.
    Missing,
}

pub struct Comparison {
    pub rows: Vec<Row>,
    pub threshold: f64,
    pub base_label: String,
    pub head_label: String,
}

impl Comparison {
    /// Whether the gate fails: a gated metric regressed past the threshold,
    /// a gated metric was measured on only one side, or no gated metric was
    /// measured at all. Informational metrics never fail it.
    pub fn fails(&self) -> bool {
        self.rows
            .iter()
            .any(|r| r.gate && matches!(r.status, Status::Regression | Status::Missing))
            || !self.rows.iter().any(|r| r.gate)
    }

    fn verdict(&self) -> &'static str {
        if self.rows.iter().any(|r| r.status == Status::Regression) {
            "❌ **Regression detected**"
        } else if self.fails() {
            "❌ **Incomplete**: a gated metric is missing on one side"
        } else {
            "✅ **No regression**"
        }
    }

    /// Relative change of the head median against the base median, as a
    /// fraction. Positive means head is larger.
    fn change(row: &Row) -> Option<f64> {
        let (base, head) = (row.base?, row.head?);
        if base.median == 0.0 {
            return None;
        }
        Some((head.median - base.median) / base.median)
    }

    /// Renders the comparison as a GitHub-flavored Markdown report.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let verdict = self.verdict();
        out.push_str(&format!(
            "## Surfpool benchmark: `{}` vs `{}`\n\n{verdict}. A gated metric fails when `{}` is more than {:.0}% worse than `{}`. Values are medians over interleaved rounds, with the range in brackets.\n\n",
            self.head_label,
            self.base_label,
            self.head_label,
            self.threshold * 100.0,
            self.base_label,
        ));
        out.push_str(&format!(
            "| Scenario | Metric | {} | {} | Change | |\n|---|---|---:|---:|---:|:--:|\n",
            self.base_label, self.head_label
        ));
        for row in &self.rows {
            let change = Self::change(row)
                .map(|c| format!("{:+.1}%", c * 100.0))
                .unwrap_or_else(|| "n/a".to_string());
            let marker = match row.status {
                Status::Pass => "✅",
                Status::Regression => "❌",
                Status::Info => "ℹ️",
                Status::Missing => "⚠️",
            };
            out.push_str(&format!(
                "| `{}` | `{}` ({}) | {} | {} | {} | {} |\n",
                row.scenario,
                row.metric,
                row.unit,
                fmt_stats(row.base.as_ref(), &row.unit),
                fmt_stats(row.head.as_ref(), &row.unit),
                change,
                marker,
            ));
        }
        out.push_str(
            "\n✅ / ❌ gated metric · ℹ️ informational, never fails the run · ⚠️ missing on one side\n",
        );
        out
    }
}

fn fmt_value(value: f64, unit: &str) -> String {
    if unit == "count" {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn fmt_stats(stats: Option<&Stats>, unit: &str) -> String {
    match stats {
        None => "n/a".to_string(),
        Some(s) if s.n == 1 => fmt_value(s.median, unit),
        Some(s) => format!(
            "{} [{}..{}]",
            fmt_value(s.median, unit),
            fmt_value(s.min, unit),
            fmt_value(s.max, unit)
        ),
    }
}

type Key = (String, String);

/// Row order for the report: scenarios in the order they were measured, the
/// gated metric of each scenario before its informational ones, then by
/// metric name.
fn ordered_keys<'a>(reports: impl Iterator<Item = &'a RunReport>) -> Vec<Key> {
    let mut scenario_order: Vec<String> = Vec::new();
    let mut keys: Vec<(Key, bool)> = Vec::new();
    for report in reports {
        for round in &report.rounds {
            for sample in &round.samples {
                if !scenario_order.contains(&sample.scenario) {
                    scenario_order.push(sample.scenario.clone());
                }
                let key = (sample.scenario.clone(), sample.metric.clone());
                if !keys.iter().any(|(k, _)| *k == key) {
                    keys.push((key, sample.gate));
                }
            }
        }
    }
    keys.sort_by_key(|(key, gate)| {
        (
            scenario_order.iter().position(|s| *s == key.0),
            !gate,
            key.1.clone(),
        )
    });
    keys.into_iter().map(|(key, _)| key).collect()
}

fn collect(reports: &[RunReport]) -> Result<BTreeMap<Key, (Sample, Vec<f64>)>> {
    let mut map: BTreeMap<Key, (Sample, Vec<f64>)> = BTreeMap::new();
    for report in reports {
        if report.schema_version != SCHEMA_VERSION {
            bail!(
                "result file for `{}` has schema version {}, this tool reads {}",
                report.label,
                report.schema_version,
                SCHEMA_VERSION
            );
        }
        for round in &report.rounds {
            for sample in &round.samples {
                map.entry((sample.scenario.clone(), sample.metric.clone()))
                    .or_insert_with(|| (sample.clone(), Vec::new()))
                    .1
                    .push(sample.value);
            }
        }
    }
    Ok(map)
}

/// Builds the comparison. Every file on a side contributes all of its rounds,
/// so interleaved single-round files and one multi-round file are equivalent.
pub fn compare(base: &[RunReport], head: &[RunReport], threshold: f64) -> Result<Comparison> {
    let (Some(first_base), Some(first_head)) = (base.first(), head.first()) else {
        bail!("both sides need at least one result file");
    };
    for report in base.iter().chain(head.iter()) {
        if report.config != first_base.config {
            bail!(
                "result file for `{}` was measured with a different load configuration than the first base file",
                report.label
            );
        }
        if report.rounds.is_empty() {
            bail!("result file for `{}` has no rounds", report.label);
        }
    }
    let base_map = collect(base)?;
    let head_map = collect(head)?;

    let keys = ordered_keys(base.iter().chain(head.iter()));

    let mut rows = Vec::new();
    for key in &keys {
        let template = base_map
            .get(key)
            .or_else(|| head_map.get(key))
            .map(|(s, _)| s)
            .expect("key came from one of the maps");
        let base_stats = base_map.get(key).and_then(|(_, v)| Stats::from_values(v));
        let head_stats = head_map.get(key).and_then(|(_, v)| Stats::from_values(v));
        let status = match (base_stats, head_stats) {
            (Some(b), Some(h)) if template.gate => {
                let regressed = match template.direction {
                    Direction::HigherIsBetter => h.median < b.median * (1.0 - threshold),
                    Direction::LowerIsBetter => h.median > b.median * (1.0 + threshold),
                };
                if regressed {
                    Status::Regression
                } else {
                    Status::Pass
                }
            }
            (Some(_), Some(_)) => Status::Info,
            _ => Status::Missing,
        };
        rows.push(Row {
            scenario: key.0.clone(),
            metric: key.1.clone(),
            unit: template.unit.clone(),
            base: base_stats,
            head: head_stats,
            gate: template.gate,
            status,
        });
    }

    Ok(Comparison {
        rows,
        threshold,
        base_label: first_base.label.clone(),
        head_label: first_head.label.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        results::{HostInfo, Round},
        workloads::LoadConfig,
    };

    fn report(label: &str, values: &[f64]) -> RunReport {
        RunReport {
            schema_version: SCHEMA_VERSION,
            label: label.to_string(),
            surfpool_version: "test".into(),
            commit: None,
            started_at: String::new(),
            host: HostInfo::current(),
            config: LoadConfig {
                duration_secs: 1,
                warmup_secs: 0,
                senders: 1,
                skip_preflight: false,
                readers: 1,
                read_accounts: 1,
                gpa_clients: 1,
                gpa_accounts: 1,
                slot_time_ms: 250,
                scenarios: None,
            },
            rounds: values
                .iter()
                .enumerate()
                .map(|(i, v)| Round {
                    index: i as u32 + 1,
                    samples: vec![
                        Sample::gated("send_tx_default", "tps_confirmed", "tx/s", *v),
                        Sample::info(
                            "send_tx_default",
                            "send_latency_p99_ms",
                            "ms",
                            *v * 10.0,
                            Direction::LowerIsBetter,
                        ),
                    ],
                })
                .collect(),
        }
    }

    #[test]
    fn median_of_rounds_decides_the_gate_not_a_single_outlier() {
        // One slow head round out of three must not fail the gate.
        let base = report("base", &[1000.0, 1010.0, 990.0]);
        let head = report("head", &[1000.0, 500.0, 980.0]);
        let cmp = compare(&[base], &[head], 0.15).unwrap();
        assert!(!cmp.fails());
    }

    #[test]
    fn regression_past_threshold_fails_only_gated_metrics() {
        let base = report("base", &[1000.0]);
        let head = report("head", &[800.0]);
        let cmp = compare(&[base], &[head], 0.15).unwrap();
        assert!(cmp.fails());
        let statuses: Vec<Status> = cmp.rows.iter().map(|r| r.status).collect();
        // The p99 latency row grew by the same 20% but is informational, and
        // the gated row is listed first.
        assert_eq!(statuses, vec![Status::Regression, Status::Info]);
    }

    #[test]
    fn improvement_never_fails() {
        let base = report("base", &[1000.0]);
        let head = report("head", &[2000.0]);
        assert!(!compare(&[base], &[head], 0.15).unwrap().fails());
    }

    #[test]
    fn a_later_file_with_different_settings_is_rejected() {
        // Only the first file on each side used to be checked, so a second
        // base file measured with other settings would be pooled in silently.
        let mut other = report("base", &[5000.0]);
        other.config.senders = 64;
        let base = [report("base", &[1000.0]), other];
        let head = [report("head", &[1000.0])];
        assert!(compare(&base, &head, 0.15).is_err());
    }

    #[test]
    fn a_gated_metric_missing_on_one_side_fails() {
        let base = report("base", &[1000.0]);
        let mut head = report("head", &[1000.0]);
        for round in &mut head.rounds {
            round.samples.retain(|s| !s.gate);
        }
        let cmp = compare(&[base], &[head], 0.15).unwrap();
        assert!(cmp.fails());
        assert!(cmp.to_markdown().contains("Incomplete"));
    }

    #[test]
    fn a_file_without_rounds_is_rejected() {
        let base = report("base", &[1000.0]);
        let head = report("head", &[]);
        assert!(compare(&[base], &[head], 0.15).is_err());
    }

    #[test]
    fn interleaved_single_round_files_aggregate_per_side() {
        let base = [report("base", &[1000.0]), report("base", &[1100.0])];
        let head = [report("head", &[1050.0]), report("head", &[1000.0])];
        let cmp = compare(&base, &head, 0.15).unwrap();
        let tps = cmp
            .rows
            .iter()
            .find(|r| r.metric == "tps_confirmed")
            .unwrap();
        assert_eq!(tps.base.unwrap().n, 2);
        assert_eq!(tps.head.unwrap().n, 2);
    }
}
