//! Surfpool's chain policy for a historical fork: the boundary block and the fork's open and
//! completed blocks, confirmations (immediate finality in frozen slots), performance samples,
//! the journal/store-receipt protocol that lets a persistent store reopen its fork, and the
//! projection of SVM Replay's advances and resets onto Surfpool's chain state. SVM Replay owns the
//! historical state, clock, blockhash age and admission; this module owns no VM and reaches the
//! fork only through the backend's typed operations. Stores, events and subscriptions stay in
//! `SurfnetSvm`, as for `local_chain`.

use std::{num::NonZeroU64, str::FromStr};

use chrono::Utc;
use solana_account::Account;
use solana_clock::Slot;
use solana_epoch_info::EpochInfo;
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::{RpcPerfSample, SlotTransactionStats, SlotUpdate};
use solana_signature::Signature;

use super::{
    BlockHeader, BlockIdentifier, GeyserEvent, GeyserSlotStatus,
    backend::HistoricalSession,
    svm::{
        HISTORICAL_JOURNAL_CHECKPOINT, HISTORICAL_REVISION_CHECKPOINT, HISTORICAL_SLOT_CHECKPOINT,
        SupplyOverrides, SurfnetSvm,
    },
};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    types::SurfnetTransactionStatus,
};

/// Completes a future that never waits here: scheduled overrides on a historical fork,
/// which has no datasource to fetch from.
fn complete_without_datasource<T>(
    future: impl std::future::Future<Output = SurfpoolResult<T>>,
) -> SurfpoolResult<T> {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => Err(SurfpoolError::internal(
            "scheduled overrides waited on a datasource a historical fork does not have",
        )),
    }
}

impl SurfnetSvm {
    /// A historical fork's status for a local transaction: processed while its frozen slot's
    /// block has not confirmed it, then finalized. `None` on a local surfnet, where slot
    /// distance decides.
    pub fn historical_confirmation(
        &self,
        signature: &Signature,
    ) -> Option<solana_transaction_status::TransactionConfirmationStatus> {
        use solana_transaction_status::TransactionConfirmationStatus;
        if !self.inner.is_historical() {
            return None;
        }
        let queued = self
            .transactions_queued_for_confirmation
            .iter()
            .any(|(tx, ..)| tx.signatures.first() == Some(signature));
        Some(if queued {
            TransactionConfirmationStatus::Processed
        } else {
            TransactionConfirmationStatus::Finalized
        })
    }

    /// Writes accounts on a historical fork as one atomic change (a program with its program
    /// data): SVM Replay applies all or none, then Surfpool's indexes and subscribers see
    /// each, as with `set_account`.
    pub fn set_historical_accounts(
        &mut self,
        accounts: Vec<(Pubkey, Account)>,
    ) -> SurfpoolResult<()> {
        let fork = self.inner.historical_fork().cloned().ok_or_else(|| {
            SurfpoolError::internal("set_historical_accounts needs a historical fork")
        })?;
        let befores = accounts
            .iter()
            .map(|(pubkey, _)| Ok(fork.peek(pubkey)?.flatten()))
            .collect::<SurfpoolResult<Vec<_>>>()?;
        fork.set_accounts(&accounts)?;
        let slot = self.get_latest_absolute_slot();
        for ((pubkey, account), before) in accounts.iter().zip(befores) {
            self.account_update_slots.insert(*pubkey, slot);
            self.update_account_registries(pubkey, before.as_ref(), account)?;
            self.notify_account_subscribers(pubkey, account);
            self.notify_program_subscribers(pubkey, account);
            let _ = self.simnet_events_tx.account_update(*pubkey);
        }
        Ok(())
    }

