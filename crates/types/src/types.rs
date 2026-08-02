#[cfg(feature = "prometheus")]
use std::time::SystemTime;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    fmt,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
};

use blake3::Hash;
use chrono::{DateTime, Local};
use crossbeam_channel::Sender;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};
use serde_with::{BytesOrString, serde_as};
use solana_account::Account;
use solana_account_decoder_client_types::{ParsedAccount, UiAccount, UiAccountEncoding};
use solana_clock::{Clock, Epoch, Slot};
use solana_epoch_info::EpochInfo;
use solana_message::inner_instruction::InnerInstructionsList;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_context::transaction::TransactionReturnData;
use solana_transaction_error::TransactionError;
use txtx_addon_kit::indexmap::IndexMap;
use uuid::Uuid;

use crate::DEFAULT_MAINNET_RPC_URL;

pub const DEFAULT_RPC_PORT: u16 = 8899;
pub const DEFAULT_WS_PORT: u16 = 8900;
pub const DEFAULT_STUDIO_PORT: u16 = 8488;
pub const CHANGE_TO_DEFAULT_STUDIO_PORT_ONCE_SUPERVISOR_MERGED: u16 = 18488;
pub const DEFAULT_NETWORK_HOST: &str = "127.0.0.1";
pub const DEFAULT_SLOT_TIME_MS: u64 = 400;
pub type Idl = anchor_lang_idl::types::Idl;
pub const DEFAULT_PROFILING_MAP_CAPACITY: usize = 200;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionMetadata {
    pub signature: Signature,
    pub logs: Vec<String>,
    pub inner_instructions: InnerInstructionsList,
    pub compute_units_consumed: u64,
    pub return_data: TransactionReturnData,
    pub fee: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionConfirmationStatus {
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BlockProductionMode {
    #[default]
    Clock,
    Transaction,
    Manual,
}

impl fmt::Display for BlockProductionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockProductionMode::Clock => write!(f, "clock"),
            BlockProductionMode::Transaction => write!(f, "transaction"),
            BlockProductionMode::Manual => write!(f, "manual"),
        }
    }
}

impl FromStr for BlockProductionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "clock" => Ok(BlockProductionMode::Clock),
            "transaction" => Ok(BlockProductionMode::Transaction),
            "manual" => Ok(BlockProductionMode::Manual),
            _ => Err(format!(
                "Invalid block production mode: {}. Valid values are: clock, transaction, manual",
                s
            )),
        }
    }
}

#[derive(Debug)]
pub enum SubgraphEvent {
    EndpointReady,
    InfoLog(DateTime<Local>, String),
    ErrorLog(DateTime<Local>, String),
    WarnLog(DateTime<Local>, String),
    DebugLog(DateTime<Local>, String),
    Shutdown,
}

impl SubgraphEvent {
    pub fn info<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::InfoLog(Local::now(), msg.into())
    }

    pub fn warn<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::WarnLog(Local::now(), msg.into())
    }

    pub fn error<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::ErrorLog(Local::now(), msg.into())
    }

    pub fn debug<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::DebugLog(Local::now(), msg.into())
    }
}

/// Result structure for compute units estimation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputeUnitsEstimationResult {
    pub success: bool,
    pub compute_units_consumed: u64,
    pub log_messages: Option<Vec<String>>,
    pub error_message: Option<String>,
}

/// The struct for storing the profiling results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyedProfileResult {
    pub slot: u64,
    pub key: UuidOrSignature,
    pub instruction_profiles: Option<Vec<ProfileResult>>,
    pub transaction_profile: ProfileResult,
    #[serde(with = "pubkey_account_map")]
    pub readonly_account_states: HashMap<Pubkey, Account>,
}

