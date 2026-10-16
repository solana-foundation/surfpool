use base64::{Engine, prelude::BASE64_STANDARD};
use solana_account::Account;
use solana_clock::Slot;
use solana_loader_v3_interface::state::UpgradeableLoaderState;
use solana_pubkey::Pubkey;
use surfpool_types::{
    AccountSnapshot, SCENARIO_SNAPSHOT_FORMAT_VERSION, ScenarioSnapshotAccountOperation,
    ScenarioSnapshotCheckpoint, ScenarioSnapshotNotification, ScenarioSnapshotPosition,
    ScenarioSnapshotProgramAccounts, ScenarioSnapshotSeries,
};

use crate::error::{SurfpoolError, SurfpoolResult};

/// In-memory journal for one registered scenario. The SVM owns the session and
/// records operations only as its scenario materializer writes account state.
#[derive(Clone)]
pub(super) struct ScenarioSnapshotCaptureState {
    pub(super) series: ScenarioSnapshotSeries,
    pub(super) pending_slot: Slot,
    pub(super) pending_operations: Vec<ScenarioSnapshotAccountOperation>,
    pub(super) bytes: u64,
    pub(super) operations: u64,
}

/// Decoded account write ready to apply when its relative slot is reached.
#[derive(Clone)]
pub(super) struct ScenarioSnapshotReplayOperation {
    pub(super) pubkey: Pubkey,
    pub(super) account: Account,
    pub(super) notification: ScenarioSnapshotNotification,
}

/// One checkpoint translated from its captured slot to a replay slot.
#[derive(Clone)]
pub(super) struct ScenarioSnapshotReplayCheckpoint {
    pub(super) slot: Slot,
    pub(super) operations: Vec<ScenarioSnapshotReplayOperation>,
}

/// Future checkpoints installed by one complete series upload.
#[derive(Clone)]
pub(super) struct ScenarioSnapshotReplayState {
    pub(super) pending: VecDeque<ScenarioSnapshotReplayCheckpoint>,
}

impl ScenarioSnapshotCaptureState {
    /// Applies the capture's program-account policy to an account.
    /// Upgradeable programs and their ProgramData are omitted together by default.
    fn includes_account(&self, account: &Account) -> bool {
        if self.series.capture.program_accounts == ScenarioSnapshotProgramAccounts::Include {
            return true;
        }
        let loader = solana_sdk_ids::bpf_loader_upgradeable::id();
        if account.owner == loader {
            if account.executable {
                return false;
            }
            let metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
            return !(account.data.len() >= metadata_size
                && matches!(
                    bincode::deserialize::<UpgradeableLoaderState>(&account.data[..metadata_size]),
                    Ok(UpgradeableLoaderState::ProgramData { .. })
                ));
        }
        !account.executable
    }

    /// Checks whether recording an account write would exceed a capture limit.
    /// Excluded accounts do not consume the byte or operation budget.
    pub(super) fn check_account_capacity(
        &self,
        pubkey: &Pubkey,
        account: &Account,
        notification: ScenarioSnapshotNotification,
    ) -> SurfpoolResult<()> {
        if self.includes_account(account) {
            self.check_capacity(&snapshot_operation(pubkey, account, notification))?;
        }
        Ok(())
    }

    /// Appends a successful account write with its replay notification behavior.
    /// The caller skips unchanged and failed writes before calling this method.
    pub(super) fn record_account(
        &mut self,
        pubkey: &Pubkey,
        account: &Account,
        notification: ScenarioSnapshotNotification,
    ) -> SurfpoolResult<()> {
        if self.includes_account(account) {
            self.check_and_record(snapshot_operation(pubkey, account, notification))?;
        }
        Ok(())
    }

    /// Returns the serialized size of an operation if both limits permit it.
    fn check_capacity(&self, operation: &ScenarioSnapshotAccountOperation) -> SurfpoolResult<u64> {
        let bytes = serde_json::to_vec(operation)
            .map_err(|e| SurfpoolError::internal(e.to_string()))?
            .len() as u64;
        let next_bytes = self.bytes.saturating_add(bytes);
        let next_operations = self.operations.saturating_add(1);
        if self
            .series
            .capture
            .max_bytes
            .is_some_and(|max| next_bytes > max)
            || self
                .series
                .capture
                .max_operations
                .is_some_and(|max| next_operations > max)
        {
            return Err(SurfpoolError::internal(
                "scenario snapshot capture limit exceeded",
            ));
        }
        Ok(bytes)
    }

    /// Adds an operation to the pending slot and updates the capture totals.
    fn check_and_record(
        &mut self,
        operation: ScenarioSnapshotAccountOperation,
    ) -> SurfpoolResult<()> {
        let bytes = self.check_capacity(&operation)?;
        self.bytes += bytes;
        self.operations += 1;
        self.pending_operations.push(operation);
        Ok(())
    }

    /// Moves the pending operations into the next slot-delta checkpoint.
    pub(super) fn seal(&mut self) -> SurfpoolResult<()> {
        let sequence = u32::try_from(self.series.checkpoints.len())
            .map_err(|_| SurfpoolError::internal("too many scenario snapshot checkpoints"))?;
        self.series.checkpoints.push(ScenarioSnapshotCheckpoint {
            format_version: SCENARIO_SNAPSHOT_FORMAT_VERSION,
            capture_id: self.series.capture_id.clone(),
            sequence,
            position: ScenarioSnapshotPosition::AfterOverrides {
                slot: self.pending_slot,
            },
            operations: std::mem::take(&mut self.pending_operations),
        });
        Ok(())
    }
}

/// Encodes the account state written by a scenario as a portable replay operation.
/// A default account is an explicit deletion rather than a remote-fetch marker.
fn snapshot_operation(
    pubkey: &Pubkey,
    account: &Account,
    notification: ScenarioSnapshotNotification,
) -> ScenarioSnapshotAccountOperation {
    if account == &Account::default() {
        ScenarioSnapshotAccountOperation::Delete {
            pubkey: pubkey.to_string(),
            notification,
        }
    } else {
        ScenarioSnapshotAccountOperation::Upsert {
            pubkey: pubkey.to_string(),
            account: AccountSnapshot::new(
                account.lamports,
                account.owner.to_string(),
                account.executable,
                account.rent_epoch,
                BASE64_STANDARD.encode(&account.data),
                None,
            ),
            notification,
        }
    }
}
use std::collections::VecDeque;