    /// Starts a historical fork: SVM Replay's branch becomes the executing VM, and the slot,
    /// block height, blockhash, clock and network identity come from it, never from local
    /// defaults. Surfpool's own stores and indexes start empty at the boundary.
    pub(crate) fn initialize_historical(&mut self) -> SurfpoolResult<()> {
        let fork = self.inner.historical_fork().ok_or_else(|| {
            SurfpoolError::internal("initialize_historical needs a historical fork")
        })?;
        let position = fork.state()?;
        let (blockhash, _) = fork.latest_blockhash()?;
        let genesis_hash = fork.session().pins().genesis_hash;
        let (slot, block_height) = (position.slot, position.block_height);
        let clock = self.inner.clock();
        let epoch_schedule = self.inner.epoch_schedule();
        let (epoch, slot_index) = epoch_schedule.get_epoch_and_slot_index(slot);
        self.latest_epoch_info = EpochInfo {
            epoch,
            slot_index,
            slots_in_epoch: epoch_schedule.get_slots_in_epoch(epoch),
            absolute_slot: slot,
            block_height,
            transaction_count: None,
        };
        self.chain_tip = BlockIdentifier::new(block_height, &blockhash.to_string());
        self.updated_at = (clock.unix_timestamp.max(0) as u64) * 1_000;
        self.genesis_updated_at = self.updated_at;
        self.cached_genesis_hash = Some(genesis_hash);
        self.historical_blockhashes.clear();
        // A store with history reopens its fork, which SVM Replay restored from the journal.
        let has_history = self
            .slot_checkpoint
            .get(&HISTORICAL_SLOT_CHECKPOINT.to_string())?
            .is_some();
        match has_history {
            true => self.restore_historical_blocks(slot),
            false => self.open_historical_boundary_block(
                slot,
                block_height,
                blockhash,
                clock.unix_timestamp,
            ),
        }
    }

    /// The blocks of a restored fork, as this store kept them: the store must be at the
    /// restored slot, have seen no more of the journal than SVM Replay replayed, and hold the
    /// fork's completed blocks and the block it left open; otherwise it is not this fork's.
    fn restore_historical_blocks(&mut self, slot: Slot) -> SurfpoolResult<()> {
        let backend = self
            .inner
            .historical_session()
            .ok_or_else(|| SurfpoolError::internal("a historical block needs its fork"))?;
        let integrity = |what: &str| {
            SurfpoolError::historical(
                "SESSION_INTEGRITY",
                &format!("the surfnet's store and its restored historical fork disagree: {what}"),
                None,
            )
        };
        let pins = backend.pins();
        let (boundary, position) = (pins.slot, u64::from(pins.transaction_index));
        let checkpoint = |key: &str| self.slot_checkpoint.get(&key.to_string());
        if checkpoint(HISTORICAL_SLOT_CHECKPOINT)? != Some(slot) {
            return Err(integrity("the store's slot is not the restored slot"));
        }
        // Every journaled operation of a store write carries that write's revision, which the
        // store commits only once all of the write's records are in it.
        let revision = checkpoint(HISTORICAL_REVISION_CHECKPOINT)?.unwrap_or(0);
        let records = checkpoint(HISTORICAL_JOURNAL_CHECKPOINT)?.unwrap_or(0);
        if backend.journal_revision().unwrap_or(0) > revision {
            return Err(integrity(
                "the journal holds operations of a store write the store never committed",
            ));
        }
        if records > backend.journaled().unwrap_or(0) {
            return Err(integrity(
                "the journal lacks operations the store committed",
            ));
        }
        self.historical_commit = (revision, records);
        let mut blocks: Vec<(Slot, BlockHeader)> = self
            .blocks
            .into_iter()?
            .filter(|(block, _)| *block >= boundary)
            .collect();
        blocks.sort_unstable_by_key(|(block, _)| *block);
        let open = match blocks.pop() {
            Some((open, header)) if open == slot && header.hash.is_empty() => header,
            _ => return Err(integrity("the store lacks the block the fork left open")),
        };
        // Each completed block opened on the blockhash the fork had produced before it.
        let mut blockhashes = Vec::with_capacity(blocks.len());
        let mut transactions = open.signatures.len() as u64;
        for (_, header) in &blocks {
            let previous = Hash::from_str(&header.previous_blockhash)
                .ok()
                .filter(|_| !header.hash.is_empty())
                .ok_or_else(|| integrity("a completed block lacks its blockhashes"))?;
            blockhashes.push(previous);
            transactions += header.signatures.len() as u64;
        }
        self.genesis_slot = boundary;
        self.historical_boundary = Some((boundary, position));
        self.historical_blockhashes = blockhashes;
        self.latest_epoch_info.transaction_count = (transactions > 0).then_some(transactions);
        self.set_historical_open_block(slot, open)
    }

