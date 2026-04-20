#[macro_use]
extern crate napi_derive;

use std::{convert::TryFrom, path::PathBuf};

use napi::{Error, Result, Status};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::{
    BlockProductionMode, SimnetEvent, Surfnet as NativeSurfnet,
    cheatcodes::builders::{DeployProgram, ResetAccount, SetTokenAccount, StreamAccount},
};
use surfpool_types::ClockCommand;

/// A running Surfpool instance with RPC/WS endpoints on dynamic ports.
#[napi]
pub struct Surfnet {
    inner: NativeSurfnet,
}

#[napi]
impl Surfnet {
    /// Start a surfnet with default settings (offline, transaction-mode blocks, 10 SOL payer).
    #[napi(factory)]
    pub fn start() -> Result<Self> {
        let inner = hiro_system_kit::nestable_block_on(NativeSurfnet::start())
            .map_err(sdk_error_to_napi)?;
        Ok(Self { inner })
    }

    /// Start a surfnet with custom configuration.
    #[napi(factory)]
    pub fn start_with_config(config: SurfnetConfig) -> Result<Self> {
        let mut builder = NativeSurfnet::builder();

        if let Some(offline) = config.offline {
            builder = builder.offline(offline);
        }
        if let Some(url) = config.remote_rpc_url {
            builder = builder.remote_rpc_url(url);
        }
        if let Some(mode) = config.block_production_mode.as_deref() {
            let mode = mode.parse::<BlockProductionMode>().map_err(|e| {
                Error::new(
                    Status::InvalidArg,
                    format!("Invalid blockProductionMode: {e}"),
                )
            })?;
            builder = builder.block_production_mode(mode);
        }
        if let Some(ms) = config.slot_time_ms {
            builder = builder.slot_time_ms(f64_to_u64(ms, "slotTimeMs")?);
        }
        if let Some(lamports) = config.airdrop_sol {
            builder = builder.airdrop_sol(f64_to_u64(lamports, "airdropSol")?);
        }
        if let Some(addresses) = config.airdrop_addresses {
            builder = builder.airdrop_addresses(parse_pubkeys(&addresses, "airdropAddresses")?);
        }
        if let Some(secret_key) = config.payer_secret_key {
            builder = builder.payer(parse_keypair(&secret_key, "payerSecretKey")?);
        }

        let inner =
            hiro_system_kit::nestable_block_on(builder.start()).map_err(sdk_error_to_napi)?;
        Ok(Self { inner })
    }

    /// The HTTP RPC URL (e.g. "http://127.0.0.1:12345").
    #[napi(getter)]
    pub fn rpc_url(&self) -> String {
        self.inner.rpc_url().to_string()
    }

    /// The WebSocket URL (e.g. "ws://127.0.0.1:12346").
    #[napi(getter)]
    pub fn ws_url(&self) -> String {
        self.inner.ws_url().to_string()
    }

    /// The pre-funded payer public key as base58 string.
    #[napi(getter)]
    pub fn payer(&self) -> String {
        self.inner.payer().pubkey().to_string()
    }

    /// The pre-funded payer secret key as a 64-byte Uint8Array.
    #[napi(getter)]
    pub fn payer_secret_key(&self) -> Vec<u8> {
        self.inner.payer().to_bytes().to_vec()
    }

    /// The unique identifier for this Surfnet instance.
    #[napi(getter)]
    pub fn instance_id(&self) -> String {
        self.inner.instance_id().to_string()
    }

    /// Drain and return currently buffered simnet events.
    #[napi]
    pub fn drain_events(&self) -> Vec<SimnetEventValue> {
        self.inner
            .events()
            .try_iter()
            .map(SimnetEventValue::from)
            .collect()
    }

    /// Fund a SOL account with lamports.
    #[napi]
    pub fn fund_sol(&self, address: String, lamports: f64) -> Result<()> {
        let pubkey = parse_pubkey(&address, "address")?;
        self.inner
            .cheatcodes()
            .fund_sol(&pubkey, f64_to_u64(lamports, "lamports")?)
            .map_err(sdk_error_to_napi)
    }

