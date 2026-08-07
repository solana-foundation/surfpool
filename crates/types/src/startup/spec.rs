//! The spec: startup's rules stated on their own, and the single source for
//! every generated table in `startup-lifecycle.md`.
//!
//! The principle: spec and implementation must be different encodings of
//! the same rules; do not be tempted to combine them, because the
//! reachability sweep proves the machine agrees with this module, and that
//! proof is empty the moment the two share code. Everything here is written
//! in the spec's vocabulary (sealed, declared, succeeded, outstanding) and
//! reads the machine only through its public accessors, never through its
//! derivation logic.
//!
//! Maintenance procedure for adding, removing, or changing a state, task,
//! or transition:
//!
//! 1. State the new rule here first, in spec vocabulary.
//! 2. Change the machine to satisfy it.
//! 3. `cargo test -p surfpool-types` fails while the two disagree, naming
//!    the first state where they part.
//! 4. `cargo surfpool-update-startup-spec` regenerates every table in
//!    `startup-lifecycle.md`; review that diff as the observable change.
//! 5. The prose around the tables is authored: revise it by hand when a
//!    rule changes meaning, and leave it alone otherwise.

use super::*;

/// The phase a state must be in:
///
/// 1. A failure at any stage is terminal.
/// 2. An unsealed plan is still planning; it can never be ready, even
///    when its task collection is empty.
/// 3. A sealed plan with every required task succeeded is ready (the
///    empty plan is ready immediately).
/// 4. Otherwise, pending hydration means initializing; after
///    hydration, deploying.
pub fn expected_phase(status: &SurfnetStartupStatus) -> SurfnetStartupPhase {
    let failed = status.error().is_some()
        || status
            .tasks()
            .iter()
            .any(|task| task.state == SurfnetStartupTaskState::Failed);
    if failed {
        return SurfnetStartupPhase::Failed;
    }
    if !status.plan_sealed() {
        return SurfnetStartupPhase::Planning;
    }
    if status
        .tasks()
        .iter()
        .all(|task| task.state == SurfnetStartupTaskState::Succeeded)
    {
        return SurfnetStartupPhase::Ready;
    }
    if status.tasks().iter().any(|task| {
        task.task == SurfnetStartupTask::RemoteAccounts
            && task.state != SurfnetStartupTaskState::Succeeded
    }) {
        return SurfnetStartupPhase::CloningRemoteAccounts;
    }
    SurfnetStartupPhase::ExecutingRunbooks
}

/// Checks the compatibility projection against the startup phase, using
/// the rule Anchor applies: it proceeds when every entry in
/// `runbookExecutions` is complete. It may proceed exactly when startup
/// is over.
///
/// `Failed` counts as over. A pending entry there would park a client in
/// a readiness loop that has no timeout, so the entry completes and the
/// reason is reported in `errors`.
///
/// Stated in terms of Anchor's predicate rather than an empty list,
/// because an empty list is only one way to satisfy it. An entry marked
/// complete regardless of phase satisfies it too, and a check written
/// against emptiness does not detect that.
pub fn compat_list_agrees_with_phase(
    runbook_executions: &[RunbookExecutionStatusReport],
    phase: SurfnetStartupPhase,
) -> bool {
    let anchor_would_proceed = runbook_executions
        .iter()
        .all(|execution| execution.completed_at.is_some());
    let startup_is_over = matches!(
        phase,
        SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
    );
    anchor_would_proceed == startup_is_over
}

/// A task-level event, named as the spec's tables name it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskEvent {
    Started,
    Succeeded,
    Failed,
}

impl TaskEvent {
    pub const ALL: [TaskEvent; 3] = [TaskEvent::Started, TaskEvent::Succeeded, TaskEvent::Failed];

    pub fn name(self) -> &'static str {
        match self {
            TaskEvent::Started => "StartupTaskStarted",
            TaskEvent::Succeeded => "StartupTaskSucceeded",
            TaskEvent::Failed => "StartupTaskFailed",
        }
    }
}

