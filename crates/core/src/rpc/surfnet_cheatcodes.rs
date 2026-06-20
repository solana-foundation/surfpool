use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use jsonrpc_core::{BoxFuture, Error, Result, futures::future};
use jsonrpc_derive::rpc;
use solana_account::Account;
use solana_client::rpc_response::{RpcLogsResponse, RpcResponseContext};
use solana_clock::Slot;
use solana_commitment_config::CommitmentConfig;
use solana_epoch_info::EpochInfo;
use solana_program_option::COption;
use solana_rpc_client_api::response::Response as RpcResponse;
use solana_system_interface::program as system_program;
use solana_transaction::versioned::VersionedTransaction;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use surfpool_types::{
    AccountSnapshot, CheatcodeControlConfig, CheatcodeFilter, ClockCommand, ExportSnapshotConfig,
    GetStreamedAccountsResponse, GetSurfnetInfoResponse, Idl, OfflineAccountConfig,
    ResetAccountConfig, RpcProfileResultConfig, Scenario, SimnetCommand, SimnetEvent,
    StreamAccountConfig, StreamAccountsEntry, UiKeyedProfileResult,
    types::{AccountUpdate, SetSomeAccount, SupplyUpdate, TokenAccountUpdate, UuidOrSignature},
};

use super::{RunloopContext, SurfnetRpcContext};
use crate::{
    error::SurfpoolError,
    rpc::{
        State,
        utils::{verify_pubkey, verify_pubkeys},
    },
    surfnet::{GetAccountResult, locker::SvmAccessContext},
    types::{TimeTravelConfig, TokenAccount},
};

pub trait AccountUpdateExt {
    fn is_full_account_data_ext(&self) -> bool;
    fn to_account_ext(&self) -> Result<Option<Account>>;
    fn apply_ext(self, account: &mut GetAccountResult) -> Result<()>;
    fn expect_hex_data_ext(&self) -> Result<Vec<u8>>;
}

impl AccountUpdateExt for AccountUpdate {
    fn is_full_account_data_ext(&self) -> bool {
        self.lamports.is_some()
            && self.owner.is_some()
            && self.executable.is_some()
            && self.rent_epoch.is_some()
            && self.data.is_some()
    }

    fn to_account_ext(&self) -> Result<Option<Account>> {
        if self.is_full_account_data_ext() {
            Ok(Some(Account {
                lamports: self.lamports.unwrap(),
                owner: verify_pubkey(&self.owner.clone().unwrap())?,
                executable: self.executable.unwrap(),
                rent_epoch: self.rent_epoch.unwrap(),
                data: self.expect_hex_data_ext()?,
            }))
        } else {
            Ok(None)
        }
    }

    fn apply_ext(self, account_result: &mut GetAccountResult) -> Result<()> {
        account_result.apply_update(|account| {
            if let Some(lamports) = self.lamports {
                account.lamports = lamports;
            }
            if let Some(owner) = &self.owner {
                account.owner = verify_pubkey(owner)?;
            }
            if let Some(executable) = self.executable {
                account.executable = executable;
            }
            if let Some(rent_epoch) = self.rent_epoch {
                account.rent_epoch = rent_epoch;
            }
            if self.data.is_some() {
                account.data = self.expect_hex_data_ext()?;
            }
            Ok(())
        })?;
        Ok(())
    }

    fn expect_hex_data_ext(&self) -> Result<Vec<u8>> {
        let data = self.data.as_ref().expect("missing expected data field");
        hex::decode(data)
            .map_err(|e| Error::invalid_params(format!("Invalid hex data provided: {}", e)))
    }
}

pub trait TokenAccountUpdateExt {
    fn apply(self, token_account: &mut TokenAccount) -> Result<()>;
}

impl TokenAccountUpdateExt for TokenAccountUpdate {
    /// Apply the update to the account
    fn apply(self, token_account: &mut TokenAccount) -> Result<()> {
        if let Some(amount) = self.amount {
            token_account.set_amount(amount);
        }
        if let Some(delegate) = self.delegate {
            match delegate {
                SetSomeAccount::Account(pubkey) => {
                    token_account.set_delegate(COption::Some(verify_pubkey(&pubkey)?));
                }
                SetSomeAccount::NoAccount => {
                    token_account.set_delegate(COption::None);
                }
            }
        }
        if let Some(state) = self.state {
            token_account.set_state_from_str(state.as_str())?;
        }
        if let Some(delegated_amount) = self.delegated_amount {
            token_account.set_delegated_amount(delegated_amount);
        }
        if let Some(close_authority) = self.close_authority {
            match close_authority {
                SetSomeAccount::Account(pubkey) => {
                    token_account.set_close_authority(COption::Some(verify_pubkey(&pubkey)?));
                }
                SetSomeAccount::NoAccount => {
                    token_account.set_close_authority(COption::None);
                }
            }
        }
        Ok(())
    }
}

#[rpc]
pub trait SurfnetCheatcodes {
    type Metadata;

    /// A "cheat code" method for developers to set or update an account in Surfpool.
    ///
    /// This method allows developers to set or update the lamports, data, owner, executable status,
    /// and rent epoch of a given account.
    ///
    /// ## Parameters
    /// - `pubkey`: The public key of the account to be updated, as a base-58 encoded string.
    /// - `update`: The `AccountUpdate` struct containing the fields to update the account.
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the account update was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_setAccount",
    ///   "params": ["account_pubkey", {"lamports": 1000, "data": "base58string", "owner": "program_pubkey"}]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {},
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// This method is designed to help developers set or modify account properties within Surfpool.
    /// Developers can quickly test or update account attributes, such as lamports, program ownership, and executable status.
    ///
    /// # See Also
    /// - `getAccount`, `getAccountInfo`, `getAccountBalance`
    #[rpc(meta, name = "surfnet_setAccount")]
    fn set_account(
        &self,
        meta: Self::Metadata,
        pubkey: String,
        update: AccountUpdate,
    ) -> BoxFuture<Result<RpcResponse<()>>>;

    /// Enables one or more Surfpool cheatcode RPC methods for the current session.
    ///
    /// This method allows developers to re-enable cheatcode methods that were previously disabled.
    /// Each cheatcode name must match a valid `surfnet_*` RPC method (e.g. `surfnet_setAccount`, `surfnet_timeTravel`).
    ///
    /// ## Parameters
    /// - `cheatcodes`: A list of cheatcode method names to enable, as strings (e.g. `["surfnet_setAccount", "surfnet_timeTravel"]`).
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the enable operation was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_enableCheatcode",
    ///   "params": [["surfnet_setAccount", "surfnet_timeTravel"]]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {},
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// Invalid cheatcode names return an error. Use `surfnet_disableCheatcode` to disable methods and `surfnet_lockout` to allow disabling `surfnet_enableCheatcode` and `surfnet_disableCheatcode` themselves.
    ///
    /// # See Also
    /// - `surfnet_disableCheatcode`, `surfnet_disableAllCheatcodes`, `surfnet_lockout`
    #[rpc(meta, name = "surfnet_enableCheatcode")]
    fn enable_cheatcode(
        &self,
        meta: Self::Metadata,
        cheatcodes_filter: CheatcodeFilter,
    ) -> Result<RpcResponse<()>>;

    /// Disables one or more Surfpool cheatcode RPC methods for the current session.
    ///
    /// This method allows developers to turn off specific cheatcode methods so they are no longer callable.
    /// Each cheatcode name must match a valid `surfnet_*` RPC method. When lockout is not enabled,
    /// `surfnet_enableCheatcode` and `surfnet_disableCheatcode` cannot be disabled.
    ///
    /// ## Parameters
    /// - `cheatcodes`: A list of cheatcode method names to disable, as strings (e.g. `["surfnet_setAccount"]`).
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the disable operation was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_disableCheatcode",
    ///   "params": [["surfnet_setAccount", "surfnet_timeTravel"]]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {},
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// Call `surfnet_lockout` first if you need to disable `surfnet_enableCheatcode` or `surfnet_disableCheatcode`.
    ///
    /// # See Also
    /// - `surfnet_enableCheatcode`, `surfnet_disableAllCheatcodes`, `surfnet_lockout`
    #[rpc(meta, name = "surfnet_disableCheatcode")]
    fn disable_cheatcode(
        &self,
        meta: Self::Metadata,
        cheatcodes_filter: CheatcodeFilter,
        lockout: Option<CheatcodeControlConfig>,
    ) -> Result<RpcResponse<()>>;

    /// A "cheat code" method for developers to set or update a token account in Surfpool.
    ///
    /// This method allows developers to set or update various properties of a token account,
    /// including the token amount, delegate, state, delegated amount, and close authority.
    ///
    /// ## Parameters
    /// - `owner`: The base-58 encoded public key of the token account's owner.
    /// - `mint`: The base-58 encoded public key of the token mint (e.g., the token type).
    /// - `update`: The `TokenAccountUpdate` struct containing the fields to update the token account.
    /// - `token_program`: The optional base-58 encoded address of the token program (defaults to the system token program).
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the token account update was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_setTokenAccount",
    ///   "params": ["owner_pubkey", "mint_pubkey", {"amount": 1000, "state": "initialized"}]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {},
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// This method is designed to help developers quickly test or modify token account properties in Surfpool.
    /// Developers can update attributes such as token amounts, delegates, and authorities for specific token accounts.
    ///
    /// # See Also
    /// - `getTokenAccountInfo`, `getTokenAccountBalance`, `getTokenAccountDelegate`
    #[rpc(meta, name = "surfnet_setTokenAccount")]
    fn set_token_account(
        &self,
        meta: Self::Metadata,
        owner: String,
        mint: String,
        update: TokenAccountUpdate,
        token_program: Option<String>,
    ) -> BoxFuture<Result<RpcResponse<()>>>;

    #[rpc(meta, name = "surfnet_cloneProgramAccount")]
    fn clone_program_account(
        &self,
        meta: Self::Metadata,
        source_program_id: String,
        destination_program_id: String,
    ) -> BoxFuture<Result<RpcResponse<()>>>;

    /// Estimates the compute units that a given transaction will consume.
    ///
    /// This method simulates the transaction without committing its state changes
    /// and returns an estimation of the compute units used, along with logs and
    /// potential errors.
    ///
    /// ## Parameters
    /// - `transaction_data`: A base64 encoded string of the `VersionedTransaction`.
    /// - `tag`: An optional tag for the transaction.
    /// - `encoding`: An optional encoding for returned account data.
    ///
    /// ## Returns
    /// A `RpcResponse<ProfileResult>` containing the estimation details and a snapshot of the accounts before and after execution.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_profileTransaction",
    ///   "params": ["base64_encoded_transaction_string", "optional_tag"]
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_profileTransaction")]
    fn profile_transaction(
        &self,
        meta: Self::Metadata,
        transaction_data: String, // Base64 encoded VersionedTransaction
        tag: Option<String>,      // Optional tag for the transaction
        config: Option<RpcProfileResultConfig>,
    ) -> BoxFuture<Result<RpcResponse<UiKeyedProfileResult>>>;

    /// Retrieves all profiling results for a given tag.
    ///
    /// ## Parameters
    /// - `tag`: The tag to retrieve profiling results for.
    ///
    /// ## Returns
    /// A `RpcResponse<Vec<ProfileResult>>` containing the profiling results.
    #[rpc(meta, name = "surfnet_getProfileResultsByTag")]
    fn get_profile_results_by_tag(
        &self,
        meta: Self::Metadata,
        tag: String,
        config: Option<RpcProfileResultConfig>,
    ) -> Result<RpcResponse<Option<Vec<UiKeyedProfileResult>>>>;

    /// A "cheat code" method for developers to set or update the network supply information in Surfpool.
    ///
    /// This method allows developers to configure the total supply, circulating supply,
    /// non-circulating supply, and non-circulating accounts list that will be returned
    /// by the `getSupply` RPC method.
    ///
    /// ## Parameters
    /// - `update`: The `SupplyUpdate` struct containing the optional fields to update:
    ///   - `total`: Optional total supply in lamports
    ///   - `circulating`: Optional circulating supply in lamports
    ///   - `non_circulating`: Optional non-circulating supply in lamports
    ///   - `non_circulating_accounts`: Optional list of non-circulating account addresses
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the supply update was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_setSupply",
    ///   "params": [{
    ///     "total": 1000000000000000,
    ///     "circulating": 800000000000000,
    ///     "non_circulating": 200000000000000,
    ///     "non_circulating_accounts": ["Account1...", "Account2..."]
    ///   }]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {},
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// This method is designed to help developers test supply-related functionality by
    /// allowing them to configure the values returned by `getSupply` without needing
    /// to connect to a real network or manipulate actual token supplies.
    ///
    /// # See Also
    /// - `getSupply`
    #[rpc(meta, name = "surfnet_setSupply")]
    fn set_supply(
        &self,
        meta: Self::Metadata,
        update: SupplyUpdate,
    ) -> BoxFuture<Result<RpcResponse<()>>>;

    /// A cheat code to set the upgrade authority of a program's ProgramData account.
    ///
    /// This method allows developers to directly patch the upgrade authority of a program's ProgramData account.
    ///
    /// ## Parameters
    /// - `program_id`: The base-58 encoded public key of the program.
    /// - `new_authority`: The base-58 encoded public key of the new authority. If omitted, the program will have no upgrade authority.
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the authority update was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_setProgramAuthority",
    ///   "params": [
    ///     "PROGRAM_ID_BASE58",
    ///     "NEW_AUTHORITY_BASE58"
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {},
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_setProgramAuthority")]
    fn set_program_authority(
        &self,
        meta: Self::Metadata,
        program_id_str: String,
        new_authority_str: Option<String>,
    ) -> BoxFuture<Result<RpcResponse<()>>>;

