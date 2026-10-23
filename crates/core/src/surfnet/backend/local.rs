use std::collections::{HashMap, HashSet};

use agave_feature_set::FeatureSet;
use itertools::Itertools;
use litesvm::{
    LiteSVM,
    types::{
        FailedTransactionMetadata, SimulatedTransactionInfo, TransactionMetadata, TransactionResult,
    },
};
use solana_account::{Account, AccountSharedData};
use solana_clock::Clock;
use solana_epoch_schedule::EpochSchedule;
use solana_fee::{FeeFeatures, calculate_fee};
use solana_hash::Hash;
use solana_loader_v3_interface::get_program_data_address;
use solana_message::{
    SanitizedMessage, SanitizedVersionedMessage, SimpleAddressLoader, VersionedMessage,
    v0::LoadedAddresses,
};
use solana_program_option::COption;
use solana_pubkey::Pubkey;
use solana_runtime_transaction::transaction_meta::TransactionConfiguration;
use solana_slot_hashes::SlotHashes;
use solana_sysvar::last_restart_slot::LastRestartSlot;
#[allow(deprecated)]
use solana_sysvar::recent_blockhashes::RecentBlockhashes;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;

use super::{Admission, IsProcessed};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    storage::{OverlayStorage, Storage, StorageBackend},
    surfnet::{AccountSource, CoupledAccount, GetAccountResult, is_supported_token_program},
};

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// A normal surfnet's backend: LiteSVM and the account database it loads from.
#[derive(Clone)]
pub struct LocalBackend {
    svm: LiteSVM,
    db: Option<Box<dyn Storage<String, AccountSharedData>>>,
}

impl LocalBackend {
    /// Read-only access to the LiteSVM (a documented normal-only compatibility accessor).
    pub fn svm(&self) -> &LiteSVM {
        &self.svm
    }

    /// Tests only: the LiteSVM, mutably (for example to install a test program or builtin).
    #[cfg(test)]
    pub fn svm_mut(&mut self) -> &mut LiteSVM {
        &mut self.svm
    }

    /// The account database (`None` in memory).
    pub fn account_db(&self) -> Option<&dyn Storage<String, AccountSharedData>> {
        self.db.as_deref()
    }

    pub fn account_db_mut(&mut self) -> Option<&mut Box<dyn Storage<String, AccountSharedData>>> {
        self.db.as_mut()
    }

    /// Surfpool's own fee schedule for a normal surfnet.
    pub fn lamports_per_signature(&self) -> u64 {
        self.svm.get_fee_structure().lamports_per_signature
    }

    /// Makes `live` continue with this sandbox's executed LiteSVM; the bundle commit drains the
    /// sandbox's database overlay separately.
    pub fn commit_into(&mut self, live: &mut Self) {
        std::mem::swap(&mut live.svm, &mut self.svm);
    }

    pub fn new(storage_backend: &StorageBackend, feature_set: FeatureSet) -> SurfpoolResult<Self> {
        let mut lite_svm = Self {
            svm: Self::litesvm_settings(feature_set),
            db: None,
        };

        create_native_mint(&mut lite_svm);

        if storage_backend.is_persistent() {
            let db: Box<dyn Storage<String, AccountSharedData>> =
                storage_backend.open_store("accounts")?;
            lite_svm.db = Some(db);
        }

        Ok(lite_svm)
    }

    /// Creates a clone of the SVM with overlay storage wrapper for the database.
    /// This allows profiling transactions without affecting the underlying database.
    /// All database writes are buffered in memory and discarded when the clone is dropped.
    pub fn clone_for_profiling(&self) -> Self {
        Self {
            svm: self.svm.clone(),
            db: self
                .db
                .as_ref()
                .map(|db| OverlayStorage::wrap(db.clone_box())),
        }
    }

    /// Builds the inner LiteSVM with `feature_set` fully applied.
    ///
    /// Ordering is significant: `with_feature_set` is called first so that the
    /// subsequent feature-gated setup steps (`with_builtins`, `with_sysvars`,
    /// `with_default_programs`) see the correct gate state.
    /// `with_feature_accounts` then writes one on-chain feature account for
    /// each currently-active feature.
    fn litesvm_settings(feature_set: FeatureSet) -> LiteSVM {
        LiteSVM::default()
            .with_feature_set(feature_set)
            .with_feature_accounts()
            .with_builtins()
            .with_lamports(1_000_000u64.wrapping_mul(LAMPORTS_PER_SOL))
            .with_sysvars()
            .with_default_programs()
            .with_precompiles()
            .with_blockhash_check(false)
            .with_sigverify(false)
    }

