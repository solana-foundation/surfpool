//! Exhaustive model check of the startup state machine. The reachable state
//! space is finite and small (two task kinds, four task states, five phases),
//! so a depth-first search from the default state can verify the startup
//! invariants at every reachable state and along every accepted transition,
//! with no sampling involved.
//!
//! Why visiting every reachable state is enough to trust the live code:
//! the sweep starts from the same `default()` state production starts
//! from, and drives the same `apply` function production calls (the named
//! methods such as `start_task` are thin wrappers over it). Every state
//! the sweep visits is checked, and every accepted transition out of a
//! checked state leads to a state that gets checked in turn. Production
//! has no other way to reach a state, so any state the live process can
//! occupy is one this sweep already checked.
//!
//! Some mistakes need no check at all, because the type cannot represent
//! them: the phase is computed from the state on every read rather than
//! stored, and only a sealed plan carries a task table to compute `Ready`
//! from. So an unsealed status cannot claim readiness, and a stored phase
//! cannot fall out of sync with the state it summarizes, because there is
//! no stored phase.
//!
//! One requirement is about sequences of events rather than single
//! states: a client must never observe readiness while declared work is
//! outstanding. Checking each state is still enough, because what a
//! client reads is computed from the current state alone and every
//! reachable state gets checked. If readiness is ever computed from
//! anything else (a cache, a debounce, an asynchronous publish), that
//! argument stops working, and event sequences need checking directly.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use surfpool_spec_harness::SpecDoc;

use super::*;

/// Any fixed instant. The projection check below cares whether an entry
/// is present, not when it started.
const STARTED_AT: u32 = 1_753_000_000;

/// The full transition alphabet, derived from `spec::TASK_KINDS`, so a
/// new task kind extends coverage without touching this function.
/// Failure transitions use a fixed error string so the reachable state
/// space stays finite.
fn commands() -> Vec<StartupTransition> {
    let mut commands: Vec<StartupTransition> = seal_payloads()
        .into_iter()
        .map(|tasks| StartupTransition::SealPlan { tasks })
        .collect();
    for task in spec::TASK_KINDS {
        commands.push(StartupTransition::StartTask { task });
        commands.push(StartupTransition::CompleteTask { task });
        commands.push(StartupTransition::FailTask {
            task,
            error: "boom".to_string(),
        });
    }
    commands.push(StartupTransition::FailPlanning {
        error: "boom".to_string(),
    });
    commands
}

/// Seal payloads: the empty plan, every single kind, every ordered
/// pair of distinct kinds, and one duplicate payload to exercise
/// deduplication.
fn seal_payloads() -> Vec<Vec<SurfnetStartupTask>> {
    let kinds = spec::TASK_KINDS;
    let mut plans = vec![vec![]];
    for &kind in &kinds {
        plans.push(vec![kind]);
    }
    for &first in &kinds {
        for &second in &kinds {
            if first != second {
                plans.push(vec![first, second]);
            }
        }
    }
    plans.push(vec![kinds[0], kinds[0]]);
    plans
}

/// The spec's name for the event a transition represents. The machine and
/// the spec name the same set of moves differently: `StartupTransition`
/// variants on the machine side, `PlanEvent` and `TaskEvent` on the spec
/// side. This function is the bridge between the two, so the observed
/// table records events under the spec's names, and `event_target` can
/// look a name back up in the spec's event lists to build its link.
fn event_name(transition: &StartupTransition) -> &'static str {
    match transition {
        StartupTransition::SealPlan { .. } => spec::PlanEvent::Sealed.name(),
        StartupTransition::FailPlanning { .. } => spec::PlanEvent::Failed.name(),
        StartupTransition::StartTask { .. } => spec::TaskEvent::Started.name(),
        StartupTransition::CompleteTask { .. } => spec::TaskEvent::Succeeded.name(),
        StartupTransition::FailTask { .. } => spec::TaskEvent::Failed.name(),
    }
}

