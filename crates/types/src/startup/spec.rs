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
/// Stated in terms of Anchor's rule rather than an empty list,
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

/// A cell's successor: the machine moves to a state, or refuses the
/// event, leaving the state untouched and the caller with an error.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Next<State> {
    To(State),
    Refuse,
}

/// One cell of a transition table. Listing every (state, event) pair,
/// refusals included, keeps the lookup total and shows each refusal
/// decided rather than defaulted; `the_plan_table_is_total` and
/// `the_task_table_is_total` assert the listings are complete.
pub struct Row<State, Event> {
    pub state: State,
    pub event: Event,
    pub next: Next<State>,
}

const fn row<State, Event>(state: State, event: Event, next: Next<State>) -> Row<State, Event> {
    Row { state, event, next }
}

use Next::{Refuse, To};
use PlanEvent as PE;
use PlanState as PS;
use SurfnetStartupTaskState as TS;
use TaskEvent as TE;

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

/// The task lifecycle as data: every (state, event) cell, refusals
/// included. `Succeeded` and `Failed` accept nothing; work can fail
/// before it starts, and cannot finish before it starts.
#[rustfmt::skip]
pub const TASK_TABLE: &[Row<SurfnetStartupTaskState, TaskEvent>] = &[
    //   state          event          next
    row( TS::Pending,   TE::Started,   To(TS::Running)   ),
    row( TS::Pending,   TE::Succeeded, Refuse            ), // cannot finish before it starts
    row( TS::Pending,   TE::Failed,    To(TS::Failed)    ), // work can fail before it starts
    row( TS::Running,   TE::Started,   Refuse            ), // started at most once
    row( TS::Running,   TE::Succeeded, To(TS::Succeeded) ),
    row( TS::Running,   TE::Failed,    To(TS::Failed)    ),
    row( TS::Succeeded, TE::Started,   Refuse            ), // terminal
    row( TS::Succeeded, TE::Succeeded, Refuse            ),
    row( TS::Succeeded, TE::Failed,    Refuse            ),
    row( TS::Failed,    TE::Started,   Refuse            ), // terminal
    row( TS::Failed,    TE::Succeeded, Refuse            ),
    row( TS::Failed,    TE::Failed,    Refuse            ),
];