    pub fn reset(&mut self, feature_set: FeatureSet) -> SurfpoolResult<()> {
        self.svm = Self::litesvm_settings(feature_set);

        create_native_mint(self);

        if let Some(db) = &mut self.db {
            db.clear()?;
        }
        Ok(())
    }

    /// Perform garbage collection by resetting the SVM state while retaining the database.
    /// This is useful for cleaning up unused accounts and reducing memory usage.
    /// If no database is configured, this function is a no-op.
    #[allow(deprecated)]
    pub fn garbage_collect(&mut self, feature_set: FeatureSet) {
        // If no DB is configured, skip garbage collection
        if self.db.is_none() {
            return;
        }

        // Preserve all critical sysvars across garbage collection
        // - RecentBlockhashes: for blockhash validation
        // - SlotHashes: for ALT resolution
        // - Clock: for time-dependent programs
        // - EpochSchedule: the surfnet's own (or the datasource's), not LiteSVM's default
        // - LastRestartSlot: the datasource's, not LiteSVM's 0
        let recent_blockhashes = self.svm.get_sysvar::<RecentBlockhashes>();
        let slot_hashes = self.svm.get_sysvar::<SlotHashes>();
        let clock = self.svm.get_sysvar::<Clock>();
        let epoch_schedule = self.svm.get_sysvar::<EpochSchedule>();
        let last_restart_slot = self.svm.get_sysvar::<LastRestartSlot>();

        // todo: this is also resetting the log bytes limit and airdrop keypair, would be nice to avoid
        self.svm = Self::litesvm_settings(feature_set);

        create_native_mint(self);

        // Restore all preserved sysvars
        self.svm.set_sysvar(&recent_blockhashes);
        self.svm.set_sysvar(&slot_hashes);
        self.svm.set_sysvar(&clock);
        self.svm.set_sysvar(&epoch_schedule);
        self.svm.set_sysvar(&last_restart_slot);
    }

    pub fn set_log_bytes_limit(&mut self, limit: Option<usize>) {
        self.svm.set_log_bytes_limit(limit);
    }

    pub fn set_sigverify(&mut self, sigverify: bool) {
        self.svm.set_sigverify(sigverify);
    }

    pub fn with_blockhash_check(mut self, check: bool) -> Self {
        self.svm = self.svm.with_blockhash_check(check);
        self
    }

    pub fn get_sysvar<T>(&self) -> T
    where
        T: solana_sysvar::Sysvar + solana_sysvar_id::SysvarId + wincode::DeserializeOwned<Dst = T>,
    {
        self.svm.get_sysvar()
    }

    pub fn set_sysvar<T>(&mut self, sysvar: &T)
    where
        T: solana_sysvar::Sysvar + solana_sysvar_id::SysvarId + wincode::Serialize<Src = T>,
    {
        self.svm.set_sysvar(sysvar);
    }

    pub fn expire_blockhash(&mut self) {
        self.svm.expire_blockhash();
    }

    /// Signature verification, then the already-processed rule (moved from
    /// `SurfnetSvm::sigverify`; the processed-signature lookup stays in Surfpool's store).
    /// ### Note
    /// LiteSVM also can do this for our transactions, but we disable it.
    /// If sigverify is enabled at the LiteSVM level, the transaction simulations are always verified as well.
    /// So, if the user is trying to skip signature verification for a simulation, we'd need to unset and set this value,
    /// requiring a mutable reference to the SVM, which we don't have/want in the simulation path.
    pub fn verify_transaction(
        &self,
        tx: &VersionedTransaction,
        is_processed: IsProcessed<'_>,
    ) -> Result<(), FailedTransactionMetadata> {
        super::verify_signatures(tx, is_processed)
    }

    /// Checks if the provided blockhash is recent (present in the RecentBlockhashes sysvar).
    pub fn blockhash_is_recent(&self, recent_blockhash: &Hash) -> bool {
        #[allow(deprecated)]
        self.svm
            .get_sysvar::<RecentBlockhashes>()
            .iter()
            .any(|entry| entry.blockhash == *recent_blockhash)
    }

