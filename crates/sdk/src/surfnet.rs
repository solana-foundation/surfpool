use std::{
    net::TcpListener,
    thread::sleep,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_core::surfnet::{locker::SurfnetSvmLocker, svm::SurfnetSvm};
use surfpool_types::{
    BlockProductionMode, RpcConfig, SimnetCommand, SimnetConfig, SimnetEvent, SurfpoolConfig,
};

use crate::{
    Cheatcodes,
    error::{SurfnetError, SurfnetResult},
};

/// Builder for configuring a [`Surfnet`] instance before starting it.
///
/// ```rust
/// use surfpool_sdk::{Surfnet, BlockProductionMode};
///
/// # async fn example() {
/// let surfnet = Surfnet::builder()
///     .offline(true)
///     .block_production_mode(BlockProductionMode::Transaction)
///     .airdrop_sol(10_000_000_000)
///     .start()
///     .await
///     .unwrap();
/// # }
/// ```
pub struct SurfnetBuilder {
    offline_mode: bool,
    remote_rpc_url: Option<String>,
    block_production_mode: BlockProductionMode,
    slot_time_ms: u64,
    airdrop_addresses: Vec<Pubkey>,
    airdrop_lamports: u64,
    payer: Option<Keypair>,
}

impl Default for SurfnetBuilder {
    fn default() -> Self {
        Self {
            offline_mode: true,
            remote_rpc_url: None,
            block_production_mode: BlockProductionMode::Transaction,
            slot_time_ms: 1,
            airdrop_addresses: vec![],
            airdrop_lamports: 10_000_000_000, // 10 SOL
            payer: None,
        }
    }
}

impl SurfnetBuilder {
    /// Run in offline mode (no mainnet RPC fallback). Default: `true`.
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline_mode = offline;
        self
    }

    /// Set a remote RPC URL for account fallback (implies `offline(false)`).
    pub fn remote_rpc_url(mut self, url: impl Into<String>) -> Self {
        self.remote_rpc_url = Some(url.into());
        self.offline_mode = false;
        self
    }

    /// How blocks are produced. Default: `Transaction` (advance on each tx).
    pub fn block_production_mode(mut self, mode: BlockProductionMode) -> Self {
        self.block_production_mode = mode;
        self
    }

    /// Slot time in milliseconds. Default: `1` (fast for tests).
    pub fn slot_time_ms(mut self, ms: u64) -> Self {
        self.slot_time_ms = ms;
        self
    }

    /// Additional addresses to airdrop SOL to at startup.
    pub fn airdrop_addresses(mut self, addresses: Vec<Pubkey>) -> Self {
        self.airdrop_addresses = addresses;
        self
    }

    /// Amount of lamports to airdrop to the payer (and additional addresses) at startup.
    /// Default: 10 SOL.
    pub fn airdrop_sol(mut self, lamports: u64) -> Self {
        self.airdrop_lamports = lamports;
        self
    }

    /// Use a specific keypair as the payer. If not set, a random one is generated.
    pub fn payer(mut self, keypair: Keypair) -> Self {
        self.payer = Some(keypair);
        self
    }

    /// Start the surfnet with the configured options.
    pub async fn start(self) -> SurfnetResult<Surfnet> {
        let payer = self.payer.unwrap_or_else(Keypair::new);
        let airdrop_lamports = self.airdrop_lamports;

        let bind_port = get_free_port()?;
        let ws_port = get_free_port()?;
        let bind_host = "127.0.0.1".to_string();

        let mut startup_airdrop_addresses = vec![payer.pubkey()];
        startup_airdrop_addresses.extend(self.airdrop_addresses);

        let surfpool_config = SurfpoolConfig {
            simnets: vec![SimnetConfig {
                offline_mode: self.offline_mode,
                remote_rpc_url: self.remote_rpc_url,
                slot_time: self.slot_time_ms,
                block_production_mode: self.block_production_mode,
                airdrop_addresses: startup_airdrop_addresses.clone(),
                airdrop_token_amount: airdrop_lamports,
                ..Default::default()
            }],
            rpc: RpcConfig {
                bind_host: bind_host.clone(),
                bind_port,
                ws_port,
                ..Default::default()
            },
            ..Default::default()
        };

        let rpc_url = format!("http://{bind_host}:{bind_port}");
        let ws_url = format!("ws://{bind_host}:{ws_port}");

        let (surfnet_svm, simnet_events_rx, geyser_events_rx) = SurfnetSvm::default();
        let (simnet_commands_tx, simnet_commands_rx) = crossbeam_channel::unbounded();

        let svm_locker = SurfnetSvmLocker::new(surfnet_svm);
        let svm_locker_clone = svm_locker.clone();
        let simnet_commands_tx_clone = simnet_commands_tx.clone();

        let _handle = std::thread::Builder::new()
            .name("surfnet-sdk".into())
            .spawn(move || {
                let future = surfpool_core::runloops::start_local_surfnet_runloop(
                    svm_locker_clone,
                    surfpool_config,
                    simnet_commands_tx_clone,
                    simnet_commands_rx,
                    geyser_events_rx,
                );
                if let Err(e) = hiro_system_kit::nestable_block_on(future) {
                    log::error!("Surfnet exited with error: {e}");
                }
            })
            .map_err(|e| SurfnetError::Runtime(e.to_string()))?;

        // Wait for the runtime to signal ready
        wait_for_ready(&simnet_events_rx)?;
        wait_for_startup_airdrops(&rpc_url, &startup_airdrop_addresses, airdrop_lamports)?;

        Ok(Surfnet {
            rpc_url,
            ws_url,
            payer,
            simnet_commands_tx,
            simnet_events_rx,
            svm_locker,
            instance_id: uuid::Uuid::new_v4().to_string(),
        })
    }
}

