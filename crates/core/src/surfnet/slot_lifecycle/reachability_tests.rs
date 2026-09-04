//! Complete check of the slot registry against its spec, in the startup state
//! machine's style (`types/src/startup`): the reachable state space over a
//! bounded slot domain is small, so a depth-first sweep can hold the machine
//! to the spec at every reachable state and across every event, with no
//! sampling involved.
//!
//! Two sweeps, two claims:
//!
//! 1. State-machine equivalence over the full alphabet: from every reachable
//!    registry state, every event produces exactly the emissions and successor
//!    registry the spec's table names. This discharges
//!    implementation-refines-model outright for the sequential core: the model
//!    is the spec module, and the machine is equal to it, not merely contained
//!    in it. Per-slot status ordering follows: the spec's cells emit statuses
//!    only from their legal predecessors, and every machine emission matches a
//!    spec cell.
//!
//! 2. Production's grammar: a driver that calls the registry as the SVM does
//!    (close a block, warp, reset) establishes the state invariant the
//!    emission guarantees rest on: at every reachable point, exactly one
//!    slot is Announced, and it is the open slot. A second Announced slot
//!    would be an orphan nobody will resolve; an unannounced open slot
//!    would emit untracked data.

use std::collections::{HashSet, VecDeque};

use agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus;

use super::{
    SlotEmission, SlotLifecycle,
    spec::{self, Event, Status, View},
};

/// The bounded slot domain for the full-alphabet sweep.
const SLOTS: u64 = 5;

fn view_of(life: &SlotLifecycle, domain: u64) -> View {
    (0..domain)
        .filter_map(|slot| life.stage(slot).map(|stage| (slot, stage)))
        .collect()
}

fn as_spec(emissions: &[SlotEmission]) -> Vec<(u64, Status)> {
    emissions
        .iter()
        .map(|emission| {
            let status = match &emission.status {
                SlotStatus::CreatedBank => Status::Created,
                SlotStatus::Processed => Status::Processed,
                SlotStatus::Confirmed => Status::Confirmed,
                SlotStatus::Rooted => Status::Rooted,
                SlotStatus::Dead(_) => Status::Dead,
                other => panic!("the registry never emits {other:?}"),
            };
            (emission.slot, status)
        })
        .collect()
}

fn drive(life: &mut SlotLifecycle, event: &Event) -> Vec<SlotEmission> {
    match event {
        Event::Announce(slot) => life.announce(*slot),
        Event::Produce(slot) => life.produce(*slot),
        Event::Confirm(slot) => life.confirm(*slot),
        Event::Root(slot) => life.root(*slot),
        Event::RootThrough(threshold) => life.root_through(*threshold),
        Event::Warp { from, to } => life.warp(*from, *to),
        Event::Clear => {
            life.clear();
            vec![]
        }
    }
}

fn alphabet() -> Vec<Event> {
    let mut events = vec![Event::Clear];
    for slot in 0..SLOTS {
        events.push(Event::Announce(slot));
        events.push(Event::Produce(slot));
        events.push(Event::Confirm(slot));
        events.push(Event::Root(slot));
        events.push(Event::RootThrough(slot));
    }
    for from in 0..SLOTS {
        for to in 0..SLOTS {
            events.push(Event::Warp { from, to });
        }
    }
    events
}

/// Rebuilds a registry whose live slots match `view`. The machine has
/// no bulk constructor, so the sweep replays each slot's stage through
/// the public transitions.
fn registry_of(view: &View) -> SlotLifecycle {
    let mut life = SlotLifecycle::default();
    for (slot, stage) in view {
        life.announce(*slot);
        if *stage >= super::SlotStage::Processed {
            life.produce(*slot);
        }
        if *stage >= super::SlotStage::Confirmed {
            life.confirm(*slot);
        }
    }
    life
}

