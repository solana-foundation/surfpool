//! A surfnet's execution backend: the VM and the account source that execute its transactions,
//! and the admission rules they are executed under. Surfpool's chain policy, block/transaction/
//! index stores, events, subscriptions and RPC stay outside, in `SurfnetSvm` and its workflows.
//!
//! [`Backend`] is the stable facade over a private concrete choice: the local LiteSVM with its
//! account database, or a historical fork that SVM Replay executes. Operations that the historical
//! implementation can fail are fallible for both (account acquisition, transactions, sandboxes,
//! arbitrary sysvars, fees, hash validity). Where only one implementation has authority, the
//! other answers with a typed error instead of a guess: SVM Replay alone admits historical
//! transactions, and normal-only chain policy reaches the local VM through the explicit
//! [`Backend::local_mut`] (an `Option`), never a panicking accessor.

pub(crate) mod historical;
pub mod local;
pub mod receipt;

use agave_feature_set::FeatureSet;
pub use local::LocalBackend;
pub use receipt::{
    FailedTransactionMetadata, SimulatedTransactionInfo, TransactionMetadata, TransactionResult,
};
use solana_account::{Account, AccountSharedData};
use solana_clock::Clock;
use solana_epoch_schedule::EpochSchedule;
use solana_hash::Hash;
use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_sysvar::rent::Rent;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;

pub(crate) use self::historical::{ArchiveReader, HistoricalFork, HistoricalSession};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    storage::{Storage, StorageBackend},
    surfnet::GetAccountResult,
};

/// Whether Surfpool's transaction store holds a signature as processed. The store stays
/// Surfpool's; the backend's admission rules consult it at their own point in the order.
pub type IsProcessed<'a> = &'a dyn Fn(&Signature) -> bool;

/// How a transaction is admitted before it executes or simulates.
#[derive(Clone, Copy)]
pub struct Admission<'a> {
    /// `Some(is_processed)`: verify the signatures, then refuse a signature the store reports as
    /// already processed. `None` skips both (Surfpool's `sigverify: false`). On a historical
    /// fork SVM Replay verifies signatures and refuses duplicates itself.
    pub sigverify: Option<IsProcessed<'a>>,
    /// Accept any blockhash (Surfpool's `skip_blockhash_check`); otherwise the transaction needs
    /// a recent blockhash or a valid durable nonce. A historical fork cannot skip SVM Replay's
    /// hash-age check.
    pub skip_blockhash_check: bool,
}

/// Who admits a surfnet's transactions (signatures, blockhash age, durable nonces, duplicates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionAuthority {
    /// Surfpool's own rules, before the local VM executes.
    Surfpool,
    /// SVM Replay's native admission, inside the historical execution.
    Engine,
}

/// The execution backend of one surfnet (or of a sandbox copy of it).
#[derive(Clone)]
pub struct Backend {
    concrete: Concrete,
}

#[derive(Clone)]
enum Concrete {
    /// LiteSVM with Surfpool's account database: a normal surfnet's VM and account source.
    Local(LocalBackend),
    /// A historical fork: one branch of SVM Replay's session.
    Historical(HistoricalFork),
}

/// Why a transaction's signature list does not fit its message, if it does not (Solana's
/// structural rule for signatures, as `VersionedTransaction::sanitize` states it): at least one
/// signature (the fee payer's), exactly as many as the message header requires, and no more than
/// the message's static account keys. Public RPCs refuse such a transaction before any of its
/// signatures is read or any account acquired; the signature rules refuse it too.
pub(crate) fn signature_structure_error(tx: &VersionedTransaction) -> Option<String> {
    let given = tx.signatures.len();
    let required = usize::from(tx.message.header().num_required_signatures);
    let keys = tx.message.static_account_keys().len();
    let signatures = |n: usize| {
        if n == 1 {
            "1 signature".to_string()
        } else {
            format!("{n} signatures")
        }
    };
    if given == 0 {
        Some("has no signatures".to_string())
    } else if given != required {
        Some(format!(
            "has {} but its message requires {}",
            signatures(given),
            signatures(required)
        ))
    } else if given > keys {
        Some(format!(
            "has {} but only {keys} static account keys",
            signatures(given)
        ))
    } else {
        None
    }
}