/// A running Surfpool instance with RPC/WS endpoints on dynamic ports.
///
/// Provides:
/// - Pre-funded payer keypair
/// - [`RpcClient`] connected to the local instance
/// - [`Cheatcodes`] for direct state manipulation (fund accounts, set token balances, etc.)
///
/// The instance is shut down when dropped.
pub struct Surfnet {
    rpc_url: String,
    ws_url: String,
    payer: Keypair,
    simnet_commands_tx: Sender<SimnetCommand>,
    simnet_events_rx: Receiver<SimnetEvent>,
    #[allow(dead_code)] // retained for future direct profiling access
    svm_locker: SurfnetSvmLocker,
    instance_id: String,
}

impl Surfnet {
    /// Start a surfnet with default settings (offline, transaction-mode blocks, 10 SOL payer).
    pub async fn start() -> SurfnetResult<Self> {
        SurfnetBuilder::default().start().await
    }

    /// Create a builder for custom configuration.
    pub fn builder() -> SurfnetBuilder {
        SurfnetBuilder::default()
    }

    /// The HTTP RPC URL (e.g. `http://127.0.0.1:12345`).
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// The WebSocket URL (e.g. `ws://127.0.0.1:12346`).
    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    /// Create a new [`RpcClient`] connected to this surfnet.
    pub fn rpc_client(&self) -> RpcClient {
        RpcClient::new(&self.rpc_url)
    }

    /// The pre-funded payer keypair.
    pub fn payer(&self) -> &Keypair {
        &self.payer
    }

    /// Access cheatcode helpers for direct state manipulation.
    pub fn cheatcodes(&self) -> Cheatcodes<'_> {
        Cheatcodes::new(&self.rpc_url)
    }

    /// Get a reference to the simnet events receiver for observing runtime events.
    pub fn events(&self) -> &Receiver<SimnetEvent> {
        &self.simnet_events_rx
    }

    /// Send a command to the simnet runtime.
    pub fn send_command(&self, command: SimnetCommand) -> SurfnetResult<()> {
        self.simnet_commands_tx
            .send(command)
            .map_err(|e| SurfnetError::Runtime(format!("failed to send command: {e}")))
    }

    /// The unique instance ID for this surfnet.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl Drop for Surfnet {
    fn drop(&mut self) {
        let _ = self.simnet_commands_tx.send(SimnetCommand::Terminate(None));
    }
}

fn get_free_port() -> SurfnetResult<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| SurfnetError::PortAllocation(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| SurfnetError::PortAllocation(e.to_string()))?
        .port();
    drop(listener);
    Ok(port)
}

fn wait_for_ready(events_rx: &Receiver<SimnetEvent>) -> SurfnetResult<()> {
    loop {
        match events_rx.recv() {
            Ok(SimnetEvent::Ready(_)) => return Ok(()),
            Ok(SimnetEvent::Aborted(err)) => return Err(SurfnetError::Aborted(err)),
            Ok(SimnetEvent::Shutdown) => {
                return Err(SurfnetError::Aborted(
                    "surfnet shut down during startup".into(),
                ));
            }
            Ok(_) => continue,
            Err(e) => {
                return Err(SurfnetError::Startup(format!(
                    "events channel closed unexpectedly: {e}"
                )));
            }
        }
    }
}

fn wait_for_startup_airdrops(
    rpc_url: &str,
    addresses: &[Pubkey],
    expected_lamports: u64,
) -> SurfnetResult<()> {
    let rpc_client = RpcClient::new(rpc_url.to_string());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error = None;
    let mut last_balances = vec![];

    while Instant::now() < deadline {
        last_balances.clear();
        let mut all_match = true;

        for address in addresses {
            match rpc_client.get_balance_with_commitment(address, CommitmentConfig::processed()) {
                Ok(response) => {
                    last_balances.push((address.to_string(), response.value));
                    if response.value != expected_lamports {
                        all_match = false;
                    }
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                    all_match = false;
                    break;
                }
            }
        }

        if all_match {
            return Ok(());
        }

        sleep(Duration::from_millis(25));
    }

    let balance_summary = if last_balances.is_empty() {
        "no balances observed".to_string()
    } else {
        last_balances
            .iter()
            .map(|(address, balance)| format!("{address}={balance}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    Err(SurfnetError::Startup(format!(
        "startup balances not visible over RPC within timeout (expected {expected_lamports}); last balances: {balance_summary}; last error: {}",
        last_error.unwrap_or_else(|| "none".to_string())
    )))
}
