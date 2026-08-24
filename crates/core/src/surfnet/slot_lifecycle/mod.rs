//! The per-slot lifecycle every geyser slot-status emission derives from.
//!
#![doc = include_str!("../slot-lifecycle.md")]

use std::collections::HashMap;

use agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus;
use solana_clock::Slot;

#[cfg(test)]
mod reachability_tests;
#[cfg(test)]
pub(crate) mod spec;

/// A slot's recorded stage. `Rooted` and `Dead` are terminal and are
/// forgotten once emitted, so the registry holds only live slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum SlotStage {
    /// `CreatedBank` has been emitted; the slot awaits its block.
    Announced,
    /// The slot's block was produced (`Processed` emitted).
    Processed,
    /// The slot's block was confirmed (`Confirmed` emitted).
    Confirmed,
}

/// One slot-status emission a transition produced, in emission order.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotEmission {
    pub slot: Slot,
    pub parent: Option<Slot>,
    pub status: SlotStatus,
}

fn emission(slot: Slot, status: SlotStatus) -> SlotEmission {
    SlotEmission {
        slot,
        parent: slot.checked_sub(1),
        status,
    }
}

/// The registry of live slots and their stages. The [module
/// documentation](self) carries the full state table and the
/// transition diagram.
#[derive(Debug, Clone, Default)]
pub struct SlotLifecycle {
    stages: HashMap<Slot, SlotStage>,
}

impl SlotLifecycle {
    /// Announces a slot. A slot already on record is left alone, so the
    /// two announcers (startup for genesis, block production for N+1)
    /// and a warp landing on an announced slot cannot double-announce.
    pub fn announce(&mut self, slot: Slot) -> Vec<SlotEmission> {
        if self.stages.contains_key(&slot) {
            return vec![];
        }
        self.stages.insert(slot, SlotStage::Announced);
        vec![emission(slot, SlotStatus::CreatedBank)]
    }

    /// The slot's block was produced. Called after the slot's block data
    /// has been emitted, which is the data-before-confirmation order the
    /// contract asks for. An unannounced slot is announced first, so no
    /// data-carrying slot can go unannounced even from a path that forgot.
    pub fn produce(&mut self, slot: Slot) -> Vec<SlotEmission> {
        let mut out = self.announce(slot);
        if self.advance(slot, SlotStage::Announced, SlotStage::Processed) {
            out.push(emission(slot, SlotStatus::Processed));
        }
        out
    }

    /// The slot's block was confirmed.
    pub fn confirm(&mut self, slot: Slot) -> Vec<SlotEmission> {
        if self.advance(slot, SlotStage::Processed, SlotStage::Confirmed) {
            vec![emission(slot, SlotStatus::Confirmed)]
        } else {
            vec![]
        }
    }

    /// The slot was rooted; it leaves the registry.
    pub fn root(&mut self, slot: Slot) -> Vec<SlotEmission> {
        match self.stages.remove(&slot) {
            Some(SlotStage::Confirmed) => vec![emission(slot, SlotStatus::Rooted)],
            Some(stage) => {
                // Rooting a slot that never confirmed would skip a status;
                // keep the record and emit nothing.
                self.stages.insert(slot, stage);
                vec![]
            }
            None => vec![],
        }
    }

    /// A clock warp from the open slot `from` to `to`. The open slot was
    /// announced and never produced, so it dies unless the warp lands on
    /// it; slots at or past `to` that the old timeline had produced are
    /// forgotten, since the new timeline rewrites them; the destination
    /// is announced if it is not already.
    pub fn warp(&mut self, from: Slot, to: Slot) -> Vec<SlotEmission> {
        let mut out = vec![];
        if from != to && self.stages.get(&from) == Some(&SlotStage::Announced) {
            self.stages.remove(&from);
            out.push(emission(
                from,
                SlotStatus::Dead(format!("abandoned by a clock warp to slot {to}")),
            ));
        }
        if to < from {
            self.stages.retain(|slot, _| *slot < to);
        }
        out.extend(self.announce(to));
        out
    }

