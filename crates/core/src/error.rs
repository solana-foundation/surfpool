use std::{fmt::Display, future::Future, pin::Pin};

use crossbeam_channel::TrySendError;
use jsonrpc_core::{Error, Result};
use litesvm::error::LiteSVMError;
use serde::Serialize;
use serde_json::json;
use solana_client::{client_error::ClientError, rpc_request::TokenAccountsFilter};
use solana_clock::Slot;
use solana_pubkey::Pubkey;
use solana_transaction::TransactionError;
use solana_transaction_status::EncodeError;

use crate::storage::StorageError;

pub type SurfpoolResult<T> = std::result::Result<T, SurfpoolError>;

#[derive(Debug, Clone)]
pub struct SurfpoolError(Error);

impl From<SurfpoolError> for String {
    fn from(e: SurfpoolError) -> Self {
        e.0.to_string()
    }
}

impl From<SurfpoolError> for Error {
    fn from(e: SurfpoolError) -> Self {
        e.0
    }
}

impl From<EncodeError> for SurfpoolError {
    fn from(e: EncodeError) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Transaction encoding error: {}",
            e.to_string()
        )));
        Self(error)
    }
}

impl std::error::Error for SurfpoolError {}

impl std::fmt::Display for SurfpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let Error {
            code,
            message,
            data,
        } = &self.0;

        let core = if code.description().eq(message) {
            code.description()
        } else {
            format!("{}: {}", code.description(), message)
        };

        if let Some(data_value) = data {
            write!(f, "{}: {}", core, data_value.to_string().as_str())
        } else {
            write!(f, "{}", core)
        }
    }
}

impl<T> From<SurfpoolError> for Pin<Box<dyn Future<Output = Result<T>> + Send>> {
    fn from(e: SurfpoolError) -> Self {
        Box::pin(async move { Err(e.into()) })
    }
}

impl<T> From<TrySendError<T>> for SurfpoolError {
    fn from(val: TrySendError<T>) -> Self {
        SurfpoolError::from_try_send_error(val)
    }
}

impl From<solana_client::client_error::ClientError> for SurfpoolError {
    fn from(e: solana_client::client_error::ClientError) -> Self {
        SurfpoolError::client_error(e)
    }
}

