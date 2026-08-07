use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use actix_web::dev::ServerHandle;
use crossbeam::channel::{Select, Sender};
use indicatif::{MultiProgress, ProgressBar};
use log::{debug, error, info, warn};
#[cfg(feature = "version_check")]
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_core::{start_local_surfnet, surfnet::svm::SurfnetSvm};
use surfpool_types::{SanitizedConfig, SimnetCommand, SimnetEvent, SubgraphEvent};
use txtx_core::kit::{channel::Receiver, helpers::fs::FileLocation, types::frontend::BlockEvent};
use txtx_gql::kit::{indexmap::IndexMap, types::frontend::LogLevel, uuid::Uuid};

use super::{Context, StartSimnet};
use crate::{
    http::start_studio_and_scenario_server,
    runbook::handle_log_event,
    tui::{self, simnet::DisplayedUrl},
};

mod startup;
use startup::{
    SealFailure, StartupPlanFailure, plan_and_dispatch_startup, seal_startup_plan,
    spawn_startup_watchdog,
};

#[cfg(feature = "version_check")]
#[derive(Debug, Serialize, Deserialize)]
struct CheckVersionResponse {
    pub latest: String,
    pub deprecation_notice: Option<String>,
}

fn default_public_host(bind_host: &str) -> &str {
    match bind_host {
        "0.0.0.0" | "::" => "127.0.0.1",
        _ => bind_host,
    }
}

fn public_service_url(
    explicit_url: Option<String>,
    public_host: Option<&str>,
    scheme: &str,
    bind_host: &str,
    port: u16,
) -> String {
    explicit_url.unwrap_or_else(|| {
        let host = public_host.unwrap_or_else(|| default_public_host(bind_host));
        format!("{scheme}://{host}:{port}")
    })
}

