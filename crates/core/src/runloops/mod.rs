#![allow(unused_imports, dead_code, unused_mut, unused_variables)]

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    thread::{JoinHandle, sleep},
    time::{Duration, Instant},
};

use agave_geyser_plugin_interface::geyser_plugin_interface::{
    GeyserPlugin, ReplicaBlockInfoV4, ReplicaBlockInfoVersions, ReplicaEntryInfoV2,
    ReplicaEntryInfoVersions, ReplicaTransactionInfoV3, ReplicaTransactionInfoVersions, SlotStatus,
};
use chrono::{Local, Utc};
use crossbeam::select;
use crossbeam_channel::{Receiver, Sender, unbounded};
use itertools::Itertools;
use jsonrpc_core::MetaIoHandler;
use jsonrpc_http_server::{DomainsValidation, ServerBuilder};
use jsonrpc_pubsub::{PubSubHandler, Session};
use jsonrpc_ws_server::{RequestContext, ServerBuilder as WsServerBuilder};
use libloading::{Library, Symbol};
use serde::Serialize;
use solana_commitment_config::CommitmentConfig;
#[cfg(feature = "geyser_plugin")]
use solana_geyser_plugin_manager::geyser_plugin_manager::{
    GeyserPluginManager, LoadedGeyserPlugin,
};
use solana_message::SimpleAddressLoader;
use solana_transaction::sanitized::{MessageHash, SanitizedTransaction};
use solana_transaction_status::RewardsAndNumPartitions;
use surfpool_types::{
    BlockProductionMode, ClockCommand, ClockEvent, DEFAULT_MAINNET_RPC_URL, DataIndexingCommand,
    SimnetCommand, SimnetConfig, SimnetEvent, SurfpoolConfig,
};
type PluginConstructor = unsafe fn() -> *mut dyn GeyserPlugin;
use txtx_addon_kit::helpers::fs::FileLocation;

use crate::{
    PluginInfo,
    rpc::{
        self, RunloopContext, SurfpoolMiddleware, SurfpoolWebsocketMeta,
        SurfpoolWebsocketMiddleware, accounts_data::AccountsData, accounts_scan::AccountsScan,
        admin::AdminRpc, bank_data::BankData, full::Full, jito::Jito, minimal::Minimal,
        surfnet_cheatcodes::SurfnetCheatcodes, ws::Rpc,
    },
    surfnet::{GeyserEvent, PluginCommand, locker::SurfnetSvmLocker, remote::SurfnetRemoteClient},
};

const BLOCKHASH_SLOT_TTL: u64 = 75;

/// A loaded geyser plugin with all metadata needed for lifecycle management.
/// Field order matters: `plugin` must be declared before `_library` so it drops first,
/// since the plugin's vtable points into the loaded dynamic library.
pub(crate) struct ManagedPlugin {
    pub(crate) uuid: crate::Uuid,
    pub(crate) name: String,
    pub(crate) plugin: Box<dyn GeyserPlugin>,
    pub(crate) _library: Library,
    pub(crate) config_path: String,
}

pub(crate) fn load_plugin_from_config(
    config_file: &str,
    is_reload: bool,
) -> Result<ManagedPlugin, String> {
    let plugin_manifest_location = FileLocation::from_path(PathBuf::from(config_file));
    let config_content = plugin_manifest_location
        .read_content_as_utf8()
        .map_err(|e| format!("Unable to read plugin config '{}': {}", config_file, e))?;
    let result: serde_json::Value = serde_json::from_str(&config_content)
        .map_err(|e| format!("Unable to parse plugin manifest '{}': {}", config_file, e))?;

    let plugin_dylib_path = result
        .get("libpath")
        .and_then(|p| p.as_str())
        .ok_or_else(|| {
            format!(
                "Plugin config file should include a 'libpath' field: {}",
                plugin_manifest_location
            )
        })?;

    let mut plugin_dylib_location = plugin_manifest_location
        .get_parent_location()
        .expect("path invalid");
    plugin_dylib_location
        .append_path(plugin_dylib_path)
        .expect("path invalid");

    let (plugin, library) = unsafe {
        let lib = Library::new(&plugin_dylib_location.to_string())
            .map_err(|e| format!("Unable to load plugin {}: {}", plugin_dylib_location, e))?;
        let constructor: Symbol<PluginConstructor> = lib
            .get(b"_create_plugin")
            .map_err(|e| format!("Plugin missing _create_plugin symbol: {}", e))?;
        let plugin_raw = constructor();
        (Box::from_raw(plugin_raw), lib)
    };

    let name = plugin.name().to_string();

    let mut plugin = plugin;
    plugin
        .on_load(config_file, is_reload)
        .map_err(|e| format!("Failed to call on_load for plugin '{}': {}", name, e))?;

    let uuid = crate::Uuid::new_v4();

    Ok(ManagedPlugin {
        uuid,
        name,
        plugin,
        _library: library,
        config_path: config_file.to_string(),
    })
}

pub(crate) fn unload_plugin_by_name(
    name: &str,
    managed_plugins: &mut Vec<ManagedPlugin>,
) -> Result<(), String> {
    let idx = managed_plugins
        .iter()
        .position(|p| p.name == name)
        .ok_or_else(|| format!("Plugin '{}' not found", name))?;

    let mut plugin = managed_plugins.remove(idx);
    plugin.plugin.on_unload();
    // plugin and _library are dropped here in correct order
    Ok(())
}

/// Checks if a port is available for binding.
fn check_port_availability(addr: SocketAddr, server_type: &str) -> Result<(), String> {
    match std::net::TcpListener::bind(addr) {
        Ok(_listener) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let msg = format!(
                "{} port {} is already in use. Try --port or --ws-port to use a different port.",
                server_type,
                addr.port()
            );
            eprintln!("Error: {}", msg);
            Err(msg)
        }
        Err(e) => {
            let msg = format!("Failed to bind {} server to {}: {}", server_type, addr, e);
            eprintln!("Error: {}", msg);
            Err(msg)
        }
    }
}

