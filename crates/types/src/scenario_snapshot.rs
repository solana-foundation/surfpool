use serde::{Deserialize, Serialize};
use solana_clock::Slot;

use crate::{AccountSnapshot, Scenario};

pub const SCENARIO_SNAPSHOT_FORMAT_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioSnapshotSeries {
    pub format_version: u16,
    pub capture_id: String,
    pub complete: bool,
    pub scenario: Scenario,
    pub base_slot: Slot,
    pub runtime: ScenarioSnapshotRuntime,
    pub capture: ScenarioSnapshotCaptureConfig,
    pub checkpoints: Vec<ScenarioSnapshotCheckpoint>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioSnapshotRuntime {
    pub surfpool_version: String,
    pub genesis_slot: Slot,
    pub slot_time_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioSnapshotCheckpoint {
    pub format_version: u16,
    pub capture_id: String,
    pub sequence: u32,
    pub position: ScenarioSnapshotPosition,
    pub operations: Vec<ScenarioSnapshotAccountOperation>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScenarioSnapshotPosition {
    Baseline { slot: Slot },
    AfterOverrides { slot: Slot },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ScenarioSnapshotAccountOperation {
    Upsert {
        pubkey: String,
        account: AccountSnapshot,
        notification: ScenarioSnapshotNotification,
    },
    Delete {
        pubkey: String,
        notification: ScenarioSnapshotNotification,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScenarioSnapshotNotification {
    Silent,
    AccountUpdate,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioSnapshotCaptureConfig {
    #[serde(default)]
    pub program_accounts: ScenarioSnapshotProgramAccounts,
    pub max_bytes: Option<u64>,
    pub max_operations: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioSnapshotCaptureRequest {
    pub capture_id: Option<String>,
    #[serde(flatten)]
    pub config: ScenarioSnapshotCaptureConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioSnapshotRetrievalConfig {
    #[serde(default)]
    pub flush: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScenarioSnapshotProgramAccounts {
    #[default]
    Exclude,
    Include,
}
