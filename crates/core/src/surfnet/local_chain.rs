//! Surfpool's normal chain policy over the local VM: the clock, blockhash queue, slot hashes,
//! epoch schedule and housekeeping that a normal surfnet's block production, time travel,
//! datasource alignment and reset apply to LiteSVM. Surfpool decides what and when from its own
//! chain state (chain tip, genesis slot, epoch info); the local backend holds the VM. This is the
//! only module with mutable implementation access to the local backend (`Backend::local_mut`);
//! on a historical fork every operation here is refused with a typed error.
//! It owns no store, index or event: those stay in `SurfnetSvm`'s workflows.

use agave_feature_set::FeatureSet;
use solana_clock::Clock;
use solana_epoch_schedule::EpochSchedule;
use solana_slot_hashes::MAX_ENTRIES as MAX_SLOT_HASHES_ENTRIES;
use solana_sysvar::last_restart_slot::LastRestartSlot;

use super::{
    BlockIdentifier, FINALIZATION_SLOT_THRESHOLD,
    backend::LocalBackend,
    svm::{MAX_RECENT_BLOCKHASHES_STANDARD, SurfnetSvm, synthetic_blockhash_for_slot},
};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    types::SyntheticBlockhash,
};

impl SurfnetSvm {
    /// The local VM this policy acts on. A historical fork refuses it: its clock, blockhashes and
    /// sysvars move only inside SVM Replay.
    fn local_vm(&mut self) -> SurfpoolResult<&mut LocalBackend> {
        self.inner.local_mut().ok_or_else(|| {
            SurfpoolError::historical(
                "UNSUPPORTED_IN_HISTORICAL_MODE",
                "normal chain policy on a historical fork: its clock, blockhashes and sysvars move only inside SVM Replay",
                None,
            )
        })
    }

    /// Reconstructs RecentBlockhashes, SlotHashes, and Clock sysvars deterministically
    /// from the current slot. Called on startup and after garbage collection to ensure
    /// consistent sysvar state without requiring database persistence.
    ///
    /// Note: SyntheticBlockhash uses chain_tip.index (relative index), while SlotHashes
    /// and Clock use absolute slots (chain_tip.index + genesis_slot).
    #[allow(deprecated)]
    pub fn reconstruct_sysvars(&mut self) -> SurfpoolResult<()> {
        use solana_slot_hashes::SlotHashes;
        use solana_sysvar::recent_blockhashes::{IterItem, RecentBlockhashes};

        let current_index = self.chain_tip.index;
        let current_absolute_slot = self.get_latest_absolute_slot();

        // Calculate range for blockhashes - use relative indices for SyntheticBlockhash
        let start_index = current_index.saturating_sub(MAX_RECENT_BLOCKHASHES_STANDARD as u64 - 1);

        // Generate all synthetic blockhashes using relative indices (chain_tip.index style)
        // This matches how new_blockhash() generates hashes
        let synthetic_hashes: Vec<_> = (start_index..=current_index)
            .rev()
            .map(SyntheticBlockhash::new)
            .collect();

        // 1. Reconstruct RecentBlockhashes (last 150 blockhashes)
        let recent_blockhashes_vec: Vec<_> = synthetic_hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| IterItem(index as u64, hash.hash(), 0))
            .collect();
        let recent_blockhashes = RecentBlockhashes::from_iter(recent_blockhashes_vec);
        self.local_vm()?.set_sysvar(&recent_blockhashes);

        // 2. Reconstruct SlotHashes - maps absolute slots to blockhashes.
        // The local ledger starts at genesis_slot, but finalized commitment is
        // reported as current_slot - FINALIZATION_SLOT_THRESHOLD. During the
        // initial warmup this points before genesis_slot, so seed those
        // synthetic slots into SlotHashes while preserving the local hash
        // mapping for genesis_slot and later.
        let slot_hash_start_index =
            current_index.saturating_sub(MAX_SLOT_HASHES_ENTRIES as u64 - 1);
        let local_start_absolute_slot = slot_hash_start_index + self.genesis_slot;
        let finalized_start_absolute_slot =
            current_absolute_slot.saturating_sub(FINALIZATION_SLOT_THRESHOLD);
        let earliest_retained_slot =
            current_absolute_slot.saturating_sub(MAX_SLOT_HASHES_ENTRIES as u64 - 1);
        let start_absolute_slot = local_start_absolute_slot
            .min(finalized_start_absolute_slot)
            .max(earliest_retained_slot);
        let slot_hashes_vec: Vec<_> = (start_absolute_slot..=current_absolute_slot)
            .rev()
            .map(|slot| {
                (
                    slot,
                    *synthetic_blockhash_for_slot(slot, self.genesis_slot).hash(),
                )
            })
            .collect();
        let slot_hashes = SlotHashes::new(&slot_hashes_vec);
        self.local_vm()?.set_sysvar(&slot_hashes);