/// Every task state, in the order the spec's table lists them.
pub const TASK_STATES: [SurfnetStartupTaskState; 4] = [
    SurfnetStartupTaskState::Pending,
    SurfnetStartupTaskState::Running,
    SurfnetStartupTaskState::Succeeded,
    SurfnetStartupTaskState::Failed,
];

/// The task lifecycle: the state an event moves a task to, or `None`
/// where the move is refused. `Succeeded` and `Failed` accept nothing;
/// work can fail before it starts, and cannot finish before it starts.
pub fn task_transition(
    from: SurfnetStartupTaskState,
    event: TaskEvent,
) -> Option<SurfnetStartupTaskState> {
    use SurfnetStartupTaskState::*;
    match (from, event) {
        (Pending, TaskEvent::Started) => Some(Running),
        (Running, TaskEvent::Succeeded) => Some(Succeeded),
        (Pending | Running, TaskEvent::Failed) => Some(Failed),
        _ => None,
    }
}

/// A plan-level event, named as the spec's tables name it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlanEvent {
    Sealed,
    Failed,
}

impl PlanEvent {
    pub const ALL: [PlanEvent; 2] = [PlanEvent::Sealed, PlanEvent::Failed];

    pub fn name(self) -> &'static str {
        match self {
            PlanEvent::Sealed => "StartupPlanSealed",
            PlanEvent::Failed => "StartupFailed",
        }
    }

    /// The table spelling, which shows the seal's payload.
    pub fn table_name(self) -> &'static str {
        match self {
            PlanEvent::Sealed => "StartupPlanSealed(tasks)",
            PlanEvent::Failed => "StartupFailed",
        }
    }
}

/// The plan's three states, in spec vocabulary: unsealed, sealed, or
/// failed before sealing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanState {
    Unsealed,
    Sealed,
    PlanningFailed,
}

pub const PLAN_STATES: [PlanState; 3] = [
    PlanState::Unsealed,
    PlanState::Sealed,
    PlanState::PlanningFailed,
];

/// The plan lifecycle: the state an event moves the plan to, or `None`
/// where the move is refused. Sealing and a planning failure exist only
/// unsealed; `Sealed` and `PlanningFailed` accept nothing.
pub fn plan_transition(from: PlanState, event: PlanEvent) -> Option<PlanState> {
    match (from, event) {
        (PlanState::Unsealed, PlanEvent::Sealed) => Some(PlanState::Sealed),
        (PlanState::Unsealed, PlanEvent::Failed) => Some(PlanState::PlanningFailed),
        _ => None,
    }
}

/// The table's name for a plan state as a transition source.
pub fn plan_state_name(state: PlanState) -> &'static str {
    match state {
        PlanState::Unsealed => "Unsealed",
        PlanState::Sealed => "Sealed",
        PlanState::PlanningFailed => "PlanningFailed",
    }
}

/// Every task kind. The match beside it makes omission a compile
/// error: a new variant fails there until this list grows, and the
/// list is what extends the sweep's alphabet and the generated tables.
pub const TASK_KINDS: [SurfnetStartupTask; 2] = [
    SurfnetStartupTask::RemoteAccounts,
    SurfnetStartupTask::RunbookExecutions,
];

const _: fn(SurfnetStartupTask) = |task| match task {
    SurfnetStartupTask::RemoteAccounts | SurfnetStartupTask::RunbookExecutions => (),
};

