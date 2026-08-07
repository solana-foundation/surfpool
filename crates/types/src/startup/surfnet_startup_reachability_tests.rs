//! Exhaustive model check of the startup state machine. The reachable state
//! space is finite and small (two task kinds, four task states, five phases),
//! so a breadth-first search from the default state can verify the startup
//! invariants at every reachable state and along every accepted transition,
//! with no sampling involved.
//!
//! The race in issue 715 was a history property: no sequence of transitions
//! may let a client observe readiness while declared work is outstanding.
//! This sweep checks state invariants instead, which suffices because the
//! projection a client reads is a pure function of the current state and
//! every reachable state is visited. If readiness ever acquires memory of
//! its own (a cache, a debounce, an asynchronous publish), that reduction
//! stops holding, and forbidden histories need checking directly.

use std::collections::{BTreeMap, BTreeSet, HashSet};

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

/// The spec's event name for a transition.
fn event_name(transition: &StartupTransition) -> &'static str {
    match transition {
        StartupTransition::SealPlan { .. } => spec::PlanEvent::Sealed.name(),
        StartupTransition::FailPlanning { .. } => spec::PlanEvent::Failed.name(),
        StartupTransition::StartTask { .. } => spec::TaskEvent::Started.name(),
        StartupTransition::CompleteTask { .. } => spec::TaskEvent::Succeeded.name(),
        StartupTransition::FailTask { .. } => spec::TaskEvent::Failed.name(),
    }
}

/// Progress order for the monotonicity check. `Failed` is handled
/// separately: it is reachable from any non-terminal phase.
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
    // The oracle equation is total: every state has exactly one expected
    // phase, so no state slips through unchecked. This subsumes the
    // headline issue-715 invariant (Ready requires a sealed plan with
    // every required task succeeded).
    //
    // Two former assertions have no runtime check anymore because the
    // sum-type representation makes their violations unrepresentable:
    // the machine-level error is now derived (so it cannot disagree
    // with the Failed phase), and an unsealed status has no task table
    // (so tasks cannot be registered before sealing).
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

    // Task-level error bookkeeping is still two stored fields, so the
    // biconditional remains a real check.
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

/// The document claims its tables are generated; holding every block
/// equal to a fresh render is what keeps that claim true.
#[test]
fn the_document_matches_the_spec() {
    let text = read_spec();
    for (name, content) in generated_blocks() {
        let (start, end) = region(&text, name);
        assert!(
            text[start..end] == content,
            "the {name} block in startup-lifecycle.md disagrees with the \
             spec. Expected:\n\n{content}\nRun `cargo \
             surfpool-update-startup-spec` to regenerate every table."
        );
    }
}

/// Ignored so a plain test run never writes to the source tree;
/// `cargo surfpool-update-startup-spec` runs it explicitly.
#[test]
#[ignore = "writes startup-lifecycle.md; run via cargo surfpool-update-startup-spec"]
fn regenerate_the_startup_spec_tables() {
    let mut text = read_spec();
    for (name, content) in generated_blocks() {
        let (start, end) = region(&text, name);
        text.replace_range(start..end, &content);
    }
    std::fs::write(SPEC_PATH, text)
        .unwrap_or_else(|error| panic!("could not write {SPEC_PATH}: {error}"));
    eprintln!("regenerated the spec tables in {SPEC_PATH}");
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

/// One row per event: the states it moves, and where to. The spec's
/// transition function is total over the small vocabulary, so the rows
/// enumerate rather than restate it.
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

    // Adequacy: an alphabet whose representatives never reach a guard
    // would pass every assertion above while exercising nothing, so
    // every event kind must be both accepted and rejected somewhere.
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

const SPEC_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/startup-lifecycle.md");

fn read_spec() -> String {
    std::fs::read_to_string(SPEC_PATH)
        .unwrap_or_else(|error| panic!("could not read {SPEC_PATH}: {error}"))
}

/// The byte range between `name`'s generated markers.
fn region(text: &str, name: &str) -> (usize, usize) {
    let begin = format!("<!-- BEGIN GENERATED: {name} -->\n");
    let end_marker = format!("<!-- END GENERATED: {name} -->");
    let start = text
        .find(&begin)
        .unwrap_or_else(|| panic!("{SPEC_PATH} has no {begin:?} marker"))
        + begin.len();
    let end = text[start..]
        .find(&end_marker)
        .unwrap_or_else(|| panic!("{SPEC_PATH} has no {end_marker:?} marker"))
        + start;
    // A second copy of the block would be neither checked nor
    // regenerated, so refuse the ambiguity.
    assert!(
        text[end..].find(&begin).is_none(),
        "{SPEC_PATH} has more than one {name} block"
    );
    (start, end)
}

/// Every mermaid region in the spec: its name and its fenced source.
/// The source in the document is the diagram; `build.rs` swaps each
/// region for its pre-rendered SVG in the rustdoc variant, so GitHub
/// renders the fence and rustdoc renders the drawing.
fn diagram_sources() -> Vec<(String, String)> {
    let text = read_spec();
    let mut sources = vec![];
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<!-- BEGIN MERMAID: ") {
        let name_start = start + "<!-- BEGIN MERMAID: ".len();
        let name_end = rest[name_start..]
            .find(" -->")
            .expect("a mermaid marker name")
            + name_start;
        let name = rest[name_start..name_end].to_string();
        let body_start = name_end + " -->".len();
        let end_marker = format!("<!-- END MERMAID: {name} -->");
        let end = rest.find(&end_marker).expect("a closing mermaid marker");
        sources.push((name, rest[body_start..end].trim().to_string()));
        rest = &rest[end + end_marker.len()..];
    }
    sources
}