/// Startup only moves forward: planning, then cloning, then runbooks,
/// then ready. The sweep asserts that every accepted transition keeps or
/// raises this rank, so no transition can move startup backward. `Failed`
/// sits outside the ordering: any non-terminal phase may fail (the sweep
/// exempts transitions into it), and nothing leaves it (terminal states
/// accept no transitions), so its rank is never actually compared.
fn phase_rank(phase: SurfnetStartupPhase) -> u8 {
    match phase {
        SurfnetStartupPhase::Planning => 0,
        SurfnetStartupPhase::CloningRemoteAccounts => 1,
        SurfnetStartupPhase::ExecutingRunbooks => 2,
        SurfnetStartupPhase::Ready => 3,
        SurfnetStartupPhase::Failed => u8::MAX,
    }
}

fn assert_state_invariants(status: &SurfnetStartupStatus) {
    // The spec gives every state exactly one expected phase, so no state
    // the sweep visits can slip through unchecked. This one equality also
    // covers the headline invariant: the spec only answers `Ready` for a
    // sealed plan whose every required task has succeeded.
    //
    // Two former assertions have no runtime check anymore because the
    // enum cannot represent their violations: the machine-level error is
    // derived (so it cannot disagree with the Failed phase), and an
    // unsealed status has no task table (so tasks cannot be registered
    // before sealing).
    assert_eq!(
        status.phase(),
        spec::expected_phase(status),
        "phase disagrees with the spec oracle: {status:?}"
    );

    // The projection a client reads, checked at every reachable state. The
    // oracle above ties Ready to a sealed plan with every task succeeded;
    // this ties what Anchor concludes to the phase. Together they make a
    // sealed plan with clones declared unable to report itself finished.
    let projected = GetSurfnetInfoResponse::with_startup(vec![], status.clone(), STARTED_AT);
    assert!(
        spec::compat_list_agrees_with_phase(&projected.runbook_executions, status.phase()),
        "the compatibility list disagrees with the phase, so a client \
         would read readiness from {status:?}"
    );

    // Task-level error bookkeeping is still two stored fields, so this
    // stays a real check: a task must carry an error exactly when it is
    // `Failed`. An error on a task that has not failed, and a failed task
    // with no reason recorded, are both bugs.
    for task in status.tasks() {
        assert_eq!(
            task.error.is_some(),
            task.state == SurfnetStartupTaskState::Failed,
            "task error and task state disagree: {status:?}"
        );
    }

    let tasks = status.tasks();
    for (index, task) in tasks.iter().enumerate() {
        assert!(
            tasks[..index]
                .iter()
                .all(|earlier| earlier.task != task.task),
            "duplicate task kind in plan: {status:?}"
        );
    }
}

#[test]
fn every_reachable_state_upholds_the_startup_invariants() {
    sweep();
}

/// The spec document beside this module, with the cargo aliases that
/// regenerate it.
fn spec_doc() -> SpecDoc {
    SpecDoc {
        path: concat!(env!("CARGO_MANIFEST_DIR"), "/src/startup-lifecycle.md"),
        diagrams_dir: concat!(env!("CARGO_MANIFEST_DIR"), "/src/diagrams"),
        update_alias: "surfpool-update-startup-spec",
        render_alias: "surfpool-render-startup-diagrams",
    }
}

/// The document claims its tables are generated; holding every block
/// equal to a fresh render is what keeps that claim true.
#[test]
fn the_document_matches_the_spec() {
    spec_doc().assert_blocks_current(&generated_blocks());
}

/// Ignored so a plain test run never writes to the source tree;
/// `cargo surfpool-update-startup-spec` runs it explicitly.
#[test]
#[ignore = "writes startup-lifecycle.md; run via cargo surfpool-update-startup-spec"]
fn regenerate_the_startup_spec_tables() {
    spec_doc().regenerate(&generated_blocks());
}

/// Every generated block, named by its marker in the document. Three
/// render from the spec module; the observed block renders from the
/// sweep.
fn generated_blocks() -> Vec<(&'static str, String)> {
    vec![
        ("plan-lifecycle", render_plan_lifecycle()),
        ("task-lifecycle", render_task_lifecycle()),
        ("projection", render_projection()),
        ("observed", sweep().render()),
        ("links", render_links()),
    ]
}

/// Proves the linked item exists and stringifies the same tokens, so a
/// rename breaks the build here and regeneration follows: the target of
/// a generated link cannot silently drift.
macro_rules! link_target {
    ($ty:ident :: $variant:ident) => {{
        let _ = |value: &$ty| matches!(value, $ty::$variant { .. });
        concat!(stringify!($ty), "::", stringify!($variant))
    }};
}

