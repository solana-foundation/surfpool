use std::{collections::BTreeMap, sync::Arc, time::Duration};

use crossbeam::channel;
use dialoguer::{Confirm, console::Style, theme::ColorfulTheme};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, error, info, trace, warn};
use surfpool_types::{SimnetEvent, SimnetEventsTx};
use tokio::{sync::RwLock, task::JoinHandle};
use txtx_addon_network_svm::SvmNetworkAddon;
use txtx_core::{
    kit::{
        Addon,
        channel::Sender,
        helpers::fs::FileLocation,
        types::{AuthorizationContext, diagnostics::Diagnostic, frontend::BlockEvent},
    },
    manifest::{
        RunbookStateLocation, WorkspaceManifest,
        file::{read_runbook_from_location, read_runbooks_from_manifest},
    },
    runbook::{ConsolidatedChanges, RunbookTopLevelInputsMap, SynthesizedChange},
    start_supervised_runbook_runloop, start_unsupervised_runbook_runloop,
    std::StdAddon,
    types::{Runbook, RunbookSnapshotContext, RunbookSources},
    utils::try_write_outputs_to_file,
};
use txtx_gql::kit::{
    indexmap::IndexMap,
    types::{
        RunbookId,
        cloud_interface::CloudServiceContext,
        frontend::{LogDetails, LogEvent, LogLevel, TransientLogEventStatus},
        types::{AddonJsonConverter, Value},
    },
    uuid::Uuid,
};

use crate::cli::{ExecuteRunbook, setup_logger};

lazy_static::lazy_static! {
    static ref CLI_SPINNER_STYLE: ProgressStyle = {
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠸", "⠴", "⠦", "⠇"])
    };
}

pub fn get_json_converters() -> Vec<AddonJsonConverter<'static>> {
    get_available_addons()
        .into_iter()
        .map(|addon| {
            Box::new(move |value: &Value| addon.to_json(value)) as AddonJsonConverter<'static>
        })
        .collect()
}

pub fn get_available_addons() -> Vec<Box<dyn Addon>> {
    vec![Box::new(StdAddon::new()), Box::new(SvmNetworkAddon::new())]
}

pub fn get_addon_by_namespace(namespace: &str) -> Option<Box<dyn Addon>> {
    get_available_addons()
        .into_iter()
        .find(|addon| namespace.starts_with(&addon.get_namespace().to_string()))
}

pub fn load_workspace_manifest_from_manifest_path(
    manifest_path: &str,
) -> Result<WorkspaceManifest, String> {
    let manifest_location = FileLocation::from_path_string(manifest_path)?;
    WorkspaceManifest::from_location(&manifest_location)
}

pub async fn handle_execute_runbook_command(cmd: ExecuteRunbook) -> Result<(), String> {
    let (simnet_events_tx, simnet_events_rx) = SimnetEventsTx::unbounded();
    let (progress_tx, progress_rx) = channel::unbounded();

    let _ = hiro_system_kit::thread_named("Runbook Execution Event Loop").spawn(move || {
        while let Ok(msg) = simnet_events_rx.recv() {
            match msg {
                SimnetEvent::InfoLog(_, msg) => {
                    println!("{} {}", purple!("→"), msg);
                }
                SimnetEvent::ErrorLog(_, msg) => {
                    println!("{} {}", red!("x"), msg);
                }
                SimnetEvent::WarnLog(_, msg) => {
                    println!("{} {}", yellow!("!"), msg);
                }
                SimnetEvent::DebugLog(_, msg) => {
                    println!("{}", msg);
                }
                _ => {}
            }
        }
    });

    let log_filter: LogLevel = cmd.log_level.as_str().into();

    let _ = hiro_system_kit::thread_named("Runbook Progress Event Loop").spawn(move || {
        let mut active_spinners: IndexMap<Uuid, ProgressBar> = IndexMap::new();
        let mut multi_progress = MultiProgress::new();
        while let Ok(msg) = progress_rx.recv() {
            if let BlockEvent::LogEvent(log) = msg {
                handle_log_event(
                    &mut multi_progress,
                    log,
                    &log_filter,
                    &mut active_spinners,
                    true,
                )
            }
        }
    });

    execute_on_disk_runbook(progress_tx, simnet_events_tx, cmd, true).await?;

    Ok(())
}

