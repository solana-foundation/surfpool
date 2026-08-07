//! Startup planning for a local surfnet: inspect the project, seal the
//! startup plan, dispatch the work, and watch the lifecycle to completion.

use std::{path::Path, sync::mpsc, time::Duration};

use crossbeam::channel::{Sender, bounded};
use notify::{
    Config, Event, EventKind, RecursiveMode, Result as NotifyResult, Watcher,
    event::{CreateKind, DataChange, ModifyKind},
};
use solana_pubkey::Pubkey;
use surfpool_types::{
    SimnetCommand, SimnetEvent, StartupError, SurfnetStartupPhase, SurfnetStartupTask,
};
use txtx_core::{
    kit::{
        channel::Receiver, futures::future::join_all, helpers::fs::FileLocation,
        types::frontend::BlockEvent,
    },
    manifest::WorkspaceManifest,
    types::RunbookSources,
};

use super::{
    super::{ExecuteRunbook, StartSimnet},
    parse_clone_addresses,
};
use crate::{
    runbook::{execute_in_memory_runbook, execute_on_disk_runbook},
    scaffold::{
        ProgramFrameworkData, detect_program_frameworks, scaffold_iac_layout,
        scaffold_in_memory_iac,
    },
};

/// How a simnet session decides to execute its startup runbooks.
///
/// Classifies the three runtime inputs that steer execution
/// (`cmd.project.artifacts_path.is_some()`, `cmd.project.anchor_compat`, whether a `txtx.yml`
/// exists at the simnet's base location) into a single variant. Downstream
/// callers `match` on the variant instead of recomputing compound boolean
/// predicates at each decision site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunbookExecutionMode {
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
    /// Whether the runbook this mode executes is built from what framework
    /// detection finds. When it is not, a detection failure costs at most the
    /// clone addresses, and the runbook on disk is still executable.
    pub(super) fn requires_framework_detection(&self) -> bool {
        match self {
            Self::ExistingOnDisk => false,
            Self::ScaffoldOnDisk | Self::InMemory => true,
        }
    }

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

/// Spawns the startup watchdog when running headless (`--no-tui` or
/// `daemon`). The watchdog sends [`SimnetCommand::Terminate`] if startup
/// fails, killing the surfnet: a headless surfnet whose startup failed would
/// otherwise serve forever to readiness pollers that have no timeout
/// (Anchor's readiness loop reads only `completed_at` and process death).
/// If not headless, no watchdog is spawned; the TUI stays alive and
/// displays the failure.
pub(super) fn spawn_startup_watchdog(
    headless: bool,
    startup_status_rx: tokio::sync::watch::Receiver<surfpool_types::SurfnetStartupStatus>,
    events_tx: Sender<SimnetEvent>,
    commands_tx: Sender<SimnetCommand>,
) -> Result<(), String> {
    if !headless {
        return Ok(());
    }
    hiro_system_kit::thread_named("Startup Watchdog")
        .spawn(move || {
            watch_startup_until_completion(startup_status_rx, events_tx, commands_tx);
            Ok::<(), String>(())
        })
        .map(|_| ())
        .map_err(|e| format!("Thread to watch startup status exited: {e}"))
}

/// Blocks until the startup lifecycle completes, in either direction. On `Failed`,
/// sends `Aborted` first (so the event loop exits with the error before the
/// shutdown reaches it) and then `Terminate` (so the runloop shuts down
/// gracefully, WAL checkpoint included). `Ready`, or a dropped sender because
/// the surfnet is already shutting down, requires nothing.
pub(super) fn watch_startup_until_completion(
    mut startup_status_rx: tokio::sync::watch::Receiver<surfpool_types::SurfnetStartupStatus>,
    events_tx: Sender<SimnetEvent>,
    commands_tx: Sender<SimnetCommand>,
) {
    let terminal = hiro_system_kit::nestable_block_on(startup_status_rx.wait_for(|status| {
        matches!(
            status.phase(),
            SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
        )
    }));
    let failure = match terminal {
        Ok(status) if status.phase() == SurfnetStartupPhase::Failed => {
            Some(status.failure_messages().join("; "))
        }
        _ => None,
    };
    if let Some(error) = failure {
        let _ = events_tx.send(SimnetEvent::Aborted(format!(
            "Surfpool startup failed: {error}"
        )));
        let _ = commands_tx.send(SimnetCommand::Terminate(None));
    }
}