impl KeyedProfileResult {
    pub fn new(
        slot: u64,
        key: UuidOrSignature,
        instruction_profiles: Option<Vec<ProfileResult>>,
        transaction_profile: ProfileResult,
        readonly_account_states: HashMap<Pubkey, Account>,
    ) -> Self {
        Self {
            slot,
            key,
            instruction_profiles,
            transaction_profile,
            readonly_account_states,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileResult {
    #[serde(with = "pubkey_option_account_map")]
    pub pre_execution_capture: ExecutionCapture,
    #[serde(with = "pubkey_option_account_map")]
    pub post_execution_capture: ExecutionCapture,
    pub compute_units_consumed: u64,
    pub log_messages: Option<Vec<String>>,
    pub error_message: Option<String>,
}

pub type ExecutionCapture = BTreeMap<Pubkey, Option<Account>>;

impl ProfileResult {
    pub fn new(
        pre_execution_capture: ExecutionCapture,
        post_execution_capture: ExecutionCapture,
        compute_units_consumed: u64,
        log_messages: Option<Vec<String>>,
        error_message: Option<String>,
    ) -> Self {
        Self {
            pre_execution_capture,
            post_execution_capture,
            compute_units_consumed,
            log_messages,
            error_message,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountProfileState {
    Readonly,
    Writable(AccountChange),
}

impl AccountProfileState {
    pub fn new(
        pubkey: Pubkey,
        pre_account: Option<Account>,
        post_account: Option<Account>,
        readonly_accounts: &[Pubkey],
    ) -> Self {
        if readonly_accounts.contains(&pubkey) {
            return AccountProfileState::Readonly;
        }

        match (pre_account, post_account) {
            (None, Some(post_account)) => {
                AccountProfileState::Writable(AccountChange::Create(post_account))
            }
            (Some(pre_account), None) => {
                AccountProfileState::Writable(AccountChange::Delete(pre_account))
            }
            (Some(pre_account), Some(post_account)) if pre_account == post_account => {
                AccountProfileState::Writable(AccountChange::Unchanged(Some(pre_account)))
            }
            (Some(pre_account), Some(post_account)) => {
                AccountProfileState::Writable(AccountChange::Update(pre_account, post_account))
            }
            (None, None) => AccountProfileState::Writable(AccountChange::Unchanged(None)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountChange {
    Create(Account),
    Update(Account, Account),
    Delete(Account),
    Unchanged(Option<Account>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct RpcProfileResultConfig {
    #[cfg_attr(
        feature = "ts-bindings",
        ts(as = "Option<crate::ts_bindings::UiAccountEncodingDef>", optional)
    )]
    pub encoding: Option<UiAccountEncoding>,
    pub depth: Option<RpcProfileDepth>,
}

impl Default for RpcProfileResultConfig {
    fn default() -> Self {
        Self {
            encoding: Some(UiAccountEncoding::JsonParsed),
            depth: Some(RpcProfileDepth::default()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub enum RpcProfileDepth {
    Transaction,
    #[default]
    Instruction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct UiKeyedProfileResult {
    pub slot: u64,
    #[cfg_attr(feature = "ts-bindings", ts(as = "String"))]
    pub key: UuidOrSignature,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-bindings", ts(optional))]
    pub instruction_profiles: Option<Vec<UiProfileResult>>,
    pub transaction_profile: UiProfileResult,
    #[serde(with = "profile_state_map")]
    #[cfg_attr(
        feature = "ts-bindings",
        ts(as = "std::collections::HashMap<String, crate::ts_bindings::UiAccountDef>")
    )]
    pub readonly_account_states: IndexMap<Pubkey, UiAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct UiProfileResult {
    #[serde(with = "profile_state_map")]
    #[cfg_attr(
        feature = "ts-bindings",
        ts(as = "std::collections::HashMap<String, crate::ts_bindings::UiAccountProfileStateDef>")
    )]
    pub account_states: IndexMap<Pubkey, UiAccountProfileState>,
    pub compute_units_consumed: u64,
    pub log_messages: Option<Vec<String>>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "type", content = "accountChange")]
#[allow(clippy::large_enum_variant)]
pub enum UiAccountProfileState {
    Readonly,
    Writable(UiAccountChange),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "type", content = "data")]
pub enum UiAccountChange {
    Create(UiAccount),
    Update(UiAccount, UiAccount),
    Delete(UiAccount),
    /// The account didn't change. If [Some], this is the initial state. If [None], the account didn't exist before/after execution.
    Unchanged(Option<UiAccount>),
}

/// P starts with 300 lamports
/// Ix 1 Transfers 100 lamports to P
/// Ix 2 Transfers 100 lamports to P
///
/// Profile result 1 is from executing just Ix 1
/// AccountProfileState::Writable(P, AccountChange::Update( UiAccount { lamports: 300, ...}, UiAccount { lamports: 400, ... }))
///
/// Profile result 2 is from executing Ix 1 and Ix 2
/// AccountProfileState::Writable(P, AccountChange::Update( UiAccount { lamports: 400, ...}, UiAccount { lamports: 500, ... }))
pub mod profile_state_map {
    use super::*;

    pub fn serialize<S, T>(map: &IndexMap<Pubkey, T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        let str_map: IndexMap<String, &T> = map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        str_map.serialize(serializer)
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<IndexMap<Pubkey, T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        let str_map: IndexMap<String, T> = IndexMap::deserialize(deserializer)?;
        str_map
            .into_iter()
            .map(|(k, v)| {
                Pubkey::from_str(&k)
                    .map(|pk| (pk, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// Serialization module for HashMap<Pubkey, Account>
pub mod pubkey_account_map {
    use super::*;

    pub fn serialize<S>(map: &HashMap<Pubkey, Account>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let str_map: HashMap<String, &Account> =
            map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        str_map.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<Pubkey, Account>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str_map: HashMap<String, Account> = HashMap::deserialize(deserializer)?;
        str_map
            .into_iter()
            .map(|(k, v)| {
                Pubkey::from_str(&k)
                    .map(|pk| (pk, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// Serialization module for BTreeMap<Pubkey, Option<Account>>
pub mod pubkey_option_account_map {
    use super::*;

    pub fn serialize<S>(
        map: &BTreeMap<Pubkey, Option<Account>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let str_map: BTreeMap<String, &Option<Account>> =
            map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        str_map.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<Pubkey, Option<Account>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str_map: BTreeMap<String, Option<Account>> = BTreeMap::deserialize(deserializer)?;
        str_map
            .into_iter()
            .map(|(k, v)| {
                Pubkey::from_str(&k)
                    .map(|pk| (pk, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// What the surfnet told a client, reduced to the predicate a property names
/// rather than the payload it answered with.
///
/// A served response is output: it is asserted on or read in a report, never
/// fed back into a surfnet. Reduced to a predicate so the event stream does
/// not carry a second copy of the wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientAnswer {
    /// A readiness question, and whether the answer conveyed readiness.
    ///
    /// Monotone: readiness does not regress, so the first answer carrying
    /// `ready: true` is the moment a client could have been misled, and an
    /// ordering assertion about it is well posed.
    Readiness { ready: bool },
}

#[derive(Debug)]
pub enum SimnetEvent {
    /// Core startup completed: RPC servers are bound and stored transactions
    /// have been replayed (the payload is their count). This is *not* public
    /// readiness: with an external startup planner (the CLI), it fires while
    /// the startup phase is still `Planning`, before clones are hydrated or
    /// deployment runbooks run. Public readiness is
    /// [`SurfnetStartupPhase::Ready`], observable via
    /// `surfnet_getSurfnetInfo` or the startup watch channel. With no
    /// external planner the two coincide: the runloop seals the empty plan
    /// immediately before emitting this event.
    Ready(u64),
    /// The surfnet answered a client, carrying what the answer conveyed.
    ///
    /// Reports what a client was told, so a test can assert on the answer a
    /// client received rather than infer it from internal state. Issue 715 is
    /// defined by client observation, which is why the answer is recorded.
    AnsweredClient {
        method: &'static str,
        answer: ClientAnswer,
    },
    /// The startup machine accepted a transition, carrying the status it
    /// produced. One event per accepted transition, in the order they were
    /// applied, so readiness can be observed as a position in a sequence.
    /// Rejections are not events: nothing
    /// changed, and the caller already holds the [`StartupError`].
    ///
    /// The startup watch channel carries the same status but coalesces, so a
    /// reader that wants every step reads it here.
    StartupStatusChanged(SurfnetStartupStatus),
    Connected(String),
    Aborted(String),
    Shutdown,
    SystemClockUpdated(Clock),
    ClockUpdate(ClockCommand),
    EpochInfoUpdate(EpochInfo),
    InfoLog(DateTime<Local>, String),
    ErrorLog(DateTime<Local>, String),
    WarnLog(DateTime<Local>, String),
    DebugLog(DateTime<Local>, String),
    PluginLoaded(String),
    TransactionReceived(DateTime<Local>, VersionedTransaction),
    TransactionProcessed(
        DateTime<Local>,
        TransactionMetadata,
        Option<TransactionError>,
    ),
    AccountUpdate(DateTime<Local>, Pubkey),
    TaggedProfile {
        result: KeyedProfileResult,
        tag: String,
        timestamp: DateTime<Local>,
    },
    RunbookStarted(String),
    RunbookCompleted(String, Option<Vec<String>>),
}

impl SimnetEvent {
    pub fn info<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::InfoLog(Local::now(), msg.into())
    }

    pub fn warn<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::WarnLog(Local::now(), msg.into())
    }

    pub fn error<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::ErrorLog(Local::now(), msg.into())
    }

    pub fn debug<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::DebugLog(Local::now(), msg.into())
    }

    pub fn transaction_processed(meta: TransactionMetadata, err: Option<TransactionError>) -> Self {
        Self::TransactionProcessed(Local::now(), meta, err)
    }

    pub fn transaction_received(tx: VersionedTransaction) -> Self {
        Self::TransactionReceived(Local::now(), tx)
    }

    pub fn account_update(pubkey: Pubkey) -> Self {
        Self::AccountUpdate(Local::now(), pubkey)
    }

    pub fn tagged_profile(result: KeyedProfileResult, tag: String) -> Self {
        Self::TaggedProfile {
            result,
            tag,
            timestamp: Local::now(),
        }
    }

    pub fn account_update_msg(&self) -> String {
        match self {
            SimnetEvent::AccountUpdate(_, pubkey) => {
                format!("Account {} updated.", pubkey)
            }
            _ => unreachable!("This function should only be called for AccountUpdate events"),
        }
    }

    pub fn epoch_info_update_msg(&self) -> String {
        match self {
            SimnetEvent::EpochInfoUpdate(epoch_info) => {
                format!(
                    "Datasource connection successful. Epoch {} / Slot index {} / Slot {}.",
                    epoch_info.epoch, epoch_info.slot_index, epoch_info.absolute_slot
                )
            }
            _ => unreachable!("This function should only be called for EpochInfoUpdate events"),
        }
    }

    pub fn plugin_loaded_msg(&self) -> String {
        match self {
            SimnetEvent::PluginLoaded(plugin_name) => {
                format!("Plugin {} successfully loaded.", plugin_name)
            }
            _ => unreachable!("This function should only be called for PluginLoaded events"),
        }
    }

    pub fn clock_update_msg(&self) -> String {
        match self {
            SimnetEvent::SystemClockUpdated(clock) => {
                format!("Clock ticking (epoch {}, slot {})", clock.epoch, clock.slot)
            }
            _ => {
                unreachable!("This function should only be called for SystemClockUpdated events")
            }
        }
    }
}

#[derive(Debug)]
pub enum TransactionStatusEvent {
    Success(TransactionConfirmationStatus),
    SimulationFailure((TransactionError, TransactionMetadata)),
    ExecutionFailure((TransactionError, TransactionMetadata)),
    VerificationFailure(String),
}

#[derive(Debug)]
pub enum SimnetCommand {
    SlotForward(Option<Hash>),
    SlotBackward(Option<Hash>),
    CommandClock(Option<(Hash, String)>, ClockCommand),
    UpdateInternalClock(Option<(Hash, String)>, Clock),
    UpdateInternalClockWithConfirmation(Option<(Hash, String)>, Clock, Sender<EpochInfo>),
    UpdateBlockProductionMode(BlockProductionMode),
    /// Executes a transaction. `sendTransaction` enqueues this on the same
    /// channel as the startup commands below, so channel order decides which
    /// accounts the transaction sees: enqueued before
    /// `CompleteStartupTask(RemoteAccounts, ..)`, it runs against unhydrated
    /// state. The readiness gate exists so clients wait for `Ready` rather
    /// than race that window.
    ProcessTransaction(
        Option<(Hash, String)>,
        VersionedTransaction,
        Sender<TransactionStatusEvent>,
        bool,
        Option<bool>,
    ),
    Terminate(Option<(Hash, String)>),
    /// Fixes the startup task list; `Ready` is unreachable until a plan is
    /// sealed. The only startup command with a reply channel: the planner
    /// must not dispatch tasks against an unsealed plan, so it blocks on the
    /// outcome. The startup commands below are fire-and-forget
    /// because no caller decision hangs on them; their failures are machine
    /// rejections, which the runloop reports as error events.
    SealStartupPlan(Vec<SurfnetStartupTask>, Sender<Result<(), StartupError>>),
    /// Reports a failure discovered while the plan was still unsealed
    /// (project inspection failed); drives the phase to `Failed`.
    FailStartupPlanning(String),
    /// Marks a sealed task `Running`. The submitter sends this before
    /// dispatching the task's work, on the same channel, so the transition
    /// is applied first.
    StartStartupTask(SurfnetStartupTask),
    /// Reports a task's outcome, mapping `Ok`/`Err` onto
    /// `Succeeded`/`Failed`.
    CompleteStartupTask(SurfnetStartupTask, Result<(), String>),
    StartRunbookExecution(String),
    CompleteRunbookExecution(String, Option<Vec<String>>),
    FetchRemoteAccounts(Vec<Pubkey>, String),
    AirdropProcessed,
}

#[derive(Debug)]
pub enum ClockCommand {
    Pause,
    /// Pause with confirmation - sends epoch info back when actually paused
    PauseWithConfirmation(Sender<EpochInfo>),
    Resume,
    Toggle,
    UpdateSlotInterval(u64),
}

pub enum ClockEvent {
    Tick,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SanitizedConfig {
    pub rpc_url: String,
    pub ws_url: String,
    pub rpc_datasource_url: Option<String>,
    pub studio_url: String,
    pub graphql_query_route_url: String,
    pub version: String,
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SurfpoolConfig {
    pub simnets: Vec<SimnetConfig>,
    pub rpc: RpcConfig,
    pub subgraph: SubgraphConfig,
    pub studio: StudioConfig,
    pub plugin_config_path: Vec<PathBuf>,
    #[serde(default)]
    pub startup_planner: StartupPlanner,
}

/// Who seals the startup plan for this surfnet.
///
/// Sealing fixes the task list, and `Ready` is unreachable without it, so
/// every surfnet needs exactly one sealer. The default hands that obligation
/// to the runloop, so embedders with no startup tasks need not set it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StartupPlanner {
    /// No external planner exists: the runloop seals an empty plan itself,
    /// right before announcing readiness, so the surfnet becomes publicly
    /// ready as soon as core startup completes.
    #[default]
    None,
    /// An external planner (the CLI) inspects the project and seals the plan
    /// via [`SimnetCommand::SealStartupPlan`]; the runloop must not seal. If
    /// the planner dies before sealing, the surfnet stays un-ready forever,
    /// which is the safe direction.
    External,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimnetConfig {
    pub offline_mode: bool,
    pub remote_rpc_url: Option<String>,
    pub slot_time: u64,
    pub block_production_mode: BlockProductionMode,
    pub airdrop_addresses: Vec<Pubkey>,
    pub airdrop_token_amount: u64,
    pub expiry: Option<u64>,
    pub instruction_profiling_enabled: bool,
    pub max_profiles: usize,
    pub log_bytes_limit: Option<usize>,
    pub skip_signature_verification: bool,
    #[serde(default)]
    pub skip_blockhash_check: bool,
    /// Unique identifier for this surfnet instance. Used to isolate database storage
    /// when multiple surfnets share the same database. Defaults to "default".
    pub surfnet_id: String,
    /// Snapshot accounts to preload at startup.
    /// Keys are pubkey strings, values can be None to fetch from remote RPC.
    pub snapshot: BTreeMap<String, Option<AccountSnapshot>>,
}

impl Default for SimnetConfig {
    fn default() -> Self {
        Self {
            offline_mode: false,
            remote_rpc_url: Some(DEFAULT_MAINNET_RPC_URL.to_string()),
            slot_time: DEFAULT_SLOT_TIME_MS, // Default to 400ms to match CLI default
            block_production_mode: BlockProductionMode::Clock,
            airdrop_addresses: vec![],
            airdrop_token_amount: 0,
            expiry: None,
            instruction_profiling_enabled: true,
            max_profiles: DEFAULT_PROFILING_MAP_CAPACITY,
            log_bytes_limit: Some(10_000),
            skip_signature_verification: false,
            skip_blockhash_check: false,
            surfnet_id: "default".to_string(),
            snapshot: BTreeMap::new(),
        }
    }
}

/// A datasource URL reduced to scheme and host, for anywhere a client or a log
/// can see it.
///
/// Datasource URLs carry credentials in three places: a query parameter
/// (`?api-key=`), a path segment, and userinfo (`https://user:pass@host`).
/// Parsing and rebuilding from the host drops all three. A URL that will not
/// parse yields `None`, so a caller substitutes rather than falling back to
/// the raw string.
pub fn sanitized_datasource_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    Some(format!("{}://{}", url.scheme(), url.host_str()?))
}

impl SimnetConfig {
    /// Returns a sanitized version of the datasource URL safe for display.
    /// Only returns scheme and host (e.g., "https://example.com") to prevent
    /// leaking API keys in paths or query parameters.
    pub fn get_sanitized_datasource_url(&self) -> Option<String> {
        sanitized_datasource_url(self.remote_rpc_url.as_ref()?)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubgraphConfig {}

pub const DEFAULT_GOSSIP_PORT: u16 = 8001;
pub const DEFAULT_TPU_PORT: u16 = 8003;
pub const DEFAULT_TPU_QUIC_PORT: u16 = 8004;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcConfig {
    pub bind_host: String,
    pub bind_port: u16,
    pub ws_port: u16,
    pub gossip_port: u16,
    pub tpu_port: u16,
    pub tpu_quic_port: u16,
}

impl RpcConfig {
    pub fn get_rpc_base_url(&self) -> String {
        format!("{}:{}", self.bind_host, self.bind_port)
    }
    pub fn get_ws_base_url(&self) -> String {
        format!("{}:{}", self.bind_host, self.ws_port)
    }
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            bind_host: DEFAULT_NETWORK_HOST.to_string(),
            bind_port: DEFAULT_RPC_PORT,
            ws_port: DEFAULT_WS_PORT,
            gossip_port: DEFAULT_GOSSIP_PORT,
            tpu_port: DEFAULT_TPU_PORT,
            tpu_quic_port: DEFAULT_TPU_QUIC_PORT,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StudioConfig {
    pub bind_host: String,
    pub bind_port: u16,
}

impl StudioConfig {
    pub fn get_studio_base_url(&self) -> String {
        format!("{}:{}", self.bind_host, self.bind_port)
    }
}

impl Default for StudioConfig {
    fn default() -> Self {
        Self {
            bind_host: DEFAULT_NETWORK_HOST.to_string(),
            bind_port: CHANGE_TO_DEFAULT_STUDIO_PORT_ONCE_SUPERVISOR_MERGED,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub struct CreateSurfnetRequest {
    pub domain: String,
    pub block_production_mode: BlockProductionMode,
    pub datasource_rpc_url: String,
    pub settings: Option<CloudSurfnetSettings>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "snake_case", default)]
pub struct CloudSurfnetSettings {
    pub database_url: Option<String>,
    pub profiling_disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gating: Option<CloudSurfnetRpcGating>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "snake_case", default)]
pub struct CloudSurfnetRpcGating {
    pub private_methods_secret_token: Option<String>,
    pub private_methods: Vec<String>,
    pub public_methods: Vec<String>,
    pub disabled_methods: Vec<String>,
}

impl CloudSurfnetRpcGating {
    pub fn restricted() -> CloudSurfnetRpcGating {
        CloudSurfnetRpcGating {
            private_methods: vec![],
            private_methods_secret_token: None,
            public_methods: vec![],
            disabled_methods: vec![
                "surfnet_cloneProgramAccount".into(),
                "surfnet_profileTransaction".into(),
                "surfnet_getProfileResultsByTag".into(),
                "surfnet_setSupply".into(),
                "surfnet_setProgramAuthority".into(),
                "surfnet_getTransactionProfile".into(),
                "surfnet_registerIdl".into(),
                "surfnet_getActiveIdl".into(),
                "surfnet_getLocalSignatures".into(),
                "surfnet_timeTravel".into(),
                "surfnet_pauseClock".into(),
                "surfnet_resumeClock".into(),
                "surfnet_resetAccount".into(),
                "surfnet_resetNetwork".into(),
                "surfnet_exportSnapshot".into(),
                "surfnet_offlineAccount".into(),
                "surfnet_streamAccount".into(),
                "surfnet_streamAccounts".into(),
                "surfnet_getStreamedAccounts".into(),
            ],
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateNetworkRequest {
    pub workspace_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub datasource_rpc_url: String,
    pub block_production_mode: BlockProductionMode,
    pub profiling_enabled: Option<bool>,
}

impl CreateNetworkRequest {
    pub fn new(
        workspace_id: Uuid,
        name: String,
        description: Option<String>,
        datasource_rpc_url: String,
        block_production_mode: BlockProductionMode,
        profiling_enabled: bool,
    ) -> Self {
        Self {
            workspace_id,
            name,
            description,
            datasource_rpc_url,
            block_production_mode,
            profiling_enabled: Some(profiling_enabled),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct CreateNetworkResponse {
    pub rpc_url: String,
}

#[derive(Serialize, Deserialize)]
pub struct DeleteNetworkRequest {
    pub workspace_id: Uuid,
    pub network_id: Uuid,
}

impl DeleteNetworkRequest {
    pub fn new(workspace_id: Uuid, network_id: Uuid) -> Self {
        Self {
            workspace_id,
            network_id,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DeleteNetworkResponse;

#[serde_as]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct AccountUpdate {
    /// providing this value sets the lamports in the account
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub lamports: Option<u64>,
    /// providing this value sets the data held in this account, as a
    /// hex-encoded string
    #[serde_as(as = "Option<BytesOrString>")]
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "string"))]
    pub data: Option<Vec<u8>>,
    ///  providing this value sets the program that owns this account. If executable, the program that loads this account.
    pub owner: Option<String>,
    /// providing this value sets whether this account's data contains a loaded program (and is now read-only)
    pub executable: Option<bool>,
    /// providing this value sets the epoch at which this account will next owe rent
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub rent_epoch: Option<Epoch>,
}

#[derive(Debug, Clone)]
pub enum SetSomeAccount {
    Account(String),
    NoAccount,
}

impl<'de> Deserialize<'de> for SetSomeAccount {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SetSomeAccountVisitor;

        impl<'de> Visitor<'de> for SetSomeAccountVisitor {
            type Value = SetSomeAccount;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a Pubkey String or the String 'null'")
            }

            fn visit_some<D_>(self, deserializer: D_) -> std::result::Result<Self::Value, D_::Error>
            where
                D_: Deserializer<'de>,
            {
                Deserialize::deserialize(deserializer).map(|v: String| match v.as_str() {
                    "null" => SetSomeAccount::NoAccount,
                    _ => SetSomeAccount::Account(v.to_string()),
                })
            }
        }

        deserializer.deserialize_option(SetSomeAccountVisitor)
    }
}

impl Serialize for SetSomeAccount {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            SetSomeAccount::Account(val) => serializer.serialize_str(val),
            SetSomeAccount::NoAccount => serializer.serialize_str("null"),
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct TokenAccountUpdate {
    /// providing this value sets the amount of the token in the account data
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub amount: Option<u64>,
    /// providing this value sets the delegate of the token account: a base58
    /// pubkey, or the literal string "null" to clear the delegate
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "string"))]
    pub delegate: Option<SetSomeAccount>,
    /// providing this value sets the state of the token account
    pub state: Option<String>,
    /// providing this value sets the amount authorized to the delegate
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub delegated_amount: Option<u64>,
    /// providing this value sets the close authority of the token account: a
    /// base58 pubkey, or the literal string "null" to clear the authority
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "string"))]
    pub close_authority: Option<SetSomeAccount>,
    /// providing this value configures the Token-2022 confidential-transfer
    /// extension on the account (Token-2022 only)
    pub confidential: Option<ConfidentialTransferAccountUpdate>,
}

/// Configures the Token-2022 `ConfidentialTransferAccount` extension on a token
/// account created via `surfnet_setTokenAccount`.
///
/// This is a test-only cheatcode: it fabricates a configured (and optionally
/// funded) confidential account directly, bypassing the real on-chain
/// configure / deposit / apply-pending-balance instruction flow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct ConfidentialTransferAccountUpdate {
    /// The owner's ElGamal public key (base58 or base64, 32 bytes). Required —
    /// the confidential balance is encrypted to this key, and confidential
    /// payment clients read it off the account to encrypt transfers.
    pub elgamal_pubkey: String,
    /// The owner's AES (authenticated-encryption) secret key (base58 or base64,
    /// 16 bytes). Required. Produces the `decryptable_available_balance` the
    /// owner reads to learn its balance — even a zero-balance receive-only
    /// account needs a valid `encrypt(0)` here (a placeholder would fail
    /// owner-side balance reads), so this is mandatory for every confidential
    /// account. Modeled as `Option` only so the field can be validated with a
    /// clear error message when omitted.
    #[cfg_attr(feature = "ts-bindings", ts(as = "String"))]
    pub aes_key: Option<String>,
    /// The confidential available balance to set (default 0).
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub amount: Option<u64>,
    /// Whether the account is approved for confidential transfers (default true).
    pub approved: Option<bool>,
    /// Whether the account accepts incoming confidential credits (default true).
    pub allow_confidential_credits: Option<bool>,
    /// Whether the base account accepts incoming non-confidential credits
    /// (default true).
    pub allow_non_confidential_credits: Option<bool>,
    /// The maximum pending-balance credit counter (default 65536).
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub maximum_pending_balance_credit_counter: Option<u64>,
}

// token supply update for set supply method in SVM tricks
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct SupplyUpdate {
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub total: Option<u64>,
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub circulating: Option<u64>,
    #[cfg_attr(feature = "ts-bindings", ts(optional, type = "number | bigint"))]
    pub non_circulating: Option<u64>,
    pub non_circulating_accounts: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum UuidOrSignature {
    Uuid(Uuid),
    Signature(Signature),
}

impl std::fmt::Display for UuidOrSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UuidOrSignature::Uuid(uuid) => write!(f, "{}", uuid),
            UuidOrSignature::Signature(signature) => write!(f, "{}", signature),
        }
    }
}

impl<'de> Deserialize<'de> for UuidOrSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        if let Ok(uuid) = Uuid::parse_str(&s) {
            return Ok(UuidOrSignature::Uuid(uuid));
        }

        if let Ok(signature) = s.parse::<Signature>() {
            return Ok(UuidOrSignature::Signature(signature));
        }

        Err(serde::de::Error::custom(
            "expected a Uuid or a valid Solana Signature",
        ))
    }
}

impl Serialize for UuidOrSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            UuidOrSignature::Uuid(uuid) => serializer.serialize_str(&uuid.to_string()),
            UuidOrSignature::Signature(signature) => {
                serializer.serialize_str(&signature.to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DataIndexingCommand {
    ProcessCollection(Uuid),
    ProcessCollectionEntriesPack(Uuid, Vec<u8>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JitoBundleStatus {
    #[serde(rename = "bundle_id")]
    pub bundle_id: String,
    pub transactions: Vec<String>,
    pub slot: u64,
    pub confirmation_status: solana_transaction_status::TransactionConfirmationStatus,
    pub err: std::result::Result<(), TransactionError>,
}

// Define a wrapper struct
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedIdl(pub Slot, pub Idl);

// Implement ordering based on Slot
impl PartialEq for VersionedIdl {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for VersionedIdl {}

impl PartialOrd for VersionedIdl {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VersionedIdl {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

#[derive(Debug, Clone)]
pub struct FifoMap<K, V> {
    // IndexMap is a map that preserves the insertion order of the keys. (It will be used for the FIFO eviction)
    map: IndexMap<K, V>,
}

impl<K: std::hash::Hash + Eq, V> Default for FifoMap<K, V> {
    fn default() -> Self {
        Self::new(DEFAULT_PROFILING_MAP_CAPACITY)
    }
}
impl<K: std::hash::Hash + Eq, V> FifoMap<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: IndexMap::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.map.capacity()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Insert a key/value. If `K` is new and we're full, evict the oldest (FIFO)
    /// Returns a tuple of (old_value, evicted_key):
    /// - old_value: The previous value if this was an update to an existing key
    /// - evicted_key: The key that was evicted if the map was at capacity
    pub fn insert(&mut self, key: K, value: V) -> (Option<V>, Option<K>) {
        if self.map.contains_key(&key) {
            // Update doesn't change insertion order in IndexMap
            return (self.map.insert(key, value), None);
        }
        let evicted_key = if self.map.len() == self.map.capacity() {
            // Evict oldest (index 0). O(n) due shifting the rest of the map
            // We could use a hashmap + vecdeque to get O(1) here, but then we'd have to handle removing from both maps, storing the index, and managing the eviction.
            // This is a good compromise between performance and simplicity. And thinking about memory usage, this is probably the best way to go.
            self.map.shift_remove_index(0).map(|(k, _)| k)
        } else {
            None
        };
        self.map.insert(key, value);
        (None, evicted_key)
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Removes a key from the map, returning the value if present.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.map.shift_remove(key)
    }

    // This is a wrapper around the IndexMap::iter() method, but it preserves the insertion order of the keys.
    // It's used to iterate over the profiling map in the order of the keys being inserted.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&K, &V)> {
        self.map.iter()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct AccountSnapshot {
    pub lamports: u64,
    pub owner: String,
    pub executable: bool,
    pub rent_epoch: u64,
    /// Base64 encoded data
    pub data: String,
    /// Parsed account data if available
    #[cfg_attr(
        feature = "ts-bindings",
        ts(as = "Option<crate::ts_bindings::ParsedAccountDef>")
    )]
    pub parsed_data: Option<ParsedAccount>,
}

impl AccountSnapshot {
    pub fn new(
        lamports: u64,
        owner: String,
        executable: bool,
        rent_epoch: u64,
        data: String,
        parsed_data: Option<ParsedAccount>,
    ) -> Self {
        Self {
            lamports,
            owner,
            executable,
            rent_epoch,
            data,
            parsed_data,
        }
    }

    /// Convert the snapshot back to a Solana Account
    pub fn to_account(&self) -> Result<solana_account::Account, String> {
        use std::str::FromStr;

        use base64::Engine;
        use solana_pubkey::Pubkey;

        let owner =
            Pubkey::from_str(&self.owner).map_err(|e| format!("Invalid owner pubkey: {}", e))?;

        let data = base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .map_err(|e| format!("Failed to decode base64 data: {}", e))?;

        Ok(solana_account::Account {
            lamports: self.lamports,
            data,
            owner,
            executable: self.executable,
            rent_epoch: self.rent_epoch,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct ExportSnapshotConfig {
    pub include_parsed_accounts: Option<bool>,
    pub filter: Option<ExportSnapshotFilter>,
    pub scope: ExportSnapshotScope,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub enum ExportSnapshotScope {
    #[default]
    Network,
    PreTransaction(String),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct ExportSnapshotFilter {
    pub include_program_accounts: Option<bool>,
    pub include_accounts: Option<Vec<String>>,
    pub exclude_accounts: Option<Vec<String>>,
    /// When true, omit accounts owned by the sysvar program.
    pub exclude_sysvars: Option<bool>,
    /// When true, omit accounts whose pubkey is a known agave feature gate
    /// (as defined by the `agave_feature_set::FEATURE_NAMES` set built into
    /// this surfpool binary). Feature gates added upstream after this version
    /// will not be excluded.
    pub exclude_feature_gates: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct ResetAccountConfig {
    pub include_owned_accounts: Option<bool>,
}

impl Default for ResetAccountConfig {
    fn default() -> Self {
        Self {
            include_owned_accounts: Some(false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct StreamAccountConfig {
    pub include_owned_accounts: Option<bool>,
}

impl Default for StreamAccountConfig {
    fn default() -> Self {
        Self {
            include_owned_accounts: Some(false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct StreamAccountsEntry {
    pub pubkey: String,
    #[serde(default)]
    pub include_owned_accounts: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct OfflineAccountConfig {
    pub include_owned_accounts: Option<bool>,
}

impl Default for OfflineAccountConfig {
    fn default() -> Self {
        Self {
            include_owned_accounts: Some(false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct StreamedAccountInfo {
    pub pubkey: String,
    pub include_owned_accounts: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct GetSurfnetInfoResponse {
    pub runbook_executions: Vec<RunbookExecutionStatusReport>,
    /// Kept out of the generated TypeScript bindings. `SurfnetStartupStatus`
    /// serializes through a hand-written impl that flattens the sum type onto
    /// `{ phase, planSealed, tasks, error }`, and ts-rs derives from the Rust
    /// shape rather than from that impl, so exporting it would need a
    /// hand-maintained `ts(type = ...)` mirroring the wire format in a second
    /// place. TypeScript clients read readiness from `runbookExecutions`,
    /// which is the compatibility path this field exists to make safe.
    #[serde(default)]
    #[cfg_attr(feature = "ts-bindings", ts(skip))]
    pub startup: SurfnetStartupStatus,
}
impl GetSurfnetInfoResponse {
    /// Runbook id of the compatibility entry projected into
    /// `runbook_executions` while startup is in flight. Part of the wire
    /// contract with legacy Anchor; do not rename.
    pub const STARTUP_COMPAT_RUNBOOK_ID: &'static str = "surfpool-startup";

    /// `started_at` is the surfnet's startup time (unix seconds), used to
    /// timestamp the compatibility entry. It must be stable across calls:
    /// clients diff `runbook_executions` between polls, and stamping "now"
    /// on each response made the synthetic entry read as a new execution
    /// every time.
    pub fn with_startup(
        mut runbook_executions: Vec<RunbookExecutionStatusReport>,
        startup: SurfnetStartupStatus,
        started_at: u32,
    ) -> Self {
        // Anchor versions that predate the explicit startup field infer
        // readiness by checking that every runbook execution is complete,
        // in a loop with no timeout that never inspects `errors`. Project
        // the lifecycle into that vocabulary: one pending compatibility
        // entry while startup is in flight, completed with errors on
        // Failed. Leaving the entry pending on Failed would starve that
        // loop forever; completing it means a legacy client proceeds and
        // fails visibly, with the reason recorded for anyone who looks.
        //
        // On Failed, completed_at reuses started_at: the machine does not
        // track the failure instant, the entry is synthetic, and the only
        // contract is non-null; reusing the stable value avoids the same
        // per-poll churn that started_at is guarding against.
        let compat = |completed_at, errors| RunbookExecutionStatusReport {
            started_at,
            completed_at,
            runbook_id: Self::STARTUP_COMPAT_RUNBOOK_ID.into(),
            errors,
        };
        match startup.phase() {
            SurfnetStartupPhase::Ready => {}
            SurfnetStartupPhase::Failed => {
                runbook_executions.push(compat(Some(started_at), Some(startup.failure_messages())))
            }
            SurfnetStartupPhase::Planning
            | SurfnetStartupPhase::Initializing
            | SurfnetStartupPhase::Deploying => runbook_executions.push(compat(None, None)),
        }
        Self {
            runbook_executions,
            startup,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct GetStreamedAccountsResponse {
    accounts: Vec<StreamedAccountInfo>,
}
impl GetStreamedAccountsResponse {
    pub fn from_iter<I>(streamed_accounts: I) -> Self
    where
        I: IntoIterator<Item = (String, bool)>,
    {
        let accounts = streamed_accounts
            .into_iter()
            .map(|(pubkey, include_owned_accounts)| StreamedAccountInfo {
                pubkey,
                include_owned_accounts,
            })
            .collect();
        Self { accounts }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct RunbookExecutionStatusReport {
    #[cfg_attr(feature = "ts-bindings", ts(type = "bigint"))]
    pub started_at: u32,
    #[cfg_attr(feature = "ts-bindings", ts(type = "bigint | null"))]
    pub completed_at: Option<u32>,
    pub runbook_id: String,
    pub errors: Option<Vec<String>>,
}
impl RunbookExecutionStatusReport {
    pub fn new(runbook_id: String) -> Self {
        Self {
            started_at: Local::now().timestamp() as u32,
            completed_at: None,
            runbook_id,
            errors: None,
        }
    }
    pub fn mark_completed(&mut self, error: Option<Vec<String>>) {
        self.completed_at = Some(Local::now().timestamp() as u32);
        self.errors = error;
    }
}

/// Public readiness lifecycle for a surfnet. `Ready` here means the sealed
/// startup plan completed: clones hydrated, deployment runbooks succeeded.
/// Not to be confused with [`SimnetEvent::Ready`], which fires when core
/// startup completes (RPC bound) and can precede this by the entire
/// clone-and-deploy window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfnetStartupPhase {
    #[default]
    Planning,
    Initializing,
    Deploying,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfnetStartupTask {
    RemoteAccounts,
    Deployment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfnetStartupTaskState {
    Pending,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfnetStartupTaskStatus {
    pub task: SurfnetStartupTask,
    pub state: SurfnetStartupTaskState,
    pub error: Option<String>,
}

/// The move a caller tried to make on a task, which a rejection names so the
/// failure says what was attempted rather than only what state blocked it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupTaskTransition {
    Start,
    Complete,
    Fail,
}

impl std::fmt::Display for StartupTaskTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start => write!(f, "start"),
            Self::Complete => write!(f, "complete"),
            Self::Fail => write!(f, "fail"),
        }
    }
}

/// Why the startup machine refused a transition.
///
/// Each variant carries what the caller needs to decide what to do next: a
/// phase, a task, or the state that blocked the move. Callers that only want
/// to report a rejection can use the `Display` text, which is what the trace
/// and the event log carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupError {
    /// The plan is already sealed, and sealing happens once.
    AlreadySealed { phase: SurfnetStartupPhase },
    /// Nothing can move until the plan is sealed.
    NotSealed,
    /// Startup has already finished, in one direction or the other.
    AlreadyTerminal { phase: SurfnetStartupPhase },
    /// The task is not one the sealed plan declared.
    TaskNotPlanned { task: SurfnetStartupTask },
    /// The task cannot make that move from the state it is in.
    TaskState {
        task: SurfnetStartupTask,
        attempted: StartupTaskTransition,
        from: SurfnetStartupTaskState,
    },
    /// Planning has already finished, so it cannot fail now.
    NotPlanning { phase: SurfnetStartupPhase },
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadySealed { phase } => write!(
                f,
                "startup plan can only be sealed once while planning (phase: {phase:?})"
            ),
            Self::NotSealed => write!(f, "startup plan has not been sealed"),
            Self::AlreadyTerminal { phase } => {
                write!(f, "startup is already terminal ({phase:?})")
            }
            Self::TaskNotPlanned { task } => {
                write!(f, "startup task {task:?} is not part of the sealed plan")
            }
            Self::TaskState {
                task,
                attempted,
                from,
            } => write!(f, "startup task {task:?} cannot {attempted} from {from:?}"),
            Self::NotPlanning { phase } => {
                write!(f, "startup planning cannot fail from phase {phase:?}")
            }
        }
    }
}

impl std::error::Error for StartupError {}

/// Lifecycle read model for surfnet startup. The representation carries the
/// issue-715 invariant structurally: the phase is a projection of the
/// variant, and only a sealed plan has a task table to derive `Ready` from,
/// so an unsealed status cannot represent readiness at all. The wire shape
/// is unchanged: the manual `Serialize` impl below projects the same flat
/// `{ phase, planSealed, tasks, error }` object the struct form produced.
///
/// The lifecycle in full, from the spec beside this file. The reachability
/// tests hold the machine to it, and the include anchors the document, so
/// renaming it breaks the build rather than leaving a dead reference:
///
/// ---
///
#[doc = include_str!("startup-lifecycle.md")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SurfnetStartupStatus {
    /// Unsealed: the required task set is not known yet. Never ready.
    #[default]
    Planning,
    /// Planning failed before a plan was sealed. Terminal.
    PlanningFailed { error: String },
    /// The task set is closed; the phase derives from the task states. An
    /// empty sealed plan derives `Ready` immediately.
    Sealed(SealedStartupPlan),
}

/// The flat wire form of [`SurfnetStartupStatus`]. `phase` and `error` are
/// projections of the variant: serialization computes them, and
/// deserialization rebuilds the variant from the authoritative fields
/// (`planSealed`, `tasks`, `error`), ignoring the `phase` a response
/// claims. A client must never manufacture readiness from a malformed
/// response, so an unsealed status deserializes to planning (or to a
/// planning failure when it carries an error) regardless of its phase
/// field.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SurfnetStartupStatusWire {
    #[allow(dead_code)]
    phase: SurfnetStartupPhase,
    plan_sealed: bool,
    tasks: Vec<SurfnetStartupTaskStatus>,
    error: Option<String>,
}

impl Serialize for SurfnetStartupStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SurfnetStartupStatus", 4)?;
        state.serialize_field("phase", &self.phase())?;
        state.serialize_field("planSealed", &self.plan_sealed())?;
        state.serialize_field("tasks", self.tasks())?;
        state.serialize_field("error", &self.error())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SurfnetStartupStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = SurfnetStartupStatusWire::deserialize(deserializer)?;
        Ok(if wire.plan_sealed {
            Self::Sealed(SealedStartupPlan::from_task_statuses(wire.tasks))
        } else if let Some(error) = wire.error {
            Self::PlanningFailed { error }
        } else {
            Self::Planning
        })
    }
}

/// A startup plan whose task set is fixed.
///
/// The task operations live here rather than on [`SurfnetStartupStatus`], so
/// holding one of these is proof the plan was sealed. Callers reach it through
/// [`SurfnetStartupStatus::sealed_mut`], which is the single place the seal is
/// checked; nothing downstream re-derives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedStartupPlan {
    tasks: Vec<SurfnetStartupTaskStatus>,
}

impl SealedStartupPlan {
    /// Seals `tasks`, dropping repeats: a task named twice is one obligation.
    fn new(tasks: Vec<SurfnetStartupTask>) -> Self {
        let mut statuses: Vec<SurfnetStartupTaskStatus> = vec![];
        for task in tasks {
            if statuses.iter().any(|status| status.task == task) {
                continue;
            }
            statuses.push(SurfnetStartupTaskStatus {
                task,
                state: SurfnetStartupTaskState::Pending,
                error: None,
            });
        }
        Self { tasks: statuses }
    }

    /// Rebuilds a plan from a wire payload, which may say anything. Callers
    /// deriving readiness from it get whatever the task states imply, which is
    /// the point: the phase is never taken on trust.
    fn from_task_statuses(tasks: Vec<SurfnetStartupTaskStatus>) -> Self {
        Self { tasks }
    }

    pub fn tasks(&self) -> &[SurfnetStartupTaskStatus] {
        &self.tasks
    }

    // Phase derivation encodes task ordering: a non-succeeded RemoteAccounts
    // pins the phase at Initializing, and Deploying is the residual case.
    // Adding a task variant requires deciding where it sits in this ordering.
    fn phase(&self) -> SurfnetStartupPhase {
        if self
            .tasks
            .iter()
            .any(|status| status.state == SurfnetStartupTaskState::Failed)
        {
            SurfnetStartupPhase::Failed
        } else if self
            .tasks
            .iter()
            .all(|status| status.state == SurfnetStartupTaskState::Succeeded)
        {
            SurfnetStartupPhase::Ready
        } else if self.tasks.iter().any(|status| {
            status.task == SurfnetStartupTask::RemoteAccounts
                && status.state != SurfnetStartupTaskState::Succeeded
        }) {
            SurfnetStartupPhase::Initializing
        } else {
            SurfnetStartupPhase::Deploying
        }
    }

    fn error(&self) -> Option<&str> {
        self.tasks
            .iter()
            .find(|status| status.state == SurfnetStartupTaskState::Failed)
            .and_then(|status| status.error.as_deref())
    }

    fn failure_messages(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter_map(|status| status.error.clone())
            .collect()
    }

    /// A terminal plan accepts no further transitions. Sealing is not checked
    /// here: this type only exists sealed.
    fn ensure_active(&self) -> Result<(), StartupError> {
        let phase = self.phase();
        if matches!(
            phase,
            SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
        ) {
            return Err(StartupError::AlreadyTerminal { phase });
        }
        Ok(())
    }

    fn task_mut(
        &mut self,
        task: SurfnetStartupTask,
    ) -> Result<&mut SurfnetStartupTaskStatus, StartupError> {
        self.tasks
            .iter_mut()
            .find(|status| status.task == task)
            .ok_or(StartupError::TaskNotPlanned { task })
    }

    pub fn start_task(&mut self, task: SurfnetStartupTask) -> Result<(), StartupError> {
        self.ensure_active()?;
        let status = self.task_mut(task)?;
        if status.state != SurfnetStartupTaskState::Pending {
            return Err(StartupError::TaskState {
                task,
                attempted: StartupTaskTransition::Start,
                from: status.state,
            });
        }
        status.state = SurfnetStartupTaskState::Running;
        Ok(())
    }

    pub fn complete_task(&mut self, task: SurfnetStartupTask) -> Result<(), StartupError> {
        self.ensure_active()?;
        let status = self.task_mut(task)?;
        if status.state != SurfnetStartupTaskState::Running {
            return Err(StartupError::TaskState {
                task,
                attempted: StartupTaskTransition::Complete,
                from: status.state,
            });
        }
        status.state = SurfnetStartupTaskState::Succeeded;
        Ok(())
    }

    pub fn fail_task(
        &mut self,
        task: SurfnetStartupTask,
        error: impl Into<String>,
    ) -> Result<(), StartupError> {
        let error = error.into();
        self.ensure_active()?;
        let status = self.task_mut(task)?;
        if !matches!(
            status.state,
            SurfnetStartupTaskState::Pending | SurfnetStartupTaskState::Running
        ) {
            return Err(StartupError::TaskState {
                task,
                attempted: StartupTaskTransition::Fail,
                from: status.state,
            });
        }
        status.state = SurfnetStartupTaskState::Failed;
        status.error = Some(error);
        Ok(())
    }
}

impl SurfnetStartupStatus {
    pub fn phase(&self) -> SurfnetStartupPhase {
        match self {
            Self::Planning => SurfnetStartupPhase::Planning,
            Self::PlanningFailed { .. } => SurfnetStartupPhase::Failed,
            Self::Sealed(plan) => plan.phase(),
        }
    }

    pub fn plan_sealed(&self) -> bool {
        matches!(self, Self::Sealed(_))
    }

    /// The sealed task table; empty until the plan is sealed.
    pub fn tasks(&self) -> &[SurfnetStartupTaskStatus] {
        match self {
            Self::Sealed(plan) => plan.tasks(),
            _ => &[],
        }
    }

    /// The machine-level failure, when the phase is `Failed`: the planning
    /// error, or the failed task's error (at most one task can fail; the
    /// first failure is terminal).
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Planning => None,
            Self::PlanningFailed { error } => Some(error),
            Self::Sealed(plan) => plan.error(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.phase() == SurfnetStartupPhase::Ready
    }

    /// Failure messages for presentation: each failed task's error; a
    /// planning failure has no task, so its error stands alone.
    pub fn failure_messages(&self) -> Vec<String> {
        match self {
            Self::Planning => vec![],
            Self::PlanningFailed { error } => vec![error.clone()],
            Self::Sealed(plan) => plan.failure_messages(),
        }
    }

    /// The one place the seal is checked. Everything a caller can do to a
    /// sealed plan happens through the returned handle, so no task operation
    /// re-derives sealedness.
    pub fn sealed_mut(&mut self) -> Result<&mut SealedStartupPlan, StartupError> {
        match self {
            Self::Sealed(plan) => Ok(plan),
            _ => Err(StartupError::NotSealed),
        }
    }

    pub fn seal_plan(&mut self, tasks: Vec<SurfnetStartupTask>) -> Result<(), StartupError> {
        if !matches!(self, Self::Planning) {
            return Err(StartupError::AlreadySealed {
                phase: self.phase(),
            });
        }
        *self = Self::Sealed(SealedStartupPlan::new(tasks));
        Ok(())
    }

    pub fn start_task(&mut self, task: SurfnetStartupTask) -> Result<(), StartupError> {
        self.sealed_mut()?.start_task(task)
    }

    pub fn complete_task(&mut self, task: SurfnetStartupTask) -> Result<(), StartupError> {
        self.sealed_mut()?.complete_task(task)
    }

    pub fn fail_task(
        &mut self,
        task: SurfnetStartupTask,
        error: impl Into<String>,
    ) -> Result<(), StartupError> {
        self.sealed_mut()?.fail_task(task, error)
    }

    pub fn fail_planning(&mut self, error: impl Into<String>) -> Result<(), StartupError> {
        let error = error.into();
        if !matches!(self, Self::Planning) {
            return Err(StartupError::NotPlanning {
                phase: self.phase(),
            });
        }
        *self = Self::PlanningFailed { error };
        Ok(())
    }
}

#[cfg(test)]
mod surfnet_startup_status_tests {
    use super::*;

    #[test]
    fn an_unsealed_empty_plan_is_not_ready() {
        let status = SurfnetStartupStatus::default();
        assert_eq!(status.phase(), SurfnetStartupPhase::Planning);
        assert!(!status.is_ready());
    }

    #[test]
    fn a_sealed_empty_plan_is_ready() {
        let mut status = SurfnetStartupStatus::default();
        status.seal_plan(vec![]).unwrap();
        assert!(status.is_ready());
    }

    #[test]
    fn required_tasks_enforce_initialization_then_deployment() {
        let mut status = SurfnetStartupStatus::default();
        status
            .seal_plan(vec![
                SurfnetStartupTask::RemoteAccounts,
                SurfnetStartupTask::Deployment,
            ])
            .unwrap();
        assert_eq!(status.phase(), SurfnetStartupPhase::Initializing);

        status
            .start_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap();
        status
            .complete_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap();
        assert_eq!(status.phase(), SurfnetStartupPhase::Deploying);

        status.start_task(SurfnetStartupTask::Deployment).unwrap();
        status
            .complete_task(SurfnetStartupTask::Deployment)
            .unwrap();
        assert!(status.is_ready());
    }

    #[test]
    fn failed_tasks_are_terminal() {
        let mut status = SurfnetStartupStatus::default();
        status
            .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
            .unwrap();
        status
            .start_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap();
        status
            .fail_task(SurfnetStartupTask::RemoteAccounts, "datasource unavailable")
            .unwrap();

        assert_eq!(status.phase(), SurfnetStartupPhase::Failed);
        assert_eq!(status.error(), Some("datasource unavailable"));
        assert_eq!(
            status.complete_task(SurfnetStartupTask::RemoteAccounts),
            Err(StartupError::AlreadyTerminal {
                phase: SurfnetStartupPhase::Failed
            })
        );
    }

    /// Each rejection names why, so a test that stops holding is one that
    /// changed which rule fired rather than one that merely still refuses.
    #[test]
    fn illegal_transitions_are_rejected() {
        let mut status = SurfnetStartupStatus::default();
        assert_eq!(
            status.start_task(SurfnetStartupTask::RemoteAccounts),
            Err(StartupError::NotSealed)
        );

        status
            .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
            .unwrap();
        assert_eq!(
            status.complete_task(SurfnetStartupTask::RemoteAccounts),
            Err(StartupError::TaskState {
                task: SurfnetStartupTask::RemoteAccounts,
                attempted: StartupTaskTransition::Complete,
                from: SurfnetStartupTaskState::Pending
            })
        );
        assert_eq!(
            status.seal_plan(vec![SurfnetStartupTask::Deployment]),
            Err(StartupError::AlreadySealed {
                phase: SurfnetStartupPhase::Initializing
            })
        );
    }

    // The flat wire shape is contract: clients read `planSealed` and
    // `phase` as plain fields (the sdk and mcp integration tests pin the
    // same shape end to end). The representation is an enum, so the manual
    // Serialize impl must keep projecting the object the struct form
    // produced.
    #[test]
    fn serializes_to_the_flat_wire_shape() {
        let mut sealed = SurfnetStartupStatus::default();
        sealed
            .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
            .unwrap();
        let json = serde_json::to_value(&sealed).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "phase": "initializing",
                "planSealed": true,
                "tasks": [
                    { "task": "remoteAccounts", "state": "pending", "error": null }
                ],
                "error": null,
            })
        );
    }

    #[test]
    fn deserializes_from_the_shape_it_serializes() {
        let mut sealed_failed = SurfnetStartupStatus::default();
        sealed_failed
            .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
            .unwrap();
        sealed_failed
            .start_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap();
        sealed_failed
            .fail_task(SurfnetStartupTask::RemoteAccounts, "boom")
            .unwrap();

        let mut planning_failed = SurfnetStartupStatus::default();
        planning_failed.fail_planning("boom").unwrap();

        let mut ready = SurfnetStartupStatus::default();
        ready.seal_plan(vec![]).unwrap();

        for status in [
            SurfnetStartupStatus::default(),
            planning_failed,
            sealed_failed,
            ready,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: SurfnetStartupStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status, "round trip changed the status: {json}");
        }
    }

    // The wire `phase` is derived output, so deserialization rebuilds the
    // variant from `planSealed` and the task table and ignores the phase a
    // response claims. The safe direction matters: a malformed unsealed
    // response must never manufacture readiness on the client side.
    #[test]
    fn deserialization_never_manufactures_readiness_from_an_unsealed_status() {
        let json = r#"{"phase":"ready","planSealed":false,"tasks":[],"error":null}"#;
        let status: SurfnetStartupStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.phase(), SurfnetStartupPhase::Planning);
        assert!(!status.is_ready());
    }

    const STARTED_AT: u32 = 1_753_000_000;

    #[test]
    fn legacy_anchor_sees_startup_as_pending_until_ready() {
        let planning = GetSurfnetInfoResponse::with_startup(
            vec![],
            SurfnetStartupStatus::default(),
            STARTED_AT,
        );
        assert_eq!(planning.runbook_executions.len(), 1);
        assert_eq!(
            planning.runbook_executions[0].runbook_id,
            GetSurfnetInfoResponse::STARTUP_COMPAT_RUNBOOK_ID
        );
        assert!(planning.runbook_executions[0].completed_at.is_none());

        let mut ready = SurfnetStartupStatus::default();
        ready.seal_plan(vec![]).unwrap();
        let ready_response = GetSurfnetInfoResponse::with_startup(vec![], ready, STARTED_AT);
        assert!(ready_response.runbook_executions.is_empty());
    }

    // The compat entry must be identical from poll to poll: clients diff
    // runbook_executions between responses, and a churning timestamp made
    // the synthetic entry read as a new execution every 500ms.
    #[test]
    fn compat_entry_is_stable_across_polls() {
        let mut failed = SurfnetStartupStatus::default();
        failed.fail_planning("boom").unwrap();

        for status in [SurfnetStartupStatus::default(), failed] {
            let first = GetSurfnetInfoResponse::with_startup(vec![], status.clone(), STARTED_AT);
            let second = GetSurfnetInfoResponse::with_startup(vec![], status, STARTED_AT);
            assert_eq!(first.runbook_executions, second.runbook_executions);
            assert_eq!(first.runbook_executions[0].started_at, STARTED_AT);
        }
    }

    // A pending compat entry on Failed would starve legacy Anchor's readiness
    // loop, which has no timeout; the entry must complete, with the reason
    // recorded in `errors`.
    #[test]
    fn legacy_anchor_sees_startup_failure_as_completed_with_errors() {
        let mut failed = SurfnetStartupStatus::default();
        failed
            .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
            .unwrap();
        failed
            .start_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap();
        failed
            .fail_task(SurfnetStartupTask::RemoteAccounts, "datasource unavailable")
            .unwrap();

        let response = GetSurfnetInfoResponse::with_startup(vec![], failed, STARTED_AT);
        assert_eq!(response.runbook_executions.len(), 1);
        let compat = &response.runbook_executions[0];
        assert_eq!(
            compat.runbook_id,
            GetSurfnetInfoResponse::STARTUP_COMPAT_RUNBOOK_ID
        );
        assert_eq!(compat.completed_at, Some(STARTED_AT));
        assert_eq!(
            compat.errors,
            Some(vec!["datasource unavailable".to_string()])
        );
    }

    // fail_planning has no task to carry the error, so the machine-level
    // error must reach the compat entry on its own.
    #[test]
    fn legacy_anchor_sees_planning_failure_as_completed_with_errors() {
        let mut failed = SurfnetStartupStatus::default();
        failed.fail_planning("could not detect framework").unwrap();

        let response = GetSurfnetInfoResponse::with_startup(vec![], failed, STARTED_AT);
        assert_eq!(response.runbook_executions.len(), 1);
        let compat = &response.runbook_executions[0];
        assert_eq!(compat.completed_at, Some(STARTED_AT));
        assert_eq!(
            compat.errors,
            Some(vec!["could not detect framework".to_string()])
        );
    }
}

/// Exhaustive model check of the startup state machine. The reachable state
/// space is finite and small (two task kinds, four task states, five phases),
/// so a breadth-first search from the default state can verify the startup
/// invariants at every reachable state and along every accepted transition,
/// with no sampling involved.
///
/// The race in issue 715 was a history property: no sequence of transitions
/// may let a client observe readiness while declared work is outstanding.
/// This sweep checks state invariants instead, which suffices because the
/// projection a client reads is a pure function of the current state and
/// every reachable state is visited. If readiness ever acquires memory of
/// its own (a cache, a debounce, an asynchronous publish), that reduction
/// stops holding, and forbidden histories need checking directly.
#[cfg(test)]
mod surfnet_startup_reachability_tests {
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    use super::*;

    /// A command's display label, the spec event it maps to, and its
    /// application.
    type Command = (
        String,
        &'static str,
        Box<dyn Fn(&mut SurfnetStartupStatus) -> Result<(), StartupError>>,
    );

    /// Any fixed instant. The projection check below cares whether an entry
    /// is present, not when it started.
    const STARTED_AT: u32 = 1_753_000_000;

    /// Checks the compatibility projection against the startup phase, using
    /// the rule Anchor applies: it proceeds when every entry in
    /// `runbookExecutions` is complete. It may proceed exactly when startup is
    /// over.
    ///
    /// `Failed` counts as over. A pending entry there would park a client in a
    /// readiness loop that has no timeout, so the entry completes and the
    /// reason is reported in `errors`.
    ///
    /// Stated in terms of Anchor's predicate rather than an empty list,
    /// because an empty list is only one way to satisfy it. An entry marked
    /// complete regardless of phase satisfies it too, and a check written
    /// against emptiness does not detect that.
    ///
    /// Used by the model check on every reachable state and by
    /// `the_forbidden_pairing_is_one_we_can_build` on a hand-built violation.
    fn compat_list_agrees_with_phase(
        runbook_executions: &[RunbookExecutionStatusReport],
        phase: SurfnetStartupPhase,
    ) -> bool {
        let anchor_would_proceed = runbook_executions
            .iter()
            .all(|execution| execution.completed_at.is_some());
        let startup_is_over = matches!(
            phase,
            SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
        );
        anchor_would_proceed == startup_is_over
    }

    /// The full command alphabet. Plans cover every subset of the two task
    /// kinds, both orderings of the two-task plan, and a duplicate entry to
    /// exercise deduplication. Failure commands use a fixed error string so
    /// the reachable state space stays finite.
    fn commands() -> Vec<Command> {
        use SurfnetStartupTask::*;

        let plans: [&[SurfnetStartupTask]; 6] = [
            &[],
            &[RemoteAccounts],
            &[Deployment],
            &[RemoteAccounts, Deployment],
            &[Deployment, RemoteAccounts],
            &[RemoteAccounts, RemoteAccounts],
        ];

        let mut commands: Vec<Command> = vec![];
        for plan in plans {
            let plan = plan.to_vec();
            commands.push((
                format!("seal_plan({plan:?})"),
                "StartupPlanSealed",
                Box::new(move |status| status.seal_plan(plan.clone())),
            ));
        }
        for task in [RemoteAccounts, Deployment] {
            commands.push((
                format!("start_task({task:?})"),
                "StartupTaskStarted",
                Box::new(move |status| status.start_task(task)),
            ));
            commands.push((
                format!("complete_task({task:?})"),
                "StartupTaskSucceeded",
                Box::new(move |status| status.complete_task(task)),
            ));
            commands.push((
                format!("fail_task({task:?})"),
                "StartupTaskFailed",
                Box::new(move |status| status.fail_task(task, "boom")),
            ));
        }
        commands.push((
            "fail_planning".to_string(),
            "StartupFailed",
            Box::new(|status| status.fail_planning("boom")),
        ));
        commands
    }

    /// Progress order for the monotonicity check. `Failed` is handled
    /// separately: it is reachable from any non-terminal phase.
    fn phase_rank(phase: SurfnetStartupPhase) -> u8 {
        match phase {
            SurfnetStartupPhase::Planning => 0,
            SurfnetStartupPhase::Initializing => 1,
            SurfnetStartupPhase::Deploying => 2,
            SurfnetStartupPhase::Ready => 3,
            SurfnetStartupPhase::Failed => u8::MAX,
        }
    }

    /// Spec oracle: the phase a state must be in, written from the projection
    /// in `startup-lifecycle.md`, the SOT, so the assertion below enforces
    /// "implementation matches spec", not "code matches itself":
    ///
    /// 1. A failure at any stage is terminal.
    /// 2. An unsealed plan is still planning; it can never be ready, even
    ///    when its task collection is empty.
    /// 3. A sealed plan with every required task succeeded is ready (the
    ///    empty plan is ready immediately).
    /// 4. Otherwise, pending hydration means initializing; after hydration,
    ///    deploying.
    fn expected_phase(status: &SurfnetStartupStatus) -> SurfnetStartupPhase {
        let failed = status.error().is_some()
            || status
                .tasks()
                .iter()
                .any(|task| task.state == SurfnetStartupTaskState::Failed);
        if failed {
            return SurfnetStartupPhase::Failed;
        }
        if !status.plan_sealed() {
            return SurfnetStartupPhase::Planning;
        }
        if status
            .tasks()
            .iter()
            .all(|task| task.state == SurfnetStartupTaskState::Succeeded)
        {
            return SurfnetStartupPhase::Ready;
        }
        if status.tasks().iter().any(|task| {
            task.task == SurfnetStartupTask::RemoteAccounts
                && task.state != SurfnetStartupTaskState::Succeeded
        }) {
            return SurfnetStartupPhase::Initializing;
        }
        SurfnetStartupPhase::Deploying
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
            expected_phase(status),
            "phase disagrees with the spec oracle: {status:?}"
        );

        // The projection a client reads, checked at every reachable state. The
        // oracle above ties Ready to a sealed plan with every task succeeded;
        // this ties what Anchor concludes to the phase. Together they make a
        // sealed plan with clones declared unable to report itself finished.
        let projected = GetSurfnetInfoResponse::with_startup(vec![], status.clone(), STARTED_AT);
        assert!(
            compat_list_agrees_with_phase(&projected.runbook_executions, status.phase()),
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

    /// Rewrites the observed block in `startup-lifecycle.md` from a fresh
    /// sweep. Ignored so a plain test run never writes to the source tree;
    /// `cargo surfpool-update-startup-spec` runs it explicitly.
    #[test]
    #[ignore = "writes startup-lifecycle.md; run via cargo surfpool-update-startup-spec"]
    fn regenerate_the_observed_block() {
        sweep().write_to_spec();
    }

    /// Walks every reachable startup state, asserting the invariants along
    /// the way, and returns what the walk observed.
    fn sweep() -> Observed {
        let commands = commands();
        let initial = SurfnetStartupStatus::default();

        let mut seen = HashSet::new();
        seen.insert(format!("{initial:?}"));
        let mut frontier = vec![initial];
        let mut visited = 0usize;
        let mut accepted = 0usize;
        let mut reached = HashSet::new();
        let mut events: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        let mut successors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        while let Some(state) = frontier.pop() {
            visited += 1;
            assert_state_invariants(&state);
            reached.insert(format!("{:?}", state.phase()));
            let terminal = matches!(
                state.phase(),
                SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
            );

            for (label, event, apply) in &commands {
                let mut next = state.clone();
                match apply(&mut next) {
                    Ok(()) => {
                        accepted += 1;
                        let phase = format!("{:?}", state.phase());
                        events.entry(phase.clone()).or_default().insert(event);
                        successors
                            .entry(phase)
                            .or_default()
                            .insert(format!("{:?}", next.phase()));
                        assert!(!terminal, "{label} accepted from terminal state: {state:?}");
                        if next.phase() != SurfnetStartupPhase::Failed {
                            assert!(
                                phase_rank(next.phase()) >= phase_rank(state.phase()),
                                "{label} regressed the phase: {state:?} -> {next:?}"
                            );
                        }
                        if seen.insert(format!("{next:?}")) {
                            frontier.push(next);
                        }
                    }
                    Err(_) => {
                        // A rejected transition must leave the state untouched;
                        // the watch-channel publisher relies on this.
                        assert_eq!(next, state, "{label} was rejected but mutated the state");
                    }
                }
            }
        }

        // A shrunken search would pass every assertion above while checking
        // nothing, so require the full space: every phase reachable, and the
        // spec's observed block equal to what this sweep just observed.
        for phase in [
            SurfnetStartupPhase::Planning,
            SurfnetStartupPhase::Initializing,
            SurfnetStartupPhase::Deploying,
            SurfnetStartupPhase::Ready,
            SurfnetStartupPhase::Failed,
        ] {
            assert!(
                reached.contains(&format!("{phase:?}")),
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

    /// What one full sweep observed, in the vocabulary the spec's tables use.
    struct Observed {
        visited: usize,
        attempted: usize,
        accepted: usize,
        events: BTreeMap<String, BTreeSet<&'static str>>,
        successors: BTreeMap<String, BTreeSet<String>>,
    }

    impl Observed {
        /// Renders the observed block of `startup-lifecycle.md`: the phase
        /// transition table and the counts line. An empty event set renders
        /// as "nothing" and an empty successor set as "terminal", so the
        /// terminal rows are derived rather than declared.
        fn render(&self) -> String {
            const PHASES: [&str; 5] =
                ["Planning", "Initializing", "Deploying", "Ready", "Failed"];
            let join = |set: Option<Vec<String>>, empty: &str| {
                set.filter(|items| !items.is_empty())
                    .map(|items| items.join(", "))
                    .unwrap_or_else(|| empty.to_string())
            };

            let headers = ["Phase", "Accepts", "Can lead to"];
            let rows: Vec<[String; 3]> = PHASES
                .iter()
                .map(|phase| {
                    [
                        phase.to_string(),
                        join(
                            self.events
                                .get(*phase)
                                .map(|set| set.iter().map(|event| event.to_string()).collect()),
                            "nothing",
                        ),
                        join(
                            self.successors.get(*phase).map(|set| set.iter().cloned().collect()),
                            "terminal",
                        ),
                    ]
                })
                .collect();

            let mut widths = headers.map(str::len);
            for row in &rows {
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

            let mut block = format_row(headers);
            block.push_str(&format!(
                "|{}|{}|{}|\n",
                "-".repeat(widths[0] + 2),
                "-".repeat(widths[1] + 2),
                "-".repeat(widths[2] + 2),
            ));
            for row in &rows {
                block.push_str(&format_row([&row[0], &row[1], &row[2]]));
            }
            block.push_str(&format!(
                "\n{} reachable states, {} attempted transitions, {} accepted.\n",
                self.visited, self.attempted, self.accepted,
            ));
            block
        }

        /// Compares the rendered block with the one recorded in the spec.
        /// The spec claims the block is derived from this sweep; holding the
        /// two equal is what keeps that claim true.
        fn check_against_spec(&self) {
            let (spec, start, end) = spec_region();
            let rendered = self.render();
            assert!(
                spec[start..end] == rendered,
                "the observed block in startup-lifecycle.md disagrees with \
                 the sweep. Expected:\n\n{rendered}\nRun `cargo \
                 surfpool-update-startup-spec` to regenerate it."
            );
        }

        /// Rewrites the recorded block in place from this sweep's
        /// observations, leaving everything outside the markers untouched.
        fn write_to_spec(&self) {
            let (spec, start, end) = spec_region();
            let updated = format!("{}{}{}", &spec[..start], self.render(), &spec[end..]);
            std::fs::write(SPEC_PATH, updated)
                .unwrap_or_else(|error| panic!("could not write {SPEC_PATH}: {error}"));
            eprintln!("regenerated the observed block in {SPEC_PATH}");
        }
    }

    const SPEC_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/startup-lifecycle.md");

    /// Reads the spec and locates the observed block, returning the file's
    /// content and the byte range between the block's markers.
    fn spec_region() -> (String, usize, usize) {
        const BEGIN: &str = "<!-- BEGIN GENERATED: observed -->\n";
        const END: &str = "<!-- END GENERATED: observed -->";

        let spec = std::fs::read_to_string(SPEC_PATH)
            .unwrap_or_else(|error| panic!("could not read {SPEC_PATH}: {error}"));
        let start = spec
            .find(BEGIN)
            .unwrap_or_else(|| panic!("{SPEC_PATH} has no {BEGIN:?} marker"))
            + BEGIN.len();
        let end = spec[start..]
            .find(END)
            .unwrap_or_else(|| panic!("{SPEC_PATH} has no {END:?} marker"))
            + start;
        (spec, start, end)
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
        assert_eq!(outstanding.phase(), SurfnetStartupPhase::Initializing);

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
            !compat_list_agrees_with_phase(
                &forbidden.runbook_executions,
                forbidden.startup.phase()
            ),
            "the rule accepted the pairing issue 715 was reported as"
        );

        // The projection the surfnet actually answers through never builds it.
        let answered =
            GetSurfnetInfoResponse::with_startup(vec![], outstanding.clone(), STARTED_AT);
        assert!(
            compat_list_agrees_with_phase(&answered.runbook_executions, outstanding.phase()),
            "the projection produced the forbidden pairing: {answered:?}"
        );
    }
}

/// WebSocket subscription counts
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct WsSubscriptions {
    pub signatures: usize,
    pub accounts: usize,
    pub slots: usize,
    pub logs: usize,
}

/// Surfpool node status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfpoolStatus {
    pub slot: u64,
    pub epoch: u64,
    pub slot_index: u64,
    pub transactions_count: u64,
    pub transactions_processed: u64,
    pub uptime_ms: u64,
    pub ws_subscriptions: WsSubscriptions,
}

#[cfg(feature = "prometheus")]
fn default_prometheus_addr() -> String {
    "0.0.0.0:9000".to_string()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheatcodeConfig {
    pub lockout: bool, // if true, allows disabling even the `surfnet_enableCheatcodes`/`surfnetdisableCheatcodes` methods
    pub filter: CheatcodeFilter,
}

#[derive(Serialize, Deserialize, Default)]
#[cfg_attr(
    feature = "ts-bindings",
    derive(ts_rs::TS),
    ts(export, optional_fields)
)]
pub struct CheatcodeControlConfig {
    pub lockout: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CheatcodeFilter {
    All(String),
    List(Vec<String>), // disables cheatcodes in a named list
}

/// Canonical list of the `surfnet_*` cheatcode JSON-RPC methods.
///
/// This is the single source of truth for downstream bindings (e.g. the
/// generated TypeScript method manifest). A test in
/// `surfpool-core/src/rpc/surfnet_cheatcodes.rs` asserts it matches the
/// methods actually registered by the `SurfnetCheatcodes` trait, so adding,
/// removing, or renaming a cheatcode without updating this list fails CI.
pub const SURFNET_CHEATCODE_METHODS: [&str; 26] = [
    "surfnet_cloneProgramAccount",
    "surfnet_disableCheatcode",
    "surfnet_enableCheatcode",
    "surfnet_exportSnapshot",
    "surfnet_getActiveIdl",
    "surfnet_getLocalSignatures",
    "surfnet_getProfileResultsByTag",
    "surfnet_getStreamedAccounts",
    "surfnet_getSurfnetInfo",
    "surfnet_getTransactionProfile",
    "surfnet_offlineAccount",
    "surfnet_pauseClock",
    "surfnet_profileTransaction",
    "surfnet_registerIdl",
    "surfnet_registerScenario",
    "surfnet_resetAccount",
    "surfnet_resetNetwork",
    "surfnet_resumeClock",
    "surfnet_setAccount",
    "surfnet_setProgramAuthority",
    "surfnet_setSupply",
    "surfnet_setTokenAccount",
    "surfnet_streamAccount",
    "surfnet_streamAccounts",
    "surfnet_timeTravel",
    "surfnet_writeProgram",
];

impl CheatcodeConfig {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(CheatcodeConfig {
            lockout: false,
            filter: CheatcodeFilter::List(vec![]),
        }))
    }

    pub fn lockout(&mut self) {
        self.lockout = true;
    }

    pub fn disable_all(&mut self, lockout: bool, available_cheatcodes: Vec<String>) {
        if lockout {
            self.lockout = true;
        }
        self.filter = Self::filter_all_list(lockout, available_cheatcodes);
    }

    pub fn disable_cheatcode(&mut self, cheatcode: &String) -> Result<(), String> {
        if !self.lockout
            && (cheatcode.eq("surfnet_enableCheatcode") || cheatcode.eq("surfnet_disableCheatcode"))
        {
            return Err("Cannot disable surfnet_disableCheatcode or surfnet_enableCheatcode while lockout is false".to_string());
        }

        if let CheatcodeFilter::List(list) = &mut self.filter {
            if !list.contains(cheatcode) {
                list.push(cheatcode.to_string());
                Ok(())
            } else {
                Err("Cheatcode already disabled".to_string())
            }
        } else {
            Err("All cheatcodes disabled".to_string())
        }
    }
    pub fn enable_cheatcode(&mut self, cheatcode: &str) -> Result<(), String> {
        if let CheatcodeFilter::List(list) = &mut self.filter {
            if let Some(pos) = list.iter().position(|c| c == cheatcode) {
                list.remove(pos);
                Ok(())
            } else {
                Err("Cheatcode isn't disabled".to_string())
            }
        } else {
            Err("All cheatcodes are disabled".to_string())
        }
    }

    pub fn is_cheatcode_disabled(&self, cheatcode: &String) -> bool {
        match &self.filter {
            CheatcodeFilter::List(list) => list.contains(cheatcode),
            CheatcodeFilter::All(_) => true,
        }
    }

    pub fn filter_all_list(lockout: bool, available_cheatcodes: Vec<String>) -> CheatcodeFilter {
        // when lockout == true, it's important to disable surfnet_disableCheatcode as well
        // since calling surfnet_disableCheatcode with lockout == false will override the current config, which is a bug
        if lockout {
            CheatcodeFilter::All("all".to_string())
        } else {
            // remove `surfnet_disableCheatcode` and `surfnet_enableCheatcode` from the list of available cheatcodes
            let filter = available_cheatcodes
                .into_iter()
                .filter(|c| c.ne("surfnet_disableCheatcode") && c.ne("surfnet_enableCheatcode"))
                .collect();
            CheatcodeFilter::List(filter)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use solana_account_decoder_client_types::{ParsedAccount, UiAccountData};

    use super::*;

    #[test]
    fn test_disable_cheatcode_with_lockout_allows_protected_methods() {
        // This test catches the bug where lockout was not propagated to
        // CheatcodeConfig before calling disable_cheatcode(), causing
        // "Cannot disable surfnet_disableCheatcode or surfnet_enableCheatcode
        // while lockout is false" even when the request included lockout: true.
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        // Simulate the RPC layer propagating lockout before processing the list
        config.lockout();

        // These should succeed because lockout is set
        assert!(
            config
                .disable_cheatcode(&"surfnet_setAccount".to_string())
                .is_ok()
        );
        assert!(
            config
                .disable_cheatcode(&"surfnet_enableCheatcode".to_string())
                .is_ok()
        );
        assert!(
            config
                .disable_cheatcode(&"surfnet_disableCheatcode".to_string())
                .is_ok()
        );
    }

    #[test]
    fn test_disable_cheatcode_without_lockout_rejects_protected_methods() {
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        // Without lockout, disabling protected methods should fail
        assert!(
            config
                .disable_cheatcode(&"surfnet_enableCheatcode".to_string())
                .is_err()
        );
        assert!(
            config
                .disable_cheatcode(&"surfnet_disableCheatcode".to_string())
                .is_err()
        );

        // But regular cheatcodes should still work
        assert!(
            config
                .disable_cheatcode(&"surfnet_setAccount".to_string())
                .is_ok()
        );
    }

    #[test]
    fn test_disable_all_with_lockout_persists_lockout_flag() {
        // This test catches the bug where disable_all() did not set
        // self.lockout = true, so subsequent operations would not see lockout.
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        let available = vec![
            "surfnet_setAccount".to_string(),
            "surfnet_enableCheatcode".to_string(),
            "surfnet_disableCheatcode".to_string(),
        ];

        config.disable_all(true, available);
        assert!(config.lockout);
    }

    #[test]
    fn test_disable_all_without_lockout_does_not_set_lockout() {
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        let available = vec![
            "surfnet_setAccount".to_string(),
            "surfnet_enableCheatcode".to_string(),
            "surfnet_disableCheatcode".to_string(),
        ];

        config.disable_all(false, available);
        assert!(!config.lockout);
    }

    #[test]
    fn print_ui_keyed_profile_result() {
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let readonly_account_state = UiAccount {
            lamports: 100,
            data: UiAccountData::Binary(
                "ABCDEFG".into(),
                solana_account_decoder_client_types::UiAccountEncoding::Base64,
            ),
            owner: owner.to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(100),
        };

        let account_1 = UiAccount {
            lamports: 100,
            data: UiAccountData::Json(ParsedAccount {
                program: "custom-program".into(),
                parsed: json!({
                    "field1": "value1",
                    "field2": "value2"
                }),
                space: 50,
            }),
            owner: owner.to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(100),
        };

        let account_2 = UiAccount {
            lamports: 100,
            data: UiAccountData::Json(ParsedAccount {
                program: "custom-program".into(),
                parsed: json!({
                    "field1": "updated-value1",
                    "field2": "updated-value2"
                }),
                space: 50,
            }),
            owner: owner.to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(100),
        };
        let profile_result = UiKeyedProfileResult {
            slot: 123,
            key: UuidOrSignature::Uuid(Uuid::new_v4()),
            instruction_profiles: Some(vec![
                UiProfileResult {
                    account_states: IndexMap::from_iter([
                        (
                            pubkey,
                            UiAccountProfileState::Writable(UiAccountChange::Create(
                                account_1.clone(),
                            )),
                        ),
                        (owner, UiAccountProfileState::Readonly),
                    ]),
                    compute_units_consumed: 100,
                    log_messages: Some(vec![
                        "Log message: Creating Account".to_string(),
                        "Log message: Account created".to_string(),
                    ]),
                    error_message: None,
                },
                UiProfileResult {
                    account_states: IndexMap::from_iter([
                        (
                            pubkey,
                            UiAccountProfileState::Writable(UiAccountChange::Update(
                                account_1,
                                account_2.clone(),
                            )),
                        ),
                        (owner, UiAccountProfileState::Readonly),
                    ]),
                    compute_units_consumed: 100,
                    log_messages: Some(vec![
                        "Log message: Updating Account".to_string(),
                        "Log message: Account updated".to_string(),
                    ]),
                    error_message: None,
                },
                UiProfileResult {
                    account_states: IndexMap::from_iter([
                        (
                            pubkey,
                            UiAccountProfileState::Writable(UiAccountChange::Delete(account_2)),
                        ),
                        (owner, UiAccountProfileState::Readonly),
                    ]),
                    compute_units_consumed: 100,
                    log_messages: Some(vec![
                        "Log message: Deleting Account".to_string(),
                        "Log message: Account deleted".to_string(),
                    ]),
                    error_message: None,
                },
            ]),
            transaction_profile: UiProfileResult {
                account_states: IndexMap::from_iter([
                    (
                        pubkey,
                        UiAccountProfileState::Writable(UiAccountChange::Unchanged(None)),
                    ),
                    (owner, UiAccountProfileState::Readonly),
                ]),
                compute_units_consumed: 300,
                log_messages: Some(vec![
                    "Log message: Creating Account".to_string(),
                    "Log message: Account created".to_string(),
                    "Log message: Updating Account".to_string(),
                    "Log message: Account updated".to_string(),
                    "Log message: Deleting Account".to_string(),
                    "Log message: Account deleted".to_string(),
                ]),
                error_message: None,
            },
            readonly_account_states: IndexMap::from_iter([(owner, readonly_account_state)]),
        };
        println!("{}", serde_json::to_string_pretty(&profile_result).unwrap());
    }

    #[test]
    fn test_profiling_map_capacity() {
        let profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        assert_eq!(profiling_map.capacity(), 10);
    }

    #[test]
    fn test_profiling_map_len() {
        let profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        assert!(profiling_map.len() == 0);
    }

    #[test]
    fn test_profiling_map_is_empty() {
        let profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        assert_eq!(profiling_map.is_empty(), true);
    }

    #[test]
    fn test_profiling_map_insert() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());
        assert_eq!(profiling_map.len(), 1);
    }

    #[test]
    fn test_profiling_map_get() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());

        assert_eq!(profiling_map.get(&key), Some(&value));
    }

    #[test]
    fn test_profiling_map_get_mut() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let mut value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());
        assert_eq!(profiling_map.get_mut(&key), Some(&mut value));
    }

    #[test]
    fn test_profiling_map_contains_key() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());

        assert_eq!(profiling_map.contains_key(&key), true);
    }

    #[test]
    fn test_profiling_map_iter() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());

        assert_eq!(profiling_map.iter().count(), 1);
    }

    #[test]
    fn test_profiling_map_evicts_oldest_on_overflow() {
        let mut profiling_map = FifoMap::<String, u32>::new(10);
        profiling_map.insert("a".to_string(), 1);
        profiling_map.insert("b".to_string(), 2);
        profiling_map.insert("c".to_string(), 3);
        profiling_map.insert("d".to_string(), 4);
        profiling_map.insert("e".to_string(), 5);
        profiling_map.insert("f".to_string(), 6);
        profiling_map.insert("g".to_string(), 7);
        profiling_map.insert("h".to_string(), 8);
        profiling_map.insert("i".to_string(), 9);
        profiling_map.insert("j".to_string(), 10);

        println!("Profiling map: {:?}", profiling_map);
        println!("Profile Map capacity: {:?}", profiling_map.capacity());
        println!("Profile Map len: {:?}", profiling_map.len());

        assert_eq!(profiling_map.len(), 10);

        // Now insert one more, which should evict the oldest
        profiling_map.insert("k".to_string(), 11);
        assert_eq!(profiling_map.len(), 10);
        assert_eq!(profiling_map.get(&"a".to_string()), None);
        assert_eq!(profiling_map.get(&"k".to_string()), Some(&11));
    }

    #[test]
    fn test_profiling_map_update_do_not_reorder() {
        let mut profiling_map = FifoMap::<&str, u32>::new(4);
        profiling_map.insert("a", 1);
        profiling_map.insert("b", 2);
        profiling_map.insert("c", 3);
        profiling_map.insert("d", 4);

        //update b, should not reorder (order remains a:1,b:2,c:3,d:4)
        println!("Profiling map: {:?}", profiling_map);
        println!("Profile Map key b holds: {:?}", profiling_map.get(&"b"));
        profiling_map.insert("b", 4);
        println!("Profile Map key b holds: {:?}", profiling_map.get(&"b"));

        //overflow with a new key, should evict the oldest (a)
        profiling_map.insert("e", 5);
        assert_eq!(profiling_map.len(), 4);
        assert_eq!(profiling_map.get(&"a"), None);
        assert_eq!(profiling_map.get(&"b"), Some(&4));
        assert_eq!(profiling_map.get(&"e"), Some(&5));

        let get: Vec<_> = profiling_map.iter().map(|(k, v)| (*k, *v)).collect();
        println!("Profiling map: {:?}", get);
        assert_eq!(get, vec![("b", 4), ("c", 3), ("d", 4), ("e", 5)]);
    }

    #[test]
    fn test_export_snapshot_scope_serialization() {
        // Test Network variant
        let network_config = ExportSnapshotConfig {
            include_parsed_accounts: None,
            filter: None,
            scope: ExportSnapshotScope::Network,
        };
        let network_json = serde_json::to_value(&network_config).unwrap();
        println!(
            "Network config: {}",
            serde_json::to_string_pretty(&network_json).unwrap()
        );
        assert_eq!(network_json["scope"], json!("network"));

        // Test PreTransaction variant
        let pre_tx_config = ExportSnapshotConfig {
            include_parsed_accounts: None,
            filter: None,
            scope: ExportSnapshotScope::PreTransaction("5signature123".to_string()),
        };
        let pre_tx_json = serde_json::to_value(&pre_tx_config).unwrap();
        println!(
            "PreTransaction config: {}",
            serde_json::to_string_pretty(&pre_tx_json).unwrap()
        );
        assert_eq!(
            pre_tx_json["scope"],
            json!({"preTransaction": "5signature123"})
        );

        // Test deserialization
        let deserialized_network: ExportSnapshotConfig =
            serde_json::from_value(network_json).unwrap();
        assert_eq!(deserialized_network.scope, ExportSnapshotScope::Network);

        let deserialized_pre_tx: ExportSnapshotConfig =
            serde_json::from_value(pre_tx_json).unwrap();
        assert_eq!(
            deserialized_pre_tx.scope,
            ExportSnapshotScope::PreTransaction("5signature123".to_string())
        );
    }

    #[test]
    fn test_sanitize_datasource_url_strips_path_and_query() {
        // API key in path should be stripped
        let config = SimnetConfig {
            remote_rpc_url: Some(
                "https://example.rpc-provider.com/v2/abc123def456ghi789".to_string(),
            ),
            ..Default::default()
        };
        let sanitized = config.get_sanitized_datasource_url().unwrap();
        assert_eq!(sanitized, "https://example.rpc-provider.com");
        assert!(!sanitized.contains("abc123"));
    }

    #[test]
    fn test_sanitize_datasource_url_strips_query_params() {
        let config = SimnetConfig {
            remote_rpc_url: Some(
                "https://mainnet.helius-rpc.com/?api-key=secret-key-12345".to_string(),
            ),
            ..Default::default()
        };
        let sanitized = config.get_sanitized_datasource_url().unwrap();
        assert_eq!(sanitized, "https://mainnet.helius-rpc.com");
        assert!(!sanitized.contains("secret-key"));
    }

    #[test]
    fn test_sanitize_datasource_url_public_rpc() {
        let config = SimnetConfig {
            remote_rpc_url: Some("https://api.mainnet-beta.solana.com".to_string()),
            ..Default::default()
        };
        let sanitized = config.get_sanitized_datasource_url().unwrap();
        assert_eq!(sanitized, "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn test_sanitize_datasource_url_none() {
        let config = SimnetConfig {
            remote_rpc_url: None,
            ..Default::default()
        };
        assert!(config.get_sanitized_datasource_url().is_none());
    }

    #[test]
    fn test_sanitize_datasource_url_invalid() {
        let config = SimnetConfig {
            remote_rpc_url: Some("not-a-valid-url".to_string()),
            ..Default::default()
        };
        assert!(config.get_sanitized_datasource_url().is_none());
    }

    #[test]
    fn test_simnet_config_skip_blockhash_check_defaults_on_deserialize() {
        let mut config_json = serde_json::to_value(SimnetConfig::default()).unwrap();
        config_json
            .as_object_mut()
            .unwrap()
            .remove("skip_blockhash_check");

        let config: SimnetConfig = serde_json::from_value(config_json).unwrap();
        assert!(!config.skip_blockhash_check);
    }

    // Configs written before the startup planner existed must keep working;
    // the runloop-seals default is also the correct reading of them, since
    // none of their writers seal a plan.
    #[test]
    fn test_surfpool_config_startup_planner_defaults_on_deserialize() {
        let mut config_json = serde_json::to_value(SurfpoolConfig::default()).unwrap();
        config_json
            .as_object_mut()
            .unwrap()
            .remove("startup_planner");

        let config: SurfpoolConfig = serde_json::from_value(config_json).unwrap();
        assert_eq!(config.startup_planner, StartupPlanner::None);
    }
}