pub async fn handle_start_local_surfnet_command(
    cmd: StartSimnet,
    ctx: &Context,
) -> Result<(), String> {
    // Local plugin loading is handled directly by `surfpool-core`.

    // We start the simnet as soon as possible. Startup work (account
    // cloning, runbook executions) is planned, sealed, and dispatched only
    // after the simnet's `Ready` event arrives below.
    let (surfnet_svm, simnet_events_rx, geyser_events_rx) =
        SurfnetSvm::new_with_db(cmd.accounts.db.as_deref(), cmd.svm_config())
            .map_err(|e| format!("Failed to initialize Surfnet SVM: {}", e))?;
    #[cfg(feature = "prometheus")]
    {
        if cmd.observability.metrics_enabled {
            match surfpool_core::telemetry::init_from_config(
                cmd.observability.metrics_enabled,
                &cmd.observability.metrics_addr,
            ) {
                Err(e) => {
                    let _ = surfnet_svm
                        .simnet_events_tx
                        .send(SimnetEvent::warn(format!("Metrics init failed: {}", e)));
                }
                Ok(_) => {
                    let _ = surfnet_svm.simnet_events_tx.send(SimnetEvent::info(format!(
                        "Metrics available at http://{}/metrics",
                        cmd.observability.metrics_addr
                    )));
                }
            }
        }
    }
    let (simnet_commands_tx, simnet_commands_rx) = crossbeam::channel::unbounded();
    let (subgraph_events_tx, subgraph_events_rx) = crossbeam::channel::unbounded();
    let simnet_events_tx = surfnet_svm.simnet_events_tx.clone();
    // Subscribe before the SVM moves into the simnet thread; the receiver
    // stays valid, and subscribing this early means no transition is missed.
    let startup_status_rx = surfnet_svm.subscribe_startup_status();

    // Check aidrop addresses
    let (mut airdrop_addresses, airdrop_events) = cmd.get_airdrop_addresses();

    let breaker = if cmd.runtime.no_tui {
        None
    } else {
        let keypair = Keypair::new();
        airdrop_addresses.push(keypair.pubkey());
        Some(keypair)
    };

    // Parse and merge snapshot files (multiple files supported, later files override earlier ones)
    // The actual loading happens in the runloop after the locker is created
    let snapshot = {
        let mut merged_snapshot: std::collections::BTreeMap<
            String,
            Option<surfpool_types::AccountSnapshot>,
        > = std::collections::BTreeMap::new();

        for snapshot_path in &cmd.accounts.snapshot {
            let file_location = FileLocation::from_path(std::path::PathBuf::from(snapshot_path));
            let content = file_location
                .read_content_as_utf8()
                .map_err(|e| format!("Failed to read snapshot file '{}': {}", snapshot_path, e))?;
            let snapshot_data: std::collections::BTreeMap<
                String,
                Option<surfpool_types::AccountSnapshot>,
            > = serde_json::from_str(&content)
                .map_err(|e| format!("Failed to parse snapshot JSON '{}': {}", snapshot_path, e))?;
            let _ = simnet_events_tx.send(SimnetEvent::info(format!(
                "Loaded {} accounts from snapshot file: {}",
                snapshot_data.len(),
                snapshot_path
            )));

            // Merge into the combined snapshot (later files override earlier ones)
            merged_snapshot.extend(snapshot_data);
        }

        merged_snapshot
    };

    // Build config
    let config = cmd.surfpool_config(airdrop_addresses, snapshot);

    let studio_binding_address = config.studio.get_studio_base_url();
    let public_host = std::env::var("SURFPOOL_PUBLIC_HOST").ok();

    // Allow overriding public-facing URLs via environment variables
    // This is useful when running behind a reverse proxy (e.g., Caddy, nginx)
    let rpc_url = public_service_url(
        std::env::var("SURFPOOL_PUBLIC_RPC_URL").ok(),
        public_host.as_deref(),
        "http",
        &config.rpc.bind_host,
        config.rpc.bind_port,
    );
    let ws_url = public_service_url(
        std::env::var("SURFPOOL_PUBLIC_WS_URL").ok(),
        public_host.as_deref(),
        "ws",
        &config.rpc.bind_host,
        config.rpc.ws_port,
    );
    let studio_url = public_service_url(
        std::env::var("SURFPOOL_PUBLIC_STUDIO_URL").ok(),
        public_host.as_deref(),
        "http",
        &config.studio.bind_host,
        config.studio.bind_port,
    );

    let graphql_query_route_url = format!("{}/workspace/v1/graphql", studio_url);
    let rpc_datasource_url = config.simnets[0].get_sanitized_datasource_url();

    let sanitized_config = SanitizedConfig {
        rpc_url,
        ws_url,
        rpc_datasource_url,
        studio_url,
        graphql_query_route_url,
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: None,
    };

    let explorer_handle = match start_studio_and_scenario_server(
        studio_binding_address,
        sanitized_config.clone(),
        subgraph_events_tx.clone(),
        ctx,
        !cmd.runtime.no_studio,
    )
    .await
    {
        Ok(explorer_handle) => Some(explorer_handle),
        Err(e) => {
            error!("Failed to start subgraph and explorer server: {}", e);
            let _ = simnet_events_tx.send(SimnetEvent::warn(format!(
                "Failed to start subgraph and explorer server: {}",
                e
            )));
            let _ = simnet_events_tx.send(SimnetEvent::info("Continuing with simnet startup..."));
            None
        }
    };

    let simnet_commands_tx_copy = simnet_commands_tx.clone();
    let config_copy = config.clone();

    let simnet_events_tx_for_thread = simnet_events_tx.clone();
    let simnet_handle = hiro_system_kit::thread_named("simnet")
        .spawn(move || {
            let future = start_local_surfnet(
                surfnet_svm,
                config_copy,
                simnet_commands_tx_copy,
                simnet_commands_rx,
                geyser_events_rx,
            );
            if let Err(e) = hiro_system_kit::nestable_block_on(future) {
                // Send the error through the event channel so the main thread can handle it
                let _ = simnet_events_tx_for_thread.send(SimnetEvent::Aborted(e.to_string()));
            }
            Ok::<(), String>(())
        })
        .map_err(|e| format!("{}", e))?;

    // Collect events that occur before Ready so we can re-send them to the TUI
    let mut early_events = Vec::new();
    let initial_transactions = loop {
        match simnet_events_rx.recv() {
            Ok(SimnetEvent::Aborted(error)) => {
                eprintln!("Error: {}", error);
                return Err(error);
            }
            Ok(SimnetEvent::Shutdown) => return Ok(()),
            Ok(SimnetEvent::Ready(initial_transactions)) => break initial_transactions,
            Ok(other) => early_events.push(other),
            Err(_) => continue,
        }
    };

    // Re-send early events (like snapshot loading messages) so the TUI receives them
    for event in early_events {
        let _ = simnet_events_tx.send(event);
    }

    for event in airdrop_events {
        let _ = simnet_events_tx.send(event);
    }

    let simnet_commands_tx_copy = simnet_commands_tx.clone();
    let mut runbook_progress_rx = vec![];
    if !cmd.project.no_deploy {
        match plan_and_dispatch_startup(&cmd, &simnet_events_tx, &simnet_commands_tx_copy).await {
            Ok(rx) => runbook_progress_rx.push(rx),
            // Planning failed before the plan was sealed. Drive the startup
            // state machine to Failed (from Planning-unsealed this always
            // applies) and keep going: whether a failed startup is fatal is
            // the watchdog's decision.
            Err(StartupPlanFailure::Planning(e)) => {
                let _ = simnet_commands_tx_copy.send(SimnetCommand::FailStartupPlanning(e.clone()));
                let _ = simnet_events_tx
                    .send(SimnetEvent::warn(format!("Startup planning failed: {e}")));
            }
            // The command loop is dead or wedged, so the startup state machine
            // is unreachable and no session can ever become ready.
            // Nothing left to display; exit.
            Err(StartupPlanFailure::Sealing(SealFailure::Unreachable(e))) => return Err(e),
            // The startup state machine refused the seal. The command loop is
            // alive and the state is known, but the CLI cannot dispatch work
            // against an unsealed plan, so this is fatal too; the reason names
            // which rule declined.
            Err(StartupPlanFailure::Sealing(SealFailure::Refused(error))) => {
                return Err(format!("Startup plan refused: {error}"));
            }
        }
    } else {
        // There are no startup tasks to execute, so seal the empty plan
        // ourselves; the surfnet cannot reach `Ready` without a sealed plan.
        // An unreachable command loop is fatal here too, same as the
        // Sealing arm above.
        seal_startup_plan(&simnet_commands_tx_copy, vec![])
            .map_err(|failure| failure.to_string())?;
    }

    let is_headless = cmd.runtime.daemon || cmd.runtime.no_tui;
    spawn_startup_watchdog(
        is_headless,
        startup_status_rx,
        simnet_events_tx.clone(),
        simnet_commands_tx.clone(),
    )?;

    // Non blocking check for new versions
    #[cfg(feature = "version_check")]
    {
        let mut local_version = env!("CARGO_PKG_VERSION").to_string();
        if cmd.runtime.ci {
            local_version = format!("{}-ci", local_version);
        }
        let response = txtx_gql::kit::reqwest::get(format!(
            "https://cloud.txtx.run/api/versions?v=/{}",
            local_version
        ))
        .await;
        if let Ok(response) = response {
            if let Ok(body) = response.json::<CheckVersionResponse>().await {
                if let Some(deprecation_notice) = body.deprecation_notice {
                    let _ =
                        simnet_events_tx.send(SimnetEvent::warn(deprecation_notice.to_string()));
                }
            }
        }
    }

    let cmd_cc = cmd.clone();
    let ctx_cc = ctx.clone();

    let runloop_terminator = Arc::new(AtomicBool::new(false));

    // service_result carries the Aborted event for startup failures, so the
    // caller can exit nonzero accordingly.
    let service_result = start_service(
        cmd_cc,
        simnet_events_rx,
        subgraph_events_rx,
        runbook_progress_rx,
        simnet_commands_tx,
        breaker,
        sanitized_config,
        explorer_handle,
        ctx_cc,
        Some(runloop_terminator),
        initial_transactions,
    )
    .await;

    // Wait for the simnet thread to finish cleanup (including Drop/checkpoint)
    let _ = simnet_handle.join();

    service_result
}