/// Bounded wait for the seal round-trip. Generous rather than tight: the
/// runloop emits `SimnetEvent::Ready` immediately before entering the
/// command loop on the same task, and the CLI waits for Ready before
/// planning, so the loop is provably alive at every call site. A timeout
/// here means the loop is dead or wedged, not merely slow.
const SEAL_STARTUP_PLAN_TIMEOUT: Duration = Duration::from_secs(5);

/// How startup planning failed, split at the seal boundary because the two
/// halves demand different responses from the caller.
pub(super) enum StartupPlanFailure {
    /// Planning failed before the plan was sealed. The startup state
    /// machine (`SurfnetStartupStatus`) is reachable and still in
    /// Planning-unsealed, so the caller can (and must) drive it to Failed;
    /// the watchdog policy then decides fatality.
    Planning(String),
    /// The seal did not take. Which half failed decides what is knowable
    /// afterwards, so the two arrive separately.
    Sealing(SealFailure),
}

/// The two ways sealing fails, which differ in what they leave known.
pub(super) enum SealFailure {
    /// The round trip did not complete: the command loop is dead or wedged
    /// (alive, but unable to make progress because something it needs is
    /// blocked), so the startup state machine's state is unknowable and no
    /// session can become ready.
    Unreachable(String),
    /// The startup state machine answered, and refused. Its state is known,
    /// and the reason says which rule declined.
    Refused(StartupError),
}

impl std::fmt::Display for SealFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(reason) => write!(f, "{reason}"),
            Self::Refused(error) => write!(f, "{error}"),
        }
    }
}

pub(super) fn seal_startup_plan(
    simnet_commands_tx: &Sender<SimnetCommand>,
    tasks: Vec<SurfnetStartupTask>,
) -> Result<(), SealFailure> {
    let (response_tx, response_rx) = bounded(1);
    simnet_commands_tx
        .send(SimnetCommand::SealStartupPlan(tasks, response_tx))
        .map_err(|e| SealFailure::Unreachable(format!("Failed to submit startup plan: {e}")))?;
    response_rx
        .recv_timeout(SEAL_STARTUP_PLAN_TIMEOUT)
        .map_err(|e| SealFailure::Unreachable(format!("Timed out sealing startup plan: {e}")))?
        .map_err(SealFailure::Refused)
}

/// Everything the planner learned about the project before sealing: the task
/// list to seal, the work to dispatch, and the context the artifact watcher
/// needs to re-execute runbooks later.
struct StartupPlan {
    progress_tx: Sender<BlockEvent>,
    progress_rx: Receiver<BlockEvent>,
    futures: RunbookExecutionFutures,
    clone_pubkeys: Vec<Pubkey>,
    startup_tasks: Vec<SurfnetStartupTask>,
    base_location: FileLocation,
    on_disk_runbook_data: Option<(FileLocation, Vec<String>)>,
    in_memory_runbook_data: Option<(String, RunbookSources, WorkspaceManifest)>,
    runbook_input: Vec<String>,
}