#[test]
fn the_machine_and_the_spec_are_the_same_table() {
    let events = alphabet();
    let mut seen: HashSet<Vec<Option<super::SlotStage>>> = HashSet::new();
    let mut queue: VecDeque<View> = VecDeque::from([View::new()]);
    let key = |view: &View| (0..SLOTS).map(|slot| view.get(&slot).copied()).collect();
    seen.insert(key(&View::new()));

    let mut states = 0u64;
    let mut transitions = 0u64;
    while let Some(view) = queue.pop_front() {
        states += 1;
        for event in &events {
            let mut life = registry_of(&view);
            assert_eq!(view_of(&life, SLOTS), view, "the rebuild is faithful");
            let emissions = drive(&mut life, event);
            assert_eq!(
                as_spec(&emissions),
                spec::expected_emissions(&view, event),
                "emissions for {event:?} from {view:?}"
            );
            let next = view_of(&life, SLOTS);
            assert_eq!(
                next,
                spec::expected_view(&view, event),
                "successor for {event:?} from {view:?}"
            );
            transitions += 1;
            if seen.insert(key(&next)) {
                queue.push_back(next);
            }
        }
    }
    // 4 stages (absent included) over the domain: the sweep must have
    // visited every combination, or the alphabet cannot express some
    // state and the claim above quietly shrank.
    assert_eq!(states, 4u64.pow(SLOTS as u32), "all states reachable");
    assert!(transitions == states * events.len() as u64);
}

/// The SVM's call grammar. `close_block` mirrors `confirm_current_block`
/// (produce and confirm the open slot, root what is due, announce the
/// next slot); `warp` mirrors `warp_clock` plus `warp_slot_lifecycle`;
/// `reset` mirrors `reset_network` (clear, then announce the open
/// slot). The rooting depth is 2 rather than production's 31 so roots
/// occur inside a small sweep; the invariant does not mention the
/// constant.
const ROOT_DEPTH: u64 = 2;
const MAX_SLOT: u64 = 7;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Driver {
    open: u64,
    registry: Vec<Option<super::SlotStage>>,
}

#[test]
fn production_grammar_keeps_exactly_the_open_slot_announced() {
    #[derive(Clone, Copy)]
    enum Op {
        CloseBlock,
        Warp(u64),
        Reset,
    }
    let ops: Vec<Op> = {
        let mut ops = vec![Op::CloseBlock, Op::Reset];
        ops.extend((0..=MAX_SLOT).map(Op::Warp));
        ops
    };

    let snapshot = |open: u64, life: &SlotLifecycle| Driver {
        open,
        registry: (0..=MAX_SLOT).map(|slot| life.stage(slot)).collect(),
    };
    let check = |open: u64, life: &SlotLifecycle, what: &str| {
        let announced: Vec<u64> = (0..=MAX_SLOT)
            .filter(|slot| life.stage(*slot) == Some(super::SlotStage::Announced))
            .collect();
        assert_eq!(
            announced,
            vec![open],
            "after {what}: the one Announced slot is the open one"
        );
    };

    let mut initial = SlotLifecycle::default();
    initial.announce(0);
    check(0, &initial, "startup");

    let mut seen: HashSet<Driver> = HashSet::new();
    let mut queue: VecDeque<(u64, SlotLifecycle)> = VecDeque::from([(0, initial.clone())]);
    seen.insert(snapshot(0, &initial));

    while let Some((open, life)) = queue.pop_front() {
        for op in &ops {
            let mut life = life.clone();
            let open = match op {
                Op::CloseBlock => {
                    if open >= MAX_SLOT {
                        continue;
                    }
                    life.produce(open);
                    life.confirm(open);
                    life.root_through((open + 1).saturating_sub(ROOT_DEPTH));
                    life.announce(open + 1);
                    open + 1
                }
                Op::Warp(to) => {
                    life.warp(open, *to);
                    *to
                }
                Op::Reset => {
                    life.clear();
                    life.announce(open);
                    open
                }
            };
            let what = match op {
                Op::CloseBlock => "close_block",
                Op::Warp(_) => "a warp",
                Op::Reset => "a reset",
            };
            check(open, &life, what);
            if seen.insert(snapshot(open, &life)) {
                queue.push_back((open, life));
            }
        }
    }
    // `root_through` keeps the steady-state registry at two slots (the
    // confirmed tip and the open slot), so the space is linear in
    // MAX_SLOT rather than combinatorial; the bound guards against the
    // grammar collapsing, not against tight rooting.
    assert!(
        seen.len() > 2 * MAX_SLOT as usize,
        "the grammar explored a real state space"
    );
}

/// The generated blocks of `slot-lifecycle.md`, named by their markers.
fn generated_blocks() -> Vec<(&'static str, String)> {
    vec![
        ("per-slot-table", spec::render_per_slot_table()),
        ("diagram", spec::render_diagram()),
    ]
}