/// Surfpool's signature rules, the same in both modes and consulting no VM: every signature
/// verifies, then the first one is not already processed in Surfpool's store. A historical fork
/// runs them before SVM Replay resolves or acquires anything for the transaction, so a refused
/// one loads no account; SVM Replay still admits the transactions it executes. A signature list
/// that does not fit its message cannot be verified: it is refused as a sanitize failure.
pub(crate) fn verify_signatures(
    tx: &VersionedTransaction,
    is_processed: IsProcessed<'_>,
) -> Result<(), litesvm::types::FailedTransactionMetadata> {
    use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
    let (Some(&signature), None) = (tx.signatures.first(), signature_structure_error(tx)) else {
        return Err(FailedTransactionMetadata {
            err: TransactionError::SanitizeFailure,
            meta: TransactionMetadata::default(),
        });
    };

    if tx.verify_with_results().iter().any(|valid| !*valid) {
        return Err(FailedTransactionMetadata {
            err: TransactionError::SignatureFailure,
            meta: TransactionMetadata::default(),
        });
    }

    if is_processed(&signature) {
        return Err(FailedTransactionMetadata {
            err: TransactionError::AlreadyProcessed,
            meta: TransactionMetadata::default(),
        });
    }
    Ok(())
}

/// The typed refusal for a decision that SVM Replay owns on a historical fork.
fn engine_owned(what: &str) -> SurfpoolError {
    SurfpoolError::historical(
        "UNSUPPORTED_IN_HISTORICAL_MODE",
        &format!(
            "{what}: SVM Replay admits historical transactions (signatures, blockhash age, durable nonces, duplicates)"
        ),
        None,
    )
}

impl Backend {
    /// A normal surfnet's backend (LiteSVM, and the account database when the storage persists).
    pub fn new_local(
        storage_backend: &StorageBackend,
        feature_set: FeatureSet,
    ) -> SurfpoolResult<Self> {
        Ok(Self {
            concrete: Concrete::Local(LocalBackend::new(storage_backend, feature_set)?),
        })
    }

    /// A historical surfnet's backend: an attached fork of SVM Replay's session.
    pub(crate) fn historical(fork: HistoricalFork) -> Self {
        Self {
            concrete: Concrete::Historical(fork),
        }
    }

    /// Whether SVM Replay executes this surfnet.
    pub fn is_historical(&self) -> bool {
        matches!(self.concrete, Concrete::Historical(_))
    }

    pub fn admission_authority(&self) -> AdmissionAuthority {
        match &self.concrete {
            Concrete::Local(_) => AdmissionAuthority::Surfpool,
            Concrete::Historical(_) => AdmissionAuthority::Engine,
        }
    }

    /// Read-only access to the local backend; `None` on a historical fork.
    pub fn local(&self) -> Option<&LocalBackend> {
        match &self.concrete {
            Concrete::Local(local) => Some(local),
            Concrete::Historical(_) => None,
        }
    }

    /// Normal-only implementation access for Surfpool's normal chain policy
    /// (`surfnet::local_chain`); `None` on a historical fork, which that policy then refuses.
    pub(in crate::surfnet) fn local_mut(&mut self) -> Option<&mut LocalBackend> {
        match &mut self.concrete {
            Concrete::Local(local) => Some(local),
            Concrete::Historical(_) => None,
        }
    }

    /// Tests only: the local backend, mutably (tests of normal surfnets).
    #[cfg(test)]
    pub(crate) fn local_mut_for_tests(&mut self) -> &mut LocalBackend {
        self.local_mut().expect("a local backend")
    }

    /// The historical fork, for Surfpool's historical chain policy; `None` on a local surfnet.
    pub(crate) fn historical_fork(&self) -> Option<&HistoricalFork> {
        match &self.concrete {
            Concrete::Historical(fork) => Some(fork),
            Concrete::Local(_) => None,
        }
    }

    pub(crate) fn historical_fork_mut(&mut self) -> Option<&mut HistoricalFork> {
        match &mut self.concrete {
            Concrete::Historical(fork) => Some(fork),
            Concrete::Local(_) => None,
        }
    }

    /// The historical fork's session (pins, journal counts, termination).
    pub(crate) fn historical_session(&self) -> Option<HistoricalSession> {
        self.historical_fork().map(HistoricalFork::session)
    }

    /// History before the historical fork's position, readable outside the SVM lock.
    pub(crate) fn archive_reader(&self) -> Option<ArchiveReader> {
        self.historical_fork().map(HistoricalFork::archive_reader)
    }

    /// An isolated copy for profiling, simulation and bundle sandboxes; dropping it discards it.
    pub fn clone_for_profiling(&self) -> SurfpoolResult<Self> {
        let concrete = match &self.concrete {
            Concrete::Local(local) => Concrete::Local(local.clone_for_profiling()),
            Concrete::Historical(fork) => Concrete::Historical(fork.clone_for_profiling()?),
        };
        Ok(Self { concrete })
    }