/// A reference-style link whose label is the target path itself:
/// `[display][Type::Variant]`. The label doubles as the definition key
/// in the links block, so two displays sharing a word ("Failed" the
/// task state, "Failed" the phase) cannot collide.
fn reference(display: &str, target: &'static str) -> String {
    format!("[{display}][{target}]")
}

fn phase_target(phase: SurfnetStartupPhase) -> &'static str {
    match phase {
        SurfnetStartupPhase::Planning => link_target!(SurfnetStartupPhase::Planning),
        SurfnetStartupPhase::CloningRemoteAccounts => {
            link_target!(SurfnetStartupPhase::CloningRemoteAccounts)
        }
        SurfnetStartupPhase::ExecutingRunbooks => {
            link_target!(SurfnetStartupPhase::ExecutingRunbooks)
        }
        SurfnetStartupPhase::Ready => link_target!(SurfnetStartupPhase::Ready),
        SurfnetStartupPhase::Failed => link_target!(SurfnetStartupPhase::Failed),
    }
}

fn task_state_target(state: SurfnetStartupTaskState) -> &'static str {
    match state {
        SurfnetStartupTaskState::Pending => link_target!(SurfnetStartupTaskState::Pending),
        SurfnetStartupTaskState::Running => link_target!(SurfnetStartupTaskState::Running),
        SurfnetStartupTaskState::Succeeded => link_target!(SurfnetStartupTaskState::Succeeded),
        SurfnetStartupTaskState::Failed => link_target!(SurfnetStartupTaskState::Failed),
    }
}

/// The machine variant a spec plan state names.
fn plan_state_target(state: spec::PlanState) -> &'static str {
    match state {
        spec::PlanState::Unsealed => link_target!(SurfnetStartupStatus::Planning),
        spec::PlanState::Sealed => link_target!(SurfnetStartupStatus::Sealed),
        spec::PlanState::PlanningFailed => link_target!(SurfnetStartupStatus::PlanningFailed),
    }
}

fn plan_event_target(event: spec::PlanEvent) -> &'static str {
    match event {
        spec::PlanEvent::Sealed => link_target!(StartupTransition::SealPlan),
        spec::PlanEvent::Failed => link_target!(StartupTransition::FailPlanning),
    }
}

fn task_event_target(event: spec::TaskEvent) -> &'static str {
    match event {
        spec::TaskEvent::Started => link_target!(StartupTransition::StartTask),
        spec::TaskEvent::Succeeded => link_target!(StartupTransition::CompleteTask),
        spec::TaskEvent::Failed => link_target!(StartupTransition::FailTask),
    }
}

/// The target for an event name out of the observed table's data,
/// recovered through the spec's event lists rather than by matching
/// strings against fresh literals.
fn event_target(name: &str) -> &'static str {
    for event in spec::PlanEvent::ALL {
        if event.name() == name {
            return plan_event_target(event);
        }
    }
    for event in spec::TaskEvent::ALL {
        if event.name() == name {
            return task_event_target(event);
        }
    }
    panic!("no event named {name}");
}

fn phase_target_by_name(name: &str) -> &'static str {
    for phase in PHASE_ORDER {
        if phase_name(phase) == name {
            return phase_target(phase);
        }
    }
    panic!("no phase named {name}");
}

/// The link definitions the tables reference: one `[path]: path` line
/// per linkable item. Rustdoc consumes these invisibly and resolves
/// each path in the module's scope; a plain markdown reader sees them
/// as the file's final block.
fn render_links() -> String {
    let mut targets: Vec<&'static str> = vec![];
    targets.extend(PHASE_ORDER.map(phase_target));
    targets.extend(spec::TASK_STATES.map(task_state_target));
    targets.extend(spec::PLAN_STATES.map(plan_state_target));
    targets.extend(spec::PlanEvent::ALL.map(plan_event_target));
    targets.extend(spec::TaskEvent::ALL.map(task_event_target));
    targets.sort_unstable();
    targets.dedup();
    targets
        .into_iter()
        .map(|target| format!("[{target}]: {target}\n"))
        .collect()
}