/// Inspects the project, scaffolds runbooks as the execution mode requires,
/// and assembles the startup plan. Never touches the startup machine: a
/// failure leaves it in Planning-unsealed for the caller to fail explicitly.
async fn plan_startup(
    cmd: &StartSimnet,
    simnet_events_tx: &Sender<SimnetEvent>,
) -> Result<StartupPlan, String> {
    let (progress_tx, progress_rx) = crossbeam::channel::unbounded();

    let base_location =
        FileLocation::from_path_string(&cmd.project.manifest_path)?.get_parent_location()?;
    let mut txtx_manifest_location = base_location.clone();
    txtx_manifest_location.append_path("txtx.yml")?;
    let txtx_manifest_exists = txtx_manifest_location.exists();

    let mut on_disk_runbook_data = None;
    let mut in_memory_runbook_data = None;
    let mut clone_pubkeys = vec![];
    let runbook_input = cmd.project.runbook_input.clone();

    let mode = RunbookExecutionMode::from_inputs(
        cmd.project.artifacts_path.is_some(),
        cmd.project.anchor_compat,
        txtx_manifest_exists,
    );

    if mode == RunbookExecutionMode::ExistingOnDisk {
        on_disk_runbook_data = Some((txtx_manifest_location.clone(), cmd.project.runbooks.clone()));
    }

    // A detection failure is fatal only when the runbook is built from what it
    // finds. With a `txtx.yml` already on disk the manifest is authoritative,
    // and an unreadable `Anchor.toml` or `Cargo.toml` costs the clone addresses
    // rather than the whole startup, so it is reported and startup continues.
    let deployment = match detect_program_frameworks(
        &cmd.project.manifest_path,
        &cmd.project.anchor_test_config_paths,
        cmd.project.artifacts_path.as_deref(),
    )
    .await
    {
        Ok(deployment) => deployment,
        Err(e) if mode.requires_framework_detection() => {
            return Err(format!("Failed to detect project framework: {e}"));
        }
        Err(e) => {
            let _ = simnet_events_tx.send(SimnetEvent::warn(format!(
                "Could not detect the project framework, continuing with the runbook on disk. \
                 Declared clones, if any, were not loaded: {e}"
            )));
            None
        }
    };

    if let Some(ProgramFrameworkData {
        framework,
        programs,
        genesis_accounts,
        accounts,
        accounts_dir,
        clones,
    }) = deployment
    {
        let (parsed, rejected) = parse_clone_addresses(&clones.unwrap_or_default());
        clone_pubkeys = parsed;
        if !rejected.is_empty() {
            let _ = simnet_events_tx.send(SimnetEvent::warn(format!(
                "Ignoring {} declared clone address(es) that could not be parsed; \
                 the rest were loaded. {}",
                rejected.len(),
                rejected.join("; ")
            )));
        }

        match mode {
            RunbookExecutionMode::ScaffoldOnDisk => {
                scaffold_iac_layout(
                    &framework,
                    &programs,
                    &base_location,
                    cmd.project.skip_runbook_generation_prompts,
                )?;
                on_disk_runbook_data =
                    Some((txtx_manifest_location.clone(), cmd.project.runbooks.clone()));
            }
            RunbookExecutionMode::InMemory => {
                in_memory_runbook_data = Some(scaffold_in_memory_iac(
                    &framework,
                    &programs,
                    &genesis_accounts,
                    &accounts,
                    &accounts_dir,
                    cmd.project.artifacts_path.as_deref(),
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
    let mut startup_tasks = vec![];
    if !clone_pubkeys.is_empty() {
        startup_tasks.push(SurfnetStartupTask::RemoteAccounts);
    }
    if !futures.is_empty() {
        startup_tasks.push(SurfnetStartupTask::RunbookExecutions);
    }

    Ok(StartupPlan {
        progress_tx,
        progress_rx,
        futures,
        clone_pubkeys,
        startup_tasks,
        base_location,
        on_disk_runbook_data,
        in_memory_runbook_data,
        runbook_input,
    })
}

/// Plans the project's startup tasks, seals the plan, and dispatches the
/// work. Formerly `write_and_execute_iac`, but the IaC part (scaffolding and
/// executing txtx runbooks) now lives in [`plan_startup`] and the
/// runbook-execution futures; this function's job is the startup
/// choreography around them.
pub(super) async fn plan_and_dispatch_startup(
    cmd: &StartSimnet,
    simnet_events_tx: &Sender<SimnetEvent>,
    simnet_commands_tx: &Sender<SimnetCommand>,
) -> Result<Receiver<BlockEvent>, StartupPlanFailure> {
    let StartupPlan {
        progress_tx,
        progress_rx,
        futures,
        clone_pubkeys,
        startup_tasks,
        base_location,
        on_disk_runbook_data,
        in_memory_runbook_data,
        runbook_input,
    } = plan_startup(cmd, simnet_events_tx)
        .await
        .map_err(StartupPlanFailure::Planning)?;

    // Sealing happens before any task is submitted. From this point forward,
    // readiness is derived solely from the registered task transitions.
    seal_startup_plan(simnet_commands_tx, startup_tasks).map_err(StartupPlanFailure::Sealing)?;

    // One choreography for all startup tasks: the submitter sends
    // StartStartupTask before dispatching the work, and the worker reports
    // the outcome via CompleteStartupTask. Nothing past the seal fails
    // startup from here: the machine owns failure reporting through task
    // transitions, and a command send can only fail once the command loop
    // is gone, when the session is already coming down.
    if !clone_pubkeys.is_empty() {
        let _ = simnet_commands_tx.send(SimnetCommand::StartStartupTask(
            SurfnetStartupTask::RemoteAccounts,
        ));
        let _ = simnet_commands_tx.send(SimnetCommand::FetchRemoteAccounts(
            clone_pubkeys,
            cmd.datasource_rpc_url(),
        ));
    }

    if !futures.is_empty() {
        let _ = simnet_commands_tx.send(SimnetCommand::StartStartupTask(
            SurfnetStartupTask::RunbookExecutions,
        ));

        let startup_commands_tx = simnet_commands_tx.clone();
        if let Err(error) =
            hiro_system_kit::thread_named("Startup Runbook Executions").spawn(move || {
                // catch_unwind, like the artifact watcher below: a panicking
                // runbook future must still complete the Deployment task, or
                // the phase pins at ExecutingRunbooks and readiness never resolves.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    hiro_system_kit::nestable_block_on(join_all(futures))
                }));
                let result = match outcome {
                    Ok(results) => {
                        let errors = results
                            .into_iter()
                            .filter_map(Result::err)
                            .collect::<Vec<_>>();
                        if errors.is_empty() {
                            Ok(())
                        } else {
                            Err(errors.join("\n"))
                        }
                    }
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| panic.downcast_ref::<&str>().copied())
                            .unwrap_or("startup runbook execution panicked");
                        Err(format!("startup runbook execution panicked: {message}"))
                    }
                };
                let _ = startup_commands_tx.send(SimnetCommand::CompleteStartupTask(
                    SurfnetStartupTask::RunbookExecutions,
                    result,
                ));
                Ok::<(), String>(())
            })
        {
            // The spawn failure is the task's outcome; report it through
            // the machine (Failed) and let the watchdog policy decide
            // fatality. The error event covers the TUI, which has no other
            // channel announcing the failed phase.
            let error = format!("Thread to execute runbooks exited: {error}");
            let _ = simnet_events_tx.send(SimnetEvent::error(error.clone()));
            let _ = simnet_commands_tx.send(SimnetCommand::CompleteStartupTask(
                SurfnetStartupTask::RunbookExecutions,
                Err(error),
            ));
        }
    }

    if cmd.project.watch {
        // The watcher is a dev convenience; the startup tasks are
        // already dispatched and may legitimately reach Ready, so a watcher
        // failure must not fail startup.
        if let Err(error) = spawn_artifact_watcher(
            base_location,
            cmd.project.artifacts_path.clone(),
            progress_tx,
            simnet_events_tx.clone(),
            on_disk_runbook_data,
            in_memory_runbook_data,
            runbook_input,
        ) {
            let _ = simnet_events_tx.send(SimnetEvent::warn(format!(
                "Failed to watch deploy artifacts for changes: {error}"
            )));
        }
    }

    Ok(progress_rx)
}