    /// The RecentBlockhashes sysvar's blockhashes, newest first.
    pub fn recent_blockhashes(&self) -> Vec<Hash> {
        #[allow(deprecated)]
        self.svm
            .get_sysvar::<RecentBlockhashes>()
            .iter()
            .map(|entry| entry.blockhash)
            .collect()
    }

    /// Validates a transaction's lifetime as Agave's `check_transactions` does: a recent
    /// blockhash, or else a durable nonce (moved from `SurfnetSvm::validate_transaction_blockhash`).
    pub fn transaction_lifetime_is_valid(
        &self,
        tx: &VersionedTransaction,
        skip_blockhash_check: bool,
    ) -> bool {
        skip_blockhash_check
            || self.blockhash_is_recent(tx.message.recent_blockhash())
            || self.durable_nonce_is_valid(&tx.message)
    }

    /// Agave's strict `check_nonce_account`: the nonce account is a system-owned, current-version
    /// nonce holding the message's blockhash, and its authority signed the advance instruction.
    ///
    /// V0 lookup tables are left unresolved, so a nonce account loaded from one is not found.
    fn durable_nonce_is_valid(&self, message: &VersionedMessage) -> bool {
        let Some(message) = SanitizedVersionedMessage::try_from(message.clone())
            .ok()
            .and_then(|message| {
                SanitizedMessage::try_new(
                    message,
                    SimpleAddressLoader::Enabled(LoadedAddresses::default()),
                    &agave_reserved_account_keys::ReservedAccountKeys::new_all_activated().active,
                )
                .ok()
            })
        else {
            return false;
        };
        message
            .get_durable_nonce()
            .and_then(|address| self.get_account(address).ok().flatten())
            .filter(|account| account.data.len() == solana_nonce::state::State::size())
            .and_then(|account| {
                solana_nonce_account::verify_nonce_account(
                    &account.into(),
                    message.recent_blockhash(),
                )
            })
            .is_some_and(|nonce| {
                message
                    .get_ix_signers(solana_nonce::NONCED_TX_MARKER_IX_INDEX as usize)
                    .any(|signer| signer == &nonce.authority)
            })
    }

    /// The fee a message pays (moved from `SurfnetSvm::estimate_fee_for_message`).
    pub fn fee_for_message(
        &self,
        message: &VersionedMessage,
        feature_set: &FeatureSet,
    ) -> SurfpoolResult<u64> {
        let address_loader = match message {
            VersionedMessage::V0(_) => SimpleAddressLoader::Enabled(LoadedAddresses::default()),
            VersionedMessage::Legacy(_) | VersionedMessage::V1(_) => SimpleAddressLoader::Disabled,
        };

        let sanitized_versioned_message = SanitizedVersionedMessage::try_from(message.clone())
            .map_err(|e| SurfpoolError::invalid_params(format!("Invalid message: {e:?}")))?;
        let sanitized_message = SanitizedMessage::try_new(
            sanitized_versioned_message,
            address_loader,
            &HashSet::new(), // reserved keys only drive writability, which fees ignore
        )
        .map_err(|e| SurfpoolError::invalid_params(format!("Invalid message: {e:?}")))?;

        let configuration =
            TransactionConfiguration::try_from_sanitized_message(&sanitized_message, feature_set)?;

        Ok(calculate_fee(
            &sanitized_message,
            self.lamports_per_signature(),
            configuration.priority_fee_lamports,
            FeeFeatures {},
        ))
    }

    /// The admission rules in their original order: signatures (and the already-processed
    /// rule) when requested, then the transaction's lifetime.
    fn admit(
        &self,
        tx: &VersionedTransaction,
        admission: &Admission<'_>,
    ) -> Result<(), FailedTransactionMetadata> {
        if let Some(is_processed) = admission.sigverify {
            self.verify_transaction(tx, is_processed)?;
        }
        if !self.transaction_lifetime_is_valid(tx, admission.skip_blockhash_check) {
            return Err(FailedTransactionMetadata {
                err: TransactionError::BlockhashNotFound,
                meta: TransactionMetadata::default(),
            });
        }
        Ok(())
    }