/// Parses declared clone addresses, keeping the ones that parse and describing
/// the ones that do not.
///
/// No runbook is built from this list: the addresses are handed to the surfnet
/// to hydrate, and everything else proceeds without them. So a malformed entry
/// costs that one account rather than the startup, and the rejected addresses
/// are named so a user can fix the typo.
fn parse_clone_addresses(clones: &[String]) -> (Vec<Pubkey>, Vec<String>) {
    let mut parsed = vec![];
    let mut rejected = vec![];
    for clone in clones {
        match clone.parse() {
            Ok(pubkey) => parsed.push(pubkey),
            Err(e) => rejected.push(format!("{clone}: {e}")),
        }
    }
    (parsed, rejected)
}

#[allow(clippy::too_many_arguments)]
async fn start_service(
    cmd: StartSimnet,
    simnet_events_rx: Receiver<SimnetEvent>,
    subgraph_events_rx: Receiver<SubgraphEvent>,
    runbook_progress_rx: Vec<Receiver<BlockEvent>>,
    simnet_commands_tx: Sender<SimnetCommand>,
    breaker: Option<Keypair>,
    sanitized_config: SanitizedConfig,
    explorer_handle: Option<ServerHandle>,
    _ctx: Context,
    runloop_terminator: Option<Arc<AtomicBool>>,
    initial_transactions: u64,
) -> Result<(), String> {
    let displayed_url = if cmd.runtime.no_studio {
        DisplayedUrl::Datasource(sanitized_config)
    } else {
        DisplayedUrl::Studio(sanitized_config)
    };
    let include_debug_logs = cmd.observability.log_level.to_lowercase().eq("debug");

    // Start frontend - kept on main thread
    if cmd.runtime.daemon || cmd.runtime.no_tui {
        log_events(
            simnet_events_rx,
            subgraph_events_rx,
            include_debug_logs,
            runbook_progress_rx,
            simnet_commands_tx,
            runloop_terminator.unwrap(),
        )?;
    } else {
        tui::simnet::start_app(
            simnet_events_rx,
            simnet_commands_tx,
            include_debug_logs,
            runbook_progress_rx,
            displayed_url,
            breaker,
            initial_transactions,
        )
        .map_err(|e| format!("{}", e))?;
    }
    if let Some(explorer_handle) = explorer_handle {
        let _ = explorer_handle.stop(true).await;
    }

    Ok(())
}