    /// A cheat code to get the transaction profile for a given signature or UUID.
    ///
    /// ## Parameters
    /// - `signature_or_uuid`: The transaction signature (as a base-58 string) or a UUID (as a string) for which to retrieve the profile.
    ///
    /// ## Returns
    /// A `RpcResponse<Option<ProfileResult>>` containing the transaction profile if found, or `None` if not found.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_getTransactionProfile",
    ///   "params": [
    ///     "5Nf3...TxSignatureOrUuidHere"
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "context": {
    ///     "slot": 355684457,
    ///     "apiVersion": "2.2.2"
    ///   },
    ///   "value": { /* ...ProfileResult object... */ },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_getTransactionProfile")]
    fn get_transaction_profile(
        &self,
        meta: Self::Metadata,
        signature_or_uuid: UuidOrSignature,
        config: Option<RpcProfileResultConfig>,
    ) -> Result<RpcResponse<Option<UiKeyedProfileResult>>>;

    /// A cheat code to register an IDL for a given program in memory.
    ///
    /// ## Parameters
    /// - `idl`: The full IDL object to be registered in memory. The `address` field should match the program's public key.
    /// - `slot` (optional): The slot at which to register the IDL. If omitted, uses the latest slot.
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the IDL registration was successful.
    ///
    /// ## Example Request (with slot)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_registerIdl",
    ///   "params": [
    ///     {
    ///       "address": "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa",
    ///       "metadata": {
    ///         "name": "test",
    ///         "version": "0.1.0",
    ///         "spec": "0.1.0",
    ///         "description": "Created with Anchor"
    ///       },
    ///       "instructions": [
    ///         {
    ///           "name": "initialize",
    ///           "discriminator": [175,175,109,31,13,152,155,237],
    ///           "accounts": [],
    ///           "args": []
    ///         }
    ///       ],
    ///       "accounts": [],
    ///       "types": [],
    ///       "events": [],
    ///       "errors": [],
    ///       "constants": [],
    ///       "state": null
    ///     },
    ///     355684457
    ///   ]
    /// }
    /// ```
    /// ## Example Request (without slot)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_registerIdl",
    ///   "params": [
    ///     {
    ///       "address": "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa",
    ///       "metadata": {
    ///         "name": "test",
    ///         "version": "0.1.0",
    ///         "spec": "0.1.0",
    ///         "description": "Created with Anchor"
    ///       },
    ///       "instructions": [
    ///         {
    ///           "name": "initialize",
    ///           "discriminator": [175,175,109,31,13,152,155,237],
    ///           "accounts": [],
    ///           "args": []
    ///         }
    ///       ],
    ///       "accounts": [],
    ///       "types": [],
    ///       "events": [],
    ///       "errors": [],
    ///       "constants": [],
    ///       "state": null
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "context": {
    ///     "slot": 355684457,
    ///     "apiVersion": "2.2.2"
    ///   },
    ///   "value": null,
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_registerIdl")]
    fn register_idl(
        &self,
        meta: Self::Metadata,
        idl: Idl,
        slot: Option<Slot>,
    ) -> Result<RpcResponse<()>>;

    /// A cheat code to get the registered IDL for a given program ID.
    ///
    /// ## Parameters
    /// - `program_id`: The base-58 encoded public key of the program whose IDL is being requested.
    /// - `slot` (optional): The slot at which to query the IDL. If omitted, uses the latest slot.
    ///
    /// ## Returns
    /// A `RpcResponse<Option<Idl>>` containing the IDL if it exists, or `None` if not found.
    ///
    /// ## Example Request (with slot)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_getIdl",
    ///   "params": [
    ///     "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa",
    ///     355684457
    ///   ]
    /// }
    /// ```
    /// ## Example Request (without slot)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_getIdl",
    ///   "params": [
    ///     "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa"
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "context": {
    ///     "slot": 355684457,
    ///     "apiVersion": "2.2.2"
    ///   },
    ///   "value": { /* ...IDL object... */ },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_getActiveIdl")]
    fn get_idl(
        &self,
        meta: Self::Metadata,
        program_id: String,
        slot: Option<Slot>,
    ) -> Result<RpcResponse<Option<Idl>>>;

    /// A cheat code to get the last 50 local signatures from the local network.
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_getLocalSignatures",
    ///   "params": [ { "limit": 50 } ]
    /// }
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "signature": String,
    ///     "err": Option<TransactionError>,
    ///     "slot": u64,
    ///   },
    ///   "id": 1
    /// }
    #[rpc(meta, name = "surfnet_getLocalSignatures")]
    fn get_local_signatures(
        &self,
        meta: Self::Metadata,
        limit: Option<u64>,
    ) -> BoxFuture<Result<RpcResponse<Vec<RpcLogsResponse>>>>;

    /// A cheat code to jump forward or backward in time on the local network.
    /// Useful for testing epoch-based or time-sensitive logic.
    ///
    /// ## Parameters
    /// - `config` (optional): A `TimeTravelConfig` specifying how to modify the clock:
    ///   - `absoluteTimestamp(u64)`: Moves time to the specified UNIX timestamp.
    ///   - `absoluteSlot(u64)`: Moves to the specified absolute slot.
    ///   - `absoluteEpoch(u64)`: Advances time to the specified epoch (each epoch = 432,000 slots).
    ///
    /// ## Returns
    /// An `EpochInfo` object reflecting the updated clock state.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_timeTravel",
    ///   "params": [ { "absoluteSlot": 512 } ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "epoch": 512,
    ///     "slot_index": 0,
    ///     "slots_in_epoch": 432000,
    ///     "absolute_slot": 221184000,
    ///     "block_height": 650000000,
    ///     "transaction_count": 923472834
    ///   },
    ///   "id": 1
    /// }
    #[rpc(meta, name = "surfnet_timeTravel")]
    fn time_travel(
        &self,
        meta: Self::Metadata,
        config: Option<TimeTravelConfig>,
    ) -> Result<EpochInfo>;

    /// A cheat code to freeze the Surfnet clock on the local network.
    /// All time progression halts until resumed.
    ///
    /// ## Returns
    /// An `EpochInfo` object showing the current clock state at the moment of pause.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_pauseClock",
    ///   "params": []
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "epoch": 512,
    ///     "slot_index": 0,
    ///     "slots_in_epoch": 432000,
    ///     "absolute_slot": 221184000,
    ///     "block_height": 650000000,
    ///     "transaction_count": 923472834
    ///   },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_pauseClock")]
    fn pause_clock(&self, meta: Self::Metadata) -> Result<EpochInfo>;

    /// A cheat code to resume Solana clock progression after it was paused.
    /// The validator will start producing new slots again.
    ///
    /// ## Parameters
    ///
    /// ## Returns
    /// An `EpochInfo` object reflecting the resumed clock state.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_resumeClock",
    ///   "params": []
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "epoch": 512,
    ///     "slot_index": 0,
    ///     "slots_in_epoch": 432000,
    ///     "absolute_slot": 221184000,
    ///     "block_height": 650000000,
    ///     "transaction_count": 923472834
    ///   },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_resumeClock")]
    fn resume_clock(&self, meta: Self::Metadata) -> Result<EpochInfo>;

    /// A cheat code to reset an account on the local network.
    ///
    /// ## Parameters
    /// - `pubkey_str`: The base-58 encoded public key of the account to reset.
    /// - `config`: A `ResetAccountConfig` specifying how to reset the account. If omitted, the account will be reset without cascading to owned accounts.
    ///
    /// ## Returns
    /// An `RpcResponse<()>` indicating whether the account reset was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_resetAccount",
    ///   "params": [ "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa", { "includeOwnedAccounts": true } ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 123456789,
    ///       "apiVersion": "2.3.8"
    ///     },
    ///     "value": null
    ///   },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_resetAccount")]
    fn reset_account(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<ResetAccountConfig>,
    ) -> Result<RpcResponse<()>>;

    /// A cheat code to reset a network.
    ///
    /// ## Returns
    /// An `RpcResponse<()>` indicating whether the network reset was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_resetNetwork",    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 123456789,
    ///       "apiVersion": "2.3.8"
    ///     },
    ///     "value": null    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///

    #[rpc(meta, name = "surfnet_resetNetwork")]
    fn reset_network(&self, meta: Self::Metadata) -> BoxFuture<Result<RpcResponse<()>>>;

    /// A cheat code to prevent an account from being downloaded from the remote RPC.
    ///
    /// ## Parameters
    /// - `pubkey_str`: The base-58 encoded public key of the account/program to block.
    /// - `config`: A `OfflineAccountConfig` specifying whether to also mark accounts offline
    ///   owned by this pubkey. If omitted, only the account itself is marked offline.
    ///
    /// ## Returns
    /// An `RpcResponse<()>` indicating whether the download block registration was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_offlineAccount",
    ///   "params": [ "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa", { "includeOwnedAccounts": true } ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 123456789,
    ///       "apiVersion": "2.3.8"
    ///     },
    ///     "value": null
    ///   },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_offlineAccount")]
    fn offline_account(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<OfflineAccountConfig>,
    ) -> BoxFuture<Result<RpcResponse<()>>>;

    /// A cheat code to export a snapshot of all accounts in the Surfnet SVM.
    ///
    /// This method retrieves the current state of all accounts stored in the Surfnet Virtual Machine (SVM)
    /// and returns them as a mapping of account public keys to their respective account snapshots.
    ///
    /// ## Parameters
    /// - `config`: An optional `ExportSnapshotConfig` to customize the export behavior. The config fields are:
    ///     - `includeParsedAccounts`: If true, includes parsed account data in the snapshot.
    ///     - `filter`: An optional filter config to limit which accounts are included in the snapshot. Fields include:
    ///         - `includeProgramAccounts`: A boolean indicating whether to include program accounts.
    ///         - `includeAccounts`: A list of specific account public keys to include. Pubkeys listed here bypass `excludeSysvars` and `excludeFeatureGates`.
    ///         - `excludeAccounts`: A list of specific account public keys to exclude. Takes precedence over `includeAccounts`.
    ///         - `excludeSysvars`: If true, omit accounts owned by the sysvar program.
    ///         - `excludeFeatureGates`: If true, omit accounts whose pubkey is a known agave feature gate (per the `agave_feature_set::FEATURE_NAMES` set compiled into this binary).
    ///     - `scope`: An optional scope to limit the accounts included in the snapshot. Options include:
    ///         - `network`: Includes all accounts in the network.
    ///         - `preTransaction`: Only includes accounts touched by the given transaction.
    ///
    ///
    /// ## Returns
    /// An `RpcResponse<BTreeMap<String, AccountSnapshot>>` containing the exported account snapshots.
    ///
    /// The keys of the map are the base-58 encoded public keys of the accounts,
    /// and the values are the corresponding `AccountSnapshot` objects.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_exportSnapshot"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa": {
    ///       "lamports": 1000000,
    ///       "owner": "11111111111111111111111111111111",
    ///       "executable": false,
    ///       "rent_epoch": 0,
    ///       "data": "base64_encoded_data_string"
    ///     },
    ///     "AnotherAccountPubkeyBase58": {
    ///       "lamports": 500000,
    ///       "owner": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    ///       "executable": false,
    ///       "rent_epoch": 0,
    ///       "data": "base64_encoded_data_string"
    ///     }
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    #[rpc(meta, name = "surfnet_exportSnapshot")]
    fn export_snapshot(
        &self,
        meta: Self::Metadata,
        config: Option<ExportSnapshotConfig>,
    ) -> Result<RpcResponse<BTreeMap<String, AccountSnapshot>>>;

    /// A cheat code to simulate account streaming.
    /// When a transaction is processed, the accounts that are accessed are downloaded from the datasource and cached in the SVM.
    /// With this method, you can simulate the streaming of accounts by providing a pubkey.
    ///
    /// ## Parameters
    /// - `pubkey_str`: The base-58 encoded public key of the account to stream.
    /// - `config`: A `StreamAccountConfig` specifying how to stream the account. If omitted, the account will be streamed without cascading to owned accounts.
    ///
    /// ## Returns
    /// An `RpcResponse<()>` indicating whether the account stream registration was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_streamAccount",
    ///   "params": [ "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa", { "includeOwnedAccounts": true } ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 123456789,
    ///       "apiVersion": "2.3.8"
    ///     },
    ///     "value": null
    ///   },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_streamAccount")]
    fn stream_account(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<StreamAccountConfig>,
    ) -> Result<RpcResponse<()>>;

    /// A cheat code to simulate account streaming for multiple accounts at once.
    /// When a transaction is processed, the accounts that are accessed are downloaded from the datasource and cached in the SVM.
    /// With this method, you can simulate the streaming of multiple accounts by providing a list of entries.
    ///
    /// ## Parameters
    /// - `accounts`: A list of `StreamAccountsEntry` objects, each containing a `pubkey` (base-58 encoded) and an optional `includeOwnedAccounts` boolean.
    ///
    /// ## Returns
    /// An `RpcResponse<()>` indicating whether the account stream registrations were successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_streamAccounts",
    ///   "params": [
    ///     [
    ///       { "pubkey": "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa", "includeOwnedAccounts": true },
    ///       { "pubkey": "7nYBm5mk15oDNewVjNFmEqJ9VgMvT4F74UVoeYDCpScd" }
    ///     ]
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 123456789,
    ///       "apiVersion": "2.3.8"
    ///     },
    ///     "value": null
    ///   },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_streamAccounts")]
    fn stream_accounts(
        &self,
        meta: Self::Metadata,
        accounts: Vec<StreamAccountsEntry>,
    ) -> Result<RpcResponse<()>>;

    /// A cheat code to retrieve the streamed accounts.
    /// When a transaction is processed, the accounts that are accessed are downloaded from the datasource and cached in the SVM.
    /// With this method, you can simulate the streaming of accounts by providing a pubkey.
    ///
    /// ## Parameters
    ///
    /// ## Returns
    /// An `RpcResponse<()>` indicating whether the account stream registration was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_getStreamedAccounts",
    ///   "params": []
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 123456789,
    ///       "apiVersion": "2.3.8"
    ///     },
    ///     "value": [
    ///       "4EXSeLGxVBpAZwq7vm6evLdewpcvE2H56fpqL2pPiLFa"
    ///     ]
    ///    },
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_getStreamedAccounts")]
    fn get_streamed_accounts(
        &self,
        meta: Self::Metadata,
    ) -> Result<RpcResponse<GetStreamedAccountsResponse>>;

    /// A cheat code to get Surfnet network information.
    ///
    /// ## Returns
    /// A `RpcResponse<GetSurfnetInfoResponse>` containing the Surfnet network information.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_getSurfnetInfo"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 369027326,
    ///       "apiVersion": "2.3.8"
    ///     },
    ///     "value": {
    ///       "runbookExecutions": [
    ///         {
    ///           "startedAt": 1758747828,
    ///           "completedAt": 1758747828,
    ///           "runbookId": "deployment"
    ///         }
    ///       ]
    ///     }
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    #[rpc(meta, name = "surfnet_getSurfnetInfo")]
    fn get_surfnet_info(&self, meta: Self::Metadata)
    -> Result<RpcResponse<GetSurfnetInfoResponse>>;

    /// A "cheat code" method for developers to write program data at a specified offset in Surfpool.
    ///
    /// This method allows developers to write large Solana programs by sending data in chunks,
    /// bypassing transaction size limits by using RPC size limits (5MB) instead.
    ///
    /// ## Parameters
    /// - `program_id`: The public key of the program account, as a base-58 encoded string.
    /// - `data`: Hex-encoded program data chunk to write.
    /// - `offset`: The byte offset at which to write this data chunk.
    /// - `authority` (optional): The base-58 encoded public key of the authority allowed to write to the program. If omitted, defaults to the system program.
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the write was successful.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_writeProgram",
    ///   "params": [
    ///     "program_pubkey",
    ///     "deadbeef...",
    ///     0
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {},
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// This method is designed to help developers deploy large programs by writing data incrementally.
    /// Multiple calls can be made with different offsets to build the complete program.
    /// The program account and program data account will be created automatically if they don't exist.
    #[rpc(meta, name = "surfnet_writeProgram")]
    fn write_program(
        &self,
        meta: Self::Metadata,
        program_id: String,
        data: String,
        offset: usize,
        authority: Option<String>,
    ) -> BoxFuture<Result<RpcResponse<()>>>;

    /// A cheat code to register a scenario with account overrides.
    ///
    /// ## Parameters
    /// - `scenario`: The Scenario object containing:
    ///   - `id`: Unique identifier for the scenario
    ///   - `name`: Human-readable name
    ///   - `description`: Description of the scenario
    ///   - `overrides`: Array of OverrideInstance objects, each containing:
    ///     - `id`: Unique identifier for this override instance
    ///     - `templateId`: Reference to the override template
    ///     - `values`: HashMap of field paths to override values
    ///     - `scenarioRelativeSlot`: The relative slot offset (from base slot) when this override should be applied
    ///     - `label`: Optional label for this override
    ///     - `enabled`: Whether this override is active
    ///     - `fetchBeforeUse`: If true, fetch fresh account data just before transaction execution (useful for price feeds, oracle updates, and dynamic balances)
    ///     - `account`: Account address (either `{ "pubkey": "..." }` or `{ "pda": { "programId": "...", "seeds": [...] } }`)
    ///   - `tags`: Array of tags for categorization
    /// - `slot` (optional): The base slot from which relative slot offsets are calculated. If omitted, uses the current slot.
    ///
    /// ## Returns
    /// A `RpcResponse<()>` indicating whether the Scenario registration was successful.
    ///
    /// ## Example Request (with slot)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_registerScenario",
    ///   "params": [
    ///     {
    ///       "id": "scenario-1",
    ///       "name": "Price Feed Override",
    ///       "description": "Override Pyth BTC/USD price at specific slots",
    ///       "overrides": [
    ///         {
    ///           "id": "override-1",
    ///           "templateId": "pyth_btcusd",
    ///           "values": {
    ///             "price_message.price_value": 67500,
    ///             "price_message.conf": 100,
    ///             "price_message.expo": -8
    ///           },
    ///           "scenarioRelativeSlot": 100,
    ///           "label": "Set BTC price to $67,500",
    ///           "enabled": true,
    ///           "fetchBeforeUse": false,
    ///           "account": {
    ///             "pubkey": "H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4QJNYH"
    ///           }
    ///         }
    ///       ],
    ///       "tags": ["defi", "price-feed"]
    ///     },
    ///     355684457
    ///   ]
    /// }
    /// ```
    /// ## Example Request (without slot)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "surfnet_registerScenario",
    ///   "params": [
    ///     {
    ///       "id": "scenario-1",
    ///       "name": "Price Feed Override",
    ///       "description": "Override Pyth BTC/USD price",
    ///       "overrides": [
    ///         {
    ///           "id": "override-1",
    ///           "templateId": "pyth_btcusd",
    ///           "values": {
    ///             "price_message.price_value": 67500
    ///           },
    ///           "scenarioRelativeSlot": 100,
    ///           "label": "Set BTC price",
    ///           "enabled": true,
    ///           "fetchBeforeUse": true,
    ///           "account": {
    ///             "pubkey": "H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4QJNYH"
    ///           }
    ///         }
    ///       ],
    ///       "tags": []
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "context": {
    ///     "slot": 355684457,
    ///     "apiVersion": "2.2.2"
    ///   },
    ///   "value": null,
    ///   "id": 1
    /// }
    /// ```
    #[rpc(meta, name = "surfnet_registerScenario")]
    fn register_scenario(
        &self,
        meta: Self::Metadata,
        scenario: Scenario,
        slot: Option<Slot>,
    ) -> Result<RpcResponse<()>>;
}

#[derive(Clone)]
pub struct SurfnetCheatcodesRpc {
    pub registered_methods: Arc<RwLock<Vec<String>>>,
}
impl SurfnetCheatcodesRpc {
    pub fn empty() -> Self {
        Self {
            registered_methods: Arc::new(RwLock::new(vec![])),
        }
    }
    pub fn is_available_cheatcode(&self, cheatcode: &String) -> bool {
        let Ok(methods) = self.registered_methods.read() else {
            return false;
        };
        methods.contains(cheatcode)
    }
}

impl SurfnetCheatcodes for SurfnetCheatcodesRpc {
    type Metadata = Option<RunloopContext>;

    fn set_account(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        update: AccountUpdate,
    ) -> BoxFuture<Result<RpcResponse<()>>> {
        let pubkey = match verify_pubkey(&pubkey_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };
        let account_update_opt = match update.to_account_ext() {
            Err(e) => return Box::pin(future::err(e)),
            Ok(res) => res,
        };

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(CommitmentConfig::confirmed()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            let (account_to_set, latest_absolute_slot) = if let Some(account) = account_update_opt {
                (
                    GetAccountResult::FoundAccount(pubkey, account, true),
                    svm_locker.get_latest_absolute_slot(),
                )
            } else {
                // otherwise, we need to fetch the account and apply the update
                let SvmAccessContext {
                    slot, inner: mut account_result_to_update,
                    ..
                } = svm_locker.get_account(&remote_ctx, &pubkey, Some(Box::new(move |svm_locker| {

                            // if the account does not exist locally or in the remote, create a new account with default values
                            let _ = svm_locker.simnet_events_tx().send(SimnetEvent::info(format!(
                                "Account {pubkey} not found, creating a new account from default values"
                            )));
                            GetAccountResult::FoundAccount(
                                pubkey,
                                solana_account::Account {
                                    lamports: 0,
                                    owner: system_program::id(),
                                    executable: false,
                                    rent_epoch: 0,
                                    data: vec![],
                                },
                                true, // indicate that the account should be updated in the SVM, since it's new
                            )
                }))).await?;

                update.apply_ext(&mut account_result_to_update)?;
                (account_result_to_update, slot)
            };

            svm_locker.write_account_update(account_to_set);

            Ok(RpcResponse {
                context: RpcResponseContext::new(latest_absolute_slot),
                value: (),
            })
        })
    }

    fn disable_cheatcode(
        &self,
        meta: Self::Metadata,
        cheatcodes_filter: CheatcodeFilter,
        control_config: Option<CheatcodeControlConfig>,
    ) -> Result<RpcResponse<()>> {
        let svm_locker = match meta.get_svm_locker() {
            Ok(locker) => locker,
            Err(e) => return Err(e.into()),
        };

        let CheatcodeControlConfig { lockout } = control_config.unwrap_or_default();
        let lockout = lockout.unwrap_or_default();

        if let Some(runloop_ctx) = meta {
            let Ok(mut cheatcode_ctx) = runloop_ctx.cheatcode_config.lock() else {
                return Err(jsonrpc_core::Error::internal_error());
            };

            if lockout {
                cheatcode_ctx.lockout();
            }

            match cheatcodes_filter {
                CheatcodeFilter::All(all) => {
                    if all.ne("all") {
                        return Err(SurfpoolError::disable_cheatcode(
                            "Invalid option provided for disabling all cheatcodes. Try using 'all' or providing an array of specific cheatcodes".to_string(),
                        )
                        .into());
                    }

                    let Ok(available_cheatcodes) = self.registered_methods.read() else {
                        return Err(jsonrpc_core::Error::internal_error());
                    };
                    cheatcode_ctx.disable_all(lockout, (*available_cheatcodes).clone());
                }
                CheatcodeFilter::List(cheatcodes) => {
                    // Validate all cheatcodes before disabling any (atomic processing)
                    for cheatcode in &cheatcodes {
                        if !lockout
                            && (cheatcode.eq("surfnet_enableCheatcode")
                                || cheatcode.eq("surfnet_disableCheatcode"))
                        {
                            return Err(SurfpoolError::disable_cheatcode(
                                "Cannot disable surfnet_enableCheatcode or surfnet_disableCheatcode rpc method when lockout is not enabled".to_string(),
                            )
                            .into());
                        }
                        if !self.is_available_cheatcode(cheatcode) {
                            return Err(SurfpoolError::disable_cheatcode(
                                "Invalid cheatcode rpc method".to_string(),
                            )
                            .into());
                        }
                    }
                    // Apply all after validation passes
                    for cheatcode in cheatcodes {
                        if let Err(e) = cheatcode_ctx.disable_cheatcode(&cheatcode) {
                            return Err(SurfpoolError::disable_cheatcode(e).into());
                        }
                    }
                }
            }
        }

        Ok(RpcResponse {
            value: (),
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
        })
    }

    fn enable_cheatcode(
        &self,
        meta: Self::Metadata,
        cheatcodes_filter: CheatcodeFilter,
    ) -> Result<RpcResponse<()>> {
        let svm_locker = match meta.get_svm_locker() {
            Ok(locker) => locker,
            Err(e) => return Err(e.into()),
        };
        if let Some(runloop_ctx) = meta {
            let Ok(mut cheatcode_ctx) = runloop_ctx.cheatcode_config.lock() else {
                return Err(jsonrpc_core::Error::internal_error());
            };
            match cheatcodes_filter {
                CheatcodeFilter::All(all) => {
                    if all.ne("all") {
                        return Err(SurfpoolError::enable_cheatcode(
                            "Invalid option provided for enabling all cheatcodes. Try using 'all' or providing an array of specific cheatcodes".to_string(),
                        )
                        .into());
                    }

                    // we probably don't need to check whether lockout == true because surfnet_enableCheatcode won't be called if it's disabled
                    cheatcode_ctx.filter = CheatcodeFilter::List(vec![]);
                }
                CheatcodeFilter::List(cheatcodes) => {
                    for ref cheatcode in cheatcodes {
                        debug!("enabling cheatcode: {cheatcode}");
                        if !self.is_available_cheatcode(cheatcode) {
                            return Err(SurfpoolError::enable_cheatcode(
                                "Invalid cheatcode rpc method".to_string(),
                            )
                            .into());
                        }

                        if let Err(e) = cheatcode_ctx.enable_cheatcode(cheatcode) {
                            return Err(SurfpoolError::enable_cheatcode(e).into());
                        }
                    }
                }
            }
        }

        Ok(RpcResponse {
            value: (),
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
        })
    }

    fn set_token_account(
        &self,
        meta: Self::Metadata,
        owner_str: String,
        mint_str: String,
        update: TokenAccountUpdate,
        some_token_program_str: Option<String>,
    ) -> BoxFuture<Result<RpcResponse<()>>> {
        let owner = match verify_pubkey(&owner_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        let mint = match verify_pubkey(&mint_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        let is_native_mint = mint == spl_token_interface::native_mint::id();
        let token_amount = update.amount.unwrap_or(0);

        let token_program_id = match some_token_program_str {
            Some(token_program_str) => match verify_pubkey(&token_program_str) {
                Ok(res) => res,
                Err(e) => return e.into(),
            },
            None => spl_token_interface::id(),
        };

        let associated_token_account =
            get_associated_token_address_with_program_id(&owner, &mint, &token_program_id);

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(CommitmentConfig::confirmed()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            let get_mint_result = svm_locker
                .get_account(&remote_ctx, &mint, None)
                .await?
                .inner;
            svm_locker.write_account_update(get_mint_result);

            let minimum_rent = svm_locker.with_svm_reader(|svm_reader| {
                svm_reader.inner.minimum_balance_for_rent_exemption(
                    TokenAccount::get_packed_len_for_token_program_id(&token_program_id),
                )
            });

            let (rent_exempt_reserve, initial_lamports) = if is_native_mint {
                // For native mint, we need to allocate enough lamports to cover the wrapped SOL amount
                (Some(minimum_rent), minimum_rent + token_amount) // 1 SOL wrapped
            } else {
                (None, minimum_rent)
            };

            let SvmAccessContext {
                slot,
                inner: mut token_account,
                ..
            } = svm_locker
                .get_account(
                    &remote_ctx,
                    &associated_token_account,
                    Some(Box::new(move |_| {
                        let default =
                            TokenAccount::new(&token_program_id, owner, mint, rent_exempt_reserve);
                        let data = default.pack_into_vec();
                        GetAccountResult::FoundAccount(
                            associated_token_account,
                            Account {
                                lamports: initial_lamports,
                                owner: token_program_id,
                                executable: false,
                                rent_epoch: 0,
                                data,
                            },
                            true, // indicate that the account should be updated in the SVM, since it's new
                        )
                    })),
                )
                .await?;

            let mut token_account_data = TokenAccount::unpack(token_account.expected_data())
                .map_err(|e| {
                    Error::invalid_params(format!("Failed to unpack token account data: {}", e))
                })?;

            update.apply(&mut token_account_data)?;

            let final_account_bytes = token_account_data.pack_into_vec();
            token_account.apply_update(|account| {
                // If this is a native mint, we need to adjust the lamports to match the wrapped SOL amount + rent
                account.lamports = initial_lamports;
                account.data = final_account_bytes.clone();
                Ok(())
            })?;
            svm_locker.write_account_update(token_account);

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: (),
            })
        })
    }

    /// Clones a program account from one program ID to another.
    /// A program account contains a pointer to a program data account, which is a PDA derived from the program ID.
    /// So, when cloning a program account, we need to clone the program data account as well.
    ///
    /// This method will:
    ///  1. Get the program account for the source program ID.
    ///  2. Get the program data account for the source program ID.
    ///  3. Calculate the program data address for the destination program ID.
    ///  4. Set the destination program account's data to point to the calculated destination program address.
    ///  5. Copy the source program data account to the destination program data account.
    fn clone_program_account(
        &self,
        meta: Self::Metadata,
        source_program_id: String,
        destination_program_id: String,
    ) -> BoxFuture<Result<RpcResponse<()>>> {
        let source_program_id = match verify_pubkey(&source_program_id) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };
        let destination_program_id = match verify_pubkey(&destination_program_id) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(CommitmentConfig::confirmed()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            let SvmAccessContext { slot, .. } = svm_locker
                .clone_program_account(&remote_ctx, &source_program_id, &destination_program_id)
                .await?;

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: (),
            })
        })
    }

    fn profile_transaction(
        &self,
        meta: Self::Metadata,
        transaction_data_b64: String,
        tag: Option<String>,
        config: Option<RpcProfileResultConfig>,
    ) -> BoxFuture<Result<RpcResponse<UiKeyedProfileResult>>> {
        Box::pin(async move {
            let transaction_bytes = STANDARD
                .decode(&transaction_data_b64)
                .map_err(|e| SurfpoolError::invalid_base64_data("transaction", e))?;
            let transaction: VersionedTransaction = bincode::deserialize(&transaction_bytes)
                .map_err(|e| SurfpoolError::deserialize_error("transaction", e))?;

            let SurfnetRpcContext {
                svm_locker,
                remote_ctx,
            } = meta.get_rpc_context(CommitmentConfig::confirmed())?;

            let SvmAccessContext {
                slot, inner: uuid, ..
            } = svm_locker
                .profile_transaction(&remote_ctx, transaction, tag.clone())
                .await?;

            let key = UuidOrSignature::Uuid(uuid);

            let config = config.unwrap_or_default();
            let ui_result = svm_locker
                .get_profile_result(key, &config)?
                .ok_or(SurfpoolError::expected_profile_not_found(&key))?;

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: ui_result,
            })
        })
    }

    fn get_profile_results_by_tag(
        &self,
        meta: Self::Metadata,
        tag: String,
        config: Option<RpcProfileResultConfig>,
    ) -> Result<RpcResponse<Option<Vec<UiKeyedProfileResult>>>> {
        let config = config.unwrap_or_default();
        let svm_locker = meta.get_svm_locker()?;
        let profiles = svm_locker.get_profile_results_by_tag(tag, &config)?;
        let slot = svm_locker.get_latest_absolute_slot();
        Ok(RpcResponse {
            context: RpcResponseContext::new(slot),
            value: profiles,
        })
    }

    fn set_supply(
        &self,
        meta: Self::Metadata,
        update: SupplyUpdate,
    ) -> BoxFuture<Result<RpcResponse<()>>> {
        let svm_locker = match meta.get_svm_locker() {
            Ok(locker) => locker,
            Err(e) => return e.into(),
        };

        // validate non-circulating accounts are valid pubkeys
        if let Some(ref non_circulating_accounts) = update.non_circulating_accounts {
            if let Err(e) = verify_pubkeys(non_circulating_accounts) {
                return e.into();
            }
        }

        Box::pin(async move {
            let latest_absolute_slot = svm_locker.with_svm_writer(|svm_writer| {
                // update the supply fields if provided
                if let Some(total) = update.total {
                    svm_writer.total_supply = total;
                }

                if let Some(circulating) = update.circulating {
                    svm_writer.circulating_supply = circulating;
                }

                if let Some(non_circulating) = update.non_circulating {
                    svm_writer.non_circulating_supply = non_circulating;
                }

                if let Some(ref accounts) = update.non_circulating_accounts {
                    svm_writer.non_circulating_accounts = accounts.clone();
                }

                svm_writer.get_latest_absolute_slot()
            });

            Ok(RpcResponse {
                context: RpcResponseContext::new(latest_absolute_slot),
                value: (),
            })
        })
    }

    fn set_program_authority(
        &self,
        meta: Self::Metadata,
        program_id_str: String,
        new_authority_str: Option<String>,
    ) -> BoxFuture<Result<RpcResponse<()>>> {
        let program_id = match verify_pubkey(&program_id_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };
        let new_authority = if let Some(ref new_authority_str) = new_authority_str {
            match verify_pubkey(new_authority_str) {
                Ok(res) => Some(res),
                Err(e) => return e.into(),
            }
        } else {
            None
        };

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(CommitmentConfig::confirmed()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };
        Box::pin(async move {
            let SvmAccessContext { slot, .. } = svm_locker
                .set_program_authority(&remote_ctx, program_id, new_authority)
                .await?;

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: (),
            })
        })
    }

    fn get_transaction_profile(
        &self,
        meta: Self::Metadata,
        signature_or_uuid: UuidOrSignature,
        config: Option<RpcProfileResultConfig>,
    ) -> Result<RpcResponse<Option<UiKeyedProfileResult>>> {
        let config = config.unwrap_or_default();
        let svm_locker = meta.get_svm_locker()?;
        let profile_result = svm_locker.get_profile_result(signature_or_uuid, &config)?;
        let context_slot = profile_result
            .as_ref()
            .map(|pr| pr.slot)
            .unwrap_or_else(|| svm_locker.get_latest_absolute_slot());
        Ok(RpcResponse {
            context: RpcResponseContext::new(context_slot),
            value: profile_result,
        })
    }

    fn register_idl(
        &self,
        meta: Self::Metadata,
        idl: Idl,
        slot: Option<Slot>,
    ) -> Result<RpcResponse<()>> {
        let svm_locker = match meta.get_svm_locker() {
            Ok(locker) => locker,
            Err(e) => return Err(e.into()),
        };
        svm_locker.register_idl(idl, slot)?;
        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value: (),
        })
    }

    fn get_idl(
        &self,
        meta: Self::Metadata,
        program_id: String,
        slot: Option<Slot>,
    ) -> Result<RpcResponse<Option<Idl>>> {
        let svm_locker = match meta.get_svm_locker() {
            Ok(locker) => locker,
            Err(e) => return Err(e.into()),
        };
        let program_id = match verify_pubkey(&program_id) {
            Ok(pk) => pk,
            Err(e) => return Err(e.into()),
        };
        let idl = svm_locker.get_idl(&program_id, slot);
        let slot = slot.unwrap_or_else(|| svm_locker.get_latest_absolute_slot());
        Ok(RpcResponse {
            context: RpcResponseContext::new(slot),
            value: idl,
        })
    }

    fn get_local_signatures(
        &self,
        meta: Self::Metadata,
        limit: Option<u64>,
    ) -> BoxFuture<Result<RpcResponse<Vec<RpcLogsResponse>>>> {
        let svm_locker = match meta.get_svm_locker() {
            Ok(locker) => locker,
            Err(e) => return e.into(),
        };

        let limit = limit.unwrap_or(50);
        let latest = svm_locker.get_latest_absolute_slot();
        if limit == 0 {
            return Box::pin(async move {
                Ok(RpcResponse {
                    context: RpcResponseContext::new(latest),
                    value: Vec::new(),
                })
            });
        }

        let mut items: Vec<(
            String,
            Slot,
            Option<solana_transaction_error::TransactionError>,
            Vec<String>,
        )> = svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .transactions
                .into_iter()
                .map(|iter| {
                    iter.map(|(sig, status)| {
                        let (transaction_with_status_meta, _) = status.expect_processed();
                        (
                            sig,
                            transaction_with_status_meta.slot,
                            transaction_with_status_meta.meta.status.clone().err(),
                            transaction_with_status_meta
                                .meta
                                .log_messages
                                .clone()
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
                })
                .unwrap_or_default()
        });

        items.sort_by(|a, b| b.1.cmp(&a.1));
        items.truncate(limit as usize);

        let value: Vec<RpcLogsResponse> = items
            .into_iter()
            .map(|(signature, _slot, err, logs)| RpcLogsResponse {
                signature,
                err: err.map(|e| e.into()),
                logs,
            })
            .collect();

        Box::pin(async move {
            Ok(RpcResponse {
                context: RpcResponseContext::new(latest),
                value,
            })
        })
    }

    fn pause_clock(&self, meta: Self::Metadata) -> Result<EpochInfo> {
        let key = meta.as_ref().map(|ctx| ctx.id.clone()).unwrap_or_default();
        let surfnet_command_tx: crossbeam_channel::Sender<SimnetCommand> =
            meta.get_surfnet_command_tx()?;

        // Create a channel to receive confirmation
        let (response_tx, response_rx) = crossbeam_channel::bounded(1);

        // Send pause command with confirmation
        let _ = surfnet_command_tx.send(SimnetCommand::CommandClock(
            key,
            ClockCommand::PauseWithConfirmation(response_tx),
        ));

        // Wait for confirmation with timeout
        response_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|e| jsonrpc_core::Error {
                code: jsonrpc_core::ErrorCode::InternalError,
                message: format!("Failed to confirm clock pause: {}", e),
                data: None,
            })
    }

    fn resume_clock(&self, meta: Self::Metadata) -> Result<EpochInfo> {
        let key = meta.as_ref().map(|ctx| ctx.id.clone()).unwrap_or_default();
        let surfnet_command_tx: crossbeam_channel::Sender<SimnetCommand> =
            meta.get_surfnet_command_tx()?;
        let _ = surfnet_command_tx.send(SimnetCommand::CommandClock(key, ClockCommand::Resume));
        meta.with_svm_reader(|svm_reader| svm_reader.latest_epoch_info.clone())
            .map_err(Into::into)
    }

    fn time_travel(
        &self,
        meta: Self::Metadata,
        config: Option<TimeTravelConfig>,
    ) -> Result<EpochInfo> {
        let key = meta.as_ref().map(|ctx| ctx.id.clone()).unwrap_or_default();
        let time_travel_config = config.unwrap_or_default();
        let simnet_command_tx = meta.get_surfnet_command_tx()?;
        let svm_locker = meta.get_svm_locker()?;

        let epoch_info = svm_locker.time_travel(key, simnet_command_tx, time_travel_config)?;

        Ok(epoch_info)
    }

    fn reset_account(
        &self,
        meta: Self::Metadata,
        pubkey: String,
        config: Option<ResetAccountConfig>,
    ) -> Result<RpcResponse<()>> {
        let svm_locker = meta.get_svm_locker()?;
        let pubkey = verify_pubkey(&pubkey)?;
        let config = config.unwrap_or_default();
        let include_owned_accounts = config.include_owned_accounts.unwrap_or_default();
        svm_locker.reset_account(pubkey, include_owned_accounts)?;
        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value: (),
        })
    }

    fn reset_network(&self, meta: Self::Metadata) -> BoxFuture<Result<RpcResponse<()>>> {
        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(CommitmentConfig::confirmed()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        // Extract just the remote client from the tuple (ignore commitment config)
        let remote_client = remote_ctx.as_ref().map(|(client, _)| client.clone());

        Box::pin(async move {
            svm_locker.reset_network(&remote_client).await?;
            Ok(RpcResponse {
                context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
                value: (),
            })
        })
    }

    fn offline_account(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<OfflineAccountConfig>,
    ) -> BoxFuture<Result<RpcResponse<()>>> {
        let SurfnetRpcContext { svm_locker, .. } =
            match meta.get_rpc_context(CommitmentConfig::confirmed()) {
                Ok(res) => res,
                Err(e) => return e.into(),
            };
        let pubkey = match verify_pubkey(&pubkey_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };
        let config = config.unwrap_or_default();
        let include_owned_accounts = config.include_owned_accounts.unwrap_or_default();

        Box::pin(async move {
            svm_locker
                .insert_offline_account(pubkey, include_owned_accounts)
                .await?;
            Ok(RpcResponse {
                context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
                value: (),
            })
        })
    }

    fn stream_account(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<StreamAccountConfig>,
    ) -> Result<RpcResponse<()>> {
        let svm_locker = meta.get_svm_locker()?;
        let pubkey = verify_pubkey(&pubkey_str)?;
        let config = config.unwrap_or_default();
        let include_owned_accounts = config.include_owned_accounts.unwrap_or_default();
        svm_locker.stream_account(pubkey, include_owned_accounts)?;
        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value: (),
        })
    }

    fn stream_accounts(
        &self,
        meta: Self::Metadata,
        accounts: Vec<StreamAccountsEntry>,
    ) -> Result<RpcResponse<()>> {
        let svm_locker = meta.get_svm_locker()?;
        for entry in accounts {
            let pubkey = verify_pubkey(&entry.pubkey)?;
            let include_owned_accounts = entry.include_owned_accounts.unwrap_or_default();
            svm_locker.stream_account(pubkey, include_owned_accounts)?;
        }
        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value: (),
        })
    }

    fn get_streamed_accounts(
        &self,
        meta: Self::Metadata,
    ) -> Result<RpcResponse<GetStreamedAccountsResponse>> {
        let svm_locker = meta.get_svm_locker()?;

        let value = svm_locker.with_svm_reader(|svm_reader| {
            let accounts: Vec<_> = svm_reader
                .streamed_accounts
                .into_iter()
                .map(|iter| iter.collect())
                .unwrap_or_default();
            GetStreamedAccountsResponse::from_iter(accounts)
        });

        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value,
        })
    }

    fn get_surfnet_info(
        &self,
        meta: Self::Metadata,
    ) -> Result<RpcResponse<GetSurfnetInfoResponse>> {
        let svm_locker = meta.get_svm_locker()?;
        let runbook_executions = svm_locker.runbook_executions();
        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value: GetSurfnetInfoResponse::new(runbook_executions),
        })
    }

    fn write_program(
        &self,
        meta: Self::Metadata,
        program_id_str: String,
        data_hex: String,
        offset: usize,
        authority: Option<String>,
    ) -> BoxFuture<Result<RpcResponse<()>>> {
        // Validate program_id
        let program_id = match verify_pubkey(&program_id_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        let authority = if let Some(ref authority_str) = authority {
            match verify_pubkey(authority_str) {
                Ok(res) => Some(res),
                Err(e) => return e.into(),
            }
        } else {
            None
        };

        // Decode hex data
        let data = match hex::decode(&data_hex) {
            Ok(data) => data,
            Err(e) => {
                return Box::pin(future::err(Error::invalid_params(format!(
                    "Invalid hex data provided: {}",
                    e
                ))));
            }
        };

        // Validate offset doesn't cause overflow
        if offset.checked_add(data.len()).is_none() {
            return Box::pin(future::err(Error::invalid_params(
                "Offset + data length causes integer overflow",
            )));
        }

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(CommitmentConfig::confirmed()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            let slot = svm_locker.get_latest_absolute_slot();

            svm_locker
                .write_program(program_id, authority, offset, &data, &remote_ctx)
                .await?;

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: (),
            })
        })
    }

    fn export_snapshot(
        &self,
        meta: Self::Metadata,
        config: Option<ExportSnapshotConfig>,
    ) -> Result<RpcResponse<BTreeMap<String, AccountSnapshot>>> {
        let config = config.unwrap_or_default();
        let svm_locker = meta.get_svm_locker()?;
        let snapshot = svm_locker.export_snapshot(config)?;
        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value: snapshot,
        })
    }

    fn register_scenario(
        &self,
        meta: Self::Metadata,
        scenario: Scenario,
        slot: Option<Slot>,
    ) -> Result<RpcResponse<()>> {
        let svm_locker = meta.get_svm_locker()?;
        svm_locker.register_scenario(scenario, slot)?;
        Ok(RpcResponse {
            context: RpcResponseContext::new(svm_locker.get_latest_absolute_slot()),
            value: (),
        })
    }
}