/// The task table's answer for (state, event): the state the event
/// moves a task to, or `None` where the table refuses the move.
pub fn task_transition(
    from: SurfnetStartupTaskState,
    event: TaskEvent,
) -> Option<SurfnetStartupTaskState> {
    let cell = TASK_TABLE
        .iter()
        .find(|row| row.state == from && row.event == event)
        .expect("the_task_table_is_total guarantees every cell exists");
    match cell.next {
        To(state) => Some(state),
        Refuse => None,
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

/// The plan lifecycle as data: every (state, event) cell, refusals
/// included. Sealing and a planning failure exist only unsealed;
/// `Sealed` and `PlanningFailed` accept nothing.
#[rustfmt::skip]
pub const PLAN_TABLE: &[Row<PlanState, PlanEvent>] = &[
    //   state               event       next
    row( PS::Unsealed,       PE::Sealed, To(PS::Sealed)         ),
    row( PS::Unsealed,       PE::Failed, To(PS::PlanningFailed) ),
    row( PS::Sealed,         PE::Sealed, Refuse                 ), // sealed at most once
    row( PS::Sealed,         PE::Failed, Refuse                 ), // failure after sealing is a task failure
    row( PS::PlanningFailed, PE::Sealed, Refuse                 ), // terminal
    row( PS::PlanningFailed, PE::Failed, Refuse                 ),
];

/// The plan table's answer for (state, event): the state the event
/// moves the plan to, or `None` where the table refuses the move.
pub fn plan_transition(from: PlanState, event: PlanEvent) -> Option<PlanState> {
    let cell = PLAN_TABLE
        .iter()
        .find(|row| row.state == from && row.event == event)
        .expect("the_plan_table_is_total guarantees every cell exists");
    match cell.next {
        To(state) => Some(state),
        Refuse => None,
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

/// The table's name for a task state.
pub fn task_state_name(state: SurfnetStartupTaskState) -> &'static str {
    match state {
        SurfnetStartupTaskState::Pending => "Pending",
        SurfnetStartupTaskState::Running => "Running",
        SurfnetStartupTaskState::Succeeded => "Succeeded",
        SurfnetStartupTaskState::Failed => "Failed",
    }
}

/// The spec's name for a task kind; the match makes a new kind a
/// compile error here until it decides its name.
pub fn task_kind_name(task: SurfnetStartupTask) -> &'static str {
    match task {
        SurfnetStartupTask::RemoteAccounts => "RemoteAccounts",
        SurfnetStartupTask::RunbookExecutions => "RunbookExecutions",
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

/// One transition table's Promela rendering: which rows, which event
/// list orders the inlines, and how states and events spell in the
/// emitted text.
struct PromelaTable<'a, State: Copy + PartialEq, Event: Copy + PartialEq> {
    rows: &'a [Row<State, Event>],
    events: &'a [Event],
    event_name: fn(Event) -> &'static str,
    state_macro: fn(State) -> String,
    /// The inline family: `apply_<prefix>_<event>`.
    prefix: &'static str,
    /// The Promela lvalue the inlines read and write.
    state_var: &'static str,
    /// The inline's parameter list, without parentheses.
    params: &'static str,
}

impl<State: Copy + PartialEq, Event: Copy + PartialEq> PromelaTable<'_, State, Event> {
    /// One inline per event column, one branch per table row, in table
    /// order. `To` assigns; `Refuse` leaves the state untouched, as
    /// the machine's error return does. Totality of the table is what
    /// lets the `if` carry no `else` arm.
    fn render(&self) -> String {
        let mut out = String::new();
        for &event in self.events {
            out.push_str(&format!(
                "inline apply_{}_{}({}) {{\n    if\n",
                self.prefix,
                (self.event_name)(event),
                self.params
            ));
            for row in self.rows.iter().filter(|row| row.event == event) {
                let guard = format!(
                    "    :: {} == {}",
                    self.state_var,
                    (self.state_macro)(row.state)
                );
                match row.next {
                    To(state) => out.push_str(&format!(
                        "{guard} -> {} = {}\n",
                        self.state_var,
                        (self.state_macro)(state)
                    )),
                    Refuse => out.push_str(&format!("{guard} -> skip /* Refuse */\n")),
                }
            }
            out.push_str("    fi\n}\n\n");
        }
        out
    }
}

/// The spec tables as Promela: state encodings, the two state
/// variables, and one apply inline per event. The encodings follow the
/// spec's list order, so Promela's zero-initialized globals start in
/// the first-listed state (Unsealed, Pending), which is the machine's
/// default.
pub fn render_promela_cells() -> String {
    let mut out = String::from(
        "/* GENERATED: the startup spec tables as Promela cells.\n\
         \x20* Source: crates/types/src/startup/spec.rs (PLAN_TABLE, TASK_TABLE).\n\
         \x20* Regenerate: cargo surfpool-update-startup-pml. Do not edit.\n\
         \x20*\n\
         \x20* State encodings follow the spec's list order, so Promela's\n\
         \x20* zero-initialized globals start in the first-listed state\n\
         \x20* (Unsealed, Pending), which is the machine's default.\n\
         \x20*/\n\n",
    );

    out.push_str(&format!("#define NTASKS {}\n", TASK_KINDS.len()));
    for (index, &kind) in TASK_KINDS.iter().enumerate() {
        out.push_str(&format!("#define KIND_{} {index}\n", task_kind_name(kind)));
    }
    out.push('\n');
    for (index, &state) in PLAN_STATES.iter().enumerate() {
        out.push_str(&format!(
            "#define PLAN_{} {index}\n",
            plan_state_name(state)
        ));
    }
    out.push('\n');
    for (index, &state) in TASK_STATES.iter().enumerate() {
        out.push_str(&format!(
            "#define TASK_{} {index}\n",
            task_state_name(state)
        ));
    }
    out.push_str("\nbyte plan_state;\nbyte task_state[NTASKS];\n\n");

    out.push_str(
        &PromelaTable {
            rows: PLAN_TABLE,
            events: &PlanEvent::ALL,
            event_name: PlanEvent::name,
            state_macro: |state| format!("PLAN_{}", plan_state_name(state)),
            prefix: "plan",
            state_var: "plan_state",
            params: "",
        }
        .render(),
    );
    out.push_str(
        &PromelaTable {
            rows: TASK_TABLE,
            events: &TaskEvent::ALL,
            event_name: TaskEvent::name,
            state_macro: |state| format!("TASK_{}", task_state_name(state)),
            prefix: "task",
            state_var: "task_state[i]",
            params: "i",
        }
        .render(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_table_is_total() {
        for state in PLAN_STATES {
            for event in PlanEvent::ALL {
                let count = PLAN_TABLE
                    .iter()
                    .filter(|row| row.state == state && row.event == event)
                    .count();
                assert_eq!(
                    count,
                    1,
                    "the cell ({}, {}) must appear exactly once",
                    plan_state_name(state),
                    event.name()
                );
            }
        }
        assert_eq!(PLAN_TABLE.len(), PLAN_STATES.len() * PlanEvent::ALL.len());
    }

    #[test]
    fn the_task_table_is_total() {
        for state in TASK_STATES {
            for event in TaskEvent::ALL {
                let count = TASK_TABLE
                    .iter()
                    .filter(|row| row.state == state && row.event == event)
                    .count();
                assert_eq!(
                    count,
                    1,
                    "the cell ({state:?}, {}) must appear exactly once",
                    event.name()
                );
            }
        }
        assert_eq!(TASK_TABLE.len(), TASK_STATES.len() * TaskEvent::ALL.len());
    }
}