    /// Opens the boundary slot's block: its archived parent, previous blockhash, height and
    /// time, which SVM Replay's own state must agree with.
    fn open_historical_boundary_block(
        &mut self,
        slot: Slot,
        block_height: u64,
        latest_blockhash: Hash,
        block_time: i64,
    ) -> SurfpoolResult<()> {
        let pins = self
            .inner
            .historical_session()
            .map(|session| session.pins().clone())
            .ok_or_else(|| SurfpoolError::internal("a historical block needs its fork"))?;
        let integrity = |what: &str| {
            SurfpoolError::historical(
                "SESSION_INTEGRITY",
                &format!("the boundary block's {what} differs from SVM Replay's state"),
                None,
            )
        };
        if pins.execution_blockhash != latest_blockhash {
            return Err(integrity("previous blockhash"));
        }
        if pins.block_height != block_height || pins.slot != slot {
            return Err(integrity("height or slot"));
        }
        let parent_slot = pins.parent_slot;
        let position = u64::from(pins.transaction_index);
        self.genesis_slot = slot;
        self.historical_boundary = Some((slot, position));
        self.set_historical_open_block(
            slot,
            BlockHeader {
                hash: String::new(),
                previous_blockhash: latest_blockhash.to_string(),
                parent_slot,
                block_time,
                block_height,
                signatures: vec![],
            },
        )
    }

    /// The fork's current block, still open: kept in the store too (without a blockhash, which
    /// marks it open) so a restart finds the block its fork left open, with the fork's slot
    /// and how much of SVM Replay's journal the store has seen.
    fn set_historical_open_block(&mut self, slot: Slot, header: BlockHeader) -> SurfpoolResult<()> {
        self.blocks.store(slot, header.clone())?;
        self.historical_open_block = Some((slot, header));
        self.slot_checkpoint
            .store(HISTORICAL_SLOT_CHECKPOINT.to_string(), slot)?;
        Ok(())
    }

    /// A journaled historical fork's session; none otherwise.
    fn journaled_backend(&self) -> Option<HistoricalSession> {
        let session = self.inner.historical_session()?;
        session.journaled().map(|_| session)
    }

    /// Starts one of the surfnet's writes (the SVM write lock is held): what SVM Replay
    /// journals meanwhile carries the store revision this write commits.
    pub fn begin_historical_write(&self) {
        if let Some(backend) = self.journaled_backend() {
            backend.set_revision(NonZeroU64::new(self.historical_commit.0 + 1));
        }
    }

    /// Ends a write. Once the store holds all of its records (no transaction still waits to
    /// join a block), the store commits the revision with how many journal records it has
    /// seen, the revision last: a restart refuses a journal holding a newer revision. A
    /// commit the store could not record is reported; a restart from that store refuses.
    pub fn end_historical_write(&mut self) {
        let Some(backend) = self.journaled_backend() else {
            return;
        };
        backend.set_revision(None);
        if self.historical_commits_stopped {
            return;
        }
        // A failed write may leave the store short of what the journal holds: it commits no
        // later write, so a restart from it refuses instead of serving an incomplete fork.
        if self.storage_backend.write_failed() {
            self.historical_commits_stopped = true;
            self.simnet_events_tx.error(
                "The surfnet's store failed a write; this historical fork can no longer restart from it"
                    .to_string(),
            );
            return;
        }
        let records = backend.journaled().unwrap_or_default();
        let (revision, committed) = self.historical_commit;
        let settled = self.transactions_queued_for_confirmation.is_empty()
            && self.transactions_queued_for_finalization.is_empty();
        if records == committed || !settled {
            return;
        }
        let commit = self
            .slot_checkpoint
            .store(HISTORICAL_JOURNAL_CHECKPOINT.to_string(), records)
            .and_then(|()| {
                self.slot_checkpoint
                    .store(HISTORICAL_REVISION_CHECKPOINT.to_string(), revision + 1)
            });
        match commit {
            Ok(()) => self.historical_commit = (revision + 1, records),
            Err(error) => {
                self.historical_commits_stopped = true;
                self.simnet_events_tx.error(format!(
                    "The surfnet's store could not commit its historical write ({error}); this historical fork can no longer restart from it"
                ));
            }
        }
    }

