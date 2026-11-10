//! Read-RPC throughput: concurrent clients mix getAccountInfo, getBalance and
//! getMultipleAccounts over a pool of seeded accounts. Any failed request aborts
//! the scenario.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_signer::Signer;
use tokio::task::JoinSet;

use super::{LoadConfig, seed_accounts};
use crate::results::{Direction, Sample, percentile, to_millis};

/// Size of each seeded account's data, the size of an SPL token account.
const ACCOUNT_DATA_LEN: usize = 165;
/// Keys per getMultipleAccounts request.
const MULTI_BATCH: usize = 20;

pub async fn run(rpc_url: &str, scenario: &str, cfg: &LoadConfig) -> Result<Vec<Sample>> {
    let client = Arc::new(RpcClient::new_with_commitment(
        rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    ));
    let owner = Keypair::new().pubkey();
    let keys: Arc<Vec<Pubkey>> = Arc::new(
        (0..cfg.read_accounts)
            .map(|_| Keypair::new().pubkey())
            .collect(),
    );
    seed_accounts(
        &client,
        &keys,
        &owner,
        1_000_000_000,
        &[0xA5; ACCOUNT_DATA_LEN],
    )
    .await
    .context("seeding read accounts")?;
    let probe = client
        .get_account_with_commitment(&keys[keys.len() - 1], CommitmentConfig::confirmed())
        .await
        .context("getAccountInfo probe")?;
    if probe.value.is_none() {
        bail!("seeded account is not visible over RPC");
    }

    if cfg.warmup_secs > 0 {
        drive(
            &client,
            &keys,
            cfg.readers,
            Duration::from_secs(cfg.warmup_secs),
        )
        .await
        .context("warm-up")?;
    }
    let duration = Duration::from_secs(cfg.duration_secs);
    let outcome = drive(&client, &keys, cfg.readers, duration).await?;
    Ok(outcome.samples(scenario, duration))
}

#[derive(Default)]
struct Outcome {
    requests: u64,
    account_info: Vec<Duration>,
    balance: Vec<Duration>,
    multiple: Vec<Duration>,
}

impl Outcome {
    fn samples(&self, scenario: &str, window: Duration) -> Vec<Sample> {
        vec![
            Sample::gated(
                scenario,
                "rps",
                "req/s",
                self.requests as f64 / window.as_secs_f64(),
            ),
            Sample::info(
                scenario,
                "get_account_info_p50_ms",
                "ms",
                percentile(&to_millis(&self.account_info), 0.50),
                Direction::LowerIsBetter,
            ),
            Sample::info(
                scenario,
                "get_balance_p50_ms",
                "ms",
                percentile(&to_millis(&self.balance), 0.50),
                Direction::LowerIsBetter,
            ),
            Sample::info(
                scenario,
                "get_multiple_accounts_p50_ms",
                "ms",
                percentile(&to_millis(&self.multiple), 0.50),
                Direction::LowerIsBetter,
            ),
        ]
    }
}

async fn drive(
    client: &Arc<RpcClient>,
    keys: &Arc<Vec<Pubkey>>,
    readers: usize,
    duration: Duration,
) -> Result<Outcome> {
    let deadline = Instant::now() + duration;
    let mut tasks = JoinSet::new();
    for reader in 0..readers {
        let client = client.clone();
        let keys = keys.clone();
        tasks.spawn(async move {
            let mut out = Outcome::default();
            // Each reader walks the key space from its own offset with a stride
            // coprime to most pool sizes, so readers spread across accounts
            // instead of hammering the same one.
            let mut cursor = reader * 7919;
            while Instant::now() < deadline {
                cursor += 7919;
                let index = cursor % keys.len();
                let t0 = Instant::now();
                let result = match cursor % 4 {
                    0 | 1 => client
                        .get_account_with_commitment(&keys[index], CommitmentConfig::confirmed())
                        .await
                        .map(|_| &mut out.account_info),
                    2 => client
                        .get_balance(&keys[index])
                        .await
                        .map(|_| &mut out.balance),
                    _ => {
                        let end = (index + MULTI_BATCH).min(keys.len());
                        client
                            .get_multiple_accounts(&keys[index..end])
                            .await
                            .map(|_| &mut out.multiple)
                    }
                };
                result.context("read request")?.push(t0.elapsed());
                out.requests += 1;
            }
            anyhow::Ok(out)
        });
    }
    let mut total = Outcome::default();
    while let Some(joined) = tasks.join_next().await {
        let out = joined.context("reader task panicked")??;
        total.requests += out.requests;
        total.account_info.extend(out.account_info);
        total.balance.extend(out.balance);
        total.multiple.extend(out.multiple);
    }
    Ok(total)
}