const SPEC_DOC_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/surfnet/slot-lifecycle.md");

fn read_spec_doc() -> String {
    std::fs::read_to_string(SPEC_DOC_PATH)
        .unwrap_or_else(|error| panic!("could not read {SPEC_DOC_PATH}: {error}"))
}

/// The character range between a block's markers, exclusive of both.
fn region(text: &str, name: &str) -> (usize, usize) {
    let begin = format!("<!-- BEGIN GENERATED: {name} -->\n");
    let end = format!("<!-- END GENERATED: {name} -->");
    let start = text
        .find(&begin)
        .unwrap_or_else(|| panic!("{SPEC_DOC_PATH} has no {begin:?} marker"))
        + begin.len();
    let stop = text[start..]
        .find(&end)
        .unwrap_or_else(|| panic!("{SPEC_DOC_PATH} has no {end:?} marker"))
        + start;
    (start, stop)
}

#[test]
fn the_spec_document_is_current() {
    let text = read_spec_doc();
    for (name, content) in generated_blocks() {
        let (start, stop) = region(&text, name);
        assert_eq!(
            &text[start..stop],
            content,
            "the {name} block in slot-lifecycle.md disagrees with the spec; \
             run `cargo surfpool-update-slot-spec` and review the diff"
        );
    }
}

#[test]
#[ignore = "writes slot-lifecycle.md; run via cargo surfpool-update-slot-spec"]
fn regenerate_the_slot_spec_tables() {
    let mut text = read_spec_doc();
    for (name, content) in generated_blocks() {
        let (start, stop) = region(&text, name);
        text.replace_range(start..stop, &content);
    }
    std::fs::write(SPEC_DOC_PATH, text)
        .unwrap_or_else(|error| panic!("could not write {SPEC_DOC_PATH}: {error}"));
    eprintln!("regenerated the spec tables in {SPEC_DOC_PATH}");
}

// The diagram pipeline below mirrors the startup spec's
// (`types/src/startup/surfnet_startup_reachability_tests.rs`); the
// helpers are duplicated because they live in that crate's test-only
// module and exporting them would put test scaffolding in the library.

/// Every mermaid region of `slot-lifecycle.md`, as (name, fenced source).
fn diagram_sources() -> Vec<(String, String)> {
    let text = read_spec_doc();
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
    format!(
        "{}/src/surfnet/diagrams/{name}.svg",
        env!("CARGO_MANIFEST_DIR")
    )
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
                 surfpool-render-slot-diagrams`"
            )
        });
        let expected = format!("<!-- mermaid-fnv1a: {:016x} -->", fnv1a(&source));
        assert!(
            svg.starts_with(&expected),
            "{name}: the rendered SVG is stale; run `cargo \
             surfpool-render-slot-diagrams`"
        );
    }
}

/// Renders each mermaid region to `src/surfnet/diagrams/<name>.svg`
/// and pins the source hash. Ignored so a plain test run never needs
/// the mermaid CLI; `cargo surfpool-render-slot-diagrams` runs it.
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
#[ignore = "runs mmdc; invoke via cargo surfpool-render-slot-diagrams"]
fn render_the_slot_diagrams() {
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
        //
        // htmlLabels off, everywhere: HTML labels sit in foreignObject
        // boxes that clip at their measured edge, and SVG text
        // overflows visibly instead. State diagrams read the flowchart
        // key for edge labels, so all three keys are needed.
        //
        // The font stack must carry no quotes: rustdoc's markdown
        // pipeline applies smart punctuation to the text inside the
        // inlined SVG's style block, so a quoted "trebuchet ms"
        // arrives as a curly-quoted unknown font and the browser falls
        // back to a wider face than the one mmdc measured, clipping
        // every label. Quote-free names survive, and mmdc then
        // measures the same face the browser renders.
        std::fs::write(
            &config,
            format!(
                r#"{{"deterministicIds": true, "deterministicIDSeed": "{name}",
                     "htmlLabels": false, "state": {{"htmlLabels": false}},
                     "flowchart": {{"htmlLabels": false}},
                     "themeVariables": {{"fontFamily": "verdana, arial, sans-serif"}}}}"#
            ),
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