    /// Makes `target` continue with this sandbox's executed state.
    pub fn commit_into(&mut self, target: &mut Self) -> SurfpoolResult<()> {
        match (&mut self.concrete, &mut target.concrete) {
            (Concrete::Local(sandbox), Concrete::Local(live)) => {
                sandbox.commit_into(live);
                Ok(())
            }
            (Concrete::Historical(sandbox), Concrete::Historical(live)) => {
                sandbox.commit_into(live)
            }
            _ => Err(SurfpoolError::internal(
                "a sandbox and its surfnet must both be historical or both local",
            )),
        }
    }

    /// Back to the start: local defaults, or a historical fork's boundary state.
    pub fn reset(&mut self, feature_set: FeatureSet) -> SurfpoolResult<()> {
        match &mut self.concrete {
            Concrete::Local(local) => local.reset(feature_set),
            Concrete::Historical(fork) => fork.reset(),
        }
    }

    /// The signature rules on their own (see [`verify_signatures`]), the same in both modes.
    /// The outer error is an infrastructure failure, the inner one the transaction's refusal.
    pub fn verify_transaction(
        &self,
        tx: &VersionedTransaction,
        is_processed: IsProcessed<'_>,
    ) -> SurfpoolResult<Result<(), FailedTransactionMetadata>> {
        Ok(verify_signatures(tx, is_processed).map_err(Into::into))
    }

    /// A recent blockhash, or else a valid durable nonce (unless `skip_blockhash_check`).
    pub fn transaction_lifetime_is_valid(
        &self,
        tx: &VersionedTransaction,
        skip_blockhash_check: bool,
    ) -> SurfpoolResult<bool> {
        match &self.concrete {
            Concrete::Local(local) => {
                Ok(local.transaction_lifetime_is_valid(tx, skip_blockhash_check))
            }
            Concrete::Historical(_) => Err(engine_owned("transaction lifetime")),
        }
    }

    /// Whether `blockhash` is accepted for processing now. A failure to decide is an error,
    /// never `false`.
    pub fn blockhash_is_recent(&self, blockhash: &Hash) -> SurfpoolResult<bool> {
        match &self.concrete {
            Concrete::Local(local) => Ok(local.blockhash_is_recent(blockhash)),
            Concrete::Historical(fork) => fork.blockhash_valid(blockhash),
        }
    }

    /// The fee a message pays, under `feature_set` locally or the historical runtime's rules:
    /// `None` when its blockhash is no longer accepted, an error when the fee cannot be
    /// determined.
    pub fn fee_for_message(
        &self,
        message: &VersionedMessage,
        feature_set: &FeatureSet,
    ) -> SurfpoolResult<Option<u64>> {
        match &self.concrete {
            Concrete::Local(local) => local.fee_for_message(message, feature_set).map(Some),
            Concrete::Historical(fork) => fork.fee_for_message(&message.serialize()),
        }
    }

    /// Admits and executes; an infrastructure failure is an error, a refused or failed
    /// transaction a result.
    pub fn send_transaction(
        &mut self,
        tx: impl Into<VersionedTransaction>,
        admission: &Admission<'_>,
    ) -> SurfpoolResult<TransactionResult> {
        match &mut self.concrete {
            Concrete::Local(local) => Ok(local
                .send_admitted(tx.into(), admission)
                .map(receipt::from_local_metadata)
                .map_err(Into::into)),
            Concrete::Historical(fork) => {
                if admission.skip_blockhash_check {
                    return Err(engine_owned("skipping the blockhash check"));
                }
                let tx = tx.into();
                // Surfpool's signature rules first: a refused transaction acquires nothing.
                if let Some(is_processed) = admission.sigverify
                    && let Err(refused) = verify_signatures(&tx, is_processed)
                {
                    return Ok(Err(refused.into()));
                }
                fork.send(tx, admission.sigverify.is_some())
            }
        }
    }

    /// Admits and simulates without committing; an infrastructure failure is an error, a
    /// refused or failed transaction a result.
    pub fn simulate_transaction(
        &self,
        tx: impl Into<VersionedTransaction>,
        admission: &Admission<'_>,
    ) -> SurfpoolResult<Result<SimulatedTransactionInfo, FailedTransactionMetadata>> {
        match &self.concrete {
            Concrete::Local(local) => Ok(local
                .simulate_admitted(tx.into(), admission)
                .map(Into::into)
                .map_err(Into::into)),
            Concrete::Historical(fork) => {
                if admission.skip_blockhash_check {
                    return Err(engine_owned("skipping the blockhash check"));
                }
                let tx = tx.into();
                // Surfpool's signature rules first: a refused transaction acquires nothing.
                if let Some(is_processed) = admission.sigverify
                    && let Err(refused) = verify_signatures(&tx, is_processed)
                {
                    return Ok(Err(refused.into()));
                }
                fork.simulate(tx, admission.sigverify.is_some())
            }
        }
    }