    /// Resets a historical fork to its boundary: SVM Replay resets the branch, and Surfpool's
    /// own stores and indexes start over empty, as at startup; nothing local is seeded.
    pub(super) fn reset_historical_network(&mut self) -> SurfpoolResult<()> {
        if !self.inner.is_historical() {
            return Err(SurfpoolError::internal(
                "reset_historical_network needs a historical fork",
            ));
        }
        self.inner.reset(self.feature_set.clone())?;
        self.blocks.clear()?;
        self.transactions.clear()?;
        self.transactions_queued_for_confirmation.clear();
        self.transactions_queued_for_finalization.clear();
        self.perf_samples.clear();
        self.transactions_processed = 0;
        self.profile_tag_map.clear()?;
        self.simulated_transaction_profiles.clear()?;
        self.executed_transaction_profiles.clear()?;
        self.accounts_by_owner.clear()?;
        self.account_associated_data.clear()?;
        self.token_accounts.clear()?;
        self.token_mints.clear()?;
        self.token_accounts_by_owner.clear()?;
        self.token_accounts_by_delegate.clear()?;
        self.token_accounts_by_mint.clear()?;
        self.non_circulating_accounts.clear();
        self.registered_idls.clear()?;
        self.register_builtin_template_idls();
        self.runbook_executions.clear();
        self.streamed_accounts.clear()?;
        self.scheduled_overrides.clear()?;
        self.slot_checkpoint.clear()?;
        self.recent_blockhashes.clear();
        self.supply_overrides = SupplyOverrides::default();
        self.historical_uploads.clear();
        self.initialize_historical()?;
        self.last_checkpoint_slot = self.genesis_slot;
        // Slot subscribers (Studio) learn the restored slot as they learn a new block's.
        let slot = self.get_latest_absolute_slot();
        let parent = self
            .historical_open_block
            .as_ref()
            .map_or(slot, |(_, header)| header.parent_slot);
        self.notify_slot_subscribers(slot, parent, parent);
        Ok(())
    }

    /// A local block's header on a historical fork, the still-open current block included.
    pub fn historical_block_header(&self, slot: Slot) -> SurfpoolResult<Option<BlockHeader>> {
        match &self.historical_open_block {
            Some((open, header)) if *open == slot => Ok(Some(header.clone())),
            _ => Ok(self.blocks.get(&slot)?),
        }
    }

    /// How many archived transactions precede a historical fork's own in `slot`'s block: the
    /// fork position in the boundary block, none in later blocks (or in normal mode).
    pub fn historical_block_offset(&self, slot: Slot) -> u64 {
        match self.historical_boundary {
            Some((boundary, position)) if boundary == slot => position,
            _ => 0,
        }
    }

    /// A local transaction's index in its block on a historical fork: in the boundary slot,
    /// the archived transactions before the fork position come first.
    pub fn historical_transaction_index(
        &self,
        slot: Slot,
        signature: &Signature,
    ) -> SurfpoolResult<Option<u32>> {
        if !self.inner.is_historical() {
            return Ok(None);
        }
        let Some(header) = self.historical_block_header(slot)? else {
            return Ok(None);
        };
        let Some(position) = header.signatures.iter().position(|s| s == signature) else {
            return Ok(None);
        };
        let index = self.historical_block_offset(slot) + position as u64;
        u32::try_from(index).map(Some).map_err(|_| {
            SurfpoolError::historical(
                "SESSION_INTEGRITY",
                "a historical transaction index exceeds the block index range",
                None,
            )
        })
    }

    /// A historical fork keeps its clock frozen: a block confirms and finalizes the queued
    /// transactions in the current slot without moving the slot, blockhash or sysvars, which
    /// only an explicit advance inside SVM Replay changes.
    pub fn confirm_historical_block(&mut self) -> SurfpoolResult<()> {
        let (confirmed_signatures, _) = self.confirm_transactions()?;
        if !confirmed_signatures.is_empty() {
            // The current slot's block stays open (no blockhash exists for it yet); its
            // transactions join it in order.
            let (slot, mut header) = self.historical_open_block.clone().ok_or_else(|| {
                SurfpoolError::internal("a historical fork's current block is not open")
            })?;
            let count = confirmed_signatures.len() as u64;
            header.signatures.extend(confirmed_signatures);
            self.set_historical_open_block(slot, header)?;
            let total = self.latest_epoch_info.transaction_count.unwrap_or(0);
            self.latest_epoch_info.transaction_count = Some(total + count);
        }
        self.finalize_transactions()
    }

