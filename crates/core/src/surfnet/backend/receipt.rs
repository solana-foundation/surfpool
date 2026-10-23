//! Execution results independent of the local LiteSVM's version-specific metadata.

use solana_account::AccountSharedData;
use solana_pubkey::Pubkey;
use solana_transaction_error::TransactionError;
pub use surfpool_types::TransactionMetadata;

#[derive(Clone, Debug, PartialEq)]
pub struct FailedTransactionMetadata {
    pub err: TransactionError,
    pub meta: TransactionMetadata,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimulatedTransactionInfo {
    pub meta: TransactionMetadata,
    pub post_accounts: Vec<(Pubkey, AccountSharedData)>,
}

pub type TransactionResult = Result<TransactionMetadata, FailedTransactionMetadata>;

pub fn from_local_metadata(meta: litesvm::types::TransactionMetadata) -> TransactionMetadata {
    TransactionMetadata {
        signature: meta.signature,
        logs: meta.logs,
        inner_instructions: meta.inner_instructions,
        compute_units_consumed: meta.compute_units_consumed,
        return_data: meta.return_data,
        fee: meta.fee,
        loaded_accounts_data_size: Some(meta.loaded_accounts_data_size),
    }
}

impl From<litesvm::types::FailedTransactionMetadata> for FailedTransactionMetadata {
    fn from(failure: litesvm::types::FailedTransactionMetadata) -> Self {
        Self {
            err: failure.err,
            meta: from_local_metadata(failure.meta),
        }
    }
}

impl From<litesvm::types::SimulatedTransactionInfo> for SimulatedTransactionInfo {
    fn from(info: litesvm::types::SimulatedTransactionInfo) -> Self {
        Self {
            meta: from_local_metadata(info.meta),
            post_accounts: info.post_accounts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_metered_size_is_kept_on_success_and_failure() {
        for bytes in [0, 357, 20_000] {
            let meta = litesvm::types::TransactionMetadata {
                loaded_accounts_data_size: bytes,
                ..Default::default()
            };
            let receipt = from_local_metadata(meta.clone());
            assert_eq!(receipt.loaded_accounts_data_size, Some(bytes));
            let failed: FailedTransactionMetadata = litesvm::types::FailedTransactionMetadata {
                err: TransactionError::BlockhashNotFound,
                meta,
            }
            .into();
            assert_eq!(failed.meta.loaded_accounts_data_size, Some(bytes));
        }
    }

    #[test]
    fn conversion_to_local_metadata_refuses_an_unreported_measurement() {
        let mut meta = TransactionMetadata::default();
        let error = crate::types::surfpool_tx_metadata_to_litesvm_tx_metadata(&meta)
            .expect_err("unknown is not a native zero measurement");
        assert!(error.to_string().contains("CAPABILITY_UNAVAILABLE"));
        meta.loaded_accounts_data_size = Some(357);
        let native = crate::types::surfpool_tx_metadata_to_litesvm_tx_metadata(&meta).unwrap();
        assert_eq!(native.loaded_accounts_data_size, 357);
    }
}
