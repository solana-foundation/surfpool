//! sendTransaction throughput: many payers each send system transfers to a
//! fresh recipient as fast as the server accepts them, and a confirmer polls
//! getSignatureStatuses until every sent transaction has landed. Any send
//! error, failed transaction or unconfirmed transaction aborts the scenario.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_system_interface::instruction::transfer;
use solana_transaction::Transaction;
use solana_transaction_status_client_types::TransactionConfirmationStatus;
use tokio::{
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
};

use super::LoadConfig;
use crate::results::{Direction, Sample, percentile, to_millis};

/// Lamports airdropped to each payer. Plenty for any plausible send rate over
/// the measurement window.
const PAYER_LAMPORTS: u64 = 10_000 * 1_000_000_000;
/// Lamports per transfer. Above the rent-exempt minimum for an empty account so
/// every recipient becomes a real new account.
const TRANSFER_LAMPORTS: u64 = 1_000_000;
/// How long after the send window to keep waiting for confirmations.
const CONFIRM_GRACE: Duration = Duration::from_secs(60);
/// Signatures per getSignatureStatuses request, the public RPC limit.
const STATUS_BATCH: usize = 256;

pub async fn run(rpc_url: &str, scenario: &str, cfg: &LoadConfig) -> Result<Vec<Sample>> {
    let client = Arc::new(RpcClient::new_with_commitment(
        rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    ));
    let payers: Vec<Arc<Keypair>> = (0..cfg.senders).map(|_| Arc::new(Keypair::new())).collect();
    for payer in &payers {
        client
            .request_airdrop(&payer.pubkey(), PAYER_LAMPORTS)
            .await
            .context("requestAirdrop")?;
    }
    wait_for_funding(&client, &payers).await?;

    let (blockhash, refresher) = spawn_blockhash_refresher(client.clone()).await?;

    if cfg.warmup_secs > 0 {
        drive(
            &client,
            &payers,
            &blockhash,
            Duration::from_secs(cfg.warmup_secs),
            cfg.skip_preflight,
        )
        .await
        .context("warm-up")?;
    }
    let duration = Duration::from_secs(cfg.duration_secs);
    let outcome = drive(&client, &payers, &blockhash, duration, cfg.skip_preflight).await?;
    refresher.abort();

    Ok(outcome.samples(scenario, duration))
}

struct Outcome {
    sent: u64,
    confirmed: u64,
    elapsed_to_last_confirm: Duration,
    send_latencies: Vec<Duration>,
    confirm_latencies: Vec<Duration>,
}

impl Outcome {
    fn samples(&self, scenario: &str, window: Duration) -> Vec<Sample> {
        let tps = if self.confirmed == 0 {
            0.0
        } else {
            self.confirmed as f64 / self.elapsed_to_last_confirm.as_secs_f64()
        };
        let send_ms = to_millis(&self.send_latencies);
        let confirm_ms = to_millis(&self.confirm_latencies);
        vec![
            Sample::gated(scenario, "tps_confirmed", "tx/s", tps),
            Sample::info(
                scenario,
                "send_rate",
                "tx/s",
                self.sent as f64 / window.as_secs_f64(),
                Direction::HigherIsBetter,
            ),
            Sample::info(
                scenario,
                "send_latency_p50_ms",
                "ms",
                percentile(&send_ms, 0.50),
                Direction::LowerIsBetter,
            ),
            Sample::info(
                scenario,
                "send_latency_p99_ms",
                "ms",
                percentile(&send_ms, 0.99),
                Direction::LowerIsBetter,
            ),
            Sample::info(
                scenario,
                "confirm_latency_p50_ms",
                "ms",
                percentile(&confirm_ms, 0.50),
                Direction::LowerIsBetter,
            ),
        ]
    }
}

/// Sends transfers from every payer until `duration` passes, then waits for
/// the confirmer to see each one land. Fails on the first send error, or if
/// any transaction fails or never confirms.
async fn drive(
    client: &Arc<RpcClient>,
    payers: &[Arc<Keypair>],
    blockhash: &watch::Receiver<Hash>,
    duration: Duration,
    skip_preflight: bool,
) -> Result<Outcome> {
    let start = Instant::now();
    let deadline = start + duration;
    let (sig_tx, sig_rx) = mpsc::unbounded_channel::<(Signature, Instant)>();

    let mut senders = JoinSet::new();
    for payer in payers {
        let client = client.clone();
        let payer = payer.clone();
        let sig_tx = sig_tx.clone();
        let blockhash = blockhash.clone();
        senders.spawn(async move {
            let mut latencies = Vec::new();
            while Instant::now() < deadline {
                let recent = *blockhash.borrow();
                let recipient = Keypair::new().pubkey();
                let tx = Transaction::new_signed_with_payer(
                    &[transfer(&payer.pubkey(), &recipient, TRANSFER_LAMPORTS)],
                    Some(&payer.pubkey()),
                    &[payer.as_ref()],
                    recent,
                );
                let sent_at = Instant::now();
                let config = RpcSendTransactionConfig {
                    skip_preflight,
                    // Left unset, the client asks for a finalized preflight, which
                    // rejects the newer confirmed blockhashes the refresher hands out.
                    preflight_commitment: Some(client.commitment().commitment),
                    ..Default::default()
                };
                let signature = client
                    .send_transaction_with_config(&tx, config)
                    .await
                    .context("sendTransaction")?;
                latencies.push(sent_at.elapsed());
                let _ = sig_tx.send((signature, sent_at));
            }
            anyhow::Ok(latencies)
        });
    }
    // The confirmer treats a closed channel as "no more signatures coming", so
    // this handle must go before the senders finish.
    drop(sig_tx);

    let confirmer = tokio::spawn(confirm_loop(
        client.clone(),
        sig_rx,
        start,
        deadline + CONFIRM_GRACE,
    ));

    let send_latencies = match join_senders(senders).await {
        Ok(latencies) => latencies,
        Err(e) => {
            confirmer.abort();
            return Err(e);
        }
    };
    let confirm = confirmer.await.context("confirmer task panicked")??;
    if confirm.failed > 0 || confirm.unconfirmed > 0 {
        bail!(
            "{} transactions failed and {} were not confirmed within {}s",
            confirm.failed,
            confirm.unconfirmed,
            CONFIRM_GRACE.as_secs()
        );
    }

    Ok(Outcome {
        sent: send_latencies.len() as u64,
        confirmed: confirm.confirmed,
        elapsed_to_last_confirm: confirm.last_confirm.duration_since(start),
        send_latencies,
        confirm_latencies: confirm.latencies,
    })
}