/// Watches the deploy-artifacts directory and re-executes the startup
/// runbooks whenever a `.so` file is created or modified.
fn spawn_artifact_watcher(
    base_location: FileLocation,
    artifacts_path: Option<String>,
    progress_tx: Sender<BlockEvent>,
    simnet_events_tx: Sender<SimnetEvent>,
    on_disk_runbook_data: Option<(FileLocation, Vec<String>)>,
    in_memory_runbook_data: Option<(String, RunbookSources, WorkspaceManifest)>,
    runbook_input: Vec<String>,
) -> Result<(), String> {
    let _handle = hiro_system_kit::thread_named("Watch Filesystem")
        .spawn(move || {
            let mut target_path = base_location;
            if let Some(ref path) = artifacts_path {
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
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    hiro_system_kit::nestable_block_on(join_all(futures))
                }));
            }
            Ok::<(), String>(())
        })
        .map_err(|e| format!("Thread to watch filesystem exited: {}", e))?;
    Ok(())
}

type RunbookExecutionFutures =
    Vec<std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>>;

fn assemble_runbook_execution_futures(
    progress_tx: &Sender<BlockEvent>,
    simnet_events_tx: &Sender<SimnetEvent>,
    on_disk_runbook_data: &Option<(FileLocation, Vec<String>)>,
    in_memory_runbook_data: &Option<(String, RunbookSources, WorkspaceManifest)>,
    runbook_input: &[String],
) -> RunbookExecutionFutures {
    let mut futures: RunbookExecutionFutures = vec![];
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
                    ec.inputs = runbook_input.to_vec();
                    ec
                },
                do_setup_logger,
            )));
        }
    }
    futures
}