    /// Admits, then executes and commits.
    pub fn send_admitted(
        &mut self,
        tx: VersionedTransaction,
        admission: &Admission<'_>,
    ) -> TransactionResult {
        self.admit(&tx, admission)?;
        self.send_transaction(tx)
    }

    /// Admits, then simulates without committing.
    pub fn simulate_admitted(
        &self,
        tx: VersionedTransaction,
        admission: &Admission<'_>,
    ) -> Result<SimulatedTransactionInfo, FailedTransactionMetadata> {
        self.admit(&tx, admission)?;
        self.simulate_transaction(tx)
    }

    pub fn send_transaction(&mut self, tx: impl Into<VersionedTransaction>) -> TransactionResult {
        self.svm.send_transaction(tx)
    }

    pub fn minimum_balance_for_rent_exemption(&self, data_len: usize) -> u64 {
        self.svm.minimum_balance_for_rent_exemption(data_len)
    }

    pub fn simulate_transaction(
        &self,
        tx: impl Into<VersionedTransaction>,
    ) -> Result<SimulatedTransactionInfo, FailedTransactionMetadata> {
        self.svm.simulate_transaction(tx)
    }

    pub fn airdrop_pubkey(&self) -> Pubkey {
        self.svm.airdrop_pubkey()
    }

    pub fn airdrop(&mut self, pubkey: &Pubkey, lamports: u64) -> TransactionResult {
        self.svm.airdrop(pubkey, lamports)
    }

    pub fn get_account_no_db(&self, pubkey: &Pubkey) -> Option<Account> {
        self.svm.get_account(pubkey)
    }