/// Whether the machine must accept a transition from this state,
/// composed entirely from the spec's own rules: the plan lifecycle
/// admits plan events, and a task event needs a sealed plan, startup
/// not yet over, a declared task, and a task lifecycle row for the
/// move.
pub fn transition_is_legal(status: &SurfnetStartupStatus, transition: &StartupTransition) -> bool {
    match transition {
        StartupTransition::SealPlan { .. } => {
            plan_transition(plan_state_of(status), PlanEvent::Sealed).is_some()
        }
        StartupTransition::FailPlanning { .. } => {
            plan_transition(plan_state_of(status), PlanEvent::Failed).is_some()
        }
        StartupTransition::StartTask { task } => {
            task_move_is_legal(status, *task, TaskEvent::Started)
        }
        StartupTransition::CompleteTask { task } => {
            task_move_is_legal(status, *task, TaskEvent::Succeeded)
        }
        StartupTransition::FailTask { task, .. } => {
            task_move_is_legal(status, *task, TaskEvent::Failed)
        }
    }
}

fn task_move_is_legal(
    status: &SurfnetStartupStatus,
    task: SurfnetStartupTask,
    event: TaskEvent,
) -> bool {
    let startup_over = matches!(
        expected_phase(status),
        SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
    );
    plan_state_of(status) == PlanState::Sealed
        && !startup_over
        && status
            .tasks()
            .iter()
            .find(|entry| entry.task == task)
            .is_some_and(|entry| task_transition(entry.state, event).is_some())
}

/// The plan state a status is in, read through the public accessors.
pub fn plan_state_of(status: &SurfnetStartupStatus) -> PlanState {
    if status.plan_sealed() {
        PlanState::Sealed
    } else if status.error().is_some() {
        PlanState::PlanningFailed
    } else {
        PlanState::Unsealed
    }
}

/// A projection-table row. The state description and the meaning are
/// authored; the phase column is computed by [`expected_phase`] on the
/// representative state, so the printed mapping cannot disagree with
/// the spec. Rows are ordered: the table reads top-down, first match
/// wins.
pub struct ProjectionRow {
    pub state: &'static str,
    pub build: fn() -> SurfnetStartupStatus,
    pub meaning: &'static str,
}

pub fn projection_rows() -> [ProjectionRow; 6] {
    [
        ProjectionRow {
            state: "`Planning`",
            build: SurfnetStartupStatus::default,
            meaning: "The required task set is still being computed.",
        },
        ProjectionRow {
            state: "`PlanningFailed { error }`",
            build: || {
                let mut status = SurfnetStartupStatus::default();
                status.fail_planning("boom").unwrap();
                status
            },
            meaning: "Planning failed before a plan was sealed.",
        },
        ProjectionRow {
            state: "`Sealed`, any task `Failed`",
            build: || {
                let mut status = SurfnetStartupStatus::default();
                status
                    .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
                    .unwrap();
                status
                    .fail_task(SurfnetStartupTask::RemoteAccounts, "boom")
                    .unwrap();
                status
            },
            meaning: "A required startup task failed.",
        },
        ProjectionRow {
            state: "`Sealed`, every task `Succeeded`",
            build: || {
                let mut status = SurfnetStartupStatus::default();
                status.seal_plan(vec![]).unwrap();
                status
            },
            meaning: "All required work has completed. An empty sealed plan reaches this state immediately.",
        },
        ProjectionRow {
            state: "`Sealed`, clones outstanding",
            build: || {
                let mut status = SurfnetStartupStatus::default();
                status
                    .seal_plan(vec![
                        SurfnetStartupTask::RemoteAccounts,
                        SurfnetStartupTask::RunbookExecutions,
                    ])
                    .unwrap();
                status
            },
            meaning: "Account hydration is still in progress.",
        },
        ProjectionRow {
            state: "`Sealed`, runbook execution outstanding",
            build: || {
                let mut status = SurfnetStartupStatus::default();
                status
                    .seal_plan(vec![
                        SurfnetStartupTask::RemoteAccounts,
                        SurfnetStartupTask::RunbookExecutions,
                    ])
                    .unwrap();
                status
                    .start_task(SurfnetStartupTask::RemoteAccounts)
                    .unwrap();
                status
                    .complete_task(SurfnetStartupTask::RemoteAccounts)
                    .unwrap();
                status
            },
            meaning: "Hydration has completed (or was unnecessary); runbook execution is the remaining startup work.",
        },
    ]
}