#[cfg(test)]
mod tests {
    use super::RunbookExecutionMode;

    /// A project that already has a `txtx.yml` executes it as written. Framework
    /// detection contributes clone addresses on that path, but nothing the
    /// runbook needs, so a project whose `Anchor.toml` or `Cargo.toml` cannot be
    /// read still starts. Treating that read as fatal would strand a working
    /// project on a file it does not depend on.
    #[test]
    fn an_existing_runbook_does_not_depend_on_framework_detection() {
        assert!(
            !RunbookExecutionMode::ExistingOnDisk.requires_framework_detection(),
            "an on-disk runbook is authoritative; detection only adds clones"
        );
    }

    /// One malformed address should not cost a project the rest of its clones,
    /// nor its startup. No runbook is built from the clone list, so a typo in
    /// `test.validator.clone` is reported and the valid addresses still load.
    #[test]
    fn a_malformed_clone_address_does_not_discard_the_valid_ones() {
        let (parsed, rejected) = super::parse_clone_addresses(&[
            "AqH29mZfQFgRpfwaPoTMWSKJ5kqauoc1FwVBRksZyQrt".to_string(),
            "not-a-pubkey".to_string(),
            "SysvarC1ock11111111111111111111111111111111".to_string(),
        ]);

        assert_eq!(parsed.len(), 2, "both valid addresses should survive");
        assert_eq!(rejected.len(), 1, "the malformed one should be reported");
        assert!(
            rejected[0].contains("not-a-pubkey"),
            "the report should name the address a user has to fix: {}",
            rejected[0]
        );
    }

    /// The other two modes build the runbook out of what detection finds, so a
    /// failure there leaves nothing to execute and must surface.
    #[test]
    fn scaffolded_and_in_memory_runbooks_require_framework_detection() {
        for mode in [
            RunbookExecutionMode::ScaffoldOnDisk,
            RunbookExecutionMode::InMemory,
        ] {
            assert!(
                mode.requires_framework_detection(),
                "{mode:?} has no runbook without detection"
            );
        }
    }

    mod startup_watchdog {
        use std::time::{Duration, Instant};

        use crossbeam::channel::{Receiver, unbounded};
        use surfpool_types::{
            SimnetCommand, SimnetEvent, SurfnetStartupStatus, SurfnetStartupTask,
        };
        use tokio::sync::watch;

        use super::super::{spawn_startup_watchdog, watch_startup_until_completion};

        const TIMEOUT: Duration = Duration::from_secs(5);

        fn failed_status(error: &str) -> SurfnetStartupStatus {
            let mut status = SurfnetStartupStatus::default();
            status
                .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
                .unwrap();
            status
                .start_task(SurfnetStartupTask::RemoteAccounts)
                .unwrap();
            status
                .fail_task(SurfnetStartupTask::RemoteAccounts, error)
                .unwrap();
            status
        }

        fn ready_status() -> SurfnetStartupStatus {
            let mut status = SurfnetStartupStatus::default();
            status.seal_plan(vec![]).unwrap();
            status
        }