    /// Readies the airdrop account: a historical fork funds its local faucet once.
    pub fn prepare_airdrop(&mut self) -> SurfpoolResult<()> {
        match &mut self.concrete {
            Concrete::Local(_) => Ok(()),
            Concrete::Historical(fork) => fork.prepare_airdrop(),
        }
    }

    pub fn airdrop_pubkey(&self) -> SurfpoolResult<Pubkey> {
        match &self.concrete {
            Concrete::Local(local) => Ok(local.airdrop_pubkey()),
            Concrete::Historical(fork) => fork.airdrop_pubkey(),
        }
    }

    pub fn airdrop(&mut self, pubkey: &Pubkey, lamports: u64) -> SurfpoolResult<TransactionResult> {
        match &mut self.concrete {
            Concrete::Local(local) => Ok(local
                .airdrop(pubkey, lamports)
                .map(receipt::from_local_metadata)
                .map_err(Into::into)),
            Concrete::Historical(fork) => fork.airdrop(pubkey, lamports),
        }
    }

    /// The executing clock (a historical fork's decoded view, refreshed on every change).
    pub fn clock(&self) -> Clock {
        match &self.concrete {
            Concrete::Local(local) => local.get_sysvar(),
            Concrete::Historical(fork) => fork.view().clock.clone(),
        }
    }

    pub fn epoch_schedule(&self) -> EpochSchedule {
        match &self.concrete {
            Concrete::Local(local) => local.get_sysvar(),
            Concrete::Historical(fork) => fork.view().epoch_schedule.clone(),
        }
    }

    pub fn rent(&self) -> Rent {
        match &self.concrete {
            Concrete::Local(local) => local.get_sysvar(),
            Concrete::Historical(fork) => fork.view().rent.clone(),
        }
    }

    pub fn minimum_balance_for_rent_exemption(&self, data_len: usize) -> u64 {
        match &self.concrete {
            Concrete::Local(local) => local.minimum_balance_for_rent_exemption(data_len),
            Concrete::Historical(fork) => fork.view().rent.minimum_balance(data_len),
        }
    }

    /// Any sysvar from the executing VM. Fallible: a backend without the account answers with
    /// an error, never a default.
    pub fn read_sysvar<T>(&self) -> SurfpoolResult<T>
    where
        T: solana_sysvar::Sysvar + solana_sysvar_id::SysvarId + wincode::DeserializeOwned<Dst = T>,
    {
        match &self.concrete {
            Concrete::Local(local) => Ok(local.get_sysvar()),
            Concrete::Historical(fork) => fork.read_sysvar(),
        }
    }