pub async fn start_local_surfnet_runloop(
    svm_locker: SurfnetSvmLocker,
    config: SurfpoolConfig,
    simnet_commands_tx: Sender<SimnetCommand>,
    simnet_commands_rx: Receiver<SimnetCommand>,
    geyser_events_rx: Receiver<GeyserEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(simnet) = config.simnets.first() else {
        return Ok(());
    };
    let block_production_mode = simnet.block_production_mode.clone();

    let remote_rpc_client = match simnet.offline_mode {
        true => None,
        false => SurfnetRemoteClient::new_unsafe(
            simnet
                .remote_rpc_url
                .as_ref()
                .unwrap_or(&DEFAULT_MAINNET_RPC_URL.to_string()),
        ),
    };

    svm_locker.initialize(&remote_rpc_client).await?;

    if let Some(remote_client) = &remote_rpc_client {
        let _ = svm_locker
            .simnet_events_tx()
            .send(SimnetEvent::Connected(remote_client.client.url()));
    }
    let _ = svm_locker
        .simnet_events_tx()
        .send(SimnetEvent::EpochInfoUpdate(svm_locker.get_epoch_info()));

    svm_locker.airdrop_pubkeys(simnet.airdrop_token_amount, &simnet.airdrop_addresses);

    // Load snapshot accounts if provided
    if !simnet.snapshot.is_empty() {
        match svm_locker
            .load_snapshot(
                &simnet.snapshot,
                remote_rpc_client.as_ref(),
                CommitmentConfig::confirmed(),
            )
            .await
        {
            Ok(loaded_count) => {
                let _ = svm_locker.with_svm_reader(|svm| {
                    svm.simnet_events_tx.send(SimnetEvent::info(format!(
                        "Preloaded {} accounts from snapshot(s) into SVM",
                        loaded_count
                    )))
                });
            }
            Err(e) => {
                let _ = svm_locker.with_svm_reader(|svm| {
                    svm.simnet_events_tx.send(SimnetEvent::warn(format!(
                        "Error loading snapshot accounts: {}",
                        e
                    )))
                });
            }
        }
    }

    let simnet_events_tx_cc = svm_locker.simnet_events_tx();

    let (plugin_commands_tx, plugin_commands_rx) = unbounded::<PluginCommand>();

    let (_rpc_handle, _ws_handle, shutdown_rpc_servers) = start_rpc_servers_runloop(
        &config,
        &simnet_commands_tx,
        svm_locker.clone(),
        &remote_rpc_client,
        plugin_commands_tx,
    )
    .await?;

    let simnet_config = simnet.clone();

    match start_geyser_runloop(
        config.plugin_config_path.clone(),
        simnet_events_tx_cc.clone(),
        geyser_events_rx,
        plugin_commands_rx,
    ) {
        Ok(_) => {}
        Err(e) => {
            let _ =
                simnet_events_tx_cc.send(SimnetEvent::error(format!("Geyser plugin failed: {e}")));
        }
    };

    let (clock_event_rx, clock_command_tx) =
        start_clock_runloop(simnet_config.slot_time, Some(simnet_events_tx_cc.clone()));

    // Emit TransactionProcessed events for each stored transaction before Ready
    let initial_transaction_count = svm_locker.with_svm_reader(|svm| {
        let iter_result = svm.transactions.into_iter();
        let mut count: u64 = 0;

        if let Ok(iter) = iter_result {
            let mut events = vec![];
            for (_, status) in iter {
                if let Some((tx_meta, _updated_accounts)) = status.as_processed() {
                    let signature = tx_meta.transaction.signatures[0];
                    let err = tx_meta.meta.status.clone().err();

                    // Build TransactionMetadata from stored data
                    let meta = surfpool_types::TransactionMetadata {
                        signature,
                        logs: tx_meta.meta.log_messages.clone().unwrap_or_default(),
                        inner_instructions: tx_meta
                            .meta
                            .inner_instructions
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|inner_ixs| {
                                inner_ixs
                                    .instructions
                                    .into_iter()
                                    .map(|ix| solana_message::inner_instruction::InnerInstruction {
                                        instruction: ix.instruction,
                                        stack_height: ix.stack_height.unwrap_or(1) as u8,
                                    })
                                    .collect()
                            })
                            .collect(),
                        compute_units_consumed: tx_meta.meta.compute_units_consumed.unwrap_or(0),
                        return_data: tx_meta.meta.return_data.clone().unwrap_or_default(),
                        fee: tx_meta.meta.fee,
                    };

                    events.push((
                        tx_meta.slot,
                        SimnetEvent::TransactionProcessed(Local::now(), meta, err.clone()),
                    ));

                    count += 1;
                }
            }
            for (_, event) in events
                .into_iter()
                .sorted_by(|(a_slot, _), (b_slot, _)| a_slot.cmp(b_slot))
            {
                let _ = svm.simnet_events_tx.send(event);
            }
        }

        count
    });
    let _ = simnet_events_tx_cc.send(SimnetEvent::Ready(initial_transaction_count));

    // Notify geyser plugins that startup is complete
    let _ = svm_locker.with_svm_reader(|svm| svm.geyser_events_tx.send(GeyserEvent::EndOfStartup));

    start_block_production_runloop(
        clock_event_rx,
        clock_command_tx,
        simnet_commands_rx,
        simnet_commands_tx.clone(),
        svm_locker,
        block_production_mode,
        &remote_rpc_client,
        simnet_config.expiry.map(|e| e * 1000),
        &simnet_config,
        Some(shutdown_rpc_servers),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn start_block_production_runloop(
    clock_event_rx: Receiver<ClockEvent>,
    clock_command_tx: Sender<ClockCommand>,
    simnet_commands_rx: Receiver<SimnetCommand>,
    simnet_commands_tx: Sender<SimnetCommand>,
    svm_locker: SurfnetSvmLocker,
    mut block_production_mode: BlockProductionMode,
    remote_rpc_client: &Option<SurfnetRemoteClient>,
    expiry_duration_ms: Option<u64>,
    simnet_config: &SimnetConfig,
    mut shutdown_rpc_servers: Option<Box<dyn FnOnce() + Send>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let remote_client_with_commitment = remote_rpc_client.as_ref().map(|c| {
        (
            c.clone(),
            solana_commitment_config::CommitmentConfig::confirmed(),
        )
    });
    let mut next_scheduled_expiry_check: Option<u64> =
        expiry_duration_ms.map(|expiry_val| Utc::now().timestamp_millis() as u64 + expiry_val);
    let global_skip_sig_verify = simnet_config.skip_signature_verification;
    let ix_profiling_initially_enabled = simnet_config.instruction_profiling_enabled;
    loop {
        let mut do_produce_block = false;

        select! {
            recv(clock_event_rx) -> msg => if let Ok(event) = msg {
                match event {
                    ClockEvent::Tick => {
                        if block_production_mode.eq(&BlockProductionMode::Clock) {
                            do_produce_block = true;
                        }

                        if let Some(expiry_ms) = expiry_duration_ms {
                            if let Some(scheduled_time_ref) = &mut next_scheduled_expiry_check {
                                let now_ms = Utc::now().timestamp_millis() as u64;
                                if now_ms >= *scheduled_time_ref {
                                    let svm = svm_locker.0.read().await;
                                    if svm.updated_at + expiry_ms < now_ms {
                                        let _ = simnet_commands_tx.send(SimnetCommand::Terminate(None));
                                    } else {
                                        *scheduled_time_ref = svm.updated_at + expiry_ms;
                                    }
                                }
                            }
                        }
                    }
                    ClockEvent::ExpireBlockHash => {
                        do_produce_block = true;
                    }
                }
            },
            recv(simnet_commands_rx) -> msg => if let Ok(event) = msg {
                match event {
                    SimnetCommand::SlotForward(_key) => {
                        block_production_mode = BlockProductionMode::Manual;
                        do_produce_block = true;
                    }
                    SimnetCommand::SlotBackward(_key) => {

                    }
                    SimnetCommand::CommandClock(_, update) => {
                        if let ClockCommand::UpdateSlotInterval(updated_slot_time) = update {
                            svm_locker.with_svm_writer(|svm_writer| {
                                svm_writer.slot_time = updated_slot_time;
                            });
                        }

                        // Handle PauseWithConfirmation specially
                        if let ClockCommand::PauseWithConfirmation(response_tx) = update {
                            // Get current slot and slot_time before pausing
                            let (current_slot, slot_time) = svm_locker.with_svm_reader(|svm_reader| {
                                (svm_reader.latest_epoch_info.absolute_slot, svm_reader.slot_time)
                            });

                            // Send Pause to clock runloop
                            let _ = clock_command_tx.send(ClockCommand::Pause);

                            // Give the clock time to process the pause command
                            tokio::time::sleep(tokio::time::Duration::from_millis(slot_time / 2)).await;

                            // Loop and check if the slot has stopped advancing
                            let max_attempts = 10;
                            let mut attempts = 0;
                            loop {
                                tokio::time::sleep(tokio::time::Duration::from_millis(slot_time)).await;

                                let new_slot = svm_locker.with_svm_reader(|svm_reader| {
                                    svm_reader.latest_epoch_info.absolute_slot
                                });

                                // If slot hasn't changed, clock has stopped
                                if new_slot == current_slot || attempts >= max_attempts {
                                    break;
                                }

                                attempts += 1;
                            }

                            // Read epoch info after clock has stopped
                            let epoch_info = svm_locker.with_svm_reader(|svm_reader| {
                                svm_reader.latest_epoch_info.clone()
                            });
                            // Send response
                            let _ = response_tx.send(epoch_info);
                        } else {
                            let _ = clock_command_tx.send(update);
                        }
                        continue
                    }
                    SimnetCommand::UpdateInternalClock(_, clock) => {
                        // Confirm the current block to materialize any scheduled overrides for this slot
                        if let Err(e) = svm_locker.confirm_current_block(&remote_client_with_commitment).await {
                            let _ = svm_locker.simnet_events_tx().send(SimnetEvent::error(format!(
                                "Failed to confirm block after time travel: {}", e
                            )));
                        }

                        svm_locker.with_svm_writer(|svm_writer| {
                            svm_writer.inner.set_sysvar(&clock);
                            svm_writer.updated_at = clock.unix_timestamp as u64 * 1_000;
                            svm_writer.latest_epoch_info.absolute_slot = clock.slot;
                            svm_writer.latest_epoch_info.epoch = clock.epoch;
                            svm_writer.latest_epoch_info.slot_index = clock.slot;
                            svm_writer.latest_epoch_info.epoch = clock.epoch;
                            svm_writer.latest_epoch_info.absolute_slot = clock.slot + clock.epoch * svm_writer.latest_epoch_info.slots_in_epoch;
                            let _ = svm_writer.simnet_events_tx.send(SimnetEvent::SystemClockUpdated(clock));
                        });
                    }
                    SimnetCommand::UpdateInternalClockWithConfirmation(_, clock, response_tx) => {
                        // Confirm the current block to materialize any scheduled overrides for this slot
                        if let Err(e) = svm_locker.confirm_current_block(&remote_client_with_commitment).await {
                            let _ = svm_locker.simnet_events_tx().send(SimnetEvent::error(format!(
                                "Failed to confirm block after time travel: {}", e
                            )));
                        }

                        let epoch_info = svm_locker.with_svm_writer(|svm_writer| {
                            svm_writer.inner.set_sysvar(&clock);
                            svm_writer.updated_at = clock.unix_timestamp as u64 * 1_000;
                            svm_writer.latest_epoch_info.absolute_slot = clock.slot;
                            svm_writer.latest_epoch_info.epoch = clock.epoch;
                            svm_writer.latest_epoch_info.slot_index = clock.slot;
                            svm_writer.latest_epoch_info.epoch = clock.epoch;
                            svm_writer.latest_epoch_info.absolute_slot = clock.slot + clock.epoch * svm_writer.latest_epoch_info.slots_in_epoch;
                            let _ = svm_writer.simnet_events_tx.send(SimnetEvent::SystemClockUpdated(clock));
                            svm_writer.latest_epoch_info.clone()
                        });

                        // Send confirmation back
                        let _ = response_tx.send(epoch_info);
                    }
                    SimnetCommand::UpdateBlockProductionMode(update) => {
                        block_production_mode = update;
                        continue
                    }
                    SimnetCommand::ProcessTransaction(_key, transaction, status_tx, skip_preflight, skip_sig_verify_override) => {
                       let skip_sig_verify = skip_sig_verify_override.unwrap_or(global_skip_sig_verify);
                       let sigverify = !skip_sig_verify;
                       if let Err(e) = svm_locker.process_transaction(&remote_client_with_commitment, transaction, status_tx, skip_preflight, sigverify).await {
                            let _ = svm_locker.simnet_events_tx().send(SimnetEvent::error(format!("Failed to process transaction: {}", e)));
                       }
                       if block_production_mode.eq(&BlockProductionMode::Transaction) {
                           do_produce_block = true;
                       }
                    }
                    SimnetCommand::Terminate(_) => {
                        // Explicitly shutdown storage to trigger WAL checkpoint before exiting
                        svm_locker.shutdown();
                        // Close RPC servers on a separate thread to avoid dropping
                        // Tokio runtimes from within an async context
                        if let Some(shutdown_fn) = shutdown_rpc_servers.take() {
                            std::thread::spawn(shutdown_fn);
                        }
                        break;
                    }
                    SimnetCommand::StartRunbookExecution(runbook_id) => {
                        svm_locker.start_runbook_execution(runbook_id);
                    }

                    SimnetCommand::CompleteRunbookExecution(runbook_id, error) => {
                        svm_locker.complete_runbook_execution(runbook_id, error, ix_profiling_initially_enabled);
                    }
                    SimnetCommand::FetchRemoteAccounts(pubkeys, remote_url) => {
                        let remote_client = SurfnetRemoteClient::new_unsafe(&remote_url);
                        if let Some(remote_client) = remote_client {
                              match svm_locker.get_multiple_accounts_with_remote_fallback(&remote_client, &pubkeys, CommitmentConfig::confirmed()).await {
                                 Ok(account_updates) => {
                                     svm_locker.write_multiple_account_updates(&account_updates.inner);
                                 }
                                 Err(e) => {
                                     svm_locker.simnet_events_tx().try_send(SimnetEvent::error(format!("Failed to fetch remote accounts {:?}: {}", pubkeys, e))).ok();
                                 }
                             };
                        }
                    }
                    SimnetCommand::AirdropProcessed => {
                       if block_production_mode.eq(&BlockProductionMode::Transaction) {
                           do_produce_block = true;
                       }
                    }
                }
            },
        }

        {
            if do_produce_block {
                svm_locker
                    .confirm_current_block(&remote_client_with_commitment)
                    .await?;
            }
        }
    }
    Ok(())
}

pub fn start_clock_runloop(
    mut slot_time: u64,
    simnet_events_tx: Option<Sender<SimnetEvent>>,
) -> (Receiver<ClockEvent>, Sender<ClockCommand>) {
    let (clock_event_tx, clock_event_rx) = unbounded::<ClockEvent>();
    let (clock_command_tx, clock_command_rx) = unbounded::<ClockCommand>();

    let _handle = hiro_system_kit::thread_named("clock").spawn(move || {
        let mut enabled = true;
        let mut block_hash_timeout = Instant::now();

        loop {
            match clock_command_rx.try_recv() {
                Ok(ClockCommand::Pause) => {
                    enabled = false;
                    if let Some(ref simnet_events_tx) = simnet_events_tx {
                        let _ =
                            simnet_events_tx.send(SimnetEvent::ClockUpdate(ClockCommand::Pause));
                    }
                }
                Ok(ClockCommand::Resume) => {
                    enabled = true;
                    if let Some(ref simnet_events_tx) = simnet_events_tx {
                        let _ =
                            simnet_events_tx.send(SimnetEvent::ClockUpdate(ClockCommand::Resume));
                    }
                }
                Ok(ClockCommand::Toggle) => {
                    enabled = !enabled;
                    if let Some(ref simnet_events_tx) = simnet_events_tx {
                        let _ =
                            simnet_events_tx.send(SimnetEvent::ClockUpdate(ClockCommand::Toggle));
                    }
                }
                Ok(ClockCommand::UpdateSlotInterval(updated_slot_time)) => {
                    slot_time = updated_slot_time;
                }
                Ok(ClockCommand::PauseWithConfirmation(_)) => {
                    // This should be handled in the block production runloop, not here
                    // If it reaches here, just treat it as a regular Pause
                    enabled = false;
                    if let Some(ref simnet_events_tx) = simnet_events_tx {
                        let _ =
                            simnet_events_tx.send(SimnetEvent::ClockUpdate(ClockCommand::Pause));
                    }
                }
                Err(_e) => {}
            }
            sleep(Duration::from_millis(slot_time));
            if enabled {
                let _ = clock_event_tx.send(ClockEvent::Tick);
                // Todo: the block expiration is not completely accurate.
                if block_hash_timeout.elapsed()
                    > Duration::from_millis(BLOCKHASH_SLOT_TTL * slot_time)
                {
                    let _ = clock_event_tx.send(ClockEvent::ExpireBlockHash);
                    block_hash_timeout = Instant::now();
                }
            }
        }
    });

    (clock_event_rx, clock_command_tx)
}

fn start_geyser_runloop(
    plugin_config_paths: Vec<PathBuf>,
    simnet_events_tx: Sender<SimnetEvent>,
    geyser_events_rx: Receiver<GeyserEvent>,
    plugin_commands_rx: Receiver<PluginCommand>,
) -> Result<JoinHandle<Result<(), String>>, String> {
    let handle: JoinHandle<Result<(), String>> = hiro_system_kit::thread_named("Geyser Plugins Handler").spawn(move || {
        let mut indexing_enabled = false;

        #[cfg(feature = "geyser_plugin")]
        let mut plugin_manager = GeyserPluginManager::new();
        #[cfg(not(feature = "geyser_plugin"))]
        let mut plugin_manager = ();

        let mut managed_plugins: Vec<ManagedPlugin> = vec![];

        // helper to log errors that can't be propagated
        let log_error = |msg:String|{
            let _ = simnet_events_tx.send(SimnetEvent::error(msg));
        };

        let log_warn = |msg:String|{
            let _ = simnet_events_tx.send(SimnetEvent::warn(msg));
        };

        let log_info = |msg:String|{
            let _ = simnet_events_tx.send(SimnetEvent::info(msg));
        };

        // Load plugins from config paths at startup
        for plugin_config_path in plugin_config_paths.iter() {
            let config_file = plugin_config_path.to_string_lossy().to_string();
            match load_plugin_from_config(&config_file, false) {
                Ok(managed) => {
                    indexing_enabled = true;
                    log_info(format!("Loaded geyser plugin '{}'", managed.name));
                    managed_plugins.push(managed);
                }
                Err(e) => {
                    log_error(format!("Failed to load geyser plugin from '{}': {}", config_file, e));
                }
            }
        }

        #[cfg(feature = "geyser_plugin")]
        for plugin_config_path in plugin_config_paths.into_iter() {
            let plugin_manifest_location = FileLocation::from_path(plugin_config_path);
            let config_file = plugin_manifest_location.read_content_as_utf8()?;
            let result: serde_json::Value = match json5::from_str(&config_file) {
                Ok(res) => res,
                Err(e) => {
                    let error = format!("Unable to read manifest: {}", e);
                    let _ = simnet_events_tx.send(SimnetEvent::error(error.clone()));
                    return Err(error)
                }
            };

            let plugin_dylib_path = match result.get("libpath").map(|p| p.as_str()) {
                Some(Some(name)) => name,
                _ => {
                    let error = format!("Plugin config file should include a 'libpath' field: {}", plugin_manifest_location);
                    let _ = simnet_events_tx.send(SimnetEvent::error(error.clone()));
                    return Err(error)
                }
            };

            let mut plugin_dylib_location = plugin_manifest_location.get_parent_location().expect("path invalid");
            plugin_dylib_location.append_path(&plugin_dylib_path).expect("path invalid");

            let (plugin, lib) = unsafe {
                let lib = match Library::new(&plugin_dylib_location.to_string()) {
                    Ok(lib) => lib,
                    Err(e) => {
                        log_error(format!("Unable to load plugin {}: {}", plugin_dylib_location.to_string(), e.to_string()));
                        continue;
                    }
                };
                let constructor: Symbol<PluginConstructor> = lib
                    .get(b"_create_plugin")
                    .map_err(|e| format!("{}", e.to_string()))?;
                let plugin_raw = constructor();
                (Box::from_raw(plugin_raw), lib)
            };
            indexing_enabled = true;

            let mut plugin = LoadedGeyserPlugin::new(lib, plugin, None);
            if let Err(e) = plugin.on_load(&plugin_manifest_location.to_string(), false) {
                let error = format!("Unable to load plugin:: {}", e.to_string());
                let _ = simnet_events_tx.send(SimnetEvent::error(error.clone()));
                return Err(error)
            }

            plugin_manager.plugins.push(plugin);
        }

        let err = loop {
            use agave_geyser_plugin_interface::geyser_plugin_interface::{ReplicaAccountInfoV3, ReplicaAccountInfoVersions};

            use crate::types::GeyserAccountUpdate;

            select! {
                recv(geyser_events_rx) -> msg => match msg {
                    Err(e) => {
                        break format!("Failed to read new transaction to send to Geyser plugin: {e}");
                    },
                    Ok(GeyserEvent::NotifyTransaction(transaction_with_status_meta, versioned_transaction)) => {

                        if !indexing_enabled {
                            continue;
                        }

                        let transaction = match versioned_transaction {
                            Some(tx) => tx,
                            None => {
                                log_warn("Unable to index sanitized transaction".to_string());
                                continue;
                            }
                        };

                        let transaction_replica = ReplicaTransactionInfoV3 {
                            signature: &transaction.signatures[0],
                            is_vote: false,
                            transaction: &transaction,
                            transaction_status_meta: &transaction_with_status_meta.meta,
                            index: 0,
                            message_hash: &transaction.message.hash(),
                        };

                        for plugin in managed_plugins.iter().map(|p| &*p.plugin) {
                            if let Err(e) = plugin.notify_transaction(ReplicaTransactionInfoVersions::V0_0_3(&transaction_replica), transaction_with_status_meta.slot) {
                                log_error(format!("Failed to notify Geyser plugin of new transaction: {:?}", e))
                            };
                        }

                        #[cfg(feature = "geyser_plugin")]
                        for plugin in plugin_manager.plugins.iter() {
                            if let Err(e) = plugin.notify_transaction(ReplicaTransactionInfoVersions::V0_0_3(&transaction_replica), transaction_with_status_meta.slot) {
                                log_error(format!("Failed to notify Geyser plugin of new transaction: {:?}", e))
                            };
                        }
                    }
                    Ok(GeyserEvent::UpdateAccount(account_update)) => {
                        let GeyserAccountUpdate {
                            pubkey,
                            account,
                            slot,
                            sanitized_transaction,
                            write_version,
                        } = account_update;

                        let account_replica = ReplicaAccountInfoV3 {
                            pubkey: pubkey.as_ref(),
                            lamports: account.lamports,
                            owner: account.owner.as_ref(),
                            executable: account.executable,
                            rent_epoch: account.rent_epoch,
                            data: account.data.as_ref(),
                            write_version,
                            txn: sanitized_transaction.as_ref(),
                        };

                        for plugin in managed_plugins.iter().map(|p| &*p.plugin) {
                            if let Err(e) = plugin.update_account(ReplicaAccountInfoVersions::V0_0_3(&account_replica), slot, false) {
                                log_error(format!("Failed to update account in Geyser plugin: {:?}", e));
                            }
                        }

                        #[cfg(feature = "geyser_plugin")]
                        for plugin in plugin_manager.plugins.iter() {
                            if let Err(e) = plugin.update_account(ReplicaAccountInfoVersions::V0_0_3(&account_replica), slot, false) {
                                log_error(format!("Failed to update account in Geyser plugin: {:?}", e))
                            }
                        }
                    }
                    Ok(GeyserEvent::StartupAccountUpdate(account_update)) => {
                        let GeyserAccountUpdate {
                            pubkey,
                            account,
                            slot,
                            sanitized_transaction,
                            write_version,
                        } = account_update;

                        let account_replica = ReplicaAccountInfoV3 {
                            pubkey: pubkey.as_ref(),
                            lamports: account.lamports,
                            owner: account.owner.as_ref(),
                            executable: account.executable,
                            rent_epoch: account.rent_epoch,
                            data: account.data.as_ref(),
                            write_version,
                            txn: sanitized_transaction.as_ref(),
                        };

                        // Send startup account updates with is_startup=true
                        for plugin in managed_plugins.iter().map(|p| &*p.plugin) {
                            if let Err(e) = plugin.update_account(ReplicaAccountInfoVersions::V0_0_3(&account_replica), slot, true) {
                                log_error(format!("Failed to send startup account update to Geyser plugin: {:?}", e));
                            }
                        }

                        #[cfg(feature = "geyser_plugin")]
                        for plugin in plugin_manager.plugins.iter() {
                            if let Err(e) = plugin.update_account(ReplicaAccountInfoVersions::V0_0_3(&account_replica), slot, true) {
                                log_error(format!("Failed to send startup account update to Geyser plugin: {:?}", e))
                            }
                        }
                    }
                    Ok(GeyserEvent::EndOfStartup) => {
                        for plugin in managed_plugins.iter().map(|p| &*p.plugin) {
                            if let Err(e) = plugin.notify_end_of_startup() {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify end of startup to Geyser plugin: {:?}", e)));
                            }
                        }

                        #[cfg(feature = "geyser_plugin")]
                        for plugin in plugin_manager.plugins.iter() {
                            if let Err(e) = plugin.notify_end_of_startup() {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify end of startup to Geyser plugin: {:?}", e)));
                            }
                        }
                    }
                    Ok(GeyserEvent::UpdateSlotStatus { slot, parent, status }) => {
                        let slot_status = match status {
                            crate::surfnet::GeyserSlotStatus::Processed => SlotStatus::Processed,
                            crate::surfnet::GeyserSlotStatus::Confirmed => SlotStatus::Confirmed,
                            crate::surfnet::GeyserSlotStatus::Rooted => SlotStatus::Rooted,
                        };

                        for plugin in managed_plugins.iter().map(|p| &*p.plugin) {
                            if let Err(e) = plugin.update_slot_status(slot, parent, &slot_status) {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to update slot status in Geyser plugin: {:?}", e)));
                            }
                        }

                        #[cfg(feature = "geyser_plugin")]
                        for plugin in plugin_manager.plugins.iter() {
                            if let Err(e) = plugin.update_slot_status(slot, parent, &slot_status) {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to update slot status in Geyser plugin: {:?}", e)));
                            }
                        }
                    }
                    Ok(GeyserEvent::NotifyBlockMetadata(block_metadata)) => {
                        let rewards = RewardsAndNumPartitions {
                            rewards: vec![],
                            num_partitions: None,
                        };

                        let block_info = ReplicaBlockInfoV4 {
                            slot: block_metadata.slot,
                            blockhash: &block_metadata.blockhash,
                            rewards: &rewards,
                            block_time: block_metadata.block_time,
                            block_height: block_metadata.block_height,
                            parent_slot: block_metadata.parent_slot,
                            parent_blockhash: &block_metadata.parent_blockhash,
                            executed_transaction_count: block_metadata.executed_transaction_count,
                            entry_count: block_metadata.entry_count,
                        };

                        for plugin in managed_plugins.iter().map(|p| &*p.plugin) {
                            if let Err(e) = plugin.notify_block_metadata(ReplicaBlockInfoVersions::V0_0_4(&block_info)) {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify block metadata to Geyser plugin: {:?}", e)));
                            }
                        }

                        #[cfg(feature = "geyser_plugin")]
                        for plugin in plugin_manager.plugins.iter() {
                            if let Err(e) = plugin.notify_block_metadata(ReplicaBlockInfoVersions::V0_0_4(&block_info)) {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify block metadata to Geyser plugin: {:?}", e)));
                            }
                        }
                    }
                    Ok(GeyserEvent::NotifyEntry(entry_info)) => {
                        let entry_replica = ReplicaEntryInfoV2 {
                            slot: entry_info.slot,
                            index: entry_info.index,
                            num_hashes: entry_info.num_hashes,
                            hash: &entry_info.hash,
                            executed_transaction_count: entry_info.executed_transaction_count,
                            starting_transaction_index: entry_info.starting_transaction_index,
                        };

                        for plugin in managed_plugins.iter().map(|p| &*p.plugin) {
                            if let Err(e) = plugin.notify_entry(ReplicaEntryInfoVersions::V0_0_2(&entry_replica)) {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify entry to Geyser plugin: {:?}", e)));
                            }
                        }

                        #[cfg(feature = "geyser_plugin")]
                        for plugin in plugin_manager.plugins.iter() {
                            if let Err(e) = plugin.notify_entry(ReplicaEntryInfoVersions::V0_0_2(&entry_replica)) {
                                let _ = simnet_events_tx.send(SimnetEvent::error(format!("Failed to notify entry to Geyser plugin: {:?}", e)));
                            }
                        }
                    }
                },
                recv(plugin_commands_rx) -> cmd => match cmd {
                    Err(_) => {
                        break "Plugin command channel closed".to_string();
                    }
                    Ok(PluginCommand::Load { config_file, response_tx }) => {
                        let result = match load_plugin_from_config(&config_file, false) {
                            Ok(managed) => {
                                indexing_enabled = true;
                                let info = PluginInfo {
                                    plugin_name: managed.name.clone(),
                                    uuid: managed.uuid.to_string(),
                                };
                                log_info(format!("Loaded geyser plugin '{}'", managed.name));
                                managed_plugins.push(managed);
                                Ok(info)
                            }
                            Err(e) => {
                                log_error(format!("Failed to load plugin: {}", e));
                                Err(e)
                            }
                        };
                        let _ = response_tx.send(result);
                    }
                    Ok(PluginCommand::Unload { name, response_tx }) => {
                        let result = unload_plugin_by_name(&name, &mut managed_plugins);
                        if result.is_ok() {
                            log_info(format!("Unloaded geyser plugin '{}'", name));
                        }
                        let _ = response_tx.send(result);
                    }
                    Ok(PluginCommand::Reload { name, config_file, response_tx }) => {
                        let result = unload_plugin_by_name(&name, &mut managed_plugins)
                            .and_then(|_| {
                                let managed = load_plugin_from_config(&config_file, true)?;
                                log_info(format!("Reloaded geyser plugin '{}'", managed.name));
                                managed_plugins.push(managed);
                                Ok(())
                            });
                        if let Err(ref e) = result {
                            log_error(format!("Failed to reload plugin '{}': {}", name, e));
                        }
                        let _ = response_tx.send(result);
                    }
                    Ok(PluginCommand::List { response_tx }) => {
                        let infos = managed_plugins.iter().map(|p| PluginInfo {
                            plugin_name: p.name.clone(),
                            uuid: p.uuid.to_string(),
                        }).collect();
                        let _ = response_tx.send(infos);
                    }
                }
            }
        };
        Err(err)
    }).map_err(|e| format!("Failed to spawn Geyser Plugins Handler thread: {:?}", e))?;
    Ok(handle)
}

async fn start_rpc_servers_runloop(
    config: &SurfpoolConfig,
    simnet_commands_tx: &Sender<SimnetCommand>,
    svm_locker: SurfnetSvmLocker,
    remote_rpc_client: &Option<SurfnetRemoteClient>,
    plugin_commands_tx: Sender<PluginCommand>,
) -> Result<(JoinHandle<()>, JoinHandle<()>, Box<dyn FnOnce() + Send>), String> {
    let rpc_addr: SocketAddr = config
        .rpc
        .get_rpc_base_url()
        .parse()
        .map_err(|e: std::net::AddrParseError| e.to_string())?;
    let ws_addr: SocketAddr = config
        .rpc
        .get_ws_base_url()
        .parse()
        .map_err(|e: std::net::AddrParseError| e.to_string())?;

    check_port_availability(rpc_addr, "RPC")?;
    check_port_availability(ws_addr, "WebSocket")?;

    let simnet_events_tx = svm_locker.simnet_events_tx();

    let middleware = SurfpoolMiddleware::new(
        svm_locker,
        simnet_commands_tx,
        &config.rpc,
        remote_rpc_client,
        plugin_commands_tx,
    );

    let (rpc_handle, rpc_close_handle) =
        start_http_rpc_server_runloop(config, middleware.clone(), simnet_events_tx.clone()).await?;
    let (ws_handle, ws_close_handle) =
        start_ws_rpc_server_runloop(config, middleware, simnet_events_tx).await?;

    let shutdown_rpc_servers: Box<dyn FnOnce() + Send> = Box::new(move || {
        rpc_close_handle.close();
        ws_close_handle.close();
    });

    Ok((rpc_handle, ws_handle, shutdown_rpc_servers))
}

async fn start_http_rpc_server_runloop(
    config: &SurfpoolConfig,
    middleware: SurfpoolMiddleware,
    simnet_events_tx: Sender<SimnetEvent>,
) -> Result<(JoinHandle<()>, jsonrpc_http_server::CloseHandle), String> {
    let server_bind: SocketAddr = config
        .rpc
        .get_rpc_base_url()
        .parse::<SocketAddr>()
        .map_err(|e| e.to_string())?;

    let mut io = MetaIoHandler::with_middleware(middleware);

    // Cheatcodes should be added first and are a special case. One of the cheatcode methods needs access
    // to the list of all cheatcode methods. The IoHandler allows us to iterate over them, so we're
    // initializing and storing that list here.
    {
        let cheatcode_methods: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(vec![]));
        let mut cheatcodes_impl = rpc::surfnet_cheatcodes::SurfnetCheatcodesRpc {
            registered_methods: Arc::clone(&cheatcode_methods),
        };
        io.extend_with(cheatcodes_impl.to_delegate());

        cheatcode_methods
            .write()
            .unwrap()
            .extend(io.iter().map(|(n, _)| n.clone()).collect::<Vec<String>>());
    }

    io.extend_with(rpc::minimal::SurfpoolMinimalRpc.to_delegate());
    io.extend_with(rpc::full::SurfpoolFullRpc.to_delegate());
    io.extend_with(rpc::jito::SurfpoolJitoRpc.to_delegate());
    io.extend_with(rpc::accounts_data::SurfpoolAccountsDataRpc.to_delegate());
    io.extend_with(rpc::accounts_scan::SurfpoolAccountsScanRpc.to_delegate());
    io.extend_with(rpc::bank_data::SurfpoolBankDataRpc.to_delegate());
    io.extend_with(rpc::admin::SurfpoolAdminRpc.to_delegate());

    let (close_handle_tx, close_handle_rx) =
        crossbeam_channel::bounded::<Result<jsonrpc_http_server::CloseHandle, String>>(1);

    let _handle = hiro_system_kit::thread_named("RPC Handler")
        .spawn(move || {
            let server = match ServerBuilder::new(io)
                .cors(DomainsValidation::Disabled)
                .threads(6)
                .max_request_body_size(15 * 1024 * 1024)
                .start_http(&server_bind)
            {
                Ok(server) => server,
                Err(e) => {
                    let error = format!("Failed to start RPC server: {:?}", e);
                    let _ = close_handle_tx.send(Err(error.clone()));
                    let _ = simnet_events_tx.send(SimnetEvent::Aborted(error));
                    return;
                }
            };

            let _ = close_handle_tx.send(Ok(server.close_handle()));
            server.wait();
            let _ = simnet_events_tx.send(SimnetEvent::Shutdown);
        })
        .map_err(|e| format!("Failed to spawn RPC Handler thread: {:?}", e))?;

    let close_handle = close_handle_rx
        .recv()
        .map_err(|_| "Failed to receive HTTP RPC server startup result".to_string())??;

    Ok((_handle, close_handle))
}
async fn start_ws_rpc_server_runloop(
    config: &SurfpoolConfig,
    middleware: SurfpoolMiddleware,
    simnet_events_tx: Sender<SimnetEvent>,
) -> Result<(JoinHandle<()>, jsonrpc_ws_server::CloseHandle), String> {
    let ws_server_bind: SocketAddr = config
        .rpc
        .get_ws_base_url()
        .parse::<SocketAddr>()
        .map_err(|e| e.to_string())?;

    let uid = std::sync::atomic::AtomicUsize::new(0);
    let ws_middleware = SurfpoolWebsocketMiddleware::new(middleware.clone(), None);

    let mut rpc_io = PubSubHandler::new(MetaIoHandler::with_middleware(ws_middleware));

    let (close_handle_tx, close_handle_rx) =
        crossbeam_channel::bounded::<Result<jsonrpc_ws_server::CloseHandle, String>>(1);

    let _ws_handle = hiro_system_kit::thread_named("WebSocket RPC Handler")
        .spawn(move || {
            // The pubsub handler needs to be able to run async tasks, so we create a Tokio runtime here
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build Tokio runtime");

            let tokio_handle = runtime.handle();
            rpc_io.extend_with(
                rpc::ws::SurfpoolWsRpc {
                    uid,
                    signature_subscription_map: Arc::new(RwLock::new(HashMap::new())),
                    account_subscription_map: Arc::new(RwLock::new(HashMap::new())),
                    program_subscription_map: Arc::new(RwLock::new(HashMap::new())),
                    slot_subscription_map: Arc::new(RwLock::new(HashMap::new())),
                    logs_subscription_map: Arc::new(RwLock::new(HashMap::new())),
                    snapshot_subscription_map: Arc::new(RwLock::new(HashMap::new())),
                    tokio_handle: tokio_handle.clone(),
                }
                .to_delegate(),
            );
            runtime.block_on(async move {
                let server = match WsServerBuilder::new(rpc_io)
                    .session_meta_extractor(move |ctx: &RequestContext| {
                        // Create meta from context + session
                        let runloop_context = RunloopContext {
                            id: None,
                            svm_locker: middleware.surfnet_svm.clone(),
                            simnet_commands_tx: middleware.simnet_commands_tx.clone(),
                            remote_rpc_client: middleware.remote_rpc_client.clone(),
                            rpc_config: middleware.config.clone(),
                            cheatcode_config: middleware.cheatcode_config.clone(),
                            plugin_commands_tx: middleware.plugin_commands_tx.clone(),
                        };
                        Some(SurfpoolWebsocketMeta::new(
                            runloop_context,
                            Some(Arc::new(Session::new(ctx.sender()))),
                        ))
                    })
                    .start(&ws_server_bind)
                {
                    Ok(server) => server,
                    Err(e) => {
                        let error = format!("Failed to start WebSocket RPC server: {:?}", e);
                        let _ = close_handle_tx.send(Err(error.clone()));
                        let _ = simnet_events_tx.send(SimnetEvent::Aborted(error));
                        return;
                    }
                };
                let _ = close_handle_tx.send(Ok(server.close_handle()));
                // The server itself is blocking, so spawn it in a separate thread if needed
                tokio::task::spawn_blocking(move || {
                    server.wait().unwrap();
                })
                .await
                .ok();

                let _ = simnet_events_tx.send(SimnetEvent::Shutdown);
            });
        })
        .map_err(|e| format!("Failed to spawn WebSocket RPC Handler thread: {:?}", e))?;

    let close_handle = close_handle_rx
        .recv()
        .map_err(|_| "Failed to receive WebSocket RPC server startup result".to_string())??;

    Ok((_ws_handle, close_handle))
}