fn log_events(
    simnet_events_rx: Receiver<SimnetEvent>,
    subgraph_events_rx: Receiver<SubgraphEvent>,
    include_debug_logs: bool,
    runbook_progress_rx: Vec<Receiver<BlockEvent>>,
    simnet_commands_tx: Sender<SimnetCommand>,
    runloop_terminator: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut deployment_completed = false;
    let do_stop_loop = runloop_terminator.clone();
    let terminate_tx = simnet_commands_tx.clone();
    ctrlc::set_handler(move || {
        do_stop_loop.store(true, Ordering::Relaxed);
        // Send terminate command to allow graceful shutdown (Drop to run)
        let _ = terminate_tx.send(SimnetCommand::Terminate(None));
    })
    .expect("Error setting Ctrl-C handler");

    let log_filter = if include_debug_logs {
        LogLevel::Debug
    } else {
        LogLevel::Info
    };
    let mut active_spinners: IndexMap<Uuid, ProgressBar> = IndexMap::new();
    let mut multi_progress = MultiProgress::new();

    loop {
        if runloop_terminator.load(Ordering::Relaxed) {
            break;
        }
        let mut selector = Select::new();
        let mut handles = vec![];

        selector.recv(&simnet_events_rx);
        selector.recv(&subgraph_events_rx);

        if !deployment_completed {
            for rx in runbook_progress_rx.iter() {
                handles.push(selector.recv(rx));
            }
        }

        // Use select_timeout to periodically check the termination flag
        // This ensures Ctrl+C is responsive even when no events are arriving
        let oper = match selector.select_timeout(Duration::from_millis(100)) {
            Ok(oper) => oper,
            Err(_) => continue, // Timeout - check termination flag at top of loop
        };
        match oper.index() {
            0 => match oper.recv(&simnet_events_rx) {
                Ok(event) => match event {
                    SimnetEvent::AccountUpdate(_dt, _) => {
                        info!("{}", event.account_update_msg());
                    }
                    // A headless run reports readiness through the watchdog,
                    // which reads the status directly.
                    SimnetEvent::StartupStatusChanged(_) => {}
                    // The answer record matters to other listeners (tests,
                    // the SDK); nothing for an operator to read.
                    SimnetEvent::AnsweredClient { .. } => {}
                    SimnetEvent::PluginLoaded(_) => {
                        info!("{}", event.plugin_loaded_msg());
                    }
                    SimnetEvent::EpochInfoUpdate(_) => {
                        info!("{}", event.epoch_info_update_msg());
                    }
                    SimnetEvent::SystemClockUpdated(_) => {}
                    SimnetEvent::ClockUpdate(_) => {}
                    SimnetEvent::ErrorLog(_dt, log) => {
                        error!("{}", log);
                    }
                    SimnetEvent::InfoLog(_dt, log) => {
                        info!("{}", log);
                    }
                    SimnetEvent::DebugLog(_dt, log) => {
                        debug!("{}", log);
                    }
                    SimnetEvent::WarnLog(_dt, log) => {
                        warn!("{}", log);
                    }
                    SimnetEvent::TransactionReceived(_dt, transaction) => {
                        if deployment_completed {
                            info!("Transaction received {}", transaction.signatures[0]);
                        }
                    }
                    SimnetEvent::TransactionProcessed(_dt, meta, _err) => {
                        if deployment_completed {
                            info!("Transaction processed {}", meta.signature);
                            for log in meta.logs {
                                info!("{}", log);
                            }
                        }
                    }
                    SimnetEvent::Aborted(error) => {
                        error!("{}", error);
                        return Err(error);
                    }
                    SimnetEvent::Ready(_) => {}
                    SimnetEvent::Connected(_rpc_url) => {}
                    SimnetEvent::Shutdown => {
                        break;
                    }
                    SimnetEvent::TaggedProfile {
                        result,
                        tag,
                        timestamp: _,
                    } => {
                        info!(
                            "Profiled [{}]: {} CUs",
                            tag, result.transaction_profile.compute_units_consumed
                        );
                    }
                    SimnetEvent::RunbookStarted(runbook_id) => {
                        deployment_completed = false;
                        info!("Runbook '{}' execution started", runbook_id);
                        let _ = simnet_commands_tx
                            .send(SimnetCommand::StartRunbookExecution(runbook_id));
                    }
                    SimnetEvent::RunbookCompleted(runbook_id, errors) => {
                        deployment_completed = true;
                        info!("Runbook '{}' execution completed", runbook_id);
                        let _ = simnet_commands_tx
                            .send(SimnetCommand::CompleteRunbookExecution(runbook_id, errors));
                    }
                },
                Err(_e) => {
                    break;
                }
            },
            1 => match oper.recv(&subgraph_events_rx) {
                Ok(event) => match event {
                    SubgraphEvent::ErrorLog(_dt, log) => {
                        error!("{}", log);
                    }
                    SubgraphEvent::InfoLog(_dt, log) => {
                        info!("{}", log);
                    }
                    SubgraphEvent::DebugLog(_dt, log) => {
                        debug!("{}", log);
                    }
                    SubgraphEvent::WarnLog(_dt, log) => {
                        warn!("{}", log);
                    }
                    SubgraphEvent::EndpointReady => {}
                    SubgraphEvent::Shutdown => {
                        break;
                    }
                },
                Err(_e) => {
                    break;
                }
            },
            i => match oper.recv(&runbook_progress_rx[i - 2]) {
                Ok(event) => {
                    if let BlockEvent::LogEvent(log) = event {
                        handle_log_event(
                            &mut multi_progress,
                            log,
                            &log_filter,
                            &mut active_spinners,
                            false,
                        )
                    }
                }
                Err(_e) => {
                    deployment_completed = true;
                }
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{default_public_host, public_service_url};

    #[test]
    fn default_public_host_maps_wildcard_binds_to_loopback() {
        assert_eq!(default_public_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(default_public_host("::"), "127.0.0.1");
    }

    #[test]
    fn default_public_host_preserves_specific_hosts() {
        assert_eq!(default_public_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(default_public_host("10.0.0.5"), "10.0.0.5");
    }

    #[test]
    fn public_service_url_prefers_explicit_url_over_everything_else() {
        assert_eq!(
            public_service_url(
                Some("https://rpc.example.com".to_string()),
                Some("staging.example.com"),
                "http",
                "0.0.0.0",
                8899,
            ),
            "https://rpc.example.com"
        );
    }

    #[test]
    fn public_service_url_uses_public_host_when_present() {
        assert_eq!(
            public_service_url(None, Some("staging.example.com"), "http", "0.0.0.0", 8899),
            "http://staging.example.com:8899"
        );
    }

    #[test]
    fn public_service_url_uses_loopback_for_wildcard_bind_when_unset() {
        assert_eq!(
            public_service_url(None, None, "http", "0.0.0.0", 8899),
            "http://127.0.0.1:8899"
        );
    }
}
