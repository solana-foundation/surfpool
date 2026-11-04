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
use surfpool_types::{SanitizedConfig, SimnetCommand, SimnetEvent, SimnetEventsTx, SubgraphEvent};
use txtx_core::kit::{channel::Receiver, helpers::fs::FileLocation, types::frontend::BlockEvent};
use txtx_gql::kit::{indexmap::IndexMap, types::frontend::LogLevel, uuid::Uuid};

use super::{Context, StartSimnet};
use crate::{
    http::start_studio_and_scenario_server,
    runbook::handle_log_event,
    tui::{self, simnet::DisplayedUrl},
};

mod endpoints;
mod startup;
use endpoints::resolve_endpoints;
use startup::{
    SealFailure, StartupPlan, dispatch_startup_plan, plan_startup, seal_startup_plan,
    spawn_startup_watchdog,
};

#[cfg(feature = "version_check")]
#[derive(Debug, Serialize, Deserialize)]
struct CheckVersionResponse {
    pub latest: String,
    pub deprecation_notice: Option<String>,
}

pub async fn handle_start_local_surfnet_command(
    cmd: StartSimnet,
    ctx: &Context,
) -> Result<(), String> {
    // Local plugin loading is handled directly by `surfpool-core`.

    // We start the simnet as soon as possible. Startup work (account
    // cloning, runbook executions) is planned while the simnet comes up and
    // sealed and dispatched once its `CoreStarted` event arrives.
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
                    surfnet_svm
                        .simnet_events_tx
                        .warn(format!("Metrics init failed: {}", e));
                }
                Ok(_) => {
                    surfnet_svm.simnet_events_tx.info(format!(
                        "Metrics available at http://{}/metrics",
                        cmd.observability.metrics_addr
                    ));
                }
            }
        }
    }
    let (simnet_commands_tx, simnet_commands_rx) = crossbeam::channel::unbounded();
    let runloop_terminator = Arc::new(AtomicBool::new(false));
    let signal_terminator = runloop_terminator.clone();
    let signal_commands_tx = simnet_commands_tx.clone();
    // Own process signals before starting HTTP; both frontends share this handler.
    ctrlc::set_handler(move || {
        signal_terminator.store(true, Ordering::Release);
        if let Err(error) = signal_commands_tx.send(SimnetCommand::Terminate(None)) {
            warn!("Unable to request surfnet shutdown: {error}");
        }
    })
    .map_err(|error| format!("Failed to install shutdown signal handler: {error}"))?;
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
            simnet_events_tx.info(format!(
                "Loaded {} accounts from snapshot file: {}",
                snapshot_data.len(),
                snapshot_path
            ));

            // Merge into the combined snapshot (later files override earlier ones)
            merged_snapshot.extend(snapshot_data);
        }

        merged_snapshot
    };

    // Build config
    let config = cmd.surfpool_config(airdrop_addresses, snapshot);

    let endpoints = resolve_endpoints(&config)?;

    let graphql_query_route_url = format!(
        "{}/workspace/v1/graphql",
        endpoints.studio_url.trim_end_matches('/')
    );
    let rpc_datasource_url = config.simnets[0].get_sanitized_datasource_url();

    let sanitized_config = SanitizedConfig {
        rpc_url: endpoints.rpc_url,
        ws_url: endpoints.ws_url,
        rpc_datasource_url,
        studio_url: endpoints.studio_url,
        graphql_query_route_url,
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: None,
    };

    let explorer_handle = match start_studio_and_scenario_server(
        endpoints.studio_bind_addr,
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
            simnet_events_tx.warn(format!(
                "Failed to start subgraph and explorer server: {}",
                e
            ));
            simnet_events_tx.info("Continuing with simnet startup...");
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
                simnet_events_tx_for_thread.aborted(e.to_string());
            }
            Ok::<(), String>(())
        })
        .map_err(|e| format!("{}", e))?;

    // Plan on this thread, before the frontend takes the terminal: scaffolding
    // may prompt the user on stdin. The plan is not sealed until the surfnet's
    // command loop is alive, which the frontend signals on `CoreStarted`.
    let mut runbook_progress_rx = vec![];
    let startup_plan = if !cmd.project.no_deploy && !runloop_terminator.load(Ordering::Acquire) {
        match plan_startup(&cmd, &simnet_events_tx).await {
            Ok(plan) => {
                runbook_progress_rx.push(plan.progress_rx.clone());
                Some(Ok(plan))
            }
            Err(e) => Some(Err(e)),
        }
    } else {
        None
    };

    let (core_started_tx, core_started_rx) = crossbeam::channel::bounded(1);
    spawn_startup_dispatch(
        cmd.clone(),
        startup_plan,
        airdrop_events,
        core_started_rx,
        simnet_events_tx.clone(),
        simnet_commands_tx.clone(),
        runloop_terminator.clone(),
    )?;

    let is_headless = cmd.runtime.daemon || cmd.runtime.no_tui;
    spawn_startup_watchdog(
        is_headless,
        startup_status_rx,
        simnet_events_tx.clone(),
        simnet_commands_tx.clone(),
    )?;

    let cmd_cc = cmd.clone();
    let ctx_cc = ctx.clone();

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
        runloop_terminator,
        core_started_tx,
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
    runloop_terminator: Arc<AtomicBool>,
    core_started_tx: Sender<()>,
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
            runloop_terminator,
            core_started_tx,
        )?;
    } else {
        tui::simnet::start_app(
            simnet_events_rx,
            simnet_commands_tx,
            include_debug_logs,
            runbook_progress_rx,
            displayed_url,
            breaker,
            runloop_terminator,
            core_started_tx,
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
    core_started_tx: Sender<()>,
) -> Result<(), String> {
    let mut deployment_completed = false;

    let log_filter = if include_debug_logs {
        LogLevel::Debug
    } else {
        LogLevel::Info
    };
    let mut active_spinners: IndexMap<Uuid, ProgressBar> = IndexMap::new();
    let mut multi_progress = MultiProgress::new();

    loop {
        if runloop_terminator.load(Ordering::Acquire) {
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
                    SimnetEvent::CoreStarted(_) => {
                        let _ = core_started_tx.try_send(());
                    }
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

/// Finishes startup off the frontend's thread once the surfnet signals
/// `CoreStarted`: announces the airdrops, then seals the plan and dispatches
/// its work. A planning failure drives the startup state machine to Failed
/// and leaves fatality to the watchdog policy; a sealing failure leaves no
/// way for the session to become ready, so it aborts the session through
/// the event channel. If the frontend exits before `CoreStarted`, there is
/// nothing left to dispatch and the thread ends.
#[allow(clippy::too_many_arguments)]
fn spawn_startup_dispatch(
    cmd: StartSimnet,
    startup_plan: Option<Result<StartupPlan, String>>,
    airdrop_events: Vec<SimnetEvent>,
    core_started_rx: Receiver<()>,
    simnet_events_tx: SimnetEventsTx,
    simnet_commands_tx: Sender<SimnetCommand>,
    runloop_terminator: Arc<AtomicBool>,
) -> Result<(), String> {
    hiro_system_kit::thread_named("startup-dispatch")
        .spawn(move || {
            if core_started_rx.recv().is_err() {
                return Ok(());
            }
            for event in airdrop_events {
                simnet_events_tx.forward(event);
            }
            if runloop_terminator.load(Ordering::Acquire) {
                return Ok(());
            }
            // A seal failure ends the session, so stop the surfnet too: the
            // frontend returns on `Aborted`, and the caller then joins the
            // simnet thread, which would otherwise keep running after a
            // timed-out seal is eventually answered.
            let abort_startup = |failure: SealFailure| {
                let _ = simnet_commands_tx.send(SimnetCommand::Terminate(None));
                simnet_events_tx.aborted(format!("Failed to seal startup plan: {failure}"));
            };
            match startup_plan {
                Some(Ok(plan)) => {
                    if let Err(failure) =
                        dispatch_startup_plan(&cmd, plan, &simnet_events_tx, &simnet_commands_tx)
                    {
                        abort_startup(failure);
                    }
                }
                Some(Err(e)) => {
                    let _ = simnet_commands_tx.send(SimnetCommand::FailStartupPlanning(e.clone()));
                    simnet_events_tx.warn(format!("Startup planning failed: {e}"));
                }
                // There are no startup tasks to execute, so seal the empty plan
                // ourselves; the surfnet cannot reach `Ready` without a sealed plan.
                None => {
                    if let Err(failure) = seal_startup_plan(&simnet_commands_tx, vec![]) {
                        abort_startup(failure);
                    }
                }
            }

            // Non blocking check for new versions
            #[cfg(feature = "version_check")]
            {
                let mut local_version = env!("CARGO_PKG_VERSION").to_string();
                if cmd.runtime.ci {
                    local_version = format!("{}-ci", local_version);
                }
                let response = hiro_system_kit::nestable_block_on(async {
                    match txtx_gql::kit::reqwest::get(format!(
                        "https://cloud.txtx.run/api/versions?v=/{}",
                        local_version
                    ))
                    .await
                    {
                        Ok(response) => response.json::<CheckVersionResponse>().await.ok(),
                        Err(_) => None,
                    }
                });
                if let Some(deprecation_notice) = response.and_then(|body| body.deprecation_notice)
                {
                    simnet_events_tx.warn(deprecation_notice.to_string());
                }
            }
            Ok::<(), String>(())
        })
        .map(|_| ())
        .map_err(|e| format!("Thread to dispatch startup exited: {e}"))
}