impl SurfpoolError {
    pub fn from_try_send_error<T>(e: TrySendError<T>) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to send command on channel: {}",
            e.to_string()
        )));
        Self(error)
    }

    pub fn client_error(e: solana_client::client_error::ClientError) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!("Solana RPC client error: {}", e.to_string())));
        Self(error)
    }

    pub fn invalid_bundle_id() -> Self {
        let mut error = Error::invalid_request();
        error.data = Some(json!(format!("Solana RPC client error: Invalid bundle id")));
        Self(error)
    }

    pub fn missing_context() -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!("Failed to access internal Surfnet context"));
        Self(error)
    }

    pub fn disable_cheatcode(e: String) -> Self {
        let mut error = Error::invalid_request();
        error.data = Some(json!(format!("Unable to disable cheatcode: {}", e)));
        Self(error)
    }

    pub fn enable_cheatcode(e: String) -> Self {
        let mut error = Error::invalid_request();
        error.data = Some(json!(format!("Unable to enable cheatcode: {}", e)));
        Self(error)
    }

    pub fn set_account<T>(pubkey: Pubkey, e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to set account {}: {}",
            pubkey,
            e.to_string()
        )));
        Self(error)
    }
    pub fn get_account<T>(pubkey: Pubkey, e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to fetch account {} from remote: {}",
            pubkey,
            e.to_string()
        )));
        Self(error)
    }

    pub fn get_token_accounts<T>(owner: Pubkey, filter: &TokenAccountsFilter, e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to get token accounts by owner {owner} for {}: {}",
            match filter {
                TokenAccountsFilter::ProgramId(token_program) => format!("program {token_program}"),
                TokenAccountsFilter::Mint(mint) => format!("mint {mint}"),
            },
            e.to_string()
        )));
        Self(error)
    }

    pub fn get_token_accounts_by_delegate_error<T>(
        delegate: Pubkey,
        filter: &TokenAccountsFilter,
        e: T,
    ) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();

        let filter_description = match filter {
            TokenAccountsFilter::ProgramId(program_id) => {
                let program_name = if *program_id == spl_token_interface::ID {
                    "SPL Token program"
                } else if *program_id == spl_token_2022_interface::ID {
                    "Token 2022 program"
                } else {
                    "custom token program"
                };
                format!("{} ({})", program_id, program_name)
            }
            TokenAccountsFilter::Mint(mint) => format!("mint {}", mint),
        };

        error.data = Some(json!(format!(
            "Failed to get token accounts by delegate {} for {}: {}",
            delegate,
            filter_description,
            e.to_string()
        )));

        Self(error)
    }

    pub fn unsupported_token_program(program_id: Pubkey) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Unsupported token program: {}. Only SPL Token ({}) and Token 2022 ({}) are currently supported.",
            program_id,
            spl_token_interface::ID,
            spl_token_2022_interface::ID
        )));
        Self(error)
    }

    pub fn get_program_accounts<T>(program_id: Pubkey, e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to fetch program accounts for {program_id}: {}",
            e.to_string()
        )));
        Self(error)
    }

    pub fn get_token_largest_accounts<T>(mint: Pubkey, e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to get largest token accounts for mint {mint}: {}",
            e.to_string()
        )));
        Self(error)
    }

    pub fn get_multiple_accounts<T>(e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to fetch accounts from remote: {}",
            e.to_string()
        )));
        Self(error)
    }
    pub fn get_largest_accounts<T>(e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to fetch largest accounts from remote: {}",
            e.to_string()
        )));
        Self(error)
    }

    pub fn get_signatures_for_address<T>(e: T) -> Self
    where
        T: ToString,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to fetch signatures for address from remote: {}",
            e.to_string()
        )));
        Self(error)
    }

    pub fn invalid_pubkey<D>(pubkey: &str, data: D) -> Self
    where
        D: Serialize,
    {
        let mut error = Error::invalid_params(format!("Invalid pubkey '{pubkey}'"));
        error.data = Some(json!(data));
        Self(error)
    }

    pub fn invalid_pubkey_at_index<D>(pubkey: &str, index: usize, data: D) -> Self
    where
        D: Serialize,
    {
        let mut error =
            Error::invalid_params(format!("Invalid pubkey '{pubkey}' at index {index}"));
        error.data = Some(json!(data));
        Self(error)
    }

    pub fn invalid_signature<D>(signature: &str, data: D) -> Self
    where
        D: Serialize,
    {
        let mut error = Error::invalid_params(format!("Invalid signature {signature}"));
        error.data = Some(json!(data));
        Self(error)
    }

    pub fn invalid_program_account<P, D>(program_id: P, data: D) -> Self
    where
        P: Display,
        D: Serialize,
    {
        let mut error = Error::invalid_params(format!("Invalid program account {program_id}"));
        error.data = Some(json!(data));
        Self(error)
    }

    pub fn invalid_program_data_account<P, D>(program_data_id: P, data: D) -> Self
    where
        P: Display,
        D: Serialize,
    {
        let mut error =
            Error::invalid_params(format!("Invalid program data account {program_data_id}"));
        error.data = Some(json!(data));
        Self(error)
    }

    pub fn expected_program_account<P>(program_id: P) -> Self
    where
        P: Display,
    {
        let error = Error::invalid_params(format!("Account {program_id} is not a program account"));
        Self(error)
    }

    pub fn account_not_found<P>(pubkey: P) -> Self
    where
        P: Display,
    {
        let error = Error::invalid_params(format!("Account {pubkey} not found"));
        Self(error)
    }

    pub fn transaction_not_found<S>(signature: S) -> Self
    where
        S: Display,
    {
        let error = Error::invalid_params(format!("Transaction {signature} not found"));
        Self(error)
    }

    pub fn invalid_account_data<P, D, M>(pubkey: P, data: D, message: Option<M>) -> Self
    where
        P: Display,
        D: Serialize,
        M: Display,
    {
        let base_msg = format!("invalid account data {pubkey}");
        let full_msg = if let Some(msg) = message {
            format!("{base_msg}: {msg}")
        } else {
            base_msg
        };
        let mut error = Error::invalid_params(full_msg);
        error.data = Some(json!(data));
        Self(error)
    }

    pub fn invalid_account_owner<P, M>(pubkey: P, message: Option<M>) -> Self
    where
        P: Display,
        M: Display,
    {
        let base_msg = format!("invalid account owner {pubkey}");
        let full_msg = if let Some(msg) = message {
            format!("{base_msg}: {msg}")
        } else {
            base_msg
        };
        let error = Error::invalid_params(full_msg);
        Self(error)
    }
    pub fn invalid_lookup_index<P>(pubkey: P) -> Self
    where
        P: Display,
    {
        let error =
            Error::invalid_params(format!("Address lookup {pubkey} contains an invalid index"));
        Self(error)
    }

    pub fn invalid_base64_data<D>(typing: &str, data: D) -> Self
    where
        D: Display,
    {
        let mut error = Error::invalid_params(format!("Invalid base64 {typing}"));
        error.data = Some(json!(data.to_string()));
        Self(error)
    }

    pub fn deserialize_error<D>(typing: &str, data: D) -> Self
    where
        D: Display,
    {
        let mut error = Error::invalid_params(format!("Failed to deserialize {typing}"));
        error.data = Some(json!(data.to_string()));
        Self(error)
    }

    pub fn rpc_method_not_supported() -> Self {
        let mut error = Error::internal_error();
        error.message = "RPC method not supported".to_string();
        Self(error)
    }

    pub fn internal<D>(data: D) -> Self
    where
        D: Serialize,
    {
        let mut error = Error::internal_error();
        error.data = Some(json!(data));
        Self(error)
    }

    pub fn sig_verify_replace_recent_blockhash_collision() -> Self {
        Self(Error::invalid_params(
            "sigVerify may not be used with replaceRecentBlockhash",
        ))
    }

    pub fn slot_too_old(slot: Slot) -> Self {
        Self(Error::invalid_params(format!(
            "Requested {slot} is before the first local slot, and no remote RPC was provided."
        )))
    }

    pub fn get_block(e: ClientError, block: Slot) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!(
            "Failed to get block {block} from remote: {e}"
        )));
        Self(error)
    }

    pub fn token_mint_not_found(mint: Pubkey) -> Self {
        let mut error = Error::internal_error();
        error.message = format!("Token mint {mint} not found");
        Self(error)
    }

    pub fn unpack_token_account() -> Self {
        let mut error = Error::parse_error();
        error.message = "Failed to unpack token account".to_string();
        Self(error)
    }

    pub fn unpack_mint_account() -> Self {
        let mut error = Error::parse_error();
        error.message = "Failed to unpack mint account".to_string();
        Self(error)
    }

    pub fn invalid_token_account_state(state: &str) -> Self {
        let error = Error::invalid_params(format!("Invalid token account state {state}"));
        Self(error)
    }

    pub fn tag_not_found(tag: &str) -> Self {
        let mut error = Error::internal_error();
        error.message = format!("Profile result associated with tag '{tag}' not found in the SVM");
        Self(error)
    }

    pub(crate) fn expected_profile_not_found(key: &surfpool_types::UuidOrSignature) -> Self {
        let mut error = Error::internal_error();
        error.message = format!("Expected profile not found for key {key}");
        Self(error)
    }
}

impl From<StorageError> for SurfpoolError {
    fn from(e: StorageError) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!("Storage error: {}", e.to_string())));
        SurfpoolError(error)
    }
}

impl From<LiteSVMError> for SurfpoolError {
    fn from(e: LiteSVMError) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!("LiteSVM error: {}", e.to_string())));
        SurfpoolError(error)
    }
}

impl From<TransactionError> for SurfpoolError {
    fn from(e: TransactionError) -> Self {
        let mut error = Error::internal_error();
        error.data = Some(json!(format!("Transaction error: {}", e.to_string())));
        SurfpoolError(error)
    }
}
