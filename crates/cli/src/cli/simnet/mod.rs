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
use surfpool_core::lifecycle::SurfnetInstance;
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
    SealFailure, StartupPlanFailure, plan_and_dispatch_startup, seal_startup_plan,
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
    // cloning, runbook executions) is planned, sealed, and dispatched only
    // after the simnet's `Ready` event arrives below.
    // A historical surfnet (`--after`) has SVM Replay execute it and no datasource; everything
    // else is the ordinary surfnet. Core selects the mode before anything is seeded.
    let historical = super::historical::config(&cmd)?;
    let mut svm_config = cmd.svm_config();
    svm_config.historical = historical.clone();
    let (instance, simnet_events_rx) =
        SurfnetInstance::open(cmd.accounts.db.as_deref(), svm_config)
            .map_err(|e| format!("Failed to initialize Surfnet SVM: {}", e))?;
    #[cfg(feature = "prometheus")]
    {
        if cmd.observability.metrics_enabled {
            match surfpool_core::telemetry::init_from_config(
                cmd.observability.metrics_enabled,
                &cmd.observability.metrics_addr,
            ) {
                Err(e) => {
                    instance
                        .simnet_events_tx()
                        .warn(format!("Metrics init failed: {}", e));
                }
                Ok(_) => {
                    instance.simnet_events_tx().info(format!(
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
    let simnet_events_tx = instance.simnet_events_tx();
    // Subscribe before the instance moves into the simnet thread; the receiver
    // stays valid, and subscribing this early means no transition is missed.
    let startup_status_rx = instance.subscribe_startup_status();
    let mut core_started_rx = instance.subscribe_core_started();

    // Check aidrop addresses
    let (airdrop_addresses, airdrop_events) = cmd.get_airdrop_addresses();

    // The TUI's burner is a key created for this session (funded as one, see
    // `SimnetConfig::session_addresses`).
    let breaker = if cmd.runtime.no_tui {
        None
    } else {
        Some(Keypair::new())
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

    // Build config. A historical surfnet has no datasource.
    let mut config = cmd.surfpool_config(airdrop_addresses, snapshot);
    if let Some(breaker) = &breaker {
        config.simnets[0].session_addresses.push(breaker.pubkey());
    }
    if historical.is_some() {
        config.simnets[0].remote_rpc_url = None;
        config.simnets[0].offline_mode = true;
    }

    let endpoints = resolve_endpoints(&config)?;

    let graphql_query_route_url = format!(
        "{}/workspace/v1/graphql",
        endpoints.studio_url.trim_end_matches('/')
    );
    // A historical surfnet has no datasource: interfaces learn what it forks after instead.
    let (rpc_datasource_url, historical_source) = super::historical::interface_sources(
        &cmd,
        config.simnets[0].get_sanitized_datasource_url(),
    );

    let sanitized_config = SanitizedConfig {
        rpc_url: endpoints.rpc_url,
        ws_url: endpoints.ws_url,
        rpc_datasource_url,
        studio_url: endpoints.studio_url,
        graphql_query_route_url,
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: None,
        historical: historical_source,
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
            let future = instance.run(config_copy, simnet_commands_tx_copy, simnet_commands_rx);
            if let Err(e) = hiro_system_kit::nestable_block_on(future) {
                // Send the error through the event channel so the main thread can handle it
                simnet_events_tx_for_thread.aborted(e.to_string());
            }
            Ok::<(), String>(())
        })
        .map_err(|e| format!("{}", e))?;

    let mut runbook_progress_rx = vec![];
    let mut historical_planner = None;
    let initial_transactions = if cmd.historical.after.is_some() {
        // SVM Replay prepares a historical fork inside the core, which can take minutes. The
        // interface starts now and shows that loading state, so Esc and Ctrl-C reach the core
        // before the fork is ready; startup planning waits for the core on its own thread.
        replay_early_events(&simnet_events_tx, vec![], airdrop_events);
        let (progress_tx, progress_rx) = txtx_core::kit::channel::unbounded();
        runbook_progress_rx.push(progress_rx);
        let (cmd, events_tx, commands_tx) = (
            cmd.clone(),
            simnet_events_tx.clone(),
            simnet_commands_tx.clone(),
        );
        let planner = hiro_system_kit::thread_named("Historical startup planning")
            .spawn(move || {
                // Plan only once the core serves; a core that ends first (startup failed or
                // was cancelled) drops the sender, and nothing is planned.
                let started = hiro_system_kit::nestable_block_on(
                    core_started_rx.wait_for(|started| *started),
                )
                .is_ok();
                if !started {
                    return;
                }
                let planned = hiro_system_kit::nestable_block_on(plan_startup_or_seal(
                    &cmd,
                    &events_tx,
                    &commands_tx,
                ));
                match planned {
                    Ok(Some(rx)) => {
                        for event in rx.iter() {
                            if progress_tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        events_tx.error(format!("Startup planning failed: {error}"));
                        let _ = commands_tx.send(SimnetCommand::Terminate(None));
                    }
                }
            })
            .map_err(|e| format!("{}", e))?;
        historical_planner = Some(planner);
        // A historical fork starts with no stored transactions.
        0
    } else {
        // Collect events that occur before Ready so we can re-send them to the TUI
        let mut early_events = Vec::new();
        let initial_transactions = loop {
            match simnet_events_rx.recv() {
                Ok(SimnetEvent::Aborted(error)) => {
                    eprintln!("Error: {}", error);
                    return Err(error);
                }
                Ok(SimnetEvent::Shutdown) => return Ok(()),
                Ok(SimnetEvent::CoreStarted(initial_transactions)) => break initial_transactions,
                Ok(other) => early_events.push(other),
                Err(_) => continue,
            }
        };

        // Re-send early events (like snapshot loading messages) so the TUI receives them
        replay_early_events(&simnet_events_tx, early_events, airdrop_events);

        if !runloop_terminator.load(Ordering::Acquire) {
            if let Some(rx) =
                plan_startup_or_seal(&cmd, &simnet_events_tx, &simnet_commands_tx).await?
            {
                runbook_progress_rx.push(rx);
            }
        }
        initial_transactions
    };

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
                    let _ = simnet_events_tx.warn(deprecation_notice.to_string());
                }
            }
        }
    }

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
        initial_transactions,
    )
    .await;

    // The CLI process is exiting: end its historical backend now, so a runloop still waiting
    // on one (stopped or wedged) finishes instead of holding the exit. This process runs only
    // this surfnet; embedders stop each instance through its own Terminate.
    surfpool_core::lifecycle::terminate_historical_backends();

    // Wait for the simnet thread to finish cleanup (including Drop/checkpoint)
    let _ = simnet_handle.join();

    // The historical planner ends with the core (its readiness wait fails) or with its
    // runbooks; it is joined unless a runbook outlives the session.
    if let Some(planner) = historical_planner {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !planner.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if planner.is_finished() {
            let _ = planner.join();
        } else {
            warn!("Historical startup planning still running at exit");
        }
    }

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
    initial_transactions: u64,
) -> Result<(), String> {
    // A historical surfnet shows its loading state until SVM Replay's fork is ready.
    let displayed_url = if cmd.historical.after.is_some() {
        DisplayedUrl::Historical(sanitized_config)
    } else if cmd.runtime.no_studio {
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
            runloop_terminator,
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
                    SimnetEvent::NetworkReset(clock) => {
                        info!("Network reset to slot {}", clock.slot);
                    }
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
                    SimnetEvent::CoreStarted(_) => {}
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

/// Plans and dispatches the project's startup work, or seals the empty plan when nothing
/// is deployed. An error leaves no session that can ever become ready.
async fn plan_startup_or_seal(
    cmd: &StartSimnet,
    simnet_events_tx: &SimnetEventsTx,
    simnet_commands_tx: &Sender<SimnetCommand>,
) -> Result<Option<Receiver<BlockEvent>>, String> {
    if cmd.project.no_deploy {
        // There are no startup tasks to execute, so seal the empty plan
        // ourselves; the surfnet cannot reach `Ready` without a sealed plan.
        // An unreachable command loop is fatal here too, same as the
        // Sealing arms below.
        seal_startup_plan(simnet_commands_tx, vec![]).map_err(|failure| failure.to_string())?;
        return Ok(None);
    }
    match plan_and_dispatch_startup(cmd, simnet_events_tx, simnet_commands_tx).await {
        Ok(rx) => Ok(Some(rx)),
        // Planning failed before the plan was sealed. Drive the startup
        // state machine to Failed (from Planning-unsealed this always
        // applies) and keep going: whether a failed startup is fatal is
        // the watchdog's decision.
        Err(StartupPlanFailure::Planning(e)) => {
            let _ = simnet_commands_tx.send(SimnetCommand::FailStartupPlanning(e.clone()));
            simnet_events_tx.warn(format!("Startup planning failed: {e}"));
            Ok(None)
        }
        // The command loop is dead or wedged, so the startup state machine
        // is unreachable and no session can ever become ready.
        // Nothing left to display; exit.
        Err(StartupPlanFailure::Sealing(SealFailure::Unreachable(e))) => Err(e),
        // The startup state machine refused the seal. The command loop is
        // alive and the state is known, but the CLI cannot dispatch work
        // against an unsealed plan, so this is fatal too; the reason names
        // which rule declined.
        Err(StartupPlanFailure::Sealing(SealFailure::Refused(error))) => {
            Err(format!("Startup plan refused: {error}"))
        }
    }
}

/// Re-send events buffered before `CoreStarted` (and the airdrop
/// announcements) so the frontend consumer receives them.
pub(crate) fn replay_early_events(
    simnet_events_tx: &SimnetEventsTx,
    early_events: Vec<SimnetEvent>,
    airdrop_events: Vec<SimnetEvent>,
) {
    // On a separate thread: the caller owns the only receiver and does not
    // drain again until the frontend consumer starts, so a blocking emit on
    // the caller's thread wedges startup once the buffer fills (a warm
    // start can buffer a full channel of replayed transactions before
    // CoreStarted). The replay thread blocks until the consumer drains,
    // intentionally.
    let tx = simnet_events_tx.clone();
    let _ = hiro_system_kit::thread_named("early-event-replay").spawn(move || {
        for event in early_events.into_iter().chain(airdrop_events) {
            tx.forward(event);
        }
        Ok::<(), String>(())
    });
}

#[cfg(test)]
mod replay_tests {
    use std::{thread, time::Duration};

    use surfpool_types::SimnetEventsTx;

    use super::*;

    /// Startup calls the replay on the thread that owns the only receiver,
    /// and concurrent producers may already have refilled the buffer, so
    /// the call must return without waiting on a drain. Buffered events
    /// route by class: lifecycle events arrive once the consumer drains;
    /// telemetry may drop, telemetry's standing contract. Small capacity
    /// for speed; the mechanism is capacity-independent.
    #[test]
    fn replay_returns_before_the_consumer_drains() {
        let (tx, rx) = SimnetEventsTx::channel(4);
        for i in 0..4 {
            tx.info(format!("producer {i}"));
        }

        let replay_tx = tx.clone();
        let replayer = thread::spawn(move || {
            replay_early_events(
                &replay_tx,
                vec![
                    SimnetEvent::info("telemetry, droppable"),
                    SimnetEvent::RunbookStarted("lifecycle, must arrive".to_string()),
                ],
                vec![],
            );
        });

        thread::sleep(Duration::from_millis(300));
        assert!(
            replayer.is_finished(),
            "replay must not block the receiver-owning thread on a full buffer"
        );

        // The consumer drains, as log_events/start_app do. The four producer
        // events and the lifecycle event arrive; the telemetry line found
        // the buffer full and dropped.
        let received: Vec<SimnetEvent> = (0..5)
            .map(|_| {
                rx.recv_timeout(Duration::from_secs(5))
                    .expect("every surviving event arrives once the consumer drains")
            })
            .collect();
        assert!(
            matches!(&received[4], SimnetEvent::RunbookStarted(msg) if msg == "lifecycle, must arrive")
        );
        assert!(rx.try_recv().is_err(), "nothing else was queued");
    }
}