    /// Advances a historical fork by `slots` local slots inside SVM Replay (within the
    /// boundary's epoch, or into later ones as a synthetic warp when `synthetic_epoch`), then
    /// mirrors the new slot, block height, blockhash and sysvars.
    /// The clock's time moves only when `unix_time_ms` is given (SVM Replay keeps seconds).
    pub fn advance_historical(
        &mut self,
        slots: u64,
        unix_time_ms: Option<u64>,
        synthetic_epoch: bool,
    ) -> SurfpoolResult<EpochInfo> {
        let unix_timestamp = unix_time_ms.map(|ms| (ms / 1_000) as i64);
        if !self.inner.is_historical() {
            return Err(SurfpoolError::internal(
                "advance_historical needs a historical fork",
            ));
        }
        // Close the current slot's block before its slot ends.
        self.confirm_historical_block()?;
        let parent_slot = self.get_latest_absolute_slot();
        let previous_blockhash = self.latest_blockhash();
        let fork = self
            .inner
            .historical_fork_mut()
            .ok_or_else(|| SurfpoolError::internal("advance_historical needs a historical fork"))?;
        let position = fork.advance(slots, unix_timestamp, synthetic_epoch)?;
        let (blockhash, _) = fork.latest_blockhash()?;
        let (new_slot, block_height) = (position.slot, position.block_height);
        if blockhash != previous_blockhash {
            self.historical_blockhashes.push(previous_blockhash);
        }
        let clock = self.inner.clock();
        // The slot the fork left is a complete block now, with the blockhash SVM Replay
        // registered on leaving it (what the next block's transactions use); the new slot's
        // block opens on it. Slots this advance passed over have no block of their own.
        let (closed, mut header) = self.historical_open_block.take().ok_or_else(|| {
            SurfpoolError::internal("a historical fork's current block is not open")
        })?;
        header.hash = blockhash.to_string();
        // The closed block is one performance sample of the fork's own transactions, as each
        // produced block is in normal mode; archived history has no local samples.
        let num_transactions = header.signatures.len() as u64;
        self.blocks.store(closed, header)?;
        if self.perf_samples.len() > 30 {
            self.perf_samples.pop_back();
        }
        self.perf_samples.push_front(RpcPerfSample {
            slot: closed,
            num_slots: 1,
            sample_period_secs: 1,
            num_transactions,
            num_non_vote_transactions: Some(num_transactions),
        });
        self.set_historical_open_block(
            new_slot,
            BlockHeader {
                hash: String::new(),
                previous_blockhash: blockhash.to_string(),
                parent_slot: closed,
                block_time: clock.unix_timestamp,
                block_height,
                signatures: vec![],
            },
        )?;
        let epoch_schedule = self.inner.epoch_schedule();
        let (epoch, slot_index) = epoch_schedule.get_epoch_and_slot_index(new_slot);
        self.latest_epoch_info.epoch = epoch;
        self.latest_epoch_info.slot_index = slot_index;
        self.latest_epoch_info.absolute_slot = new_slot;
        self.latest_epoch_info.block_height = block_height;
        self.chain_tip = BlockIdentifier::new(block_height, &blockhash.to_string());
        if let Some(ms) = unix_time_ms {
            self.updated_at = ms;
        }
        // Scheduled overrides for every local slot this advance produced, in slot order, as
        // each block does.
        let mut scheduled: Vec<Slot> = self
            .scheduled_overrides
            .keys()?
            .into_iter()
            .filter(|slot| (parent_slot + 1..=new_slot).contains(slot))
            .collect();
        scheduled.sort_unstable();
        for slot in scheduled {
            complete_without_datasource(self.materialize_overrides_for_slot(&None, slot))?;
        }
        self.publish_historical_slot(parent_slot, new_slot)?;
        let _ = self.simnet_events_tx.system_clock_updated(clock);
        Ok(self.latest_epoch_info.clone())
    }