/// FNV-1a, implemented locally: the pin must be stable across Rust
/// releases, which std's `DefaultHasher` does not promise.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn diagram_svg_path(name: &str) -> String {
    format!("{}/src/diagrams/{name}.svg", env!("CARGO_MANIFEST_DIR"))
}

/// Replaces each distinct random token after `state-id-` with its
/// first-occurrence index, rewriting every reference to it the same
/// way, so internal id links inside the SVG stay consistent.
fn normalize_state_ids(svg: &str) -> String {
    const MARKER: &str = "state-id-";
    let mut tokens: Vec<String> = vec![];
    let mut output = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(found) = rest.find(MARKER) {
        let after = found + MARKER.len();
        let token_len = rest[after..]
            .bytes()
            .take_while(u8::is_ascii_alphanumeric)
            .count();
        let token = rest[after..after + token_len].to_string();
        let index = match tokens.iter().position(|seen| *seen == token) {
            Some(index) => index,
            None => {
                tokens.push(token);
                tokens.len() - 1
            }
        };
        output.push_str(&rest[..after]);
        output.push_str(&format!("d{index}"));
        rest = &rest[after + token_len..];
    }
    output.push_str(rest);
    output
}

/// Each rendered SVG pins the hash of the mermaid source it was
/// rendered from, so editing a diagram without re-rendering fails
/// here, and CI needs no mermaid toolchain to detect the drift.
#[test]
fn the_diagrams_match_their_renderings() {
    for (name, source) in diagram_sources() {
        let path = diagram_svg_path(&name);
        let svg = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "could not read {path}: {error}; run `cargo \
                 surfpool-render-startup-diagrams`"
            )
        });
        let expected = format!("<!-- mermaid-fnv1a: {:016x} -->", fnv1a(&source));
        assert!(
            svg.starts_with(&expected),
            "{name}: the rendered SVG is stale; run `cargo \
             surfpool-render-startup-diagrams`"
        );
    }
}

/// Renders each mermaid region to `src/diagrams/<name>.svg` and pins
/// the source hash. Ignored so a plain test run never needs the
/// mermaid CLI; `cargo surfpool-render-startup-diagrams` runs it.
///
/// Determinism boundary: with the id fixes below, re-rendering an
/// unchanged source is byte-stable on one machine, and CI never
/// compares SVG bytes at all (the check test pins the source hash),
/// so environments can differ without breaking anything. Across
/// machines, Chromium versions and font fallbacks still move text
/// metrics, so a re-render on different hardware may churn measured
/// coordinates; that churn is confined to commits that edit a
/// diagram. Byte-stability across machines would need a pinned
/// render container, which this mechanism deliberately omits.
#[test]
#[ignore = "runs mmdc; invoke via cargo surfpool-render-startup-diagrams"]
fn render_the_startup_diagrams() {
    for (name, source) in diagram_sources() {
        let body: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n");
        let input = std::env::temp_dir().join(format!("{name}.mmd"));
        let rendered = std::env::temp_dir().join(format!("{name}.svg"));
        let config = std::env::temp_dir().join(format!("{name}.mermaid.json"));
        std::fs::write(&input, &body).expect("write the mermaid source");
        // Deterministic ids, seeded by the diagram name: mermaid
        // otherwise embeds a random token in every render, and a
        // re-render with an unchanged source would dirty the tree.
        std::fs::write(
            &config,
            format!(r#"{{"deterministicIds": true, "deterministicIDSeed": "{name}"}}"#),
        )
        .expect("write the mermaid config");

        let status = std::process::Command::new("mmdc")
            .arg("-i")
            .arg(&input)
            .arg("-o")
            .arg(&rendered)
            .arg("-c")
            .arg(&config)
            .status()
            .expect("mmdc should be installed: npm i -g @mermaid-js/mermaid-cli");
        assert!(status.success(), "mmdc failed for {name}");

        let svg = std::fs::read_to_string(&rendered).expect("read the rendered svg");
        // mmdc can prepend an XML declaration; rustdoc wants raw <svg>.
        let svg = svg.trim_start_matches(|c| c != '<');
        let svg = if svg.starts_with("<?xml") {
            &svg[svg.find("?>").map(|i| i + 2).unwrap_or(0)..]
        } else {
            svg
        };
        // The deterministicIds config misses the internal ids of
        // composite states, which carry a fresh random token on every
        // render; normalize them so an unchanged source re-renders
        // byte-identically and never dirties the tree.
        let svg = normalize_state_ids(svg);
        let svg = svg.as_str();
        let path = diagram_svg_path(&name);
        std::fs::create_dir_all(
            std::path::Path::new(&path)
                .parent()
                .expect("the diagrams directory"),
        )
        .expect("create the diagrams directory");
        std::fs::write(
            &path,
            format!(
                "<!-- mermaid-fnv1a: {:016x} -->\n{}",
                fnv1a(&source),
                svg.trim_start()
            ),
        )
        .expect("write the pinned svg");
        eprintln!("rendered {path}");
    }
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
        "a legacy client reads this response as startup finished"
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
