//! `surfpool-bench` measures the throughput of a surfpool binary through its
//! public JSON-RPC interface and compares two binaries for regressions.

mod compare;
mod results;
mod server;
mod workloads;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use results::{Direction, HostInfo, Round, RunReport, SCHEMA_VERSION, median};
use workloads::LoadConfig;

/// Exit code when a gated metric regressed past the threshold.
const EXIT_REGRESSION: u8 = 1;
/// Exit code when the benchmark itself failed to run.
const EXIT_ERROR: u8 = 2;

#[derive(Parser)]
#[command(
    name = "surfpool-bench",
    about = "Throughput benchmarks for a surfpool binary over JSON-RPC"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Benchmark one binary and write a result file.
    Run {
        /// Path to the surfpool binary to measure.
        #[arg(long)]
        surfpool_bin: PathBuf,
        /// Name recorded in the result file, e.g. `base` or `head`.
        #[arg(long, default_value = "head")]
        label: String,
        /// Git commit the binary was built from, recorded for reference.
        #[arg(long)]
        commit: Option<String>,
        /// Full passes over the selected scenarios.
        #[arg(long, default_value_t = 1)]
        rounds: u32,
        /// Where to write the JSON result. Rewritten after every round.
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        load: LoadConfig,
    },
    /// Benchmark two binaries in interleaved rounds on this machine, then
    /// compare them. Exits 1 on a regression.
    Ab {
        #[arg(long)]
        base_bin: PathBuf,
        #[arg(long)]
        head_bin: PathBuf,
        #[arg(long)]
        base_commit: Option<String>,
        #[arg(long)]
        head_commit: Option<String>,
        /// Interleaved rounds. The side that goes first alternates each round.
        #[arg(long, default_value_t = 3)]
        rounds: u32,
        /// Directory for `base.json`, `head.json` and `summary.md`.
        #[arg(long)]
        out_dir: PathBuf,
        /// Relative regression on a gated metric that fails the run.
        #[arg(long, default_value_t = 0.15)]
        threshold: f64,
        /// Report the comparison but always exit 0.
        #[arg(long, default_value_t = false)]
        no_fail: bool,
        #[command(flatten)]
        load: LoadConfig,
    },
    /// Compare existing result files. Exits 1 on a regression.
    Compare {
        /// Result files for the base side. Repeatable.
        #[arg(long, required = true)]
        base: Vec<PathBuf>,
        /// Result files for the head side. Repeatable.
        #[arg(long, required = true)]
        head: Vec<PathBuf>,
        #[arg(long, default_value_t = 0.15)]
        threshold: f64,
        /// Write the Markdown report here as well as to stdout.
        #[arg(long)]
        summary: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        no_fail: bool,
    },
    /// Convert result files to the `customBiggerIsBetter` format consumed by
    /// benchmark-action/github-action-benchmark for trend history.
    Export {
        #[arg(long, required = true)]
        input: Vec<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Run {
            surfpool_bin,
            label,
            commit,
            rounds,
            out,
            load,
        } => {
            let mut report = new_report(&surfpool_bin, &label, commit, &load).await?;
            for round in 1..=rounds {
                run_round(&surfpool_bin, &mut report, round, rounds, &load).await?;
                write_json(&out, &report)?;
            }
            eprintln!("wrote {}", out.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Ab {
            base_bin,
            head_bin,
            base_commit,
            head_commit,
            rounds,
            out_dir,
            threshold,
            no_fail,
            load,
        } => {
            std::fs::create_dir_all(&out_dir)
                .with_context(|| format!("creating {}", out_dir.display()))?;
            let base_out = out_dir.join("base.json");
            let head_out = out_dir.join("head.json");
            let mut base = new_report(&base_bin, "base", base_commit, &load).await?;
            let mut head = new_report(&head_bin, "head", head_commit, &load).await?;
            for round in 1..=rounds {
                // Alternate which side goes first so slow thermal or background
                // drift on the machine does not favour one side.
                let base_first = round % 2 == 1;
                if base_first {
                    run_round(&base_bin, &mut base, round, rounds, &load).await?;
                    write_json(&base_out, &base)?;
                }
                run_round(&head_bin, &mut head, round, rounds, &load).await?;
                write_json(&head_out, &head)?;
                if !base_first {
                    run_round(&base_bin, &mut base, round, rounds, &load).await?;
                    write_json(&base_out, &base)?;
                }
            }
            let comparison = compare::compare(&[base], &[head], threshold)?;
            let markdown = comparison.to_markdown();
            std::fs::write(out_dir.join("summary.md"), &markdown)?;
            println!("{markdown}");
            Ok(verdict(comparison.has_regressions(), no_fail))
        }
        Command::Compare {
            base,
            head,
            threshold,
            summary,
            no_fail,
        } => {
            let base = read_reports(&base)?;
            let head = read_reports(&head)?;
            let comparison = compare::compare(&base, &head, threshold)?;
            let markdown = comparison.to_markdown();
            if let Some(path) = summary {
                std::fs::write(&path, &markdown)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            println!("{markdown}");
            Ok(verdict(comparison.has_regressions(), no_fail))
        }
        Command::Export { input, out } => {
            let reports = read_reports(&input)?;
            let entries = export_bigger_is_better(&reports);
            write_json(&out, &entries)?;
            eprintln!("wrote {} entries to {}", entries.len(), out.display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn verdict(regressed: bool, no_fail: bool) -> ExitCode {
    if regressed && !no_fail {
        eprintln!("benchmark regression detected");
        ExitCode::from(EXIT_REGRESSION)
    } else {
        ExitCode::SUCCESS
    }
}

async fn new_report(
    bin: &Path,
    label: &str,
    commit: Option<String>,
    load: &LoadConfig,
) -> Result<RunReport> {
    Ok(RunReport {
        schema_version: SCHEMA_VERSION,
        label: label.to_string(),
        surfpool_version: server::surfpool_version(bin).await?,
        commit,
        started_at: chrono::Utc::now().to_rfc3339(),
        host: HostInfo::current(),
        config: load.clone(),
        rounds: Vec::new(),
    })
}

/// Runs every selected scenario once and appends the round to `report`,
/// logging each scenario's gated metric to stderr as it completes.
async fn run_round(
    bin: &Path,
    report: &mut RunReport,
    round: u32,
    rounds: u32,
    load: &LoadConfig,
) -> Result<()> {
    let mut samples = Vec::new();
    for scenario in load.selected_scenarios() {
        let (scenario_samples, wall) = workloads::run_scenario(bin, scenario, load).await?;
        let headline = scenario_samples
            .iter()
            .filter(|s| s.gate)
            .map(|s| format!("{} {:.1} {}", s.metric, s.value, s.unit))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[{} round {round}/{rounds}] {}: {headline} ({:.1}s wall)",
            report.label,
            scenario.name(),
            wall.as_secs_f64()
        );
        samples.extend(scenario_samples);
    }
    report.rounds.push(Round {
        index: round,
        samples,
    });
    Ok(())
}

#[derive(serde::Serialize)]
struct BenchmarkActionEntry {
    name: String,
    unit: String,
    value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<String>,
}

/// Collapses every higher-is-better metric to its median across all rounds
/// and files. Lower-is-better metrics are left out because the target format
/// carries a single direction per file.
fn export_bigger_is_better(reports: &[RunReport]) -> Vec<BenchmarkActionEntry> {
    let mut by_key: std::collections::BTreeMap<(String, String), (String, Vec<f64>)> =
        Default::default();
    for report in reports {
        for round in &report.rounds {
            for sample in &round.samples {
                if sample.direction != Direction::HigherIsBetter {
                    continue;
                }
                by_key
                    .entry((sample.scenario.clone(), sample.metric.clone()))
                    .or_insert_with(|| (sample.unit.clone(), Vec::new()))
                    .1
                    .push(sample.value);
            }
        }
    }
    by_key
        .into_iter()
        .map(|((scenario, metric), (unit, values))| {
            let min = values.iter().copied().fold(f64::INFINITY, f64::min);
            let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            BenchmarkActionEntry {
                name: format!("{scenario}/{metric}"),
                unit,
                value: median(&values),
                range: (values.len() > 1).then(|| format!("{min:.1}..{max:.1}")),
            }
        })
        .collect()
}

fn read_reports(paths: &[PathBuf]) -> Result<Vec<RunReport>> {
    paths
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        })
        .collect()
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}