        fn assert_channels_stay_empty(
            events_rx: &Receiver<SimnetEvent>,
            commands_rx: &Receiver<SimnetCommand>,
        ) {
            assert!(events_rx.try_recv().is_err(), "unexpected simnet event");
            assert!(commands_rx.try_recv().is_err(), "unexpected simnet command");
        }

        #[test]
        fn headless_watchdog_aborts_and_terminates_on_failed_startup() {
            let (status_tx, status_rx) = watch::channel(SurfnetStartupStatus::default());
            let (events_tx, events_rx) = unbounded();
            let (commands_tx, commands_rx) = unbounded();

            let watchdog = std::thread::spawn(move || {
                watch_startup_until_completion(status_rx, events_tx, commands_tx)
            });
            status_tx.send_replace(failed_status("datasource unavailable"));

            match events_rx.recv_timeout(TIMEOUT) {
                Ok(SimnetEvent::Aborted(error)) => {
                    assert!(
                        error.contains("datasource unavailable"),
                        "abort lost the failure reason: {error}"
                    );
                }
                other => panic!("expected Aborted, got {other:?}"),
            }
            assert!(matches!(
                commands_rx.recv_timeout(TIMEOUT),
                Ok(SimnetCommand::Terminate(_))
            ));
            watchdog.join().unwrap();
        }

        #[test]
        fn headless_watchdog_stays_quiet_when_startup_reaches_ready() {
            let (status_tx, status_rx) = watch::channel(SurfnetStartupStatus::default());
            let (events_tx, events_rx) = unbounded();
            let (commands_tx, commands_rx) = unbounded();

            let watchdog = std::thread::spawn(move || {
                watch_startup_until_completion(status_rx, events_tx, commands_tx)
            });
            status_tx.send_replace(ready_status());

            // The join proves the watchdog saw the terminal phase, so an
            // empty channel afterwards is not a timing artifact.
            watchdog.join().unwrap();
            assert_channels_stay_empty(&events_rx, &commands_rx);
        }

        #[test]
        fn headless_watchdog_stays_quiet_when_the_surfnet_shuts_down_first() {
            let (status_tx, status_rx) = watch::channel(SurfnetStartupStatus::default());
            let (events_tx, events_rx) = unbounded();
            let (commands_tx, commands_rx) = unbounded();

            let watchdog = std::thread::spawn(move || {
                watch_startup_until_completion(status_rx, events_tx, commands_tx)
            });
            // The watchdog sees shutdown as a dropped sender, and must not
            // report it as a failure.
            drop(status_tx);

            watchdog.join().unwrap();
            assert_channels_stay_empty(&events_rx, &commands_rx);
        }

        #[test]
        fn tui_mode_spawns_no_watchdog() {
            let (status_tx, status_rx) = watch::channel(SurfnetStartupStatus::default());
            let (events_tx, events_rx) = unbounded();
            let (commands_tx, commands_rx) = unbounded();

            spawn_startup_watchdog(false, status_rx, events_tx, commands_tx).unwrap();

            // The receiver was dropped rather than parked in a thread, so
            // nobody is watching: a later failure cannot trigger an abort.
            // No sleep needed; with zero receivers there is no race to lose.
            assert_eq!(status_tx.receiver_count(), 0);
            status_tx.send_replace(failed_status("boom"));
            assert_channels_stay_empty(&events_rx, &commands_rx);
        }

        #[test]
        fn headless_mode_spawns_a_watchdog_that_exits_after_ready() {
            let (status_tx, status_rx) = watch::channel(SurfnetStartupStatus::default());
            let (events_tx, events_rx) = unbounded();
            let (commands_tx, commands_rx) = unbounded();

            spawn_startup_watchdog(true, status_rx, events_tx, commands_tx).unwrap();

            // The spawned thread holds the receiver, distinguishing headless
            // from TUI mode above.
            assert_eq!(status_tx.receiver_count(), 1);

            // Ready releases the watchdog; the receiver drops with it.
            status_tx.send_replace(ready_status());
            let deadline = Instant::now() + TIMEOUT;
            while status_tx.receiver_count() != 0 {
                assert!(Instant::now() < deadline, "watchdog did not exit on Ready");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_channels_stay_empty(&events_rx, &commands_rx);
        }
    }
}