#[cfg(test)]
mod tests {
    use solana_account_decoder::{
        UiAccountData, UiAccountEncoding, parse_account_data::ParsedAccount,
    };
    use solana_keypair::Keypair;
    use solana_program_pack::Pack;
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;
    use solana_system_interface::instruction::create_account;
    use solana_transaction::{Transaction, versioned::VersionedTransaction};
    use spl_associated_token_account_interface::{
        address::get_associated_token_address_with_program_id,
        instruction::create_associated_token_account,
    };
    use spl_token_2022_interface::instruction::{initialize_mint2, mint_to, transfer_checked};
    use spl_token_interface::state::Mint;
    use surfpool_types::{
        ExportSnapshotFilter, ExportSnapshotScope, RpcProfileDepth, UiAccountChange,
        UiAccountProfileState,
    };

    use super::*;
    use crate::{rpc::surfnet_cheatcodes::SurfnetCheatcodesRpc, tests::helpers::TestSetup};

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_transaction_profile() {
        // Create connection to local validator
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let recent_blockhash = client
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Generate a new keypair for the fee payer
        let payer = Keypair::new();

        let owner = Keypair::new();

        // Generate a second keypair for the token recipient
        let recipient = Keypair::new();

        // Airdrop 1 SOL to fee payer
        client
            .context
            .svm_locker
            .airdrop(&payer.pubkey(), 1_000_000_000)
            .unwrap()
            .unwrap();

        // Airdrop 1 SOL to recipient for rent exemption
        client
            .context
            .svm_locker
            .airdrop(&recipient.pubkey(), 1_000_000_000)
            .unwrap()
            .unwrap();

        // Generate keypair to use as address of mint
        let mint = Keypair::new();

        // Get default mint account size (in bytes), no extensions enabled
        let mint_space = Mint::LEN;
        let mint_rent = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .minimum_balance_for_rent_exemption(mint_space)
        });

        // Instruction to create new account for mint (token 2022 program)
        let create_account_instruction = create_account(
            &payer.pubkey(),                 // payer
            &mint.pubkey(),                  // new account (mint)
            mint_rent,                       // lamports
            mint_space as u64,               // space
            &spl_token_2022_interface::id(), // program id
        );

        // Instruction to initialize mint account data
        let initialize_mint_instruction = initialize_mint2(
            &spl_token_2022_interface::id(),
            &mint.pubkey(),        // mint
            &payer.pubkey(),       // mint authority
            Some(&payer.pubkey()), // freeze authority
            2,                     // decimals
        )
        .unwrap();

        // Calculate the associated token account address for fee_payer
        let source_ata = get_associated_token_address_with_program_id(
            &owner.pubkey(),                 // owner
            &mint.pubkey(),                  // mint
            &spl_token_2022_interface::id(), // program_id
        );

        // Instruction to create associated token account for fee_payer
        let create_source_ata_instruction = create_associated_token_account(
            &payer.pubkey(),                 // funding address
            &owner.pubkey(),                 // wallet address
            &mint.pubkey(),                  // mint address
            &spl_token_2022_interface::id(), // program id
        );

        // Calculate the associated token account address for recipient
        let destination_ata = get_associated_token_address_with_program_id(
            &recipient.pubkey(),             // owner
            &mint.pubkey(),                  // mint
            &spl_token_2022_interface::id(), // program_id
        );

        // Instruction to create associated token account for recipient
        let create_destination_ata_instruction = create_associated_token_account(
            &payer.pubkey(),                 // funding address
            &recipient.pubkey(),             // wallet address
            &mint.pubkey(),                  // mint address
            &spl_token_2022_interface::id(), // program id
        );

        // Amount of tokens to mint (100 tokens with 2 decimal places)
        let amount = 10_000;

        // Create mint_to instruction to mint tokens to the source token account
        let mint_to_instruction = mint_to(
            &spl_token_2022_interface::id(),
            &mint.pubkey(),     // mint
            &source_ata,        // destination
            &payer.pubkey(),    // authority
            &[&payer.pubkey()], // signer
            amount,             // amount
        )
        .unwrap();

        // Create transaction and add instructions
        let transaction = Transaction::new_signed_with_payer(
            &[
                create_account_instruction,
                initialize_mint_instruction,
                create_source_ata_instruction,
                create_destination_ata_instruction,
                mint_to_instruction,
            ],
            Some(&payer.pubkey()),
            &[&payer, &mint],
            recent_blockhash,
        );

        let signature = transaction.signatures[0];

        let (status_tx, _status_rx) = crossbeam_channel::unbounded();
        client
            .context
            .svm_locker
            .process_transaction(&None, transaction.into(), status_tx.clone(), false, true)
            .await
            .unwrap();

        // get profile and verify data
        {
            let ui_profile_result = client
                .rpc
                .get_transaction_profile(
                    Some(client.context.clone()),
                    UuidOrSignature::Signature(signature),
                    Some(RpcProfileResultConfig {
                        depth: Some(RpcProfileDepth::Instruction),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .value
                .expect("missing profile result for processed transaction");

            // instruction 1: create_account
            {
                let ix_profile = ui_profile_result
                    .instruction_profiles
                    .as_ref()
                    .unwrap()
                    .first()
                    .expect("instruction profile should exist");
                assert!(
                    ix_profile.error_message.is_none(),
                    "Profile should succeed, found error: {}",
                    ix_profile.error_message.as_ref().unwrap()
                );
                assert_eq!(ix_profile.compute_units_consumed, 150);
                assert!(ix_profile.error_message.is_none());
                let account_states = &ix_profile.account_states;

                let UiAccountProfileState::Writable(sender_account_change) = account_states
                    .get(&payer.pubkey())
                    .expect("Payer account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match sender_account_change {
                    UiAccountChange::Update(before, after) => {
                        assert_eq!(
                            after.lamports,
                            before.lamports - mint_rent - (2 * 5000), // two signers, so 2 * 5000 for fees
                            "Payer account should be original balance minus rent"
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }

                let UiAccountProfileState::Writable(mint_account_change) = account_states
                    .get(&mint.pubkey())
                    .expect("Mint account state should be present")
                else {
                    panic!("Expected mint account state to be Writable");
                };
                match mint_account_change {
                    UiAccountChange::Create(mint_account) => {
                        assert_eq!(
                            mint_account.lamports, mint_rent,
                            "Mint account should have the correct rent amount"
                        );
                        assert_eq!(
                            mint_account.owner,
                            spl_token_2022_interface::id().to_string(),
                            "Mint account should be owned by the SPL Token program"
                        );
                        // initialized account data should be empty bytes
                        assert_eq!(
                        mint_account.data,
                        UiAccountData::Binary(
                            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==".into(),
                            UiAccountEncoding::Base64
                        ),
                    );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }

            // instruction 2: initialize mint
            {
                let ix_profile = ui_profile_result
                    .instruction_profiles
                    .as_ref()
                    .unwrap()
                    .get(1)
                    .expect("instruction profile should exist");

                assert!(
                    ix_profile.error_message.is_none(),
                    "Profile should succeed, found error: {}",
                    ix_profile.error_message.as_ref().unwrap()
                );
                assert_eq!(ix_profile.compute_units_consumed, 1278);
                let account_states = &ix_profile.account_states;

                assert!(account_states.get(&payer.pubkey()).is_none());

                let UiAccountProfileState::Writable(mint_account_change) = account_states
                    .get(&mint.pubkey())
                    .expect("Mint account state should be present")
                else {
                    panic!("Expected mint account state to be Writable");
                };
                match mint_account_change {
                    UiAccountChange::Update(_before, after) => {
                        assert_eq!(
                            after.lamports, mint_rent,
                            "Mint account should have the correct rent amount"
                        );
                        assert_eq!(
                            after.owner,
                            spl_token_2022_interface::id().to_string(),
                            "Mint account should be owned by the SPL Token program"
                        );
                        // initialized account data should be empty bytes
                        assert_eq!(
                            after.data,
                            UiAccountData::Json(ParsedAccount {
                                program: "spl-token-2022".to_string(),
                                parsed: json!({
                                    "info": {
                                        "decimals": 2,
                                        "freezeAuthority": payer.pubkey().to_string(),
                                        "mintAuthority": payer.pubkey().to_string(),
                                        "isInitialized": true,
                                        "supply": "0",
                                    },
                                    "type": "mint"
                                }),
                                space: 82,
                            }),
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }

            // instruction 3: create token account
            {
                let ix_profile = ui_profile_result
                    .instruction_profiles
                    .as_ref()
                    .unwrap()
                    .get(2)
                    .expect("instruction profile should exist");
                assert!(
                    ix_profile.error_message.is_none(),
                    "Profile should succeed, found error: {}",
                    ix_profile.error_message.as_ref().unwrap()
                );

                let account_states = &ix_profile.account_states;

                let UiAccountProfileState::Writable(sender_account_change) = account_states
                    .get(&payer.pubkey())
                    .expect("Payer account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match sender_account_change {
                    UiAccountChange::Update(before, after) => {
                        assert_eq!(
                            after.lamports,
                            before.lamports - 2074080,
                            "Payer account should be original balance minus rent"
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }

                let UiAccountProfileState::Writable(mint_account_change) = account_states
                    .get(&mint.pubkey())
                    .expect("Mint account state should be present")
                else {
                    panic!("Expected mint account state to be Writable");
                };
                match mint_account_change {
                    UiAccountChange::Unchanged(mint_account) => {
                        assert!(mint_account.is_some())
                    }
                    other => {
                        panic!("Expected account state to be Unchanged, got: {:?}", other);
                    }
                }

                let UiAccountProfileState::Writable(source_ata_change) = account_states
                    .get(&source_ata)
                    .expect("account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match source_ata_change {
                    UiAccountChange::Create(new) => {
                        assert_eq!(
                            new.lamports, 2074080,
                            "Source ATA should have the correct lamports after creation"
                        );
                        assert_eq!(
                            new.owner,
                            spl_token_2022_interface::id().to_string(),
                            "Source ATA should be owned by the SPL Token program"
                        );

                        match &new.data {
                            UiAccountData::Json(parsed) => {
                                assert_eq!(
                                    parsed,
                                    &ParsedAccount {
                                        program: "spl-token-2022".into(),
                                        parsed: json!({
                                            "info": {
                                                "extensions": [
                                                    {
                                                        "extension": "immutableOwner"
                                                    }
                                                ],
                                                "isNative": false,
                                                "mint": mint.pubkey().to_string(),
                                                "owner": owner.pubkey().to_string(),
                                                "state": "initialized",
                                                "tokenAmount": {
                                                    "amount": "0",
                                                    "decimals": 2,
                                                    "uiAmount": 0.0,
                                                    "uiAmountString": "0"
                                                }
                                            },
                                            "type": "account"
                                        }),
                                        space: 170
                                    }
                                );
                            }
                            _ => panic!("Expected source ATA data to be JSON"),
                        }
                    }
                    other => {
                        panic!("Expected account state to be Create, got: {:?}", other);
                    }
                }
            }

            // instruction 4: create destination ATA
            {
                let ix_profile = ui_profile_result
                    .instruction_profiles
                    .as_ref()
                    .unwrap()
                    .get(3)
                    .expect("instruction profile should exist");
                assert!(
                    ix_profile.error_message.is_none(),
                    "Profile should succeed, found error: {}",
                    ix_profile.error_message.as_ref().unwrap()
                );

                let account_states = &ix_profile.account_states;

                let UiAccountProfileState::Writable(sender_account_change) = account_states
                    .get(&payer.pubkey())
                    .expect("Payer account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match sender_account_change {
                    UiAccountChange::Update(before, after) => {
                        assert_eq!(
                            after.lamports,
                            before.lamports - 2074080,
                            "Payer account should be original balance minus rent"
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }

                let UiAccountProfileState::Writable(mint_account_change) = account_states
                    .get(&mint.pubkey())
                    .expect("Mint account state should be present")
                else {
                    panic!("Expected mint account state to be Writable");
                };
                match mint_account_change {
                    UiAccountChange::Unchanged(mint_account) => {
                        assert!(mint_account.is_some())
                    }
                    other => {
                        panic!("Expected account state to be Unchanged, got: {:?}", other);
                    }
                }

                let UiAccountProfileState::Writable(destination_ata_change) = account_states
                    .get(&destination_ata)
                    .expect("account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match destination_ata_change {
                    UiAccountChange::Create(new) => {
                        assert_eq!(
                            new.lamports, 2074080,
                            "Source ATA should have the correct lamports after creation"
                        );
                        assert_eq!(
                            new.owner,
                            spl_token_2022_interface::id().to_string(),
                            "Source ATA should be owned by the SPL Token program"
                        );
                        match &new.data {
                            UiAccountData::Json(parsed) => {
                                assert_eq!(
                                    parsed,
                                    &ParsedAccount {
                                        program: "spl-token-2022".into(),
                                        parsed: json!({
                                            "info": {
                                                "extensions": [
                                                    {
                                                        "extension": "immutableOwner"
                                                    }
                                                ],
                                                "isNative": false,
                                                "mint": mint.pubkey().to_string(),
                                                "owner": recipient.pubkey().to_string(),
                                                "state": "initialized",
                                                "tokenAmount": {
                                                    "amount": "0",
                                                    "decimals": 2,
                                                    "uiAmount": 0.0,
                                                    "uiAmountString": "0"
                                                }
                                            },
                                            "type": "account"
                                        }),
                                        space: 170
                                    }
                                );
                            }
                            _ => panic!("Expected source ATA data to be JSON"),
                        }
                    }
                    other => {
                        panic!("Expected account state to be Create, got: {:?}", other);
                    }
                }
            }

            // instruction 5: mint to
            {
                let ix_profile = ui_profile_result
                    .instruction_profiles
                    .as_ref()
                    .unwrap()
                    .get(4)
                    .expect("instruction profile should exist");
                assert!(
                    ix_profile.error_message.is_none(),
                    "Profile should succeed, found error: {}",
                    ix_profile.error_message.as_ref().unwrap()
                );

                let account_states = &ix_profile.account_states;

                let UiAccountProfileState::Writable(sender_account_change) = account_states
                    .get(&payer.pubkey())
                    .expect("Payer account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match sender_account_change {
                    UiAccountChange::Unchanged(unchanged) => {
                        assert!(unchanged.is_some(), "Payer account should remain unchanged");
                    }
                    other => {
                        panic!("Expected account state to be Unchanged, got: {:?}", other);
                    }
                }

                let UiAccountProfileState::Writable(mint_account_change) = account_states
                    .get(&mint.pubkey())
                    .expect("Mint account state should be present")
                else {
                    panic!("Expected mint account state to be Writable");
                };
                match mint_account_change {
                    UiAccountChange::Update(before, after) => {
                        assert_eq!(
                            after.lamports, before.lamports,
                            "Lamports should stay the same for mint account"
                        );
                        assert_eq!(
                            after.data,
                            UiAccountData::Json(ParsedAccount {
                                program: "spl-token-2022".into(),
                                parsed: json!({
                                    "info": {
                                        "decimals": 2,
                                        "freezeAuthority": payer.pubkey().to_string(),
                                        "isInitialized": true,
                                        "mintAuthority": payer.pubkey().to_string(),
                                        "supply": "10000",
                                    },
                                    "type": "mint"
                                }),
                                space: 82
                            }),
                            "Data should stay the same for mint account"
                        );
                    }
                    other => {
                        panic!("Expected account state to be Update, got: {:?}", other);
                    }
                }
            }

            assert_eq!(
                ui_profile_result.transaction_profile.compute_units_consumed,
                ui_profile_result
                    .instruction_profiles
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|ix| ix.compute_units_consumed)
                    .sum::<u64>(),
            )
        }
        // Get the latest blockhash for the transfer transaction
        let recent_blockhash = client
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Amount of tokens to transfer (0.50 tokens with 2 decimals)
        let transfer_amount = 50;

        // Create transfer_checked instruction to send tokens from source to destination
        let transfer_instruction = transfer_checked(
            &spl_token_2022_interface::id(),     // program id
            &source_ata,                         // source
            &mint.pubkey(),                      // mint
            &destination_ata,                    // destination
            &owner.pubkey(),                     // owner of source
            &[&payer.pubkey(), &owner.pubkey()], // signers
            transfer_amount,                     // amount
            2,                                   // decimals
        )
        .unwrap();

        // Create transaction for transferring tokens
        let transaction = Transaction::new_signed_with_payer(
            &[transfer_instruction],
            Some(&payer.pubkey()),
            &[&payer, &owner],
            recent_blockhash,
        );
        let signature = transaction.signatures[0];
        let (status_tx, _status_rx) = crossbeam_channel::unbounded();
        // Send and confirm transaction
        client
            .context
            .svm_locker
            .process_transaction(&None, transaction.clone().into(), status_tx, true, true)
            .await
            .unwrap();

        {
            let profile_result = client
                .rpc
                .get_transaction_profile(
                    Some(client.context.clone()),
                    UuidOrSignature::Signature(signature),
                    Some(RpcProfileResultConfig {
                        depth: Some(RpcProfileDepth::Instruction),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .value
                .expect("missing profile result for processed transaction");

            assert!(
                profile_result.transaction_profile.error_message.is_none(),
                "Transaction should succeed, found error: {}",
                profile_result
                    .transaction_profile
                    .error_message
                    .as_ref()
                    .unwrap()
            );

            assert_eq!(
                profile_result.instruction_profiles.as_ref().unwrap().len(),
                1
            );

            let ix_profile = profile_result
                .instruction_profiles
                .as_ref()
                .unwrap()
                .first()
                .expect("instruction profile should exist");
            assert!(
                ix_profile.error_message.is_none(),
                "Profile should succeed, found error: {}",
                ix_profile.error_message.as_ref().unwrap()
            );

            let mut account_states = ix_profile.account_states.clone();

            let UiAccountProfileState::Writable(owner_account_change) = account_states
                .swap_remove(&owner.pubkey())
                .expect("account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            let UiAccountChange::Unchanged(unchanged) = owner_account_change else {
                panic!(
                    "Expected account state to be Unchanged, got: {:?}",
                    owner_account_change
                );
            };
            assert!(unchanged.is_none(), "Owner account shouldn't exist");

            let UiAccountProfileState::Writable(sender_account_change) = account_states
                .swap_remove(&payer.pubkey())
                .expect("Payer account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            match sender_account_change {
                UiAccountChange::Update(before, after) => {
                    assert_eq!(after.lamports, before.lamports - 10000);
                }
                other => {
                    panic!("Expected account state to be an Update, got: {:?}", other);
                }
            }

            let UiAccountProfileState::Readonly = account_states
                .swap_remove(&mint.pubkey())
                .expect("Mint account state should be present")
            else {
                panic!("Expected mint account state to be Readonly");
            };
            let UiAccountProfileState::Readonly = account_states
                .swap_remove(&spl_token_2022_interface::ID)
                .expect("account state should be present")
            else {
                panic!("Expected account state to be Readonly");
            };

            let UiAccountProfileState::Writable(source_ata_change) = account_states
                .swap_remove(&source_ata)
                .expect("account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            match source_ata_change {
                UiAccountChange::Update(before, after) => {
                    assert_eq!(
                        after.lamports, before.lamports,
                        "Source ATA lamports should remain unchanged"
                    );
                    assert_eq!(
                        after.data,
                        UiAccountData::Json(ParsedAccount {
                            program: "spl-token-2022".into(),
                            parsed: json!({
                                "info": {
                                    "extensions": [
                                        {
                                            "extension": "immutableOwner"
                                        }
                                    ],
                                    "isNative": false,
                                    "mint": mint.pubkey().to_string(),
                                    "owner": owner.pubkey().to_string(),
                                    "state": "initialized",
                                    "tokenAmount": {
                                        "amount": "9950",
                                        "decimals": 2,
                                        "uiAmount": 99.5,
                                        "uiAmountString": "99.5"
                                    }
                                },
                                "type": "account"
                            }),
                            space: 170
                        }),
                        "Source ATA data should be updated after transfer"
                    );
                }
                other => {
                    panic!("Expected account state to be Update, got: {:?}", other);
                }
            }

            let UiAccountProfileState::Writable(destination_ata_change) = account_states
                .swap_remove(&destination_ata)
                .expect("account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            match destination_ata_change {
                UiAccountChange::Update(before, after) => {
                    assert_eq!(
                        after.lamports, before.lamports,
                        "Destination ATA lamports should remain unchanged"
                    );
                    assert_eq!(
                        after.data,
                        UiAccountData::Json(ParsedAccount {
                            program: "spl-token-2022".into(),
                            parsed: json!({
                                "info": {
                                    "extensions": [
                                        {
                                            "extension": "immutableOwner"
                                        }
                                    ],
                                    "isNative": false,
                                    "mint": mint.pubkey().to_string(),
                                    "owner": recipient.pubkey().to_string(),
                                    "state": "initialized",
                                    "tokenAmount": {
                                        "amount": transfer_amount.to_string(),
                                        "decimals": 2,
                                        "uiAmount": 0.5,
                                        "uiAmountString": "0.5"
                                    }
                                },
                                "type": "account"
                            }),
                            space: 170
                        }),
                        "Destination ATA data should be updated after transfer"
                    );
                }
                other => {
                    panic!("Expected account state to be Update, got: {:?}", other);
                }
            }

            assert!(
                account_states.is_empty(),
                "All account states should have been processed, found: {:?}",
                account_states
            );
        }
    }

    fn set_account(client: &TestSetup<SurfnetCheatcodesRpc>, pubkey: &Pubkey, account: &Account) {
        client
            .context
            .svm_locker
            .with_svm_writer(|svm| svm.inner.set_account(*pubkey, account.clone()))
            .expect("Failed to set account");
    }

    fn verify_snapshot_account(
        snapshot: &BTreeMap<String, AccountSnapshot>,
        expected_account_pubkey: &Pubkey,
        expected_account: &Account,
    ) {
        let account = snapshot
            .get(&expected_account_pubkey.to_string())
            .unwrap_or_else(|| {
                panic!(
                    "Account fixture not found for pubkey {}",
                    expected_account_pubkey
                )
            });
        assert_eq!(expected_account.lamports, account.lamports);
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(&expected_account.data),
            account.data
        );
        assert_eq!(expected_account.owner.to_string(), account.owner);
        assert_eq!(expected_account.executable, account.executable);
        assert_eq!(expected_account.rent_epoch, account.rent_epoch);
    }

    #[test]
    fn test_export_snapshot() {
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let pubkey1 = Pubkey::new_unique();
        let account1 = Account {
            lamports: 1_000_000,
            data: vec![1, 2, 3, 4],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        };

        set_account(&client, &pubkey1, &account1);

        let pubkey2 = Pubkey::new_unique();
        let account2 = Account {
            lamports: 2_000_000,
            data: vec![5, 6, 7, 8, 9],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        };

        set_account(&client, &pubkey2, &account2);

        let snapshot = client
            .rpc
            .export_snapshot(Some(client.context.clone()), None)
            .expect("Failed to export snapshot")
            .value;

        verify_snapshot_account(&snapshot, &pubkey1, &account1);
        verify_snapshot_account(&snapshot, &pubkey2, &account2);
    }

    #[test]
    fn test_export_snapshot_json_parsed() {
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let pubkey1 = Pubkey::new_unique();
        println!("Pubkey1: {}", pubkey1);
        let account1 = Account {
            lamports: 1_000_000,
            data: vec![1, 2, 3, 4],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        };

        set_account(&client, &pubkey1, &account1);

        let mint_pubkey = Pubkey::new_unique();
        println!("Mint Pubkey: {}", mint_pubkey);
        let mint_authority = Pubkey::new_unique();

        let mut mint_data = [0u8; Mint::LEN];
        let mint = Mint {
            mint_authority: COption::Some(mint_authority),
            supply: 1000,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        mint.pack_into_slice(&mut mint_data);

        let mint_account = Account {
            lamports: 1_000_000,
            data: mint_data.to_vec(),
            owner: spl_token_interface::id(),
            executable: false,
            rent_epoch: 0,
        };

        set_account(&client, &mint_pubkey, &mint_account);

        let snapshot = client
            .rpc
            .export_snapshot(
                Some(client.context.clone()),
                Some(ExportSnapshotConfig {
                    include_parsed_accounts: Some(true),
                    filter: None,
                    scope: ExportSnapshotScope::Network,
                }),
            )
            .expect("Failed to export snapshot")
            .value;

        verify_snapshot_account(&snapshot, &pubkey1, &account1);
        let actual_account1 = snapshot
            .get(&pubkey1.to_string())
            .expect("Account fixture not found");
        assert!(
            actual_account1.parsed_data.is_none(),
            "Account1 should not have parsed data"
        );

        verify_snapshot_account(&snapshot, &mint_pubkey, &mint_account);
        let mint_snapshot = snapshot
            .get(&mint_pubkey.to_string())
            .expect("Mint account snapshot not found");
        let parsed = mint_snapshot
            .parsed_data
            .as_ref()
            .expect("Parsed data should be present");

        assert_eq!(parsed.program, "spl-token");
        assert_eq!(parsed.space, Mint::LEN as u64);

        let parsed_info = parsed
            .parsed
            .as_object()
            .expect("Parsed data should be an object");
        let info = parsed_info
            .get("info")
            .expect("Parsed data should have info field")
            .as_object()
            .expect("Info field should be an object");
        assert_eq!(
            info.get("mintAuthority")
                .and_then(|v| v.as_str())
                .expect("mintAuthority should be a string"),
            mint_authority.to_string()
        );
    }

    #[test]
    fn test_export_snapshot_pre_transaction() {
        use std::collections::HashMap;

        use solana_signature::Signature;
        use surfpool_types::{ProfileResult, types::KeyedProfileResult};

        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        // Create several accounts in the network
        let account1_pubkey = Pubkey::new_unique();
        let account1 = Account {
            lamports: 1_000_000,
            data: vec![1, 2, 3, 4],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        };
        set_account(&client, &account1_pubkey, &account1);

        let account2_pubkey = Pubkey::new_unique();
        let account2 = Account {
            lamports: 2_000_000,
            data: vec![5, 6, 7, 8],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        };
        set_account(&client, &account2_pubkey, &account2);

        let account3_pubkey = Pubkey::new_unique();
        let account3 = Account {
            lamports: 3_000_000,
            data: vec![9, 10, 11, 12],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        };
        set_account(&client, &account3_pubkey, &account3);

        // Create a mock transaction profile that only touches account1 and account2
        let signature = Signature::new_unique();
        let mut pre_execution_capture = BTreeMap::new();
        pre_execution_capture.insert(account1_pubkey, Some(account1.clone()));
        pre_execution_capture.insert(account2_pubkey, Some(account2.clone()));

        let mut post_execution_capture = BTreeMap::new();
        let mut modified_account1 = account1.clone();
        modified_account1.lamports = 500_000;
        post_execution_capture.insert(account1_pubkey, Some(modified_account1.clone()));
        let mut modified_account2 = account2.clone();
        modified_account2.lamports = 2_500_000;
        post_execution_capture.insert(account2_pubkey, Some(modified_account2.clone()));

        let profile = ProfileResult {
            pre_execution_capture,
            post_execution_capture,
            compute_units_consumed: 1000,
            log_messages: None,
            error_message: None,
        };

        let keyed_profile = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(signature),
            None,
            profile,
            HashMap::new(),
        );

        // Insert the profile into executed_transaction_profiles
        client.context.svm_locker.with_svm_writer(|svm| {
            svm.executed_transaction_profiles
                .store(signature.to_string(), keyed_profile)
                .unwrap();
        });

        // Export snapshot with PreTransaction scope
        let snapshot = client
            .rpc
            .export_snapshot(
                Some(client.context.clone()),
                Some(ExportSnapshotConfig {
                    include_parsed_accounts: Some(false),
                    filter: None,
                    scope: ExportSnapshotScope::PreTransaction(signature.to_string()),
                }),
            )
            .expect("Failed to export snapshot")
            .value;

        // Verify that only account1 and account2 are in the snapshot
        assert!(
            snapshot.contains_key(&account1_pubkey.to_string()),
            "Snapshot should contain account1 (touched by transaction)"
        );
        assert!(
            snapshot.contains_key(&account2_pubkey.to_string()),
            "Snapshot should contain account2 (touched by transaction)"
        );
        assert!(
            !snapshot.contains_key(&account3_pubkey.to_string()),
            "Snapshot should NOT contain account3 (not touched by transaction)"
        );

        // Verify the accounts have the PRE-EXECUTION state (original values, not modified)
        verify_snapshot_account(&snapshot, &account1_pubkey, &account1);
        verify_snapshot_account(&snapshot, &account2_pubkey, &account2);

        // Double-check that we're NOT getting the post-execution values
        let snapshot_account1 = snapshot
            .get(&account1_pubkey.to_string())
            .expect("Account1 should be in snapshot");
        assert_eq!(
            snapshot_account1.lamports, 1_000_000,
            "Account1 should have pre-execution lamports (1M), not post-execution (500K)"
        );

        let snapshot_account2 = snapshot
            .get(&account2_pubkey.to_string())
            .expect("Account2 should be in snapshot");
        assert_eq!(
            snapshot_account2.lamports, 2_000_000,
            "Account2 should have pre-execution lamports (2M), not post-execution (2.5M)"
        );

        // Verify account count
        // Note: The snapshot may contain more accounts if system accounts are included
        // but we verify that at least our touched accounts are there and untouched ones are not
        println!(
            "Snapshot contains {} accounts (expected at least 2)",
            snapshot.len()
        );
    }

    #[test]
    fn test_export_snapshot_filtering() {
        let system_account_pubkey = Pubkey::new_unique();
        println!("System Account Pubkey: {}", system_account_pubkey);
        let excluded_system_account_pubkey = Pubkey::new_unique();
        println!(
            "Excluded System Account Pubkey: {}",
            excluded_system_account_pubkey
        );
        let program_account_pubkey = Pubkey::new_unique();
        println!("Program Account Pubkey: {}", program_account_pubkey);
        let included_program_account_pubkey = Pubkey::new_unique();
        println!(
            "Included Program Account Pubkey: {}",
            included_program_account_pubkey
        );

        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let system_account = Account {
            lamports: 1_000_000,
            data: vec![1, 2, 3, 4],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        };
        set_account(&client, &system_account_pubkey, &system_account);
        set_account(&client, &excluded_system_account_pubkey, &system_account);

        let program_account = Account {
            lamports: 2_000_000,
            data: vec![5, 6, 7, 8, 9],
            owner: solana_sdk_ids::bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        };
        set_account(&client, &program_account_pubkey, &program_account);
        set_account(&client, &included_program_account_pubkey, &program_account);

        let snapshot = client
            .rpc
            .export_snapshot(Some(client.context.clone()), None)
            .expect("Failed to export snapshot")
            .value;
        assert!(
            !snapshot.contains_key(&program_account_pubkey.to_string()),
            "Program account should be excluded by default"
        );
        assert!(
            !snapshot.contains_key(&included_program_account_pubkey.to_string()),
            "Program account should be excluded by default"
        );
        let snapshot = client
            .rpc
            .export_snapshot(
                Some(client.context.clone()),
                Some(ExportSnapshotConfig {
                    filter: Some(ExportSnapshotFilter {
                        include_accounts: Some(vec![included_program_account_pubkey.to_string()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            )
            .expect("Failed to export snapshot")
            .value;
        assert!(
            !snapshot.contains_key(&program_account_pubkey.to_string()),
            "Program account should be excluded by default"
        );
        assert!(
            snapshot.contains_key(&included_program_account_pubkey.to_string()),
            "Program account should be included when explicitly listed"
        );

        let snapshot = client
            .rpc
            .export_snapshot(
                Some(client.context.clone()),
                Some(ExportSnapshotConfig {
                    filter: Some(ExportSnapshotFilter {
                        include_program_accounts: Some(true),
                        exclude_accounts: Some(vec![excluded_system_account_pubkey.to_string()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            )
            .expect("Failed to export snapshot")
            .value;

        assert!(
            snapshot.contains_key(&program_account_pubkey.to_string()),
            "Program account should be included when filter is set"
        );
        assert!(
            snapshot.contains_key(&included_program_account_pubkey.to_string()),
            "Included program account should be present"
        );
        assert!(
            snapshot.contains_key(&system_account_pubkey.to_string()),
            "System account should be present"
        );
        assert!(
            !snapshot.contains_key(&excluded_system_account_pubkey.to_string()),
            "Excluded system account should not be present"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_creates_accounts_automatically() {
        // Test that both program and program data accounts are created if they don't exist
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        // Verify accounts don't exist initially
        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());

        let program_account_before = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader.inner.get_account(&program_id.pubkey()).unwrap()
        });
        assert!(
            program_account_before.is_none(),
            "Program account should not exist initially"
        );

        // Write some data
        let program_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&program_data),
                0,
                None,
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to write program: {:?}",
            result.err()
        );

        // Verify program account was created
        let program_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader.inner.get_account(&program_id.pubkey()).unwrap()
        });
        assert!(
            program_account.is_some(),
            "Program account should be created"
        );

        let program_account = program_account.unwrap();
        assert_eq!(
            program_account.owner,
            solana_sdk_ids::bpf_loader_upgradeable::id()
        );
        assert!(
            program_account.executable,
            "Program account should be executable"
        );

        // Verify program data account was created
        let program_data_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader.inner.get_account(&program_data_address).unwrap()
        });
        assert!(
            program_data_account.is_some(),
            "Program data account should be created"
        );

        let program_data_account = program_data_account.unwrap();
        assert_eq!(
            program_data_account.owner,
            solana_sdk_ids::bpf_loader_upgradeable::id()
        );
        assert!(
            !program_data_account.executable,
            "Program data account should not be executable"
        );

        println!("✅ Both accounts created successfully");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_single_chunk_small() {
        // Test writing a small program in a single write
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let program_data = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        let data_hex = hex::encode(&program_data);

        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                data_hex,
                0,
                None,
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to write program: {:?}",
            result.err()
        );

        // Verify the data was written correctly
        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );
        let written_data = &account.data[metadata_size..metadata_size + program_data.len()];
        assert_eq!(
            written_data, &program_data,
            "Written data should match input"
        );

        println!("✅ Small program written correctly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_single_chunk_large() {
        // Test writing a large program (1MB) in a single write
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let program_data = vec![0xAB; 1024 * 1024]; // 1 MB
        let data_hex = hex::encode(&program_data);

        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                data_hex,
                0,
                None,
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to write large program: {:?}",
            result.err()
        );

        // Verify the data was written
        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );
        assert_eq!(
            account.data.len(),
            metadata_size + program_data.len(),
            "Account should have correct size"
        );

        println!("✅ Large program (1MB) written successfully");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_multiple_sequential_chunks() {
        // Test writing a program in multiple sequential chunks
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let chunks = vec![
            (vec![0x01; 1024], 0),    // First 1KB at offset 0
            (vec![0x02; 1024], 1024), // Second 1KB at offset 1024
            (vec![0x03; 1024], 2048), // Third 1KB at offset 2048
            (vec![0x04; 512], 3072),  // Last 512 bytes at offset 3072
        ];

        for (i, (chunk_data, offset)) in chunks.iter().enumerate() {
            let result = client
                .rpc
                .write_program(
                    Some(client.context.clone()),
                    program_id.pubkey().to_string(),
                    hex::encode(chunk_data),
                    *offset,
                    None,
                )
                .await;

            assert!(
                result.is_ok(),
                "Failed to write chunk {} at offset {}: {:?}",
                i,
                offset,
                result.err()
            );
        }

        // Verify all chunks were written correctly
        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );

        // Verify each chunk
        for (chunk_data, offset) in chunks {
            let start = metadata_size + offset as usize;
            let end = start + chunk_data.len();
            let written = &account.data[start..end];
            assert_eq!(
                written,
                &chunk_data[..],
                "Chunk at offset {} should match",
                offset
            );
        }

        println!("✅ Multiple sequential chunks written correctly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_non_sequential_chunks() {
        // Test writing chunks out of order (backwards)
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let chunks = vec![
            (vec![0x03; 512], 2048),  // Write third chunk first
            (vec![0x01; 1024], 0),    // Write first chunk second
            (vec![0x02; 1024], 1024), // Write second chunk last
        ];

        for (chunk_data, offset) in chunks.iter() {
            let result = client
                .rpc
                .write_program(
                    Some(client.context.clone()),
                    program_id.pubkey().to_string(),
                    hex::encode(chunk_data),
                    *offset,
                    None,
                )
                .await;

            assert!(
                result.is_ok(),
                "Failed at offset {}: {:?}",
                offset,
                result.err()
            );
        }

        // Verify data integrity
        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );

        // Check first chunk
        assert_eq!(
            &account.data[metadata_size..metadata_size + 1024],
            &vec![0x01; 1024][..]
        );
        // Check second chunk
        assert_eq!(
            &account.data[metadata_size + 1024..metadata_size + 2048],
            &vec![0x02; 1024][..]
        );
        // Check third chunk
        assert_eq!(
            &account.data[metadata_size + 2048..metadata_size + 2560],
            &vec![0x03; 512][..]
        );

        println!("✅ Non-sequential chunks written correctly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_overlapping_writes() {
        // Test that overlapping writes correctly overwrite previous data
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        // Write initial data
        let initial_data = vec![0xFF; 1024];
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&initial_data),
                0,
                None,
            )
            .await
            .unwrap();

        // Overwrite middle section
        let overwrite_data = vec![0xAA; 512];
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&overwrite_data),
                256, // Start in the middle
                None,
            )
            .await
            .unwrap();

        // Verify the result
        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );

        // First 256 bytes should be 0xFF
        assert_eq!(
            &account.data[metadata_size..metadata_size + 256],
            &vec![0xFF; 256][..]
        );
        // Middle 512 bytes should be 0xAA
        assert_eq!(
            &account.data[metadata_size + 256..metadata_size + 768],
            &vec![0xAA; 512][..]
        );
        // Last 256 bytes should be 0xFF
        assert_eq!(
            &account.data[metadata_size + 768..metadata_size + 1024],
            &vec![0xFF; 256][..]
        );

        println!("✅ Overlapping writes handled correctly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_zero_offset() {
        // Test writing at offset 0 (should write immediately after metadata)
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let data = vec![0x42; 128];
        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&data),
                0,
                None,
            )
            .await;

        assert!(result.is_ok());

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );
        assert_eq!(&account.data[metadata_size..metadata_size + 128], &data[..]);

        println!("✅ Write at offset 0 works correctly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_large_offset() {
        // Test writing at a large offset (account should expand)
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let large_offset = 1024 * 1024; // 1 MB offset
        let data = vec![0x99; 256];

        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&data),
                large_offset,
                None,
            )
            .await;

        assert!(result.is_ok(), "Should handle large offset");

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );
        let expected_size = metadata_size + large_offset as usize + data.len();
        assert_eq!(
            account.data.len(),
            expected_size,
            "Account should expand to fit data"
        );

        // Verify data at the large offset
        let start = metadata_size + large_offset as usize;
        assert_eq!(&account.data[start..start + 256], &data[..]);

        println!("✅ Large offset handled with account expansion");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_empty_data() {
        // Test writing empty data (should succeed but not write anything)
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&[]),
                0,
                None,
            )
            .await;

        assert!(result.is_ok(), "Should handle empty data");

        println!("✅ Empty data handled correctly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_single_byte() {
        // Test writing a single byte
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let data = vec![0x42];
        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&data),
                0,
                None,
            )
            .await;

        assert!(result.is_ok());

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );
        assert_eq!(account.data[metadata_size], 0x42);

        println!("✅ Single byte write works");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_invalid_program_id() {
        // Test with invalid program ID
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                "not_a_valid_pubkey".to_string(),
                "deadbeef".to_string(),
                0,
                None,
            )
            .await;

        assert!(result.is_err(), "Should fail with invalid program ID");
        assert!(
            result.unwrap_err().to_string().contains("Invalid pubkey"),
            "Error should mention invalid pubkey"
        );

        println!("✅ Invalid program ID rejected");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_invalid_hex_data() {
        // Test with invalid hex encoding
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let invalid_hex_strings = vec![
            "not_hex_at_all",
            "GHIJKLMN",
            "0x123", // odd length
            "12 34", // contains space
        ];

        for invalid_hex in invalid_hex_strings {
            let result = client
                .rpc
                .write_program(
                    Some(client.context.clone()),
                    program_id.pubkey().to_string(),
                    invalid_hex.to_string(),
                    0,
                    None,
                )
                .await;

            assert!(
                result.is_err(),
                "Should fail with invalid hex: {}",
                invalid_hex
            );
            assert!(
                result.unwrap_err().to_string().contains("Invalid hex"),
                "Error should mention invalid hex"
            );
        }

        println!("✅ Invalid hex data rejected");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_rent_exemption() {
        // Test that rent exemption is maintained when account expands
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        // Write initial small data
        let small_data = vec![0x01; 128];
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&small_data),
                0,
                None,
            )
            .await
            .unwrap();

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());

        let initial_lamports = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
                .lamports
        });

        // Write at large offset to expand account
        let large_data = vec![0x02; 1024];
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&large_data),
                10240, // Large offset
                None,
            )
            .await
            .unwrap();

        let final_lamports = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
                .lamports
        });

        assert!(
            final_lamports > initial_lamports,
            "Lamports should increase to maintain rent exemption"
        );

        // Verify rent exemption
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let required_lamports = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .minimum_balance_for_rent_exemption(account.data.len())
        });

        assert_eq!(
            account.lamports, required_lamports,
            "Account should have exact rent-exempt lamports"
        );

        println!("✅ Rent exemption maintained during expansion");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_account_ownership() {
        // Test that created accounts have correct ownership
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();
        let authority = Keypair::new();

        let data = vec![0xAB; 64];
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&data),
                0,
                Some(authority.pubkey().to_string()),
            )
            .await
            .unwrap();

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());

        // Check program account ownership
        let program_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_id.pubkey())
                .unwrap()
                .unwrap()
        });
        assert_eq!(
            program_account.owner,
            solana_sdk_ids::bpf_loader_upgradeable::id(),
            "Program account should be owned by upgradeable loader"
        );
        assert!(
            program_account.executable,
            "Program account should be executable"
        );

        // Check program data account ownership
        let program_data_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });
        assert_eq!(
            program_data_account.owner,
            solana_sdk_ids::bpf_loader_upgradeable::id(),
            "Program data account should be owned by upgradeable loader"
        );
        assert!(
            !program_data_account.executable,
            "Program data account should not be executable"
        );

        // Check program authority

        let Ok(solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
            slot: _,
            upgrade_authority_address,
        }) = bincode::deserialize::<solana_loader_v3_interface::state::UpgradeableLoaderState>(
            &program_data_account.data,
        )
        else {
            panic!("Program data account has incorrect state");
        };

        assert_eq!(
            upgrade_authority_address,
            Some(authority.pubkey()),
            "Upgrade authority should match"
        );

        println!("✅ Account ownership is correct");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_metadata_preservation() {
        // Test that program data account metadata is preserved across writes
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        // First write
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&vec![0x01; 128]),
                0,
                None,
            )
            .await
            .unwrap();

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());

        // Get initial metadata
        let initial_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );
        let initial_metadata = initial_account.data[..metadata_size].to_vec();

        // Second write (should preserve metadata)
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&vec![0x02; 256]),
                128,
                None,
            )
            .await
            .unwrap();

        // Verify metadata is preserved
        let final_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let final_metadata = final_account.data[..metadata_size].to_vec();
        assert_eq!(
            initial_metadata, final_metadata,
            "Metadata should be preserved"
        );

        println!("✅ Metadata preserved across writes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_idempotent() {
        // Test that writing the same data twice produces the same result
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let data = vec![0x55; 512];
        let offset = 100;

        // First write
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&data),
                offset,
                None,
            )
            .await
            .unwrap();

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());

        let first_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        // Second write (same data, same offset)
        client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&data),
                offset,
                None,
            )
            .await
            .unwrap();

        let second_account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        assert_eq!(
            first_account.data, second_account.data,
            "Data should be identical"
        );
        assert_eq!(
            first_account.lamports, second_account.lamports,
            "Lamports should be identical"
        );

        println!("✅ Writes are idempotent");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_context_slot() {
        // Test that response context contains valid slot
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&vec![0x42; 64]),
                0,
                None,
            )
            .await
            .unwrap();

        assert!(result.context.slot > 0, "Context slot should be valid");

        println!("✅ Response context is valid");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_small_no_minimum_program_artifacts() {
        // Regression test: writing a program smaller than minimum_program.so (3312 bytes)
        // should not leave leftover minimum_program.so bytes in the account.
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        let program_data = vec![0xAB; 100];
        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&program_data),
                0,
                None,
            )
            .await;

        assert!(
            result.is_ok(),
            "Failed to write program: {:?}",
            result.err()
        );

        let program_data_address =
            solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
        let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .get_account(&program_data_address)
                .unwrap()
                .unwrap()
        });

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );

        // Account data length should be exactly metadata + program data, not metadata + 3312
        assert_eq!(
            account.data.len(),
            metadata_size + 100,
            "Account data length should be metadata_size ({}) + 100 = {}, but was {}",
            metadata_size,
            metadata_size + 100,
            account.data.len()
        );

        // Verify written content matches exactly
        let written_data = &account.data[metadata_size..];
        assert_eq!(
            written_data, &program_data,
            "Written data should match exactly"
        );

        // Verify no trailing non-zero bytes beyond written data
        assert!(
            account.data[metadata_size + 100..].is_empty(),
            "There should be no trailing bytes beyond the written data"
        );

        println!("✅ Small program has no minimum_program.so artifacts");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_exact_account_size() {
        // Regression test: verify account size is exactly correct for various data sizes,
        // including sizes around the minimum_program.so boundary (3312 bytes).
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );

        for data_len in [1usize, 100, 3311, 3312, 3313, 5000] {
            let program_id = Keypair::new();
            let program_data: Vec<u8> = (0..data_len).map(|i| (i % 256) as u8).collect();

            let result = client
                .rpc
                .write_program(
                    Some(client.context.clone()),
                    program_id.pubkey().to_string(),
                    hex::encode(&program_data),
                    0,
                    None,
                )
                .await;

            assert!(
                result.is_ok(),
                "Failed to write program of size {}: {:?}",
                data_len,
                result.err()
            );

            let program_data_address =
                solana_loader_v3_interface::get_program_data_address(&program_id.pubkey());
            let account = client.context.svm_locker.with_svm_reader(|svm_reader| {
                svm_reader
                    .inner
                    .get_account(&program_data_address)
                    .unwrap()
                    .unwrap()
            });

            assert_eq!(
                account.data.len(),
                metadata_size + data_len,
                "For data_len={}, account data length should be {} but was {}",
                data_len,
                metadata_size + data_len,
                account.data.len()
            );

            let written_data = &account.data[metadata_size..metadata_size + data_len];
            assert_eq!(
                written_data, &program_data,
                "For data_len={}, written content should match exactly",
                data_len
            );
        }

        println!("✅ All program sizes produce exact account sizes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_write_program_execution_uses_written_bytes_not_noop() {
        // Regression test: after write_program, executing the program should use
        // the written bytes, not the noop placeholder from init_programdata_account.
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());
        let program_id = Keypair::new();

        // Create an "error program" ELF: identical to noop but returns r0=1 (error)
        // instead of r0=0 (success). This lets us distinguish noop vs real execution.
        let mut error_program_elf = crate::surfnet::noop_program::NOOP_PROGRAM_ELF.to_vec();
        // Byte 124 is the first byte of the imm field in `mov64 r0, 0` at .text offset.
        // Changing it to 1 makes the instruction `mov64 r0, 1` (program returns error).
        error_program_elf[124] = 0x01;

        // Write the error program
        let result = client
            .rpc
            .write_program(
                Some(client.context.clone()),
                program_id.pubkey().to_string(),
                hex::encode(&error_program_elf),
                0,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "Failed to write program: {:?}",
            result.err()
        );

        // Create a payer and fund it
        let payer = Keypair::new();
        client
            .context
            .svm_locker
            .airdrop(&payer.pubkey(), 1_000_000_000)
            .unwrap()
            .unwrap();

        // Build a transaction invoking the written program
        let recent_blockhash = client
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());
        // Build a minimal instruction that invokes the written program
        let invoke_ix = solana_instruction::Instruction {
            program_id: program_id.pubkey(),
            accounts: vec![],
            data: vec![],
        };
        let message = solana_message::Message::new_with_blockhash(
            &[invoke_ix],
            Some(&payer.pubkey()),
            &recent_blockhash,
        );
        let tx = VersionedTransaction::try_new(
            solana_message::VersionedMessage::Legacy(message),
            &[&payer],
        )
        .unwrap();

        // Simulate the transaction
        let sim_result = client.context.svm_locker.simulate_transaction(tx, false);

        // The error program returns r0=1, which should cause an InstructionError.
        // If the noop (r0=0) is still cached, this would incorrectly succeed.
        assert!(
            sim_result.is_err(),
            "Transaction should fail because the written program returns error (r0=1). \
             If it succeeded, the noop placeholder is still being executed instead of \
             the written program bytes."
        );
    }

    #[test]
    fn test_stream_accounts_registers_multiple() {
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();
        let pubkey3 = Pubkey::new_unique();

        let entries = vec![
            StreamAccountsEntry {
                pubkey: pubkey1.to_string(),
                include_owned_accounts: Some(true),
            },
            StreamAccountsEntry {
                pubkey: pubkey2.to_string(),
                include_owned_accounts: Some(false),
            },
            StreamAccountsEntry {
                pubkey: pubkey3.to_string(),
                include_owned_accounts: None,
            },
        ];

        let result = client
            .rpc
            .stream_accounts(Some(client.context.clone()), entries);
        assert!(result.is_ok(), "stream_accounts should succeed");

        // Verify all accounts are registered via get_streamed_accounts
        let streamed = client
            .rpc
            .get_streamed_accounts(Some(client.context.clone()))
            .expect("Failed to get streamed accounts")
            .value;

        let accounts = serde_json::to_value(&streamed).unwrap();
        let accounts_arr = accounts["accounts"].as_array().unwrap();
        assert_eq!(accounts_arr.len(), 3, "Should have 3 streamed accounts");

        // Check individual entries
        let find = |pk: &str| {
            accounts_arr
                .iter()
                .find(|a| a["pubkey"].as_str().unwrap() == pk)
                .unwrap()
                .clone()
        };
        assert_eq!(find(&pubkey1.to_string())["includeOwnedAccounts"], true);
        assert_eq!(find(&pubkey2.to_string())["includeOwnedAccounts"], false);
        assert_eq!(find(&pubkey3.to_string())["includeOwnedAccounts"], false);
    }

    #[test]
    fn test_stream_accounts_empty_list() {
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let result = client
            .rpc
            .stream_accounts(Some(client.context.clone()), vec![]);
        assert!(
            result.is_ok(),
            "stream_accounts with empty list should succeed"
        );

        let streamed = client
            .rpc
            .get_streamed_accounts(Some(client.context.clone()))
            .expect("Failed to get streamed accounts")
            .value;

        let accounts = serde_json::to_value(&streamed).unwrap();
        let accounts_arr = accounts["accounts"].as_array().unwrap();
        assert_eq!(
            accounts_arr.len(),
            0,
            "Should have no streamed accounts after empty call"
        );
    }

    #[test]
    fn test_stream_accounts_invalid_pubkey() {
        let client = TestSetup::new(SurfnetCheatcodesRpc::empty());

        let entries = vec![StreamAccountsEntry {
            pubkey: "not-a-valid-pubkey".to_string(),
            include_owned_accounts: None,
        }];

        let result = client
            .rpc
            .stream_accounts(Some(client.context.clone()), entries);
        assert!(
            result.is_err(),
            "stream_accounts with invalid pubkey should fail"
        );
    }
}