/// Collects every sender's send latencies. Dropping `senders` on the first
/// error aborts the senders still running.
async fn join_senders(mut senders: JoinSet<Result<Vec<Duration>>>) -> Result<Vec<Duration>> {
    let mut latencies = Vec::new();
    while let Some(joined) = senders.join_next().await {
        latencies.extend(joined.context("sender task panicked")??);
    }
    Ok(latencies)
}

struct ConfirmStats {
    confirmed: u64,
    failed: u64,
    unconfirmed: u64,
    last_confirm: Instant,
    latencies: Vec<Duration>,
}

/// Polls getSignatureStatuses for every signature the senders report until
/// each has reached confirmed commitment, the senders are done and nothing is
/// pending, or `give_up_at` passes.
async fn confirm_loop(
    client: Arc<RpcClient>,
    mut rx: mpsc::UnboundedReceiver<(Signature, Instant)>,
    start: Instant,
    give_up_at: Instant,
) -> Result<ConfirmStats> {
    let mut pending: Vec<(Signature, Instant)> = Vec::new();
    let mut senders_done = false;
    let mut stats = ConfirmStats {
        confirmed: 0,
        failed: 0,
        unconfirmed: 0,
        last_confirm: start,
        latencies: Vec::new(),
    };

    loop {
        loop {
            match rx.try_recv() {
                Ok(item) => pending.push(item),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    senders_done = true;
                    break;
                }
            }
        }
        if pending.is_empty() {
            if senders_done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }

        let mut still_pending = Vec::with_capacity(pending.len());
        for chunk in pending.chunks(STATUS_BATCH) {
            let signatures: Vec<Signature> = chunk.iter().map(|(s, _)| *s).collect();
            let statuses = client
                .get_signature_statuses(&signatures)
                .await
                .context("getSignatureStatuses")?
                .value;
            let now = Instant::now();
            for ((signature, sent_at), status) in chunk.iter().zip(statuses) {
                let landed = status.as_ref().is_some_and(|s| {
                    matches!(
                        s.confirmation_status,
                        Some(
                            TransactionConfirmationStatus::Confirmed
                                | TransactionConfirmationStatus::Finalized
                        )
                    )
                });
                match status {
                    Some(s) if landed => {
                        if s.err.is_some() {
                            stats.failed += 1;
                        } else {
                            stats.confirmed += 1;
                            stats.latencies.push(now.duration_since(*sent_at));
                            stats.last_confirm = now;
                        }
                    }
                    _ => still_pending.push((*signature, *sent_at)),
                }
            }
        }
        pending = still_pending;

        if Instant::now() > give_up_at {
            break;
        }
        if !pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    stats.unconfirmed = pending.len() as u64;
    Ok(stats)
}

/// Blocks until every payer's airdrop is visible at confirmed commitment.
async fn wait_for_funding(client: &RpcClient, payers: &[Arc<Keypair>]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    for payer in payers {
        loop {
            let balance = client
                .get_balance(&payer.pubkey())
                .await
                .context("getBalance")?;
            if balance >= PAYER_LAMPORTS {
                break;
            }
            if Instant::now() > deadline {
                bail!("payer {} never received its airdrop", payer.pubkey());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(())
}

/// Keeps a shared recent blockhash fresh so senders never pay for the lookup
/// on the hot path and never send an expired one.
async fn spawn_blockhash_refresher(
    client: Arc<RpcClient>,
) -> Result<(watch::Receiver<Hash>, JoinHandle<()>)> {
    let initial = client
        .get_latest_blockhash()
        .await
        .context("getLatestBlockhash")?;
    let (tx, rx) = watch::channel(initial);
    let handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(hash) = client.get_latest_blockhash().await {
                let _ = tx.send(hash);
            }
        }
    });
    Ok((rx, handle))
}