        // 3. Reconstruct Clock using absolute slot
        let unix_timestamp = self.calculate_block_time_for_slot(current_absolute_slot) / 1_000;
        let clock = Clock {
            slot: current_absolute_slot,
            epoch: self.latest_epoch_info.epoch,
            unix_timestamp: unix_timestamp as i64,
            epoch_start_timestamp: 0,
            leader_schedule_epoch: 0,
        };
        self.local_vm()?.set_sysvar(&clock);
        Ok(())
    }

    /// Generates and sets a new blockhash, updating the RecentBlockhashes sysvar.
    ///
    /// # Returns
    /// A new `BlockIdentifier` for the updated blockhash.
    #[allow(deprecated)]
    pub(super) fn new_blockhash(&mut self) -> SurfpoolResult<BlockIdentifier> {
        use solana_slot_hashes::SlotHashes;
        use solana_sysvar::recent_blockhashes::{IterItem, RecentBlockhashes};
        // Backup the current block hashes
        let recent_blockhashes_backup = self.local_vm()?.get_sysvar::<RecentBlockhashes>();
        let num_blockhashes_expected = recent_blockhashes_backup
            .len()
            .min(MAX_RECENT_BLOCKHASHES_STANDARD);
        // Invalidate the current block hash.
        // LiteSVM bug / feature: calling this method empties `sysvar::<RecentBlockhashes>()`
        self.local_vm()?.expire_blockhash();
        // Rebuild recent blockhashes
        let mut recent_blockhashes = Vec::with_capacity(num_blockhashes_expected);
        let recent_blockhashes_overriden = self.local_vm()?.get_sysvar::<RecentBlockhashes>();
        let latest_entry = recent_blockhashes_overriden
            .first()
            .expect("Latest blockhash not found");

        let new_synthetic_blockhash = SyntheticBlockhash::new(self.chain_tip.index);
        let new_synthetic_blockhash_str = new_synthetic_blockhash.to_string();

        recent_blockhashes.push(IterItem(
            0,
            new_synthetic_blockhash.hash(),
            latest_entry.fee_calculator.lamports_per_signature,
        ));

        // Append the previous blockhashes, ignoring the first one
        for (index, entry) in recent_blockhashes_backup.iter().enumerate() {
            if recent_blockhashes.len() >= MAX_RECENT_BLOCKHASHES_STANDARD {
                break;
            }
            recent_blockhashes.push(IterItem(
                (index + 1) as u64,
                &entry.blockhash,
                entry.fee_calculator.lamports_per_signature,
            ));
        }

        self.local_vm()?
            .set_sysvar(&RecentBlockhashes::from_iter(recent_blockhashes));

        let mut slot_hashes = self.local_vm()?.get_sysvar::<SlotHashes>();
        slot_hashes.add(
            self.get_latest_absolute_slot() + 1,
            *new_synthetic_blockhash.hash(),
        );
        self.local_vm()?.set_sysvar(&SlotHashes::new(&slot_hashes));

        Ok(BlockIdentifier::new(
            self.chain_tip.index + 1,
            new_synthetic_blockhash_str.as_str(),
        ))
    }

    /// The local VM's own settings (its log limit), from the surfnet's configuration.
    pub(super) fn configure_local_vm(
        &mut self,
        log_bytes_limit: Option<usize>,
    ) -> SurfpoolResult<()> {
        self.local_vm()?.set_log_bytes_limit(log_bytes_limit);
        Ok(())
    }

    /// The surfnet's epoch schedule (its own, or the datasource's) applied to the local VM.
    pub(crate) fn apply_local_epoch_schedule(
        &mut self,
        epoch_schedule: &EpochSchedule,
    ) -> SurfpoolResult<()> {
        self.local_vm()?.set_sysvar(epoch_schedule);
        Ok(())
    }

    /// The clock Surfpool's block production or time travel moved to, applied to the local VM.
    pub(crate) fn apply_local_clock(&mut self, clock: &Clock) -> SurfpoolResult<()> {
        self.local_vm()?.set_sysvar(clock);
        Ok(())
    }

    /// Clears the local VM's account cache, keeping the database and the chain's sysvars.
    pub(super) fn collect_local_garbage(&mut self) -> SurfpoolResult<()> {
        let feature_set: FeatureSet = self.feature_set.clone();
        self.local_vm()?.garbage_collect(feature_set);
        Ok(())
    }

    /// The local VM's LastRestartSlot (the datasource's, once aligned).
    pub(crate) fn local_last_restart_slot(&mut self) -> SurfpoolResult<LastRestartSlot> {
        Ok(self.local_vm()?.get_sysvar::<LastRestartSlot>())
    }

    /// Applies a LastRestartSlot (the datasource's, or one kept across a reset) to the local VM.
    pub(crate) fn apply_local_last_restart_slot(
        &mut self,
        last_restart_slot: &LastRestartSlot,
    ) -> SurfpoolResult<()> {
        self.local_vm()?.set_sysvar(last_restart_slot);
        Ok(())
    }
}
