//! getProgramAccounts throughput over a program with a few thousand accounts,
//! the shape Anchor test suites and indexers hit surfpool with. Any failed
//! request aborts the scenario.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_signer::Signer;
use tokio::task::JoinSet;

use super::{LoadConfig, seed_accounts};
use crate::results::{Direction, Sample, percentile, to_millis};

/// Size of each seeded account's data.
const ACCOUNT_DATA_LEN: usize = 128;

pub async fn run(rpc_url: &str, scenario: &str, cfg: &LoadConfig) -> Result<Vec<Sample>> {
    let client = Arc::new(RpcClient::new_with_commitment(
        rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    ));
    let program = Keypair::new().pubkey();
    let keys: Vec<Pubkey> = (0..cfg.gpa_accounts)
        .map(|_| Keypair::new().pubkey())
        .collect();
    seed_accounts(
        &client,
        &keys,
        &program,
        1_000_000_000,
        &[0x5A; ACCOUNT_DATA_LEN],
    )
    .await
    .context("seeding program accounts")?;

    let found = client
        .get_program_ui_accounts_with_config(&program, request_config())
        .await
        .context("getProgramAccounts probe")?
        .len();
    if found != cfg.gpa_accounts {
        bail!(
            "getProgramAccounts returned {found} accounts, expected {}",
            cfg.gpa_accounts
        );
    }

    if cfg.warmup_secs > 0 {
        drive(
            &client,
            &program,
            cfg.gpa_clients,
            Duration::from_secs(cfg.warmup_secs),
        )
        .await
        .context("warm-up")?;
    }
    let duration = Duration::from_secs(cfg.duration_secs);
    let outcome = drive(&client, &program, cfg.gpa_clients, duration).await?;
    Ok(outcome.samples(scenario, duration))
}

fn request_config() -> RpcProgramAccountsConfig {
    RpcProgramAccountsConfig {
        filters: None,
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: None,
            commitment: Some(CommitmentConfig::confirmed()),
            min_context_slot: None,
        },
        with_context: None,
        sort_results: None,
    }
}

#[derive(Default)]
struct Outcome {
    requests: u64,
    latencies: Vec<Duration>,
}

impl Outcome {
    fn samples(&self, scenario: &str, window: Duration) -> Vec<Sample> {
        let ms = to_millis(&self.latencies);
        vec![
            Sample::gated(
                scenario,
                "rps",
                "req/s",
                self.requests as f64 / window.as_secs_f64(),
            ),
            Sample::info(
                scenario,
                "latency_p50_ms",
                "ms",
                percentile(&ms, 0.50),
                Direction::LowerIsBetter,
            ),
            Sample::info(
                scenario,
                "latency_p99_ms",
                "ms",
                percentile(&ms, 0.99),
                Direction::LowerIsBetter,
            ),
        ]
    }
}

async fn drive(
    client: &Arc<RpcClient>,
    program: &Pubkey,
    clients: usize,
    duration: Duration,
) -> Result<Outcome> {
    let deadline = Instant::now() + duration;
    let mut tasks = JoinSet::new();
    for _ in 0..clients {
        let client = client.clone();
        let program = *program;
        tasks.spawn(async move {
            let mut out = Outcome::default();
            while Instant::now() < deadline {
                let t0 = Instant::now();
                client
                    .get_program_ui_accounts_with_config(&program, request_config())
                    .await
                    .context("getProgramAccounts")?;
                out.latencies.push(t0.elapsed());
                out.requests += 1;
            }
            anyhow::Ok(out)
        });
    }
    let mut total = Outcome::default();
    while let Some(joined) = tasks.join_next().await {
        let out = joined.context("gPA task panicked")??;
        total.requests += out.requests;
        total.latencies.extend(out.latencies);
    }
    Ok(total)
}