    /// The recorded stage of a slot, if it is live. The spec and the
    /// reachability sweep read the registry only through this accessor.
    #[cfg(test)]
    pub(crate) fn stage(&self, slot: Slot) -> Option<SlotStage> {
        self.stages.get(&slot).copied()
    }

    /// Forgets every slot (a network reset).
    pub fn clear(&mut self) {
        self.stages.clear();
    }

    /// Moves a slot from exactly `from` to `to`. Any other recorded stage
    /// leaves the slot alone: a status cannot be skipped and cannot repeat.
    fn advance(&mut self, slot: Slot, from: SlotStage, to: SlotStage) -> bool {
        match self.stages.get_mut(&slot) {
            Some(stage) if *stage == from => {
                *stage = to;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The slot table as the oracle: one test per deciding cell.

    use super::*;

    fn statuses(emissions: &[SlotEmission]) -> Vec<(Slot, String)> {
        emissions
            .iter()
            .map(|e| (e.slot, format!("{:?}", e.status)))
            .collect()
    }

    #[test]
    fn a_slot_advances_in_order_and_each_status_emits_once() {
        let mut life = SlotLifecycle::default();
        assert_eq!(statuses(&life.announce(7)), vec![(7, "CreatedBank".into())]);
        assert!(life.announce(7).is_empty(), "announced at most once");
        assert_eq!(statuses(&life.produce(7)), vec![(7, "Processed".into())]);
        assert!(life.produce(7).is_empty(), "processed at most once");
        assert_eq!(statuses(&life.confirm(7)), vec![(7, "Confirmed".into())]);
        assert_eq!(statuses(&life.root(7)), vec![(7, "Rooted".into())]);
        assert!(life.root(7).is_empty(), "rooted slots are forgotten");
    }

    #[test]
    fn statuses_cannot_be_skipped() {
        let mut life = SlotLifecycle::default();
        life.announce(3);
        assert!(
            life.confirm(3).is_empty(),
            "confirm before produce is ignored"
        );
        assert!(life.root(3).is_empty(), "root before confirm is ignored");
        assert_eq!(statuses(&life.produce(3)), vec![(3, "Processed".into())]);
    }

    #[test]
    fn producing_an_unannounced_slot_announces_it_first() {
        let mut life = SlotLifecycle::default();
        assert_eq!(
            statuses(&life.produce(4)),
            vec![(4, "CreatedBank".into()), (4, "Processed".into())]
        );
    }

    #[test]
    fn a_forward_warp_kills_the_orphan_and_announces_the_destination() {
        // The open slot 3 was announced by closing slot 2 and never
        // produced; the warp lands on 9.
        let mut life = SlotLifecycle::default();
        life.announce(3);
        let out = life.warp(3, 9);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].slot, 3);
        assert!(matches!(out[0].status, SlotStatus::Dead(_)));
        assert_eq!(statuses(&out[1..]), vec![(9, "CreatedBank".into())]);
        assert!(life.announce(9).is_empty(), "the destination is on record");
    }

    #[test]
    fn a_warp_landing_on_the_open_slot_changes_nothing() {
        // An unconditional announce here would double-announce.
        let mut life = SlotLifecycle::default();
        life.announce(3);
        assert!(life.warp(3, 3).is_empty());
    }

    #[test]
    fn a_backward_warp_forgets_the_rewritten_slots() {
        let mut life = SlotLifecycle::default();
        for slot in 5..8 {
            life.announce(slot);
            life.produce(slot);
            life.confirm(slot);
        }
        life.announce(8);
        let out = life.warp(8, 6);
        assert!(matches!(out[0].status, SlotStatus::Dead(_)));
        assert_eq!(statuses(&out[1..]), vec![(6, "CreatedBank".into())]);
        assert!(
            life.announce(7).len() == 1,
            "slot 7 was forgotten with the old timeline"
        );
        assert!(life.announce(5).is_empty(), "slot 5 stays on record");
    }

    #[test]
    fn a_reset_forgets_everything() {
        let mut life = SlotLifecycle::default();
        life.announce(1);
        life.clear();
        assert_eq!(life.announce(1).len(), 1);
    }
}
