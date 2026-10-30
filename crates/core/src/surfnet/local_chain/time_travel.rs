//! Normal-chain time travel; historical clocks remain owned by SVM Replay.

use solana_clock::{Clock, Slot};
use solana_epoch_info::EpochInfo;
use solana_slot_hashes::SlotHashes;

use crate::{error::SurfpoolResult, surfnet::svm::SurfnetSvm};

impl SurfnetSvm {
    /// Moves the open slot to `slot` for a time travel, and records the time `updated_at` holds
    /// there, even if `slot` is already open. Moving forward skips the slots in between, as a
    /// leader's missed slots are: the block at `slot` is built on the last one, even if the open
    /// slot was itself a landing.
    pub(crate) fn travel_to(&mut self, slot: Slot) -> SurfpoolResult<()> {
        self.local_vm()?;
        let open = self.get_latest_absolute_slot();
        // The block `slot` is built on: the last one before the open slot, or before `slot` if
        // it travels back.
        let parent = self.block_at_or_before(slot.min(open).saturating_sub(1));
        // A landing or restart at the open slot is replaced; one past a backward travel never
        // happened. Write the store before memory, so a failed write leaves the chain in memory
        // as it was; the store may then have dropped stale landings and restarts already.
        for &stale in self.landings.range(slot.min(open)..).map(|(s, _)| s) {
            if stale != slot {
                self.landings_db.take(&stale)?;
            }
        }
        for &stale in self.restarts.range(slot.min(open)..).map(|(s, _)| s) {
            self.restarts_db.take(&stale)?;
        }
        self.landings_db.store(slot, (parent, self.updated_at))?;
        self.landings.split_off(&slot.min(open));
        self.restarts.split_off(&slot.min(open));
        self.landings.insert(slot, (parent, self.updated_at));
        self.set_latest_absolute_slot(slot);
        // Announce the slot it lands on, as its bank is created; a skipped slot gets none.
        if slot > open {
            // The open slot's SlotHashes entry moves with it: the skipped slot holds none.
            let slot_hashes: Vec<_> = self
                .local_vm()?
                .get_sysvar::<SlotHashes>()
                .iter()
                .map(|&(key, hash)| (if key == open { slot } else { key }, hash))
                .collect();
            self.local_vm()?.set_sysvar(&SlotHashes::new(&slot_hashes));
            self.announce_open_slot()?;
        }
        Ok(())
    }

    /// Applies a time travel's `clock`: records its landing, then moves the Clock, the time and
    /// the open slot to it. A slot already closed, as the one open when the travel was asked for
    /// is by now, lands on the open slot: only its time changes. If the landing can't be
    /// recorded, nothing changes in memory.
    pub(crate) fn apply_time_travel(&mut self, clock: Clock) -> SurfpoolResult<EpochInfo> {
        self.local_vm()?;
        let previous = std::mem::replace(&mut self.updated_at, clock.unix_timestamp as u64 * 1_000);
        if let Err(e) = self.travel_to(clock.slot.max(self.get_latest_absolute_slot())) {
            self.updated_at = previous;
            return Err(e);
        }
        let clock = Clock {
            slot: self.latest_epoch_info.absolute_slot,
            epoch: self.latest_epoch_info.epoch,
            ..clock
        };
        self.apply_local_clock(&clock)?;
        self.simnet_events_tx.system_clock_updated(clock);
        Ok(self.latest_epoch_info.clone())
    }
}
