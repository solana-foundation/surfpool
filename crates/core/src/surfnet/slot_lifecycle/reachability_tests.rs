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
//!    (close a block, warp, reset) establishes the state invariant on which
//!    the contract's Existence and Liveness rest: at every reachable point,
//!    exactly one slot is Announced, and it is the open slot. A second
//!    Announced slot would be an orphan nobody will resolve; an unannounced
//!    open slot would emit untracked data.

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
                    if let Some(root) = open.checked_sub(ROOT_DEPTH) {
                        life.root(root);
                    }
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
            check(open, &life, "an op");
            if seen.insert(snapshot(open, &life)) {
                queue.push_back((open, life));
            }
        }
    }
    assert!(seen.len() > 100, "the grammar explored a real state space");
}
