//! The scenarios the benchmark measures. Each one starts a fresh surfpool
//! process, seeds whatever state it needs through public RPC methods, runs an
//! unmeasured warm-up, then measures for a fixed duration.

mod gpa;
mod reads;
mod send_tx;

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::request::RpcRequest;
use tokio::task::JoinSet;

use crate::{
    results::Sample,
    server::{ServerOptions, SurfpoolServer},
};

/// Load shape shared by every scenario. Recorded in the result file so two
/// runs can only be compared when they used the same shape.
#[derive(clap::Args, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LoadConfig {
    /// Seconds of measured load per scenario.
    #[arg(long, default_value_t = 10)]
    pub duration_secs: u64,

    /// Seconds of unmeasured load before each measurement, so caches and
    /// allocators settle on the same footing for both sides of a comparison.
    #[arg(long, default_value_t = 2)]
    pub warmup_secs: u64,

    /// Concurrent sendTransaction clients. Each has its own funded payer.
    #[arg(long, default_value_t = 16)]
    pub senders: usize,

    /// Send with skipPreflight. Off by default because that is what Solana
    /// clients do unless told otherwise.
    #[arg(long, default_value_t = false)]
    pub skip_preflight: bool,

    /// Concurrent clients for the read-RPC scenario.
    #[arg(long, default_value_t = 16)]
    pub readers: usize,

    /// Accounts seeded for the read-RPC scenario.
    #[arg(long, default_value_t = 2000)]
    pub read_accounts: usize,

    /// Concurrent clients for the getProgramAccounts scenario.
    #[arg(long, default_value_t = 4)]
    pub gpa_clients: usize,

    /// Accounts owned by the benchmark program in the getProgramAccounts
    /// scenario.
    #[arg(long, default_value_t = 3000)]
    pub gpa_accounts: usize,

    /// Slot time passed to `surfpool start --slot-time`.
    #[arg(long, default_value_t = 250)]
    pub slot_time_ms: u64,

    /// Scenarios to run, comma separated. Defaults to all of them.
    #[arg(long, value_delimiter = ',')]
    pub scenarios: Option<Vec<ScenarioId>>,
}

impl LoadConfig {
    pub fn selected_scenarios(&self) -> Vec<ScenarioId> {
        self.scenarios
            .clone()
            .unwrap_or_else(|| ScenarioId::ALL.to_vec())
    }
}

/// Identifies a scenario in the CLI, the result file, and the comparison.
#[derive(ValueEnum, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum ScenarioId {
    /// Transfers from many payers to fresh recipients, instruction profiling
    /// on as it is by default.
    SendTxDefault,
    /// The same transfers with `--disable-instruction-profiling`.
    SendTxNoProfiling,
    /// getAccountInfo, getBalance and getMultipleAccounts over seeded accounts.
    Reads,
    /// getProgramAccounts with base64 encoding over a seeded program.
    Gpa,
}

impl ScenarioId {
    pub const ALL: [ScenarioId; 4] = [
        ScenarioId::SendTxDefault,
        ScenarioId::SendTxNoProfiling,
        ScenarioId::Reads,
        ScenarioId::Gpa,
    ];

    /// The name used for this scenario in result files and reports.
    pub fn name(&self) -> &'static str {
        match self {
            ScenarioId::SendTxDefault => "send_tx_default",
            ScenarioId::SendTxNoProfiling => "send_tx_no_profiling",
            ScenarioId::Reads => "reads",
            ScenarioId::Gpa => "gpa",
        }
    }

    fn server_options(&self, cfg: &LoadConfig) -> ServerOptions {
        ServerOptions {
            instruction_profiling: !matches!(self, ScenarioId::SendTxNoProfiling),
            slot_time_ms: cfg.slot_time_ms,
        }
    }
}

/// Runs one scenario against a fresh process of `bin` and returns its samples
/// along with the wall time the whole scenario took, including startup and
/// seeding.
pub async fn run_scenario(
    bin: &Path,
    scenario: ScenarioId,
    cfg: &LoadConfig,
) -> Result<(Vec<Sample>, Duration)> {
    let started = Instant::now();
    let server = SurfpoolServer::start(bin, &scenario.server_options(cfg))
        .await
        .with_context(|| format!("starting surfpool for {}", scenario.name()))?;
    let result = match scenario {
        ScenarioId::SendTxDefault | ScenarioId::SendTxNoProfiling => {
            send_tx::run(&server.rpc_url, scenario.name(), cfg).await
        }
        ScenarioId::Reads => reads::run(&server.rpc_url, scenario.name(), cfg).await,
        ScenarioId::Gpa => gpa::run(&server.rpc_url, scenario.name(), cfg).await,
    };
    server.stop().await?;
    let samples = result.with_context(|| format!("running scenario {}", scenario.name()))?;
    Ok((samples, started.elapsed()))
}

/// How many `surfnet_setAccount` calls to keep in flight while seeding.
const SEED_CONCURRENCY: usize = 32;

/// Creates every account in `keys` with the same lamports, owner and data via
/// the `surfnet_setAccount` cheatcode, which is how surfpool users seed state.
pub async fn seed_accounts(
    client: &Arc<RpcClient>,
    keys: &[Pubkey],
    owner: &Pubkey,
    lamports: u64,
    data: &[u8],
) -> Result<()> {
    let data_hex = Arc::new(hex::encode(data));
    let owner = owner.to_string();
    let mut tasks = JoinSet::new();
    let chunk_len = keys.len().div_ceil(SEED_CONCURRENCY).max(1);
    for chunk in keys.chunks(chunk_len) {
        let client = client.clone();
        let chunk = chunk.to_vec();
        let owner = owner.clone();
        let data_hex = data_hex.clone();
        tasks.spawn(async move {
            for key in chunk {
                let params = serde_json::json!([
                    key.to_string(),
                    { "lamports": lamports, "data": *data_hex, "owner": owner }
                ]);
                client
                    .send::<serde_json::Value>(
                        RpcRequest::Custom {
                            method: "surfnet_setAccount",
                        },
                        params,
                    )
                    .await
                    .context("surfnet_setAccount")?;
            }
            Ok::<_, anyhow::Error>(())
        });
    }
    while let Some(joined) = tasks.join_next().await {
        joined.context("seeding task panicked")??;
    }
    Ok(())
}
