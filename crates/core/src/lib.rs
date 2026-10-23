#[macro_use]
extern crate log;

#[allow(unused_imports)]
#[macro_use]
extern crate serde_derive;

#[allow(unused_imports)]
#[cfg(test)]
#[macro_use]
extern crate serde_json;

pub mod error;
pub mod helpers;
pub mod lifecycle;
pub mod rpc;
pub mod runloops;
pub mod scenarios;
pub mod storage;
pub mod surfnet;
#[cfg(feature = "prometheus")]
pub mod telemetry;
pub mod types;

use crossbeam_channel::{Receiver, Sender};
pub use jsonrpc_core;
pub use jsonrpc_http_server;
pub use litesvm;
pub use solana_account;
pub use solana_commitment_config;
use solana_pubkey::Pubkey;
pub use solana_rpc_client;
use surfnet::{GeyserEvent, locker::SurfnetSvmLocker, svm::SurfnetSvm};
use surfpool_types::{SimnetCommand, SurfpoolConfig};
use uuid::Uuid;

pub const SURFPOOL_IDENTITY_PUBKEY: Pubkey =
    Pubkey::from_str_const("SUrFPooLSUrFPooLSUrFPooLSUrFPooLSUrFPooLSUr");

/// Runs an already-built normal surfnet state (the legacy low-level entry point, normal-only).
/// Frontends open and run instances through [`lifecycle::SurfnetInstance`].
pub async fn start_local_surfnet(
    surfnet_svm: SurfnetSvm,
    config: SurfpoolConfig,
    simnet_commands_tx: Sender<SimnetCommand>,
    simnet_commands_rx: Receiver<SimnetCommand>,
    geyser_events_rx: Receiver<GeyserEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);
    runloops::start_local_surfnet_runloop(
        svm_locker,
        config,
        simnet_commands_tx,
        simnet_commands_rx,
        geyser_events_rx,
    )
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub plugin_name: String,
    pub uuid: String,
}

#[cfg(test)]
mod tests;