    pub fn get_account(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Account>> {
        if let Some(account) = self.svm.get_account(pubkey) {
            return Ok(Some(account));
        } else if let Some(db) = &self.db {
            return Ok(db.get(&pubkey.to_string())?.map::<Account, _>(Into::into));
        }
        Ok(None)
    }

    pub fn get_account_result(&self, pubkey: &Pubkey) -> SurfpoolResult<GetAccountResult> {
        if let Some(account) = self.svm.get_account(pubkey) {
            return Ok(GetAccountResult::FoundAccount(
                *pubkey,
                account,
                AccountSource::Svm,
            ));
        } else if let Some(db) = &self.db {
            let mut result = None;
            if let Some(account) = db.get(&pubkey.to_string())?.map::<Account, _>(Into::into) {
                if is_supported_token_program(&account.owner) {
                    if let Ok(token_account) = crate::types::TokenAccount::unpack(&account.data) {
                        let mint = db.get(&token_account.mint().to_string())?.map(Into::into);

                        result = Some(GetAccountResult::FoundCoupledAccount(
                            (*pubkey, account.clone()),
                            CoupledAccount::Mint(token_account.mint(), mint),
                            AccountSource::Database,
                        ));
                    };
                } else if account.executable {
                    let program_data_address = get_program_data_address(pubkey);

                    let program_data = db.get(&program_data_address.to_string())?.map(Into::into);

                    result = Some(GetAccountResult::FoundCoupledAccount(
                        (*pubkey, account.clone()),
                        CoupledAccount::ProgramData(program_data_address, program_data),
                        AccountSource::Database,
                    ));
                }

                return Ok(result.unwrap_or(GetAccountResult::FoundAccount(
                    *pubkey,
                    account,
                    AccountSource::Database,
                )));
            }
        }
        Ok(GetAccountResult::None(*pubkey))
    }

    pub fn set_account(&mut self, pubkey: Pubkey, account: Account) -> SurfpoolResult<()> {
        self.set_account_in_db(pubkey, account.clone().into())?;

        self.svm.set_account(pubkey, account)?;
        Ok(())
    }

    pub fn delete_account(&mut self, pubkey: &Pubkey) -> SurfpoolResult<()> {
        self.delete_account_in_db(pubkey)?;

        // You can't delete an account using the LiteSvm, so we set it to an empty account
        // so it can be garbage collected later
        self.svm
            .set_account(*pubkey, Account::default())
            .map_err(|e| SurfpoolError::set_account(*pubkey, e))?;
        Ok(())
    }

    pub fn set_account_in_db(
        &mut self,
        pubkey: Pubkey,
        account: AccountSharedData,
    ) -> SurfpoolResult<()> {
        if let Some(db) = &mut self.db {
            db.store(pubkey.to_string(), account)?;
        }
        Ok(())
    }

    pub fn delete_account_in_db(&mut self, pubkey: &Pubkey) -> SurfpoolResult<()> {
        if let Some(db) = &mut self.db {
            db.take(&pubkey.to_string())?;
        }
        Ok(())
    }

    /// Get all accounts from both the LiteSVM state and the database, merging them together.
    /// Accounts in the LiteSVM state take precedence over those in the database.
    /// The resulting accounts are sorted by Pubkey.
    pub fn get_all_accounts(&self) -> SurfpoolResult<Vec<(Pubkey, AccountSharedData)>> {
        // In general, we trust the LiteSVM state as the most up-to-date source of truth for any given account,
        // But there's a chance that the account was garbage collected, meaning it exists in the DB but not in the SVM.
        // Therefore, we need to merge the two sources of accounts, prioritizing the SVM state.
        let mut accounts = HashMap::new();
        if let Some(db) = &self.db {
            let db_accounts = db.into_iter()?;
            for (key, account) in db_accounts {
                let pubkey = Pubkey::from_str_const(&key);
                accounts.insert(pubkey, account);
            }
        }
        for (pubkey, account) in self.svm.accounts_db().inner.iter() {
            if !accounts.contains_key(pubkey) {
                accounts.insert(*pubkey, account.clone());
            }
        }
        Ok(accounts
            .into_iter()
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect())
    }
}

fn create_native_mint(svm: &mut LocalBackend) {
    use solana_program_pack::Pack;
    use solana_sysvar::rent::Rent;
    use spl_token_interface::state::Mint;

    let mut data = vec![0; Mint::LEN];
    let mint = Mint {
        mint_authority: COption::None,
        supply: 0,
        decimals: spl_token_interface::native_mint::DECIMALS,
        is_initialized: true,
        freeze_authority: COption::None,
    };
    Mint::pack(mint, &mut data).unwrap();
    let account = Account {
        lamports: svm.get_sysvar::<Rent>().minimum_balance(data.len()),
        data,
        owner: spl_token_interface::ID,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(spl_token_interface::native_mint::ID, account)
        .expect("Failed to create native mint account in SVM");
}
#[cfg(test)]
mod tests {
    use ed25519_dalek::Signer as DalekSigner;
    use solana_compute_budget_interface::ComputeBudgetInstruction;
    use solana_ed25519_program::new_ed25519_instruction_with_signature;
    use solana_keypair::Keypair;
    use solana_message::{Message, VersionedMessage};
    use solana_signer::Signer;
    use solana_transaction::versioned::VersionedTransaction;

    use super::*;

    fn build_ed25519_transaction(
        payer: &Keypair,
        blockhash: solana_hash::Hash,
    ) -> VersionedTransaction {
        let payer_dalek = ed25519_dalek::Keypair::from_bytes(&payer.to_bytes())
            .expect("failed to create dalek keypair");
        let message = b"surfpool ed25519 precompile regression";
        let signature = payer_dalek.sign(message);
        let ed25519_ix = new_ed25519_instruction_with_signature(
            message,
            &signature.to_bytes(),
            payer.pubkey().as_array(),
        );
        let compute_budget_ixs = [
            ComputeBudgetInstruction::set_compute_unit_limit(100_000),
            ComputeBudgetInstruction::set_compute_unit_price(1),
        ];

        let tx_message = Message::new_with_blockhash(
            &[
                compute_budget_ixs[0].clone(),
                compute_budget_ixs[1].clone(),
                ed25519_ix,
            ],
            Some(&payer.pubkey()),
            &blockhash,
        );

        VersionedTransaction::try_new(VersionedMessage::Legacy(tx_message), &[&payer])
            .expect("failed to create ed25519 transaction")
    }

    #[test]
    fn test_litesvm_settings_registers_ed25519_precompile() {
        let mut svm = LocalBackend::litesvm_settings(LiteSVM::mainnet_feature_set());
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
            .expect("failed to fund test payer");
        let tx = build_ed25519_transaction(&payer, svm.latest_blockhash());

        let result = svm.send_transaction(tx);

        assert!(
            result.is_ok(),
            "ed25519 precompile should be available via litesvm_settings"
        );
    }
}