/// One row per event, enumerated from the spec's plan transition
/// function, like the task table.
fn render_plan_lifecycle() -> String {
    let rows: Vec<[String; 3]> = spec::PlanEvent::ALL
        .iter()
        .map(|event| {
            let sources: Vec<String> = spec::PLAN_STATES
                .iter()
                .filter(|state| spec::plan_transition(**state, *event).is_some())
                .map(|state| reference(spec::plan_state_name(*state), plan_state_target(*state)))
                .collect();
            let mut targets: Vec<spec::PlanState> = spec::PLAN_STATES
                .iter()
                .filter_map(|state| spec::plan_transition(*state, *event))
                .collect();
            targets.dedup();
            assert_eq!(
                targets.len(),
                1,
                "one row per event requires a single target state"
            );
            [
                sources.join(", "),
                reference(event.table_name(), plan_event_target(*event)),
                plan_target_cell(targets[0]),
            ]
        })
        .collect();
    render_table(["State", "Event", "New state"], &rows)
}

/// The plan table's target cell. `Sealed` carries its postcondition,
/// which `the_plan_lifecycle_matches_the_spec` holds to the machine.
fn plan_target_cell(state: spec::PlanState) -> String {
    match state {
        spec::PlanState::Sealed => format!(
            "{}, every task {}",
            reference("Sealed", plan_state_target(state)),
            reference(
                "Pending",
                task_state_target(SurfnetStartupTaskState::Pending)
            )
        ),
        other => reference(spec::plan_state_name(other), plan_state_target(other)),
    }
}

/// One row per event: the states it moves, and where to.
/// `spec::task_transition` answers for every (state, event) pair, so the
/// rows are read straight off it rather than written by hand.
fn render_task_lifecycle() -> String {
    let rows: Vec<[String; 3]> = spec::TaskEvent::ALL
        .iter()
        .map(|event| {
            let sources: Vec<String> = spec::TASK_STATES
                .iter()
                .filter(|state| spec::task_transition(**state, *event).is_some())
                .map(|state| reference(task_state_name(*state), task_state_target(*state)))
                .collect();
            let mut targets: Vec<SurfnetStartupTaskState> = spec::TASK_STATES
                .iter()
                .filter_map(|state| spec::task_transition(*state, *event))
                .collect();
            targets.dedup();
            assert_eq!(
                targets.len(),
                1,
                "one row per event requires a single target state"
            );
            [
                sources.join(", "),
                reference(event.name(), task_event_target(*event)),
                reference(task_state_name(targets[0]), task_state_target(targets[0])),
            ]
        })
        .collect();
    render_table(["State", "Event", "New state"], &rows)
}

fn render_projection() -> String {
    let rows: Vec<[String; 3]> = spec::projection_rows()
        .iter()
        .map(|row| {
            let phase = spec::expected_phase(&(row.build)());
            let wire = serde_json::to_value(phase).expect("a phase serializes");
            [
                row.state.to_string(),
                reference(
                    &format!("`{}`", wire.as_str().expect("a phase is a string")),
                    phase_target(phase),
                ),
                row.meaning.to_string(),
            ]
        })
        .collect();
    render_table(["State", "Phase", "Meaning"], &rows)
}

fn render_table(headers: [&str; 3], rows: &[[String; 3]]) -> String {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(cell.len());
        }
    }
    let format_row = |cells: [&str; 3]| {
        format!(
            "| {:<w0$} | {:<w1$} | {:<w2$} |\n",
            cells[0],
            cells[1],
            cells[2],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        )
    };
    let mut table = format_row(headers);
    table.push_str(&format!(
        "|{}|{}|{}|\n",
        "-".repeat(widths[0] + 2),
        "-".repeat(widths[1] + 2),
        "-".repeat(widths[2] + 2),
    ));
    for row in rows {
        table.push_str(&format_row([&row[0], &row[1], &row[2]]));
    }
    table
}

fn task_state_name(state: SurfnetStartupTaskState) -> &'static str {
    match state {
        SurfnetStartupTaskState::Pending => "Pending",
        SurfnetStartupTaskState::Running => "Running",
        SurfnetStartupTaskState::Succeeded => "Succeeded",
        SurfnetStartupTaskState::Failed => "Failed",
    }
}