    /// The slot lifecycle Surfpool publishes for each block, for a historical advance from
    /// `parent` to `slot`. Local transactions finalize immediately, so `parent` is the root.
    fn publish_historical_slot(&mut self, parent: Slot, slot: Slot) -> SurfpoolResult<()> {
        let timestamp = Utc::now().timestamp_millis().max(0) as u64;
        // A failed store read is reported, never published as a block of zero transactions;
        // an absent block has none.
        let (transactions, failed) = match self.blocks.get(&parent)? {
            Some(block) => {
                let mut failed = 0;
                for signature in &block.signatures {
                    if matches!(
                        self.transactions.get(&signature.to_string())?,
                        Some(SurfnetTransactionStatus::Processed(data))
                            if data.as_ref().0.meta.status.is_err()
                    ) {
                        failed += 1;
                    }
                }
                (block.signatures.len() as u64, failed)
            }
            None => (0, 0),
        };
        self.notify_slot_subscribers(slot, parent, parent);
        self.notify_slots_updates_subscribers(SlotUpdate::Frozen {
            slot: parent,
            timestamp,
            stats: SlotTransactionStats {
                num_transaction_entries: 1,
                num_successful_transactions: transactions.saturating_sub(failed),
                num_failed_transactions: failed,
                max_transactions_per_entry: transactions,
            },
        });
        self.notify_slots_updates_subscribers(SlotUpdate::CreatedBank {
            slot,
            parent,
            timestamp,
        });
        self.notify_slots_updates_subscribers(SlotUpdate::OptimisticConfirmation {
            slot: parent,
            timestamp,
        });
        self.notify_slots_updates_subscribers(SlotUpdate::Root {
            slot: parent,
            timestamp,
        });
        for status in [GeyserSlotStatus::Confirmed, GeyserSlotStatus::Rooted] {
            let _ = self.geyser_events_tx.send(GeyserEvent::UpdateSlotStatus {
                slot: parent,
                parent: parent.checked_sub(1),
                status,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use solana_signature::Signature;

    use crate::{
        storage::{Storage, StorageError, StorageResult},
        surfnet::{
            BlockHeader,
            svm::{SurfnetSvm, SurfnetSvmConfig},
        },
    };

    /// A store whose every read fails, as a broken database's does.
    #[derive(Clone)]
    struct Unreadable;

    impl<K: 'static, V: 'static> Storage<K, V> for Unreadable {
        fn store(&mut self, _key: K, _value: V) -> StorageResult<()> {
            Ok(())
        }
        fn clear(&mut self) -> StorageResult<()> {
            Ok(())
        }
        fn get(&self, _key: &K) -> StorageResult<Option<V>> {
            Err(StorageError::LockError)
        }
        fn take(&mut self, _key: &K) -> StorageResult<Option<V>> {
            Err(StorageError::LockError)
        }
        fn keys(&self) -> StorageResult<Vec<K>> {
            Err(StorageError::LockError)
        }
        fn into_iter(&self) -> StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>> {
            Err(StorageError::LockError)
        }
        fn count(&self) -> StorageResult<u64> {
            Err(StorageError::LockError)
        }
        fn clone_box(&self) -> Box<dyn Storage<K, V>> {
            Box::new(Unreadable)
        }
    }

    /// A slot whose block cannot be read is not published, neither to slot subscribers nor
    /// to Geyser: the read failure is the result.
    #[test]
    fn a_store_read_failure_publishes_no_slot() {
        for unreadable_transactions in [false, true] {
            let (mut svm, _events, geyser) = SurfnetSvm::new(SurfnetSvmConfig::default()).unwrap();
            let parent = svm.get_latest_absolute_slot();
            if unreadable_transactions {
                // The block is readable and lists a transaction whose status cannot be read.
                let block = BlockHeader {
                    hash: String::new(),
                    previous_blockhash: String::new(),
                    parent_slot: parent.saturating_sub(1),
                    block_time: 0,
                    block_height: 0,
                    signatures: vec![Signature::from([3u8; 64])],
                };
                svm.blocks.store(parent, block).unwrap();
                svm.transactions = Box::new(Unreadable);
            } else {
                svm.blocks = Box::new(Unreadable);
            }
            let slots = svm.subscribe_for_slot_updates();
            let updates = svm.subscribe_for_slots_updates();
            while geyser.try_recv().is_ok() {}

            let published = svm.publish_historical_slot(parent, parent + 1);

            assert!(
                published.is_err(),
                "unreadable transactions: {unreadable_transactions}"
            );
            assert!(slots.try_recv().is_err(), "a slot was published");
            assert!(updates.try_recv().is_err(), "a slot update was published");
            assert!(
                geyser.try_recv().is_err(),
                "a Geyser slot status was published"
            );
        }
    }
}