    /// The account as the executing VM holds it (without the local database).
    pub fn get_account_no_db(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Account>> {
        match &self.concrete {
            Concrete::Local(local) => Ok(local.get_account_no_db(pubkey)),
            Concrete::Historical(fork) => fork.get_account(pubkey),
        }
    }

    pub fn get_account(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Account>> {
        match &self.concrete {
            Concrete::Local(local) => local.get_account(pubkey),
            Concrete::Historical(fork) => fork.get_account(pubkey),
        }
    }

    pub fn get_account_result(&self, pubkey: &Pubkey) -> SurfpoolResult<GetAccountResult> {
        match &self.concrete {
            Concrete::Local(local) => local.get_account_result(pubkey),
            Concrete::Historical(fork) => fork.get_account_result(pubkey),
        }
    }

    pub fn set_account(&mut self, pubkey: Pubkey, account: Account) -> SurfpoolResult<()> {
        match &mut self.concrete {
            Concrete::Local(local) => local.set_account(pubkey, account),
            Concrete::Historical(fork) => fork.set_account(&pubkey, &account),
        }
    }

    /// Locally evicts the account (it is read again on next access); historically an explicit
    /// absence on the branch.
    pub fn delete_account(&mut self, pubkey: &Pubkey) -> SurfpoolResult<()> {
        match &mut self.concrete {
            Concrete::Local(local) => local.delete_account(pubkey),
            Concrete::Historical(fork) => fork.delete_account(pubkey),
        }
    }

    /// Writes the local account database (if any) without touching the VM. A historical fork
    /// has no local account database: there is nothing to write.
    pub fn set_account_in_db(
        &mut self,
        pubkey: Pubkey,
        account: AccountSharedData,
    ) -> SurfpoolResult<()> {
        match &mut self.concrete {
            Concrete::Local(local) => local.set_account_in_db(pubkey, account),
            Concrete::Historical(_) => Ok(()),
        }
    }

    /// Removes an account from the local account database (if any) without touching the VM.
    /// A historical fork has no local account database: there is nothing to remove.
    pub fn delete_account_in_db(&mut self, pubkey: &Pubkey) -> SurfpoolResult<()> {
        match &mut self.concrete {
            Concrete::Local(local) => local.delete_account_in_db(pubkey),
            Concrete::Historical(_) => Ok(()),
        }
    }

    /// Every account the executing VM holds: LiteSVM and its database locally, the accounts a
    /// historical branch loaded otherwise; neither is every account on the chain.
    pub fn get_all_accounts(&self) -> SurfpoolResult<Vec<(Pubkey, AccountSharedData)>> {
        match &self.concrete {
            Concrete::Local(local) => local.get_all_accounts(),
            Concrete::Historical(fork) => fork.get_all_accounts(),
        }
    }

    /// The account database the backend loads from (`None` in memory and on a historical fork).
    pub fn account_db(&self) -> Option<&dyn Storage<String, AccountSharedData>> {
        match &self.concrete {
            Concrete::Local(local) => local.account_db(),
            Concrete::Historical(_) => None,
        }
    }

    pub fn account_db_mut(&mut self) -> Option<&mut Box<dyn Storage<String, AccountSharedData>>> {
        match &mut self.concrete {
            Concrete::Local(local) => local.account_db_mut(),
            Concrete::Historical(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use solana_hash::Hash;
    use solana_keypair::Keypair;
    use solana_message::{Message, VersionedMessage, v0};
    use solana_signer::Signer;
    use solana_system_interface::instruction::transfer;

    use super::*;

    fn message(v0_message: bool, signers: &[&Keypair]) -> VersionedMessage {
        let to = Pubkey::new_unique();
        let instructions: Vec<_> = signers
            .iter()
            .map(|signer| transfer(&signer.pubkey(), &to, 1))
            .collect();
        let payer = signers[0].pubkey();
        if v0_message {
            VersionedMessage::V0(
                v0::Message::try_compile(&payer, &instructions, &[], Hash::new_unique()).unwrap(),
            )
        } else {
            VersionedMessage::Legacy(Message::new_with_blockhash(
                &instructions,
                Some(&payer),
                &Hash::new_unique(),
            ))
        }
    }

    /// A signature list that does not fit its message (none, fewer or more than the message
    /// requires) is refused as a sanitize failure without a signature being read; a well-formed
    /// list still meets the cryptographic rule, and a signed transaction passes.
    #[test]
    fn the_signature_rules_refuse_a_signature_list_that_does_not_fit_its_message() {
        let (payer, cosigner) = (Keypair::new(), Keypair::new());
        let never_processed: IsProcessed<'_> = &|_| false;
        for v0_message in [false, true] {
            let one = message(v0_message, &[&payer]);
            let two = message(v0_message, &[&payer, &cosigner]);
            for (msg, given, reason) in [
                (&one, 0, "has no signatures"),
                (
                    &one,
                    2,
                    "has 2 signatures but its message requires 1 signature",
                ),
                (
                    &two,
                    1,
                    "has 1 signature but its message requires 2 signatures",
                ),
            ] {
                let tx = VersionedTransaction {
                    signatures: vec![Signature::default(); given],
                    message: msg.clone(),
                };
                assert_eq!(signature_structure_error(&tx).as_deref(), Some(reason));
                let refused = verify_signatures(&tx, never_processed).unwrap_err();
                assert_eq!(refused.err, TransactionError::SanitizeFailure);
            }
            let placeholders = VersionedTransaction {
                signatures: vec![Signature::default()],
                message: one.clone(),
            };
            assert_eq!(signature_structure_error(&placeholders), None);
            assert_eq!(
                verify_signatures(&placeholders, never_processed)
                    .unwrap_err()
                    .err,
                TransactionError::SignatureFailure
            );
            let signed = VersionedTransaction::try_new(one, &[&payer]).unwrap();
            assert!(verify_signatures(&signed, never_processed).is_ok());
        }
    }
}