fn sweep() -> Observed {
    let commands = commands();
    let initial = SurfnetStartupStatus::default();

    let mut seen = HashSet::new();
    seen.insert(format!("{initial:?}"));
    let mut frontier = vec![initial];
    let mut visited = 0usize;
    let mut accepted = 0usize;
    let mut reached = HashSet::new();
    let mut events: BTreeMap<&'static str, BTreeSet<&'static str>> = BTreeMap::new();
    let mut successors: BTreeMap<&'static str, BTreeSet<&'static str>> = BTreeMap::new();
    let mut adequacy: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();

    while let Some(state) = frontier.pop() {
        visited += 1;
        assert_state_invariants(&state);
        reached.insert(phase_name(state.phase()));
        let terminal = matches!(
            state.phase(),
            SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
        );

        for transition in &commands {
            let event = event_name(transition);
            let legal = spec::transition_is_legal(&state, transition);
            let mut next = state.clone();
            match next.apply(transition.clone()) {
                Ok(()) => {
                    assert!(
                        legal,
                        "the machine accepted a transition the spec forbids: \
                         {transition:?} from {state:?}"
                    );
                    adequacy.entry(event).or_default().0 += 1;
                    accepted += 1;
                    let phase = phase_name(state.phase());
                    events.entry(phase).or_default().insert(event);
                    successors
                        .entry(phase)
                        .or_default()
                        .insert(phase_name(next.phase()));
                    assert!(
                        !terminal,
                        "{transition:?} accepted from terminal state: {state:?}"
                    );
                    if next.phase() != SurfnetStartupPhase::Failed {
                        assert!(
                            phase_rank(next.phase()) >= phase_rank(state.phase()),
                            "{transition:?} regressed the phase: {state:?} -> {next:?}"
                        );
                    }
                    if seen.insert(format!("{next:?}")) {
                        frontier.push(next);
                    }
                }
                Err(error) => {
                    assert!(
                        !legal,
                        "the machine refused a transition the spec allows: \
                         {transition:?} from {state:?}"
                    );
                    adequacy.entry(event).or_default().1 += 1;
                    // A rejected transition must leave the state untouched;
                    // the watch-channel publisher relies on this.
                    assert_eq!(
                        next, state,
                        "{transition:?} was rejected but mutated the state"
                    );
                    // And the refusal must carry the exact pair.
                    assert_eq!(error.attempted, *transition);
                    assert_eq!(error.from, state);
                }
            }
        }
    }

    // Every event kind must be both accepted and rejected somewhere in
    // the walk. A command list that never reached a refusal (or never
    // landed) would pass every check above while proving nothing,
    // because those checks only fire when a transition is attempted.
    for name in spec::PlanEvent::ALL
        .map(spec::PlanEvent::name)
        .into_iter()
        .chain(spec::TaskEvent::ALL.map(spec::TaskEvent::name))
    {
        let (accepted_count, rejected_count) = adequacy.get(name).copied().unwrap_or((0, 0));
        assert!(
            accepted_count > 0,
            "no {name} transition was ever accepted by the sweep"
        );
        assert!(
            rejected_count > 0,
            "no {name} transition was ever rejected by the sweep"
        );
    }

    // A shrunken search would pass every assertion above while checking
    // nothing, so require the full space: every phase reachable, and the
    // spec's observed block equal to what this sweep just observed.
    for phase in PHASE_ORDER {
        assert!(
            reached.contains(phase_name(phase)),
            "model check never reached {phase:?}"
        );
    }
    Observed {
        visited,
        attempted: visited * commands.len(),
        accepted,
        events,
        successors,
    }
}

/// The order the spec's tables list phases in.
const PHASE_ORDER: [SurfnetStartupPhase; 5] = [
    SurfnetStartupPhase::Planning,
    SurfnetStartupPhase::CloningRemoteAccounts,
    SurfnetStartupPhase::ExecutingRunbooks,
    SurfnetStartupPhase::Ready,
    SurfnetStartupPhase::Failed,
];

/// The spec's row label for a phase.
fn phase_name(phase: SurfnetStartupPhase) -> &'static str {
    match phase {
        SurfnetStartupPhase::Planning => "Planning",
        SurfnetStartupPhase::CloningRemoteAccounts => "CloningRemoteAccounts",
        SurfnetStartupPhase::ExecutingRunbooks => "ExecutingRunbooks",
        SurfnetStartupPhase::Ready => "Ready",
        SurfnetStartupPhase::Failed => "Failed",
    }
}

