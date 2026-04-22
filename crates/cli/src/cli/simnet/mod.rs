use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use actix_web::dev::ServerHandle;
use crossbeam::channel::{Select, Sender};
use indicatif::{MultiProgress, ProgressBar};
use log::{debug, error, info, warn};
use notify::{
    Config, Event, EventKind, RecursiveMode, Result as NotifyResult, Watcher,
    event::{CreateKind, DataChange, ModifyKind},
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_signer::Signer;
use surfpool_core::{start_local_surfnet, surfnet::svm::SurfnetSvm};
use surfpool_types::{SanitizedConfig, SimnetCommand, SimnetEvent, SubgraphEvent};
use txtx_core::{
    kit::{
        channel::Receiver, futures::future::join_all, helpers::fs::FileLocation,
        types::frontend::BlockEvent,
    },
    manifest::WorkspaceManifest,
    types::RunbookSources,
};
use txtx_gql::kit::{indexmap::IndexMap, types::frontend::LogLevel, uuid::Uuid};

use super::{Context, ExecuteRunbook, StartSimnet};
use crate::{
    http::start_studio_and_scenario_server,
    runbook::{execute_in_memory_runbook, execute_on_disk_runbook, handle_log_event},
    scaffold::{
        ProgramFrameworkData, detect_program_frameworks, scaffold_iac_layout,
        scaffold_in_memory_iac,
    },
    tui::{self, simnet::DisplayedUrl},
};

#[derive(Debug, Serialize, Deserialize)]
struct CheckVersionResponse {
    pub latest: String,
    pub deprecation_notice: Option<String>,
}

pub async fn handle_start_local_surfnet_command(
    cmd: StartSimnet,
    ctx: &Context,
) -> Result<(), String> {
    if !cmd.plugin_config_path.is_empty() && !cfg!(feature = "geyser_plugin") {
        return Err(
            "Recompile surfpool and enable the feature 'geyser_plugin' to load geyser plugins"
                .to_string(),
        );
    }

    // We start the simnet as soon as possible, as it needs to be ready for deployments
    let (surfnet_svm, simnet_events_rx, geyser_events_rx) =
        SurfnetSvm::new_with_db(cmd.db.as_deref(), cmd.svm_config())
            .map_err(|e| format!("Failed to initialize Surfnet SVM: {}", e))?;
    #[cfg(feature = "prometheus")]
    {
        if cmd.metrics_enabled {
            match surfpool_core::telemetry::init_from_config(cmd.metrics_enabled, &cmd.metrics_addr)
            {
                Err(e) => {
                    let _ = surfnet_svm
                        .simnet_events_tx
                        .send(SimnetEvent::warn(format!("Metrics init failed: {}", e)));
                }
                Ok(_) => {
                    let _ = surfnet_svm.simnet_events_tx.send(SimnetEvent::info(format!(
                        "Metrics available at http://{}/metrics",
                        cmd.metrics_addr
                    )));
                }
            }
        }
    }
    let (simnet_commands_tx, simnet_commands_rx) = crossbeam::channel::unbounded();
    let (subgraph_events_tx, subgraph_events_rx) = crossbeam::channel::unbounded();
    let simnet_events_tx = surfnet_svm.simnet_events_tx.clone();

    // Check aidrop addresses
    let (mut airdrop_addresses, airdrop_events) = cmd.get_airdrop_addresses();

    let breaker = if cmd.no_tui {
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

        for snapshot_path in &cmd.snapshot {
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

    // Allow overriding public-facing URLs via environment variables
    // This is useful when running behind a reverse proxy (e.g., Caddy, nginx)
    let rpc_url = std::env::var("SURFPOOL_PUBLIC_RPC_URL")
        .unwrap_or_else(|_| format!("http://{}", config.rpc.get_rpc_base_url()));
    let ws_url = std::env::var("SURFPOOL_PUBLIC_WS_URL")
        .unwrap_or_else(|_| format!("ws://{}", config.rpc.get_ws_base_url()));
    let studio_url = std::env::var("SURFPOOL_PUBLIC_STUDIO_URL")
        .unwrap_or_else(|_| format!("http://{}", studio_binding_address));

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
        !cmd.no_studio,
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
    let mut deploy_progress_rx = vec![];
    if !cmd.no_deploy {
        match write_and_execute_iac(&cmd, &simnet_events_tx, &simnet_commands_tx_copy).await {
            Ok(rx) => deploy_progress_rx.push(rx),
            Err(e) => {
                let _ = simnet_events_tx.send(SimnetEvent::warn(format!(
                    "Automatic protocol deployment failed: {e}"
                )));
            }
        }
    };

    // Non blocking check for new versions
    #[cfg(feature = "version_check")]
    {
        let mut local_version = env!("CARGO_PKG_VERSION").to_string();
        if cmd.ci {
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

    let _ = start_service(
        cmd_cc,
        simnet_events_rx,
        subgraph_events_rx,
        deploy_progress_rx,
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

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn start_service(
    cmd: StartSimnet,
    simnet_events_rx: Receiver<SimnetEvent>,
    subgraph_events_rx: Receiver<SubgraphEvent>,
    deploy_progress_rx: Vec<Receiver<BlockEvent>>,
    simnet_commands_tx: Sender<SimnetCommand>,
    breaker: Option<Keypair>,
    sanitized_config: SanitizedConfig,
    explorer_handle: Option<ServerHandle>,
    _ctx: Context,
    runloop_terminator: Option<Arc<AtomicBool>>,
    initial_transactions: u64,
) -> Result<(), String> {
    let displayed_url = if cmd.no_studio {
        DisplayedUrl::Datasource(sanitized_config)
    } else {
        DisplayedUrl::Studio(sanitized_config)
    };
    let include_debug_logs = cmd.log_level.to_lowercase().eq("debug");

    // Start frontend - kept on main thread
    if cmd.daemon || cmd.no_tui {
        log_events(
            simnet_events_rx,
            subgraph_events_rx,
            include_debug_logs,
            deploy_progress_rx,
            simnet_commands_tx,
            runloop_terminator.unwrap(),
        )?;
    } else {
        tui::simnet::start_app(
            simnet_events_rx,
            simnet_commands_tx,
            include_debug_logs,
            deploy_progress_rx,
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
    deploy_progress_rx: Vec<Receiver<BlockEvent>>,
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
            for rx in deploy_progress_rx.iter() {
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
                    SimnetEvent::BlockHashExpired => {}
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
            i => match oper.recv(&deploy_progress_rx[i - 2]) {
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

/// How a simnet session decides to execute its deployment runbooks.
///
/// Classifies the three runtime inputs that steer execution
/// (`cmd.artifacts_path.is_some()`, `cmd.anchor_compat`, whether a `txtx.yml`
/// exists at the simnet's base location) into a single variant. Downstream
/// callers `match` on the variant instead of recomputing compound boolean
/// predicates at each decision site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunbookExecutionMode {
    /// A `txtx.yml` already exists on disk: execute it as-is.
    ExistingOnDisk,
    /// No `txtx.yml`: scaffold one on disk from the detected framework and
    /// execute it. Requires framework detection to take effect.
    ScaffoldOnDisk,
    /// Execute an in-memory runbook. Triggered by `--artifacts-path` (custom
    /// `bin_path` injection) or `--anchor-compat` without an existing
    /// `txtx.yml`. Requires framework detection.
    InMemory,
}

impl RunbookExecutionMode {
    fn from_inputs(
        has_custom_artifacts_path: bool,
        anchor_compat: bool,
        txtx_exists: bool,
    ) -> Self {
        match (has_custom_artifacts_path, anchor_compat, txtx_exists) {
            // `--artifacts-path` always wins: the custom bin_path has to be
            // injected, which only the in-memory runbook can do.
            (true, _, _) => Self::InMemory,
            // An on-disk `txtx.yml` is authoritative regardless of `--anchor-compat`.
            (false, _, true) => Self::ExistingOnDisk,
            (false, true, false) => Self::InMemory,
            (false, false, false) => Self::ScaffoldOnDisk,
        }
    }
}

async fn write_and_execute_iac(
    cmd: &StartSimnet,
    simnet_events_tx: &Sender<SimnetEvent>,
    simnet_commands_tx: &Sender<SimnetCommand>,
) -> Result<Receiver<BlockEvent>, String> {
    let (progress_tx, progress_rx) = crossbeam::channel::unbounded();

    let base_location =
        FileLocation::from_path_string(&cmd.manifest_path)?.get_parent_location()?;
    let mut txtx_manifest_location = base_location.clone();
    txtx_manifest_location.append_path("txtx.yml")?;
    let txtx_manifest_exists = txtx_manifest_location.exists();

    let mut on_disk_runbook_data = None;
    let mut in_memory_runbook_data = None;
    let runbook_input = cmd.runbook_input.clone();

    let mode = RunbookExecutionMode::from_inputs(
        cmd.artifacts_path.is_some(),
        cmd.anchor_compat,
        txtx_manifest_exists,
    );

    if mode == RunbookExecutionMode::ExistingOnDisk {
        on_disk_runbook_data = Some((txtx_manifest_location.clone(), cmd.runbooks.clone()));
    }

    // Are we in a project directory?
    if let Ok(deployment) = detect_program_frameworks(
        &cmd.manifest_path,
        &cmd.anchor_test_config_paths,
        cmd.artifacts_path.as_deref(),
    )
    .await
    {
        if let Some(ProgramFrameworkData {
            framework,
            programs,
            genesis_accounts,
            accounts,
            accounts_dir,
            clones,
        }) = deployment
        {
            if let Some(clones) = clones.as_ref() {
                if !clones.is_empty() {
                    let _ = simnet_commands_tx.try_send(SimnetCommand::FetchRemoteAccounts(
                        clones
                            .iter()
                            .map(|c| {
                                c.parse().map_err(|e| {
                                    format!("Failed to parse clone address {}: {}", c, e)
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                        cmd.datasource_rpc_url(),
                    ));
                }
            }

            match mode {
                RunbookExecutionMode::ScaffoldOnDisk => {
                    scaffold_iac_layout(
                        &framework,
                        &programs,
                        &base_location,
                        cmd.skip_runbook_generation_prompts,
                    )?;
                    on_disk_runbook_data =
                        Some((txtx_manifest_location.clone(), cmd.runbooks.clone()));
                }
                RunbookExecutionMode::InMemory => {
                    in_memory_runbook_data = Some(scaffold_in_memory_iac(
                        &framework,
                        &programs,
                        &genesis_accounts,
                        &accounts,
                        &accounts_dir,
                        cmd.artifacts_path.as_deref(),
                    )?);
                }
                RunbookExecutionMode::ExistingOnDisk => {}
            }
        }

        let futures = assemble_runbook_execution_futures(
            &progress_tx,
            simnet_events_tx,
            &on_disk_runbook_data,
            &in_memory_runbook_data,
            &runbook_input,
        );

        let simnet_events_tx = simnet_events_tx.clone();
        let _handle = hiro_system_kit::thread_named("Deployment Runbook Executions")
            .spawn(move || {
                let _ = hiro_system_kit::nestable_block_on(join_all(futures.into_iter()));
                Ok::<(), String>(())
            })
            .map_err(|e| format!("Thread to execute runbooks exited: {}", e))?;

        if cmd.watch {
            let artifacts_path_for_watch = cmd.artifacts_path.clone();
            let _handle = hiro_system_kit::thread_named("Watch Filesystem")
                .spawn(move || {
                    let mut target_path = base_location.clone();
                    if let Some(ref path) = artifacts_path_for_watch {
                        let _ = target_path.append_path(path);
                    } else {
                        let _ = target_path.append_path("target");
                        let _ = target_path.append_path("deploy");
                    }
                    let (tx, rx) = mpsc::channel::<NotifyResult<Event>>();
                    let mut watcher = notify::recommended_watcher(tx).map_err(|e| e.to_string())?;
                    watcher
                        .watch(
                            Path::new(&target_path.to_string()),
                            RecursiveMode::NonRecursive,
                        )
                        .map_err(|e| e.to_string())?;
                    let _ = watcher.configure(
                        Config::default()
                            .with_poll_interval(Duration::from_secs(1))
                            .with_compare_contents(true),
                    );
                    for res in rx {
                        // Disregard any event that would not create or modify a .so file
                        let mut found_candidates = false;
                        match res {
                            Ok(Event {
                                kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                                paths,
                                attrs: _,
                            })
                            | Ok(Event {
                                kind: EventKind::Create(CreateKind::File),
                                paths,
                                attrs: _,
                            })
                            // Linux: inotify reports Data(Any) instead of Data(Content)
                            | Ok(Event {
                                kind: EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                                paths,
                                attrs: _,
                            })
                            // Linux: atomic file replacement via rename
                            | Ok(Event {
                                kind: EventKind::Modify(ModifyKind::Name(_)),
                                paths,
                                attrs: _,
                            }) => {
                                for path in paths.iter() {
                                    if path.to_string_lossy().ends_with(".so") {
                                        found_candidates = true;
                                    }
                                }
                            }
                            _ => continue,
                        }

                        if !found_candidates {
                            continue;
                        }

                        let futures = assemble_runbook_execution_futures(
                            &progress_tx,
                            &simnet_events_tx,
                            &on_disk_runbook_data,
                            &in_memory_runbook_data,
                            &runbook_input,
                        );

                        // Catch panics to keep the watch thread alive
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            hiro_system_kit::nestable_block_on(join_all(futures))
                        }));
                    }
                    Ok::<(), String>(())
                })
                .map_err(|e| format!("Thread to watch filesystem exited: {}", e))?;
        }
    }
    Ok(progress_rx)
}

fn assemble_runbook_execution_futures(
    progress_tx: &Sender<BlockEvent>,
    simnet_events_tx: &Sender<SimnetEvent>,
    on_disk_runbook_data: &Option<(FileLocation, Vec<String>)>,
    in_memory_runbook_data: &Option<(String, RunbookSources, WorkspaceManifest)>,
    runbook_input: &Vec<String>,
) -> Vec<std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>> {
    let mut futures: Vec<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>,
    > = vec![];
    let simnet_events_tx_copy = simnet_events_tx.clone();
    let do_setup_logger = false;
    if let Some((runbook_id, runbook_sources, manifest)) = in_memory_runbook_data {
        // Clone owned values so all arguments are 'static
        let runbook_id_owned = runbook_id.clone();
        let runbook_sources_owned = runbook_sources.clone();
        let manifest_owned = manifest.clone();
        futures.push(Box::pin(execute_in_memory_runbook(
            progress_tx.clone(),
            simnet_events_tx_copy.clone(),
            ExecuteRunbook::default_localnet(&runbook_id_owned),
            do_setup_logger,
            runbook_id_owned,
            manifest_owned,
            runbook_sources_owned,
        )));
    }

    if let Some((file_location, runbooks_ids_to_execute)) = on_disk_runbook_data {
        let file_location_owned = file_location.clone();
        let runbooks_ids_to_execute_owned = runbooks_ids_to_execute.clone();
        let simnet_events_tx_copy = simnet_events_tx.clone();
        for runbook_id in runbooks_ids_to_execute_owned.iter() {
            let runbook_id_owned = runbook_id.clone();
            futures.push(Box::pin(execute_on_disk_runbook(
                progress_tx.clone(),
                simnet_events_tx_copy.clone(),
                {
                    let mut ec = ExecuteRunbook::default_localnet(&runbook_id_owned)
                        .with_manifest_path(file_location_owned.to_string());
                    ec.inputs = runbook_input.clone();
                    ec
                },
                do_setup_logger,
            )));
        }
    }
    futures
}