    /// Fund multiple SOL accounts with explicit lamport balances.
    #[napi]
    pub fn fund_sol_many(&self, accounts: Vec<SolAccountFunding>) -> Result<()> {
        let parsed_accounts = accounts
            .iter()
            .map(|account| {
                Ok((
                    parse_pubkey(&account.address, "address")?,
                    f64_to_u64(account.lamports, "lamports")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let account_refs = parsed_accounts
            .iter()
            .map(|(pubkey, lamports)| (pubkey, *lamports))
            .collect::<Vec<_>>();

        self.inner
            .cheatcodes()
            .fund_sol_many(&account_refs)
            .map_err(sdk_error_to_napi)
    }

    /// Fund a token account (creates the ATA if needed).
    /// Uses spl_token program by default. Pass token_program for Token-2022.
    #[napi]
    pub fn fund_token(
        &self,
        owner: String,
        mint: String,
        amount: f64,
        token_program: Option<String>,
    ) -> Result<()> {
        let owner_pk = parse_pubkey(&owner, "owner")?;
        let mint_pk = parse_pubkey(&mint, "mint")?;
        let token_program = parse_optional_pubkey(token_program, "tokenProgram")?;

        self.inner
            .cheatcodes()
            .fund_token(
                &owner_pk,
                &mint_pk,
                f64_to_u64(amount, "amount")?,
                token_program.as_ref(),
            )
            .map_err(sdk_error_to_napi)
    }

    /// Set the token balance for a wallet/mint pair.
    #[napi]
    pub fn set_token_balance(
        &self,
        owner: String,
        mint: String,
        amount: f64,
        token_program: Option<String>,
    ) -> Result<()> {
        let owner_pk = parse_pubkey(&owner, "owner")?;
        let mint_pk = parse_pubkey(&mint, "mint")?;
        let token_program = parse_optional_pubkey(token_program, "tokenProgram")?;

        self.inner
            .cheatcodes()
            .set_token_balance(
                &owner_pk,
                &mint_pk,
                f64_to_u64(amount, "amount")?,
                token_program.as_ref(),
            )
            .map_err(sdk_error_to_napi)
    }

    /// Fund multiple wallets with the same token and amount.
    #[napi]
    pub fn fund_token_many(
        &self,
        owners: Vec<String>,
        mint: String,
        amount: f64,
        token_program: Option<String>,
    ) -> Result<()> {
        let owner_pubkeys = owners
            .iter()
            .map(|owner| parse_pubkey(owner, "owner"))
            .collect::<Result<Vec<_>>>()?;
        let owner_refs = owner_pubkeys.iter().collect::<Vec<_>>();
        let mint_pk = parse_pubkey(&mint, "mint")?;
        let token_program = parse_optional_pubkey(token_program, "tokenProgram")?;

        self.inner
            .cheatcodes()
            .fund_token_many(
                &owner_refs,
                &mint_pk,
                f64_to_u64(amount, "amount")?,
                token_program.as_ref(),
            )
            .map_err(sdk_error_to_napi)
    }

    /// Set or clear advanced token-account state for a wallet/mint pair.
    #[napi]
    pub fn set_token_account(
        &self,
        owner: String,
        mint: String,
        update: SetTokenAccountUpdate,
        token_program: Option<String>,
    ) -> Result<()> {
        let owner_pk = parse_pubkey(&owner, "owner")?;
        let mint_pk = parse_pubkey(&mint, "mint")?;
        let mut builder = SetTokenAccount::new(owner_pk, mint_pk);

        if let Some(amount) = update.amount {
            builder = builder.amount(f64_to_u64(amount, "amount")?);
        }
        if let Some(state) = update.state {
            builder = builder.state(state);
        }
        if let Some(delegated_amount) = update.delegated_amount {
            builder = builder.delegated_amount(f64_to_u64(delegated_amount, "delegatedAmount")?);
        }
        builder = match (update.delegate, update.clear_delegate.unwrap_or(false)) {
            (Some(delegate), false) => builder.delegate(parse_pubkey(&delegate, "delegate")?),
            (None, true) => builder.clear_delegate(),
            (Some(_), true) => {
                return Err(Error::new(
                    Status::InvalidArg,
                    "delegate and clearDelegate are mutually exclusive".to_string(),
                ));
            }
            (None, false) => builder,
        };
        builder = match (
            update.close_authority,
            update.clear_close_authority.unwrap_or(false),
        ) {
            (Some(close_authority), false) => {
                builder.close_authority(parse_pubkey(&close_authority, "closeAuthority")?)
            }
            (None, true) => builder.clear_close_authority(),
            (Some(_), true) => {
                return Err(Error::new(
                    Status::InvalidArg,
                    "closeAuthority and clearCloseAuthority are mutually exclusive".to_string(),
                ));
            }
            (None, false) => builder,
        };

        if let Some(token_program) = parse_optional_pubkey(token_program, "tokenProgram")? {
            builder = builder.token_program(token_program);
        }

        self.inner
            .cheatcodes()
            .execute(builder)
            .map_err(sdk_error_to_napi)
    }

    /// Set arbitrary account data.
    #[napi]
    pub fn set_account(
        &self,
        address: String,
        lamports: f64,
        data: Vec<u8>,
        owner: String,
    ) -> Result<()> {
        let address = parse_pubkey(&address, "address")?;
        let owner = parse_pubkey(&owner, "owner")?;

        self.inner
            .cheatcodes()
            .set_account(&address, f64_to_u64(lamports, "lamports")?, &data, &owner)
            .map_err(sdk_error_to_napi)
    }

    /// Reset a previously modified account to its upstream or absent state.
    #[napi]
    pub fn reset_account(
        &self,
        address: String,
        options: Option<ResetAccountOptions>,
    ) -> Result<()> {
        let address = parse_pubkey(&address, "address")?;
        let mut builder = ResetAccount::new(address);

        if let Some(options) = options {
            if let Some(include_owned_accounts) = options.include_owned_accounts {
                builder = builder.include_owned_accounts(include_owned_accounts);
            }
        }

        self.inner
            .cheatcodes()
            .execute(builder)
            .map_err(sdk_error_to_napi)
    }

    /// Register an account for background streaming from the remote datasource.
    #[napi]
    pub fn stream_account(
        &self,
        address: String,
        options: Option<StreamAccountOptions>,
    ) -> Result<()> {
        let address = parse_pubkey(&address, "address")?;
        let mut builder = StreamAccount::new(address);

        if let Some(options) = options {
            if let Some(include_owned_accounts) = options.include_owned_accounts {
                builder = builder.include_owned_accounts(include_owned_accounts);
            }
        }

        self.inner
            .cheatcodes()
            .execute(builder)
            .map_err(sdk_error_to_napi)
    }

    /// Move Surfnet time forward to an absolute epoch.
    #[napi]
    pub fn time_travel_to_epoch(&self, epoch: f64) -> Result<EpochInfoValue> {
        self.inner
            .cheatcodes()
            .time_travel_to_epoch(f64_to_u64(epoch, "epoch")?)
            .map(EpochInfoValue::from)
            .map_err(sdk_error_to_napi)
    }

    /// Move Surfnet time forward to an absolute slot.
    #[napi]
    pub fn time_travel_to_slot(&self, slot: f64) -> Result<EpochInfoValue> {
        self.inner
            .cheatcodes()
            .time_travel_to_slot(f64_to_u64(slot, "slot")?)
            .map(EpochInfoValue::from)
            .map_err(sdk_error_to_napi)
    }

    /// Move Surfnet time forward to an absolute Unix timestamp in milliseconds.
    #[napi]
    pub fn time_travel_to_timestamp(&self, timestamp: f64) -> Result<EpochInfoValue> {
        self.inner
            .cheatcodes()
            .time_travel_to_timestamp(f64_to_u64(timestamp, "timestamp")?)
            .map(EpochInfoValue::from)
            .map_err(sdk_error_to_napi)
    }

    /// Deploy a program by discovering local program artifacts.
    #[napi]
    pub fn deploy_program(&self, program_name: String) -> Result<String> {
        self.inner
            .cheatcodes()
            .deploy_program(&program_name)
            .map(|program_id| program_id.to_string())
            .map_err(sdk_error_to_napi)
    }

    /// Deploy a program from explicit bytes or an explicit `.so` path.
    #[napi]
    pub fn deploy(&self, options: DeployOptions) -> Result<String> {
        let mut builder = DeployProgram::new(parse_pubkey(&options.program_id, "programId")?);

        match (&options.so_path, &options.so_bytes) {
            (None, None) => {
                return Err(Error::new(
                    Status::InvalidArg,
                    "deploy requires either soPath or soBytes".to_string(),
                ));
            }
            (Some(path), _) => {
                builder = builder.so_path(PathBuf::from(path));
            }
            (None, Some(_)) => {}
        }

        if let Some(bytes) = options.so_bytes {
            builder = builder.so_bytes(bytes);
        }
        if let Some(idl_path) = options.idl_path {
            builder = builder.idl_path(PathBuf::from(idl_path));
        }

        self.inner
            .cheatcodes()
            .deploy(builder)
            .map(|program_id| program_id.to_string())
            .map_err(sdk_error_to_napi)
    }

    /// Get the associated token address for a wallet/mint pair.
    #[napi]
    pub fn get_ata(
        &self,
        owner: String,
        mint: String,
        token_program: Option<String>,
    ) -> Result<String> {
        let owner = parse_pubkey(&owner, "owner")?;
        let mint = parse_pubkey(&mint, "mint")?;
        let token_program = parse_optional_pubkey(token_program, "tokenProgram")?;

        Ok(self
            .inner
            .cheatcodes()
            .get_ata(&owner, &mint, token_program.as_ref())
            .to_string())
    }

    /// Generate a new random keypair. Returns [publicKey, secretKey] as base58 and bytes.
    #[napi]
    pub fn new_keypair() -> KeypairInfo {
        let keypair = Keypair::new();
        KeypairInfo {
            public_key: keypair.pubkey().to_string(),
            secret_key: keypair.to_bytes().to_vec(),
        }
    }
}

#[napi(object)]
pub struct SurfnetConfig {
    pub offline: Option<bool>,
    pub remote_rpc_url: Option<String>,
    pub block_production_mode: Option<String>,
    pub slot_time_ms: Option<f64>,
    pub airdrop_sol: Option<f64>,
    pub airdrop_addresses: Option<Vec<String>>,
    pub payer_secret_key: Option<Vec<u8>>,
}

#[napi(object)]
pub struct ResetAccountOptions {
    pub include_owned_accounts: Option<bool>,
}

#[napi(object)]
pub struct StreamAccountOptions {
    pub include_owned_accounts: Option<bool>,
}

#[napi(object)]
pub struct SetTokenAccountUpdate {
    pub amount: Option<f64>,
    pub delegate: Option<String>,
    pub clear_delegate: Option<bool>,
    pub state: Option<String>,
    pub delegated_amount: Option<f64>,
    pub close_authority: Option<String>,
    pub clear_close_authority: Option<bool>,
}

#[napi(object)]
pub struct DeployOptions {
    pub program_id: String,
    pub so_path: Option<String>,
    pub so_bytes: Option<Vec<u8>>,
    pub idl_path: Option<String>,
}

#[napi(object)]
pub struct KeypairInfo {
    pub public_key: String,
    pub secret_key: Vec<u8>,
}

#[napi(object)]
pub struct SolAccountFunding {
    pub address: String,
    pub lamports: f64,
}

#[napi(object)]
pub struct ClockValue {
    pub slot: f64,
    pub epoch_start_timestamp: f64,
    pub epoch: f64,
    pub leader_schedule_epoch: f64,
    pub unix_timestamp: f64,
}

#[napi(object)]
pub struct EpochInfoValue {
    pub epoch: f64,
    pub slot_index: f64,
    pub slots_in_epoch: f64,
    pub absolute_slot: f64,
    pub block_height: f64,
    pub transaction_count: Option<f64>,
}

#[napi(object)]
pub struct SimnetEventValue {
    pub kind: String,
    pub message: Option<String>,
    pub timestamp: Option<String>,
    pub initial_transaction_count: Option<f64>,
    pub clock: Option<ClockValue>,
    pub epoch_info: Option<EpochInfoValue>,
    pub account_pubkey: Option<String>,
    pub clock_command: Option<String>,
    pub slot_interval_ms: Option<f64>,
    pub transaction_signature: Option<String>,
    pub logs: Option<Vec<String>>,
    pub compute_units_consumed: Option<f64>,
    pub fee: Option<f64>,
    pub error_message: Option<String>,
    pub tag: Option<String>,
    pub profile_key: Option<String>,
    pub profile_slot: Option<f64>,
    pub runbook_id: Option<String>,
    pub runbook_errors: Option<Vec<String>>,
}

impl EpochInfoValue {
    fn from_epoch_info(value: solana_epoch_info::EpochInfo) -> Self {
        Self {
            epoch: value.epoch as f64,
            slot_index: value.slot_index as f64,
            slots_in_epoch: value.slots_in_epoch as f64,
            absolute_slot: value.absolute_slot as f64,
            block_height: value.block_height as f64,
            transaction_count: value.transaction_count.map(|count| count as f64),
        }
    }
}

impl From<solana_epoch_info::EpochInfo> for EpochInfoValue {
    fn from(value: solana_epoch_info::EpochInfo) -> Self {
        Self::from_epoch_info(value)
    }
}

impl SimnetEventValue {
    fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: None,
            timestamp: None,
            initial_transaction_count: None,
            clock: None,
            epoch_info: None,
            account_pubkey: None,
            clock_command: None,
            slot_interval_ms: None,
            transaction_signature: None,
            logs: None,
            compute_units_consumed: None,
            fee: None,
            error_message: None,
            tag: None,
            profile_key: None,
            profile_slot: None,
            runbook_id: None,
            runbook_errors: None,
        }
    }
}

impl From<SimnetEvent> for SimnetEventValue {
    fn from(event: SimnetEvent) -> Self {
        match event {
            SimnetEvent::Ready(count) => {
                let mut value = Self::new("ready");
                value.initial_transaction_count = Some(count as f64);
                value
            }
            SimnetEvent::Connected(url) => {
                let mut value = Self::new("connected");
                value.message = Some(url);
                value
            }
            SimnetEvent::Aborted(reason) => {
                let mut value = Self::new("aborted");
                value.message = Some(reason);
                value
            }
            SimnetEvent::Shutdown => Self::new("shutdown"),
            SimnetEvent::SystemClockUpdated(clock) => {
                let mut value = Self::new("systemClockUpdated");
                value.clock = Some(ClockValue {
                    slot: clock.slot as f64,
                    epoch_start_timestamp: clock.epoch_start_timestamp as f64,
                    epoch: clock.epoch as f64,
                    leader_schedule_epoch: clock.leader_schedule_epoch as f64,
                    unix_timestamp: clock.unix_timestamp as f64,
                });
                value
            }
            SimnetEvent::ClockUpdate(command) => {
                let mut value = Self::new("clockUpdate");
                match command {
                    ClockCommand::Pause => value.clock_command = Some("pause".to_string()),
                    ClockCommand::PauseWithConfirmation(_) => {
                        value.clock_command = Some("pauseWithConfirmation".to_string())
                    }
                    ClockCommand::Resume => value.clock_command = Some("resume".to_string()),
                    ClockCommand::Toggle => value.clock_command = Some("toggle".to_string()),
                    ClockCommand::UpdateSlotInterval(ms) => {
                        value.clock_command = Some("updateSlotInterval".to_string());
                        value.slot_interval_ms = Some(ms as f64);
                    }
                }
                value
            }
            SimnetEvent::EpochInfoUpdate(epoch_info) => {
                let mut value = Self::new("epochInfoUpdate");
                value.epoch_info = Some(epoch_info.into());
                value
            }
            SimnetEvent::BlockHashExpired => Self::new("blockHashExpired"),
            SimnetEvent::InfoLog(timestamp, message) => {
                log_event("infoLog", timestamp.to_rfc3339(), message)
            }
            SimnetEvent::ErrorLog(timestamp, message) => {
                log_event("errorLog", timestamp.to_rfc3339(), message)
            }
            SimnetEvent::WarnLog(timestamp, message) => {
                log_event("warnLog", timestamp.to_rfc3339(), message)
            }
            SimnetEvent::DebugLog(timestamp, message) => {
                log_event("debugLog", timestamp.to_rfc3339(), message)
            }
            SimnetEvent::PluginLoaded(plugin_name) => {
                let mut value = Self::new("pluginLoaded");
                value.message = Some(plugin_name);
                value
            }
            SimnetEvent::TransactionReceived(timestamp, transaction) => {
                let mut value = Self::new("transactionReceived");
                value.timestamp = Some(timestamp.to_rfc3339());
                value.transaction_signature =
                    transaction.signatures.first().map(|sig| sig.to_string());
                value
            }
            SimnetEvent::TransactionProcessed(timestamp, metadata, error) => {
                let mut value = Self::new("transactionProcessed");
                value.timestamp = Some(timestamp.to_rfc3339());
                value.transaction_signature = Some(metadata.signature.to_string());
                value.logs = Some(metadata.logs);
                value.compute_units_consumed = Some(metadata.compute_units_consumed as f64);
                value.fee = Some(metadata.fee as f64);
                value.error_message = error.map(|err| err.to_string());
                value
            }
            SimnetEvent::AccountUpdate(timestamp, pubkey) => {
                let mut value = Self::new("accountUpdate");
                value.timestamp = Some(timestamp.to_rfc3339());
                value.account_pubkey = Some(pubkey.to_string());
                value
            }
            SimnetEvent::TaggedProfile {
                result,
                tag,
                timestamp,
            } => {
                let mut value = Self::new("taggedProfile");
                value.timestamp = Some(timestamp.to_rfc3339());
                value.tag = Some(tag);
                value.profile_key = Some(result.key.to_string());
                value.profile_slot = Some(result.slot as f64);
                value.logs = result.transaction_profile.log_messages;
                value.compute_units_consumed =
                    Some(result.transaction_profile.compute_units_consumed as f64);
                value.error_message = result.transaction_profile.error_message;
                value
            }
            SimnetEvent::RunbookStarted(runbook_id) => {
                let mut value = Self::new("runbookStarted");
                value.runbook_id = Some(runbook_id);
                value
            }
            SimnetEvent::RunbookCompleted(runbook_id, errors) => {
                let mut value = Self::new("runbookCompleted");
                value.runbook_id = Some(runbook_id);
                value.runbook_errors = errors;
                value
            }
        }
    }
}

fn log_event(kind: &str, timestamp: String, message: String) -> SimnetEventValue {
    let mut value = SimnetEventValue::new(kind);
    value.timestamp = Some(timestamp);
    value.message = Some(message);
    value
}

fn sdk_error_to_napi(error: impl ToString) -> Error {
    Error::new(Status::GenericFailure, error.to_string())
}

fn parse_pubkey(value: &str, field_name: &str) -> Result<Pubkey> {
    value
        .parse::<Pubkey>()
        .map_err(|e| Error::new(Status::InvalidArg, format!("Invalid {field_name}: {e}")))
}

fn parse_optional_pubkey(value: Option<String>, field_name: &str) -> Result<Option<Pubkey>> {
    value
        .map(|value| parse_pubkey(&value, field_name))
        .transpose()
}

fn parse_pubkeys(values: &[String], field_name: &str) -> Result<Vec<Pubkey>> {
    values
        .iter()
        .map(|value| parse_pubkey(value, field_name))
        .collect()
}

fn parse_keypair(bytes: &[u8], field_name: &str) -> Result<Keypair> {
    Keypair::try_from(bytes)
        .map_err(|e| Error::new(Status::InvalidArg, format!("Invalid {field_name}: {e}")))
}

fn f64_to_u64(value: f64, field_name: &str) -> Result<u64> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > u64::MAX as f64 {
        return Err(Error::new(
            Status::InvalidArg,
            format!("{field_name} must be a non-negative integer"),
        ));
    }

    Ok(value as u64)
}