/// One full sweep's observations, in the vocabulary the spec's tables
/// use.
struct Observed {
    visited: usize,
    attempted: usize,
    accepted: usize,
    events: BTreeMap<&'static str, BTreeSet<&'static str>>,
    successors: BTreeMap<&'static str, BTreeSet<&'static str>>,
}

impl Observed {
    /// An empty event set renders as "nothing" and an empty successor
    /// set as "terminal", so the terminal rows are derived rather than
    /// declared.
    fn render(&self) -> String {
        let join = |set: Option<&BTreeSet<&'static str>>,
                    empty: &str,
                    target_of: &dyn Fn(&str) -> &'static str| {
            set.filter(|items| !items.is_empty())
                .map(|items| {
                    items
                        .iter()
                        .map(|item| reference(item, target_of(item)))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| empty.to_string())
        };

        let rows: Vec<[String; 3]> = PHASE_ORDER
            .iter()
            .map(|phase| {
                let name = phase_name(*phase);
                [
                    reference(name, phase_target(*phase)),
                    join(self.events.get(name), "nothing", &event_target),
                    join(self.successors.get(name), "terminal", &phase_target_by_name),
                ]
            })
            .collect();

        let mut block = render_table(["Phase", "Accepts", "Can lead to"], &rows);
        block.push_str(&format!(
            "\n{} reachable states, {} attempted transitions, {} accepted.\n",
            self.visited, self.attempted, self.accepted,
        ));
        block
    }
}

/// Each rendered SVG pins the hash of the mermaid source it was
/// rendered from, so editing a diagram without re-rendering fails
/// here, and CI needs no mermaid toolchain to detect the drift.
#[test]
fn the_diagrams_match_their_renderings() {
    spec_doc().assert_diagrams_current();
}

/// Renders each mermaid region to `src/diagrams/<name>.svg` and pins
/// the source hash. Ignored so a plain test run never needs the
/// mermaid CLI; `cargo surfpool-render-startup-diagrams` runs it.
#[test]
#[ignore = "runs mmdc; invoke via cargo surfpool-render-startup-diagrams"]
fn render_the_startup_diagrams() {
    spec_doc().render_diagrams();
}

/// Forges a state the machine cannot derive, to demonstrate the rule
/// rejects it.
///
/// The machine makes illegal startup states unrepresentable, so a rule
/// applied only to machine-derived states never meets a violation and
/// cannot be shown to discriminate. `with_startup` cannot produce this
/// pairing; a struct literal can, because the response's fields are public.
/// Rejecting the forged pairing establishes that a client sees only
/// legally derivable states.
#[test]
fn the_forbidden_pairing_is_one_we_can_build() {
    let mut outstanding = SurfnetStartupStatus::default();
    outstanding
        .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
        .expect("an unsealed plan should accept a seal");
    assert_eq!(
        outstanding.phase(),
        SurfnetStartupPhase::CloningRemoteAccounts
    );

    // The response a client received during the clone window before the
    // fix: a sealed plan with the clone outstanding, and an empty list.
    let forbidden = GetSurfnetInfoResponse {
        runbook_executions: vec![],
        startup: outstanding.clone(),
    };

    // Anchor's readiness rule, applied to that response.
    assert!(
        forbidden
            .runbook_executions
            .iter()
            .all(|execution| execution.completed_at.is_some()),
        "a polling client reads this response as startup finished"
    );
    assert!(
        !forbidden.startup.is_ready(),
        "and it reads that from a surfnet that is not ready"
    );

    assert!(
        !spec::compat_list_agrees_with_phase(
            &forbidden.runbook_executions,
            forbidden.startup.phase()
        ),
        "the rule accepted the pairing issue 715 was reported as"
    );

    // The projection the surfnet actually answers through never builds it.
    let answered = GetSurfnetInfoResponse::with_startup(vec![], outstanding.clone(), STARTED_AT);
    assert!(
        spec::compat_list_agrees_with_phase(&answered.runbook_executions, outstanding.phase()),
        "the projection produced the forbidden pairing: {answered:?}"
    );
}