pub async fn load_runbook_from_file_path(
    file_path: &str,
) -> Result<(String, Runbook, RunbookSources), String> {
    let location = FileLocation::from_path_string(file_path)?;
    let (runbook_name, runbook, runbook_sources) =
        read_runbook_from_location(&location, &None, &None, None)?;

    // Select first runbook by default
    Ok((runbook_name, runbook, runbook_sources))
}

pub async fn execute_in_memory_runbook(
    progress_tx: Sender<BlockEvent>,
    simnet_events_tx: SimnetEventsTx,
    cmd: ExecuteRunbook,
    do_setup_logger: bool,
    runbook_id: String,
    manifest: WorkspaceManifest,
    runbook_sources: RunbookSources,
) -> Result<(), String> {
    let runbook = Runbook::new(RunbookId::new(None, None, &runbook_id), None);
    execute_runbook(
        progress_tx,
        simnet_events_tx,
        cmd,
        do_setup_logger,
        runbook_id,
        manifest,
        runbook,
        runbook_sources,
        None,
    )
    .await
}

pub async fn execute_on_disk_runbook(
    progress_tx: Sender<BlockEvent>,
    simnet_events_tx: SimnetEventsTx,
    cmd: ExecuteRunbook,
    do_setup_logger: bool,
) -> Result<(), String> {
    let manifest = load_workspace_manifest_from_manifest_path(&cmd.manifest_path)?;
    let runbook_id = cmd.runbook.to_string();
    let runbook_selector = vec![runbook_id.clone()];
    let mut runbooks =
        read_runbooks_from_manifest(&manifest, &cmd.environment, Some(&runbook_selector))?;
    let (runbook, runbook_sources, _, runbook_state_location) =
        match runbooks.swap_remove(&runbook_id) {
            Some(res) => res,
            None => {
                let (runbook_name, runbook, sources) =
                    load_runbook_from_file_path(&cmd.runbook).await?;
                (runbook, sources, runbook_name, None)
            }
        };
    execute_runbook(
        progress_tx,
        simnet_events_tx,
        cmd,
        do_setup_logger,
        runbook_id,
        manifest,
        runbook,
        runbook_sources,
        runbook_state_location,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_runbook(
    progress_tx: Sender<BlockEvent>,
    simnet_events_tx: SimnetEventsTx,
    cmd: ExecuteRunbook,
    do_setup_logger: bool,
    runbook_id: String,
    manifest: WorkspaceManifest,
    runbook: Runbook,
    runbook_sources: RunbookSources,
    runbook_state_location: Option<RunbookStateLocation>,
) -> Result<(), String> {
    simnet_events_tx.runbook_started(runbook_id.clone());
    let result = execute_runbook_inner(
        progress_tx,
        simnet_events_tx.clone(),
        cmd,
        do_setup_logger,
        runbook_id.clone(),
        manifest,
        runbook,
        runbook_sources,
        runbook_state_location,
    )
    .await;
    let errors = result.as_ref().err().cloned();
    simnet_events_tx.runbook_completed(runbook_id, errors);
    result.map_err(|errors| errors.join("\n"))
}

#[allow(clippy::too_many_arguments)]
async fn execute_runbook_inner(
    progress_tx: Sender<BlockEvent>,
    simnet_events_tx: SimnetEventsTx,
    cmd: ExecuteRunbook,
    do_setup_logger: bool,
    runbook_id: String,
    manifest: WorkspaceManifest,
    mut runbook: Runbook,
    runbook_sources: RunbookSources,
    runbook_state_location: Option<RunbookStateLocation>,
) -> Result<(), Vec<String>> {
    let top_level_inputs_map = manifest
        .get_runbook_inputs(&cmd.environment, &cmd.inputs, None)
        .unwrap_or_else(|_| RunbookTopLevelInputsMap::new());

    let authorization_context = manifest
        .location
        .map(AuthorizationContext::new)
        .unwrap_or_else(AuthorizationContext::empty);

    if do_setup_logger {
        setup_logger(
            &cmd.log_dir,
            Some(&top_level_inputs_map.current_top_level_input_name()),
            &runbook_id,
            &cmd.log_level,
            false,
        )
        .map_err(|error| vec![error])?;
    }

    let cloud_svc_context = CloudServiceContext::new();

    let res = runbook
        .build_contexts_from_sources(
            runbook_sources,
            top_level_inputs_map,
            authorization_context,
            get_addon_by_namespace,
            cloud_svc_context,
        )
        .await;
    handle_context_build_result(res, &simnet_events_tx)?;

    runbook.enable_full_execution_mode();

    let previous_state_opt = if let Some(state_file_location) = runbook_state_location.clone() {
        match state_file_location.load_execution_snapshot(
            true,
            &runbook.runbook_id.name,
            &runbook.top_level_inputs_map.current_top_level_input_name(),
        ) {
            Ok(snapshot) => Some(snapshot),
            Err(e) => {
                simnet_events_tx.warn(format!("{:?}", e));
                None
            }
        }
    } else {
        None
    };

    if let Some(old) = previous_state_opt {
        let ctx = RunbookSnapshotContext::new();

        let execution_context_backups = runbook.backup_execution_contexts();
        let new = runbook
            .simulate_and_snapshot_flows(&old)
            .await
            .map_err(|error| vec![error.to_string()])?;

        for flow_context in runbook.flow_contexts.iter() {
            if old.flows.get(&flow_context.name).is_none() {
                simnet_events_tx.info(format!(
                    "Previous snapshot not found for flow {}",
                    flow_context.name
                ));
            };
        }

        let consolidated_changes =
            handle_snapshot_diff_result(ctx.diff(old, new), &simnet_events_tx)?;

        let Some(consolidated_changes) = display_snapshot_diffing(consolidated_changes) else {
            return process_runbook_execution_output(
                Ok(()),
                &mut runbook,
                runbook_state_location,
                &simnet_events_tx,
                cmd.output_json,
            );
        };

        runbook.prepare_flows_for_new_plans(
            &consolidated_changes.new_plans_to_add,
            execution_context_backups,
        );

        let (actions_to_re_execute, actions_to_execute) =
            runbook.prepared_flows_for_updated_plans(&consolidated_changes.plans_to_update);

        let has_actions = actions_to_re_execute
            .iter()
            .filter(|(_, actions)| !actions.is_empty())
            .count();
        if has_actions > 0 {
            log_actions_to_execute(&actions_to_re_execute, false, &simnet_events_tx);
        }

        let has_actions = actions_to_execute
            .iter()
            .filter(|(_, actions)| !actions.is_empty())
            .count();
        if has_actions > 0 {
            log_actions_to_execute(&actions_to_execute, true, &simnet_events_tx);
        }

        let theme = ColorfulTheme {
            values_style: Style::new().green(),
            ..ColorfulTheme::default()
        };

        let confirm = Confirm::with_theme(&theme)
            .with_prompt("Do you want to continue?")
            .interact()
            .unwrap();

        if !confirm {
            return Ok(());
        }
    }

    if cmd.unsupervised {
        let res = start_unsupervised_runbook_runloop(&mut runbook, &progress_tx).await;
        process_runbook_execution_output(
            res,
            &mut runbook,
            runbook_state_location,
            &simnet_events_tx,
            cmd.output_json,
        )?;
    } else {
        let (kill_supervised_execution_tx, block_store_handle, execution_result_rx) =
            configure_supervised_execution(runbook, runbook_state_location, &cmd, simnet_events_tx)
                .await
                .map_err(|error| vec![error])?;

        ctrlc::set_handler(move || {
            kill_supervised_execution_tx.send(true).unwrap();
        })
        .expect("Error setting Ctrl-C handler");

        let _ = tokio::join!(block_store_handle);
        execution_result_rx
            .await
            .map_err(|_| vec!["Supervised runbook execution stopped unexpectedly".to_string()])??;
    };

    Ok(())
}

pub async fn configure_supervised_execution(
    mut runbook: Runbook,
    runbook_state_location: Option<RunbookStateLocation>,
    cmd: &ExecuteRunbook,
    simnet_events_tx: SimnetEventsTx,
) -> Result<
    (
        Sender<bool>,
        JoinHandle<()>,
        tokio::sync::oneshot::Receiver<Result<(), Vec<String>>>,
    ),
    String,
> {
    #[cfg(feature = "supervisor_ui")]
    let runbook_name = runbook.runbook_id.name.clone();
    #[cfg(feature = "supervisor_ui")]
    let runbook_description = runbook.description.clone();
    #[cfg(feature = "supervisor_ui")]
    let supervisor_addon_data = {
        let flow = runbook.flow_contexts.first().unwrap();
        let mut addons = vec![];
        for addon in flow.execution_context.addon_instances.values() {
            if let Some(addon_defaults) = flow
                .workspace_context
                .addons_defaults
                .get(&(addon.package_id.did(), addon.addon_id.clone()))
            {
                use txtx_gql::kit::types::frontend::SupervisorAddonData;

                if !addons
                    .iter()
                    .any(|a: &SupervisorAddonData| a.addon_name.eq(&addon.addon_id))
                {
                    addons.push(SupervisorAddonData::new(&addon.addon_id, addon_defaults));
                }
            }
        }
        addons
    };

    let (block_tx, block_rx) = channel::unbounded::<BlockEvent>();
    let (block_broadcaster, _) = tokio::sync::broadcast::channel(5);
    let (log_broadcaster, _) = tokio::sync::broadcast::channel(5);
    let block_store = Arc::new(RwLock::new(BTreeMap::new()));
    let log_store = Arc::new(RwLock::new(vec![]));
    let (kill_loops_tx, kill_loops_rx) = channel::bounded(1);
    #[cfg(feature = "supervisor_ui")]
    let (action_item_events_tx, action_item_events_rx) = tokio::sync::broadcast::channel(32);
    #[cfg(not(feature = "supervisor_ui"))]
    let (_, action_item_events_rx) = tokio::sync::broadcast::channel(32);

    let moved_block_tx = block_tx.clone();
    let moved_kill_loops_tx = kill_loops_tx.clone();
    let moved_runbook_state = runbook_state_location.clone();
    let moved_simnet_events_tx = simnet_events_tx.clone();
    let output_json = cmd.output_json.clone();
    let (execution_result_tx, execution_result_rx) = tokio::sync::oneshot::channel();
    let _ = hiro_system_kit::thread_named("Runbook Runloop").spawn(move || {
        let simnet_events_tx = moved_simnet_events_tx;
        let runloop_future =
            start_supervised_runbook_runloop(&mut runbook, moved_block_tx, action_item_events_rx);

        let result = process_runbook_execution_output(
            hiro_system_kit::nestable_block_on(runloop_future),
            &mut runbook,
            moved_runbook_state,
            &simnet_events_tx,
            output_json,
        );

        let _ = moved_kill_loops_tx.send(true);
        let _ = execution_result_tx.send(result);
    });

    let moved_kill_loops_tx = kill_loops_tx.clone();
    #[cfg(feature = "supervisor_ui")]
    let web_ui_handle = if cmd.do_start_supervisor_ui() {
        use txtx_supervisor_ui::start_supervisor_ui;
        let (supervisor_events_tx, supervisor_events_rx) = channel::unbounded();
        let web_ui_handle = start_supervisor_ui(
            runbook_name,
            runbook_description,
            supervisor_addon_data,
            block_store.clone(),
            log_store.clone(),
            block_broadcaster.clone(),
            log_broadcaster.clone(),
            action_item_events_tx,
            kill_loops_tx.clone(),
            &cmd.network_binding_ip_address,
            cmd.network_binding_port,
            supervisor_events_tx,
        )
        .await
        .map_err(|e| format!("failed to start web console: {}", e))?;

        let moved_simnet_events_tx = simnet_events_tx.clone();
        let _ = hiro_system_kit::thread_named("Supervisor UI Event Runloop").spawn(move || {
            while let Ok(msg) = supervisor_events_rx.recv() {
                match msg {
                    txtx_supervisor_ui::SupervisorEvents::Started(network_binding) => {
                        let _ = moved_simnet_events_tx
                            .info("Starting the supervisor web console".to_string());
                        let _ = moved_simnet_events_tx.info(format!("http://{}", network_binding));
                    }
                }
            }
        });
        Some(web_ui_handle)
    } else {
        None
    };
    #[cfg(not(feature = "supervisor_ui"))]
    if cmd.do_start_supervisor_ui() {
        panic!("Supervisor UI is not enabled in this build");
    }

    let log_filter = cmd.log_level.as_str().into();
    let block_store_handle = tokio::spawn(async move {
        let mut active_spinners: IndexMap<Uuid, ProgressBar> = IndexMap::new();
        let mut multi_progress = MultiProgress::new();
        loop {
            if let Ok(mut block_event) = block_rx.try_recv() {
                let mut block_store = block_store.write().await;
                let mut do_propagate_event = true;
                match block_event.clone() {
                    BlockEvent::Action(new_block) => {
                        let len = block_store.len();
                        block_store.insert(len, new_block.clone());
                    }
                    BlockEvent::Clear => {
                        *block_store = BTreeMap::new();
                    }
                    BlockEvent::UpdateActionItems(updates) => {
                        // for action item updates, track if we actually changed anything before propagating the event
                        do_propagate_event = false;
                        let mut filtered_updates = vec![];
                        for update in updates.iter() {
                            for (_, block) in block_store.iter_mut() {
                                let did_update = block.apply_action_item_updates(update.clone());
                                if did_update {
                                    do_propagate_event = true;
                                    filtered_updates.push(update.clone());
                                }
                            }
                        }
                        block_event = BlockEvent::UpdateActionItems(filtered_updates);
                    }
                    BlockEvent::Modal(new_block) => {
                        let len = block_store.len();
                        block_store.insert(len, new_block.clone());
                    }
                    BlockEvent::RunbookCompleted(_) => {
                        simnet_events_tx.info("Runbook completed");
                    }
                    BlockEvent::Error(new_block) => {
                        let len = block_store.len();
                        block_store.insert(len, new_block.clone());
                    }
                    BlockEvent::LogEvent(log_event) => {
                        handle_log_event(
                            &mut multi_progress,
                            log_event.clone(),
                            &log_filter,
                            &mut active_spinners,
                            true,
                        );
                        let mut log_store = log_store.write().await;
                        log_store.push(log_event.clone());
                        let _ = log_broadcaster.send(log_event);
                    }
                    BlockEvent::Exit => break,
                }

                if do_propagate_event {
                    let _ = block_broadcaster.send(block_event.clone());
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    });

    let _ = hiro_system_kit::thread_named("Kill Runloops Thread")
        .spawn(move || {
            let future = async {
                if kill_loops_rx.recv().is_ok() {
                    let _ = block_tx.send(BlockEvent::Exit);
                    #[cfg(feature = "supervisor_ui")]
                    if let Some(handle) = web_ui_handle {
                        let _ = handle.stop(true).await;
                    }
                };
            };

            hiro_system_kit::nestable_block_on(future)
        })
        .unwrap();

    Ok((moved_kill_loops_tx, block_store_handle, execution_result_rx))
}

pub fn display_snapshot_diffing(
    consolidated_changes: ConsolidatedChanges,
) -> Option<ConsolidatedChanges> {
    let synthesized_changes = consolidated_changes.get_synthesized_changes();

    if synthesized_changes.is_empty() && consolidated_changes.new_plans_to_add.is_empty() {
        println!(
            "{} Latest snapshot in sync with latest runbook updates\n",
            green!("✓")
        );
        return None;
    }

    if !consolidated_changes.new_plans_to_add.is_empty() {
        println!("\n{}", yellow!("New chain to synchronize:"));
        println!("{}\n", consolidated_changes.new_plans_to_add.join(", "));
    }

    let has_critical_changes = synthesized_changes
        .iter()
        .filter(|(c, _)| match c {
            SynthesizedChange::Edition(_, _) => true,
            SynthesizedChange::FormerFailure(_, _) => false,
            SynthesizedChange::Addition(_) => false,
        })
        .count();
    if has_critical_changes > 0 {
        println!("\n{}\n", yellow!("Changes detected:"));
        for (i, (change, _impacted)) in synthesized_changes.iter().enumerate() {
            match change {
                SynthesizedChange::Edition(change, _) => {
                    let formatted_change = change
                        .iter()
                        .map(|c| {
                            if c.starts_with("-") {
                                red!(c)
                            } else {
                                green!(c)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    println!(
                        "{}. The following edits:\n-------------------------\n{}\n-------------------------",
                        i + 1,
                        formatted_change
                    );
                    println!("will introduce breaking changes.\n\n");
                }
                SynthesizedChange::FormerFailure(_construct_to_run, command_name) => {
                    println!(
                        "{}. The action error:\n-------------------------\n{}\n-------------------------",
                        i + 1,
                        command_name
                    );
                    println!("will be re-executed.\n\n");
                }
                SynthesizedChange::Addition(_new_construct_did) => {}
            }
        }
    }

    let unexecuted = synthesized_changes
        .iter()
        .filter(|(c, _)| match c {
            SynthesizedChange::Edition(_, _) => false,
            SynthesizedChange::FormerFailure(_, _) => true,
            SynthesizedChange::Addition(_) => false,
        })
        .count();
    if unexecuted > 0 {
        println!("\n{}", yellow!("Runbook Recovery Plan"));
        println!(
            "The previous runbook execution was interrupted before completion, causing the following actions to be aborted:"
        );

        for (change, _impacted) in synthesized_changes.iter() {
            match change {
                SynthesizedChange::Edition(_, _) => {}
                SynthesizedChange::FormerFailure(_construct_to_run, command_name) => {
                    println!("- {}", command_name);
                }
                SynthesizedChange::Addition(_new_construct_did) => {}
            }
        }
        println!("These actions will be re-executed in the next run.\n");
    }

    Some(consolidated_changes)
}

fn log_diagnostic_lines(diags: Vec<Diagnostic>, simnet_events_tx: &SimnetEventsTx) {
    for diag in diags.iter() {
        let diag_str = diag.to_string();
        for line in diag_str.lines() {
            simnet_events_tx.warn(line);
        }
    }
}

fn handle_context_build_result(
    result: Result<bool, Vec<Diagnostic>>,
    simnet_events_tx: &SimnetEventsTx,
) -> Result<(), Vec<String>> {
    match result {
        Ok(_) => Ok(()),
        Err(diags) => {
            let errors = diags.iter().map(|diag| diag.message.clone()).collect();
            log_diagnostic_lines(diags, simnet_events_tx);
            Err(errors)
        }
    }
}

fn handle_snapshot_diff_result<T, E: std::fmt::Display>(
    result: Result<T, E>,
    simnet_events_tx: &SimnetEventsTx,
) -> Result<T, Vec<String>> {
    match result {
        Ok(changes) => Ok(changes),
        Err(error) => {
            let _ = simnet_events_tx.warn("Failed to process runbook snapshot");
            simnet_events_tx.warn(error.to_string());
            Err(vec![
                "Failed to process runbook snapshot".to_string(),
                error.to_string(),
            ])
        }
    }
}

fn log_actions_to_execute(
    actions_to_execute: &IndexMap<String, Vec<(String, Option<String>)>>,
    is_first_time: bool,
    simnet_events_tx: &SimnetEventsTx,
) {
    let msg = if is_first_time {
        "The following actions have been added and will be executed for the first time:"
    } else {
        "The following actions will be re-executed:"
    };
    simnet_events_tx.info(msg);
    let documentation_missing = black!("<description field empty>");
    for (context, actions) in actions_to_execute.iter() {
        simnet_events_tx.info(context.to_string());
        for (action_name, documentation) in actions.iter() {
            simnet_events_tx.info(format!(
                "- {}: {}",
                action_name,
                documentation.as_ref().unwrap_or(&documentation_missing)
            ));
        }
    }
}

fn write_runbook_transient_state(
    runbook: &mut Runbook,
    runbook_state_location: Option<RunbookStateLocation>,
    simnet_events_tx: &SimnetEventsTx,
) {
    match runbook.mark_failed_and_write_transient_state(runbook_state_location) {
        Ok(Some(location)) => {
            simnet_events_tx.warn(format!("! Saving transient state to {}", location));
        }
        Ok(None) => {}
        Err(e) => {
            simnet_events_tx.warn(format!("x Failed to write transient runbook state: {}", e));
        }
    };
}

fn write_runbook_state(
    runbook: &mut Runbook,
    runbook_state_location: Option<RunbookStateLocation>,
    simnet_events_tx: &SimnetEventsTx,
) {
    match runbook.write_runbook_state(runbook_state_location) {
        Ok(Some(location)) => {
            simnet_events_tx.info(format!("Saved execution state to {}", location));
        }
        Ok(None) => {}
        Err(e) => {
            simnet_events_tx.warn(format!("Failed to write runbook state: {}", e));
        }
    };
}

fn process_runbook_execution_output(
    execution_result: Result<(), Vec<Diagnostic>>,
    runbook: &mut Runbook,
    runbook_state_location: Option<RunbookStateLocation>,
    simnet_events_tx: &SimnetEventsTx,
    output_json: Option<Option<String>>,
) -> Result<(), Vec<String>> {
    if let Err(diags) = execution_result {
        simnet_events_tx.warn("Runbook execution aborted");
        let errors = diags.iter().map(|diag| diag.message.clone()).collect();
        log_diagnostic_lines(diags, simnet_events_tx);
        write_runbook_transient_state(runbook, runbook_state_location, simnet_events_tx);
        return Err(errors);
    } else {
        let mut output_error = None;
        let runbook_outputs = runbook.collect_formatted_outputs();
        if !runbook_outputs.is_empty() {
            if let Some(some_output_loc) = output_json {
                if let Some(output_loc) = some_output_loc {
                    match try_write_outputs_to_file(
                        &output_loc,
                        runbook_outputs,
                        &runbook
                            .runtime_context
                            .authorization_context
                            .workspace_location,
                        &runbook.runbook_id.name,
                        &runbook.top_level_inputs_map.current_top_level_input_name(),
                        &get_json_converters(),
                    ) {
                        Ok(output_location) => {
                            simnet_events_tx
                                .info(format!("Outputs written to {}", output_location));
                        }
                        Err(e) => {
                            let message = format!("Failed to write runbook outputs: {}", e);
                            simnet_events_tx.warn(message.clone());
                            output_error = Some(message);
                        }
                    }
                } else {
                    simnet_events_tx.info(
                        serde_json::to_string_pretty(
                            &runbook_outputs.to_json(&get_json_converters()),
                        )
                        .unwrap(),
                    );
                }
            } else {
                simnet_events_tx.debug(
                    serde_json::to_string_pretty(&runbook_outputs.to_json(&get_json_converters()))
                        .unwrap(),
                );
            }
        }
        write_runbook_state(runbook, runbook_state_location, simnet_events_tx);
        match output_error {
            Some(error) => Err(vec![error]),
            None => Ok(()),
        }
    }
}

pub fn persist_log(
    message: &str,
    summary: &str,
    namespace: &str,
    log_level: &LogLevel,
    log_filter: &LogLevel,
    do_log_to_cli: bool,
) {
    let msg = format!("{} - {}", summary, message);
    match log_level {
        LogLevel::Trace => {
            trace!(target: &namespace, "{}", msg);
            if do_log_to_cli && log_filter.should_log(log_level) {
                println!("→ {}", msg);
            }
        }
        LogLevel::Debug => {
            debug!(target: &namespace, "{}", msg);
            if do_log_to_cli && log_filter.should_log(log_level) {
                println!("→ {}", msg);
            }
        }

        LogLevel::Info => {
            info!(target: &namespace, "{}", msg);
            if do_log_to_cli && log_filter.should_log(log_level) {
                println!("{} {} - {}", purple!("→"), purple!(summary), message);
            }
        }
        LogLevel::Warn => {
            warn!(target: &namespace, "{}", msg);
            if do_log_to_cli && log_filter.should_log(log_level) {
                println!("{} {} - {}", yellow!("!"), yellow!(summary), message);
            }
        }
        LogLevel::Error => {
            error!(target: &namespace, "{}", msg);
            if do_log_to_cli && log_filter.should_log(log_level) {
                println!("{} {} - {}", red!("x"), red!(summary), message);
            }
        }
    }
}

pub fn handle_log_event(
    multi_progress: &mut MultiProgress,
    log: LogEvent,
    log_filter: &LogLevel,
    active_spinners: &mut IndexMap<Uuid, ProgressBar>,
    do_log_to_cli: bool,
) {
    match log {
        LogEvent::Static(static_log_event) => {
            let LogDetails { message, summary } = static_log_event.details;
            persist_log(
                &message,
                &summary,
                &static_log_event.namespace,
                &static_log_event.level,
                log_filter,
                do_log_to_cli,
            );
        }
        LogEvent::Transient(log) => match log.status {
            TransientLogEventStatus::Pending(LogDetails { message, summary }) => {
                if let Some(pb) = active_spinners.get(&log.uuid) {
                    if do_log_to_cli {
                        // update existing spinner
                        pb.set_message(format!("{} {}", yellow!(&summary), &message));
                    }
                } else {
                    // create new spinner
                    let pb = multi_progress.add(ProgressBar::new_spinner());
                    if do_log_to_cli {
                        pb.set_style(CLI_SPINNER_STYLE.clone());
                        pb.enable_steady_tick(Duration::from_millis(80));
                        pb.set_message(format!("{} {}", yellow!(&summary), message));
                    }
                    active_spinners.insert(log.uuid, pb);
                    persist_log(
                        &message,
                        &summary,
                        &log.namespace,
                        &log.level,
                        log_filter,
                        false,
                    );
                }
            }
            TransientLogEventStatus::Success(LogDetails { summary, message }) => {
                let msg = format!("{} {} {}", green!("✓"), green!(&summary), message);
                if let Some(pb) = active_spinners.swap_remove(&log.uuid) {
                    if do_log_to_cli {
                        pb.finish_with_message(msg);
                    }
                } else if do_log_to_cli {
                    println!("{}", msg);
                }

                persist_log(
                    &message,
                    &summary,
                    &log.namespace,
                    &log.level,
                    log_filter,
                    false,
                );
            }
            TransientLogEventStatus::Failure(LogDetails { summary, message }) => {
                let msg = format!("{} {}: {}", red!("x"), red!(&summary), message);
                if let Some(pb) = active_spinners.swap_remove(&log.uuid) {
                    if do_log_to_cli {
                        pb.finish_with_message(msg);
                    }
                } else if do_log_to_cli {
                    println!("{}", msg);
                }
                persist_log(
                    &message,
                    &summary,
                    &log.namespace,
                    &log.level,
                    log_filter,
                    false,
                );
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceManifest, execute_runbook};
    use crate::cli::ExecuteRunbook;
    use crossbeam::channel;
    use surfpool_types::{SimnetEvent, SimnetEventsTx};
    use txtx_core::{
        kit::helpers::fs::FileLocation,
        types::{Runbook, RunbookSources},
    };
    use txtx_gql::kit::types::RunbookId;

    #[tokio::test]
    async fn execute_runbook_reports_hcl_context_failure_as_completed_with_errors() {
        let runbook_id = "broken-runbook";
        let mut sources = RunbookSources::new();
        sources.add_source(
            runbook_id.to_string(),
            FileLocation::working_dir(),
            "this is not valid HCL {".to_string(),
        );
        let (events_tx, events_rx) = SimnetEventsTx::unbounded();
        let (progress_tx, _progress_rx) = channel::unbounded();

        let result = execute_runbook(
            progress_tx,
            events_tx,
            ExecuteRunbook::default_localnet(runbook_id),
            false,
            runbook_id.to_string(),
            WorkspaceManifest::new("memory".to_string()),
            Runbook::new(RunbookId::new(None, None, runbook_id), None),
            sources,
            None,
        )
        .await;

        assert!(result.is_err());
        let events = events_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            SimnetEvent::RunbookCompleted(id, Some(errors))
                if id == runbook_id
                    && !errors.is_empty()
                    && matches!(&result, Err(message) if message == &errors.join("\n"))
        )));
    }
}
