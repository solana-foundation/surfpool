//! The spec: the slot table's rules stated on their own, as a second
//! encoding the reachability sweep holds the machine to.
//!
//! The principle, from the startup state machine (types/src/startup/
//! spec.rs): spec and implementation must be different encodings of the
//! same rules, because the sweep proves the machine agrees with this
//! module, and that proof is empty the moment the two share code.
//! Everything here is written from the slot table (one arm per cell)
//! and reads the machine only through its public accessors.
//!
//! Maintenance procedure for changing a state, event, or transition:
//!
//! 1. State the new cell here first, in the table's vocabulary.
//! 2. Change the machine to satisfy it.
//! 3. `cargo test -p surfpool-core --lib slot_lifecycle` fails while
//!    the two disagree, naming the first state and event where they
//!    part.
//! 4. Update the table in the notes (and the Promela model, whose
//!    process bodies are this table transcribed) as the observable
//!    change.

use std::collections::BTreeMap;

use solana_clock::Slot;

use super::SlotStage;

/// The events the registry reacts to; the sweep drives every one from
/// every reachable state.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    Announce(Slot),
    Produce(Slot),
    Confirm(Slot),
    Root(Slot),
    Warp { from: Slot, to: Slot },
    Clear,
}

/// A status in spec vocabulary, so comparisons read as the table does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Created,
    Processed,
    Confirmed,
    Rooted,
    Dead,
}

pub(crate) type View = BTreeMap<Slot, SlotStage>;

/// The emissions the table's cell for (view, event) requires, in order.
pub(crate) fn expected_emissions(view: &View, event: &Event) -> Vec<(Slot, Status)> {
    match event {
        Event::Announce(slot) => match view.get(slot) {
            None => vec![(*slot, Status::Created)],
            Some(_) => vec![],
        },
        Event::Produce(slot) => match view.get(slot) {
            None => vec![(*slot, Status::Created), (*slot, Status::Processed)],
            Some(SlotStage::Announced) => vec![(*slot, Status::Processed)],
            Some(_) => vec![],
        },
        Event::Confirm(slot) => match view.get(slot) {
            Some(SlotStage::Processed) => vec![(*slot, Status::Confirmed)],
            _ => vec![],
        },
        Event::Root(slot) => match view.get(slot) {
            Some(SlotStage::Confirmed) => vec![(*slot, Status::Rooted)],
            _ => vec![],
        },
        Event::Warp { from, to } => {
            let mut out = vec![];
            let killed = from != to && view.get(from) == Some(&SlotStage::Announced);
            if killed {
                out.push((*from, Status::Dead));
            }
            // The destination is announced exactly when it is not on
            // record once the kill and, for a backward warp, the
            // forgetting of every slot at or past the destination have
            // taken effect.
            let mut interim = view.clone();
            if killed {
                interim.remove(from);
            }
            if to < from {
                interim.retain(|slot, _| slot < to);
            }
            if !interim.contains_key(to) {
                out.push((*to, Status::Created));
            }
            out
        }
        Event::Clear => vec![],
    }
}

/// The registry the table's cell for (view, event) leaves behind.
pub(crate) fn expected_view(view: &View, event: &Event) -> View {
    let mut next = view.clone();
    match event {
        Event::Announce(slot) => {
            next.entry(*slot).or_insert(SlotStage::Announced);
        }
        Event::Produce(slot) => match next.get(slot) {
            None | Some(SlotStage::Announced) => {
                next.insert(*slot, SlotStage::Processed);
            }
            Some(_) => {}
        },
        Event::Confirm(slot) => {
            if next.get(slot) == Some(&SlotStage::Processed) {
                next.insert(*slot, SlotStage::Confirmed);
            }
        }
        Event::Root(slot) => {
            if next.get(slot) == Some(&SlotStage::Confirmed) {
                next.remove(slot);
            }
        }
        Event::Warp { from, to } => {
            if from != to && next.get(from) == Some(&SlotStage::Announced) {
                next.remove(from);
            }
            if to < from {
                next.retain(|slot, _| slot < to);
            }
            next.entry(*to).or_insert(SlotStage::Announced);
        }
        Event::Clear => next.clear(),
    }
    next
}
