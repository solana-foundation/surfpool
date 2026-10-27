//! The historical backend: SVM Replay executes the surfnet on historical state, holds every
//! account and decides admission and hash age. `client` is M63's private SVM Replay process
//! client, relocated unchanged; this module is the concrete backend over one branch of it.
//! Nothing local is seeded: the sysvars Surfpool reads itself (clock and epochs, rent) are a
//! view decoded from SVM Replay's own accounts after each attach, advance and reset.

pub(crate) mod client;

use std::sync::Arc;

use itertools::Itertools;
use serde_json::Value;
use solana_account::{Account, AccountSharedData};
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_clock::Clock;
use solana_epoch_schedule::EpochSchedule;
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_sysvar::rent::Rent;
use solana_transaction::versioned::VersionedTransaction;
use wincode::DeserializeOwned;

use self::client::{HistoricalBackend as Session, HistoricalVm};
pub use self::client::{HistoricalLaunch, HistoricalLog};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::{
        AccountSource, GetAccountResult,
        backend::{FailedTransactionMetadata, SimulatedTransactionInfo, TransactionResult},
    },
};

/// The sysvars Surfpool reads itself on a historical fork, decoded from SVM Replay's accounts.
/// Each must exist there; any other sysvar is read from SVM Replay on demand like every account.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StateView {
    pub clock: Clock,
    pub epoch_schedule: EpochSchedule,
    pub rent: Rent,
}

/// A sysvar from SVM Replay's state, decoded as LiteSVM decodes its own; absent or malformed
/// is a typed error, never a default.
fn sysvar<T>(vm: &HistoricalVm) -> SurfpoolResult<T>
where
    T: solana_sysvar::Sysvar + solana_sysvar_id::SysvarId + DeserializeOwned<Dst = T>,
{
    let id = T::id();
    let account = vm.account(&id)?.ok_or_else(|| {
        SurfpoolError::historical(
            "HISTORICAL_SYSVAR_MISSING",
            &format!("the historical state has no {id} sysvar"),
            None,
        )
    })?;
    T::deserialize_from(account.data.as_slice()).map_err(|e| {
        SurfpoolError::historical(
            "SOURCE_INTEGRITY",
            &format!("the historical {id} sysvar does not decode: {e}"),
            None,
        )
    })
}

impl StateView {
    fn read(vm: &HistoricalVm) -> SurfpoolResult<Self> {
        Ok(Self {
            clock: sysvar(vm)?,
            epoch_schedule: sysvar(vm)?,
            rent: sysvar(vm)?,
        })
    }
}

/// One historical fork as a surfnet's execution backend: a branch of SVM Replay's session,
/// the decoded sysvar view and the local faucet. Cloning shares the branch; `clone_for_profiling`
/// forks it.
#[derive(Clone)]
pub struct HistoricalFork {
    vm: HistoricalVm,
    view: StateView,
    /// Funds historical airdrops through a real transfer, as LiteSVM's airdrop account does.
    faucet: Option<(Arc<solana_keypair::Keypair>, VersionedTransaction)>,
    pins: Arc<Pins>,
}

/// Where a branch stands inside SVM Replay, read from its `state` reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) slot: u64,
    pub(crate) block_height: u64,
}

/// The session pins Surfpool's chain policy and version answer use, read once when the backend
/// is ready: a missing or malformed chain pin fails the start. The runtime version is reported
/// only where it is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pins {
    pub(crate) genesis_hash: Hash,
    /// The boundary slot, its parent and its block height.
    pub(crate) slot: u64,
    pub(crate) parent_slot: u64,
    pub(crate) block_height: u64,
    /// How many of the boundary block's transactions precede the fork position.
    pub(crate) transaction_index: u32,
    /// The blockhash the boundary block's transactions executed with.
    pub(crate) execution_blockhash: Hash,
    /// The runtime SVM Replay selected (`runtime.agave`), when the pins name one.
    pub(crate) runtime: Option<String>,
}

fn malformed_native(what: &str) -> SurfpoolError {
    SurfpoolError::historical(
        "HISTORICAL_BACKEND",
        &format!("SVM Replay's {what} is missing or malformed"),
        None,
    )
}

fn native_u64(value: &Value, field: &str, what: &str) -> SurfpoolResult<u64> {
    value[field]
        .as_u64()
        .ok_or_else(|| malformed_native(&format!("{what} {field}")))
}

fn native_hash(value: &Value, field: &str, what: &str) -> SurfpoolResult<Hash> {
    value[field]
        .as_str()
        .and_then(|hash| hash.parse().ok())
        .ok_or_else(|| malformed_native(&format!("{what} {field}")))
}

impl Position {
    fn parse(state: &Value) -> SurfpoolResult<Self> {
        Ok(Self {
            slot: native_u64(state, "slot", "state")?,
            block_height: native_u64(state, "blockHeight", "state")?,
        })
    }
}

impl Pins {
    fn parse(pins: &Value) -> SurfpoolResult<Self> {
        let runtime = match &pins["runtime"]["agave"] {
            Value::Null => None,
            Value::String(version) => Some(version.clone()),
            _ => return Err(malformed_native("pin runtime.agave")),
        };
        Ok(Self {
            genesis_hash: native_hash(pins, "genesisHash", "pin")?,
            slot: native_u64(pins, "slot", "pin")?,
            parent_slot: native_u64(pins, "parentSlot", "pin")?,
            block_height: native_u64(pins, "blockHeight", "pin")?,
            transaction_index: native_u64(pins, "transactionIndex", "pin")?
                .try_into()
                .map_err(|_| malformed_native("pin transactionIndex"))?,
            execution_blockhash: native_hash(pins, "executionBlockhash", "pin")?,
            runtime,
        })
    }
}

impl HistoricalFork {
    /// The fork on the session's main branch, once its sysvars decode.
    pub(crate) fn attach(session: &HistoricalSession) -> SurfpoolResult<Self> {
        let vm = HistoricalVm::main(session.session.clone());
        let view = StateView::read(&vm)?;
        Ok(Self {
            vm,
            view,
            faucet: None,
            pins: session.pins.clone(),
        })
    }

    /// Re-reads the view after the branch's clock or state changed inside SVM Replay.
    pub(crate) fn refresh_view(&mut self) -> SurfpoolResult<()> {
        self.view = StateView::read(&self.vm)?;
        Ok(())
    }

    pub(crate) fn view(&self) -> &StateView {
        &self.view
    }

    /// The session this fork's branch belongs to (pins, journal counts, termination).
    pub(crate) fn session(&self) -> HistoricalSession {
        HistoricalSession {
            session: self.vm.backend().clone(),
            pins: self.pins.clone(),
        }
    }

    /// History before the fork position, for Surfpool's query policies; usable outside the
    /// SVM lock.
    pub(crate) fn archive_reader(&self) -> ArchiveReader {
        ArchiveReader(self.vm.clone())
    }

    /// An isolated copy for profiling, simulation and bundle sandboxes: a forked branch.
    pub(crate) fn clone_for_profiling(&self) -> SurfpoolResult<Self> {
        Ok(Self {
            vm: self.vm.fork()?,
            view: self.view.clone(),
            faucet: self.faucet.clone(),
            pins: self.pins.clone(),
        })
    }

    /// Makes `live` continue as this sandbox: SVM Replay refuses when `live` changed after the
    /// fork (its per-branch parent identity).
    pub(crate) fn commit_into(&self, live: &mut Self) -> SurfpoolResult<()> {
        self.vm.commit_into(&live.vm)?;
        live.view = self.view.clone();
        Ok(())
    }

    /// Back to the boundary state; the boundary has no faucet, the next airdrop funds a new one.
    pub(crate) fn reset(&mut self) -> SurfpoolResult<()> {
        self.vm.reset()?;
        self.faucet = None;
        self.refresh_view()
    }

    /// Executes and commits through SVM Replay: admitted (signatures, blockhash age, durable
    /// nonces, duplicates) when `sig_verify`, else unverified as LiteSVM does for profiling
    /// prefixes on a disposable fork.
    pub(crate) fn send(
        &self,
        tx: VersionedTransaction,
        sig_verify: bool,
    ) -> SurfpoolResult<TransactionResult> {
        match sig_verify {
            true => self.vm.send(&tx),
            false => self.vm.send_unverified(&tx),
        }
    }

    /// Simulates through SVM Replay, which verifies signatures when `sig_verify` and returns the
    /// post-execution images of every account the transaction loads.
    pub(crate) fn simulate(
        &self,
        tx: VersionedTransaction,
        sig_verify: bool,
    ) -> SurfpoolResult<Result<SimulatedTransactionInfo, FailedTransactionMetadata>> {
        let keys = self.transaction_keys(&tx)?;
        self.vm.simulate(&tx, sig_verify, &keys)
    }

    /// The transaction's static keys followed by the addresses its lookup tables load, as the
    /// historical branch holds those tables.
    fn transaction_keys(&self, tx: &VersionedTransaction) -> SurfpoolResult<Vec<Pubkey>> {
        let vm = &self.vm;
        let mut keys = tx.message.static_account_keys().to_vec();
        let Some(lookups) = tx.message.address_table_lookups() else {
            return Ok(keys);
        };
        let (mut writable, mut readonly) = (Vec::new(), Vec::new());
        for lookup in lookups {
            let table = vm.account(&lookup.account_key)?.ok_or_else(|| {
                SurfpoolError::historical(
                    "ADDRESS_LOOKUP_TABLE_NOT_FOUND",
                    &format!("lookup table {} does not exist", lookup.account_key),
                    None,
                )
            })?;
            let table = AddressLookupTable::deserialize(&table.data).map_err(|e| {
                SurfpoolError::historical(
                    "INVALID_ADDRESS_LOOKUP_TABLE",
                    &format!("lookup table {}: {e}", lookup.account_key),
                    None,
                )
            })?;
            let address = |index: &u8| {
                table
                    .addresses
                    .get(usize::from(*index))
                    .copied()
                    .ok_or_else(|| {
                        SurfpoolError::historical(
                            "INVALID_ADDRESS_LOOKUP_TABLE_INDEX",
                            &format!("lookup table {} has no index {index}", lookup.account_key),
                            None,
                        )
                    })
            };
            for index in &lookup.writable_indexes {
                writable.push(address(index)?);
            }
            for index in &lookup.readonly_indexes {
                readonly.push(address(index)?);
            }
        }
        keys.extend(writable);
        keys.extend(readonly);
        Ok(keys)
    }

    /// The fee for a message under the historical runtime's rules: none for a blockhash the
    /// fork no longer accepts.
    pub(crate) fn fee_for_message(&self, message: &[u8]) -> SurfpoolResult<Option<u64>> {
        self.vm.fee_for_message(message)
    }

    /// Whether SVM Replay accepts `blockhash` for processing now (its hash-age queue).
    pub(crate) fn blockhash_valid(&self, blockhash: &Hash) -> SurfpoolResult<bool> {
        self.vm.blockhash_valid(blockhash)
    }

    /// Any sysvar from SVM Replay's state, decoded; absent or malformed is a typed error.
    pub(crate) fn read_sysvar<T>(&self) -> SurfpoolResult<T>
    where
        T: solana_sysvar::Sysvar + solana_sysvar_id::SysvarId + DeserializeOwned<Dst = T>,
    {
        sysvar(&self.vm)
    }

    /// Where the branch stands: its slot and block height.
    pub(crate) fn state(&self) -> SurfpoolResult<Position> {
        Position::parse(&self.vm.state()?)
    }

    /// The branch's latest blockhash and its last valid block height.
    pub(crate) fn latest_blockhash(&self) -> SurfpoolResult<(Hash, u64)> {
        self.vm.latest_blockhash()
    }

    /// Local slots inside SVM Replay; the view follows the new clock. Returns where the branch
    /// then stands.
    pub(crate) fn advance(
        &mut self,
        slots: u64,
        unix_timestamp: Option<i64>,
        synthetic_epoch: bool,
    ) -> SurfpoolResult<Position> {
        self.vm.advance(slots, unix_timestamp, synthetic_epoch)?;
        self.refresh_view()?;
        self.state()
    }

    pub(crate) fn airdrop_pubkey(&self) -> SurfpoolResult<Pubkey> {
        self.faucet
            .as_ref()
            .map(|(faucet, _)| faucet.pubkey())
            .ok_or_else(|| SurfpoolError::internal("historical faucet missing"))
    }

    /// A fresh faucet signs each airdrop, including repeated requests in a frozen slot.
    /// Fund exactly the transfer plus the engine's fee: no unused faucet balance remains.
    pub(crate) fn prepare_airdrop(&mut self, pubkey: &Pubkey, lamports: u64) -> SurfpoolResult<()> {
        self.faucet = None;
        let faucet = Arc::new(solana_keypair::Keypair::new());
        let (blockhash, _) = self.vm.latest_blockhash()?;
        let message = solana_message::Message::new_with_blockhash(
            &[solana_system_interface::instruction::transfer(
                &faucet.pubkey(),
                pubkey,
                lamports,
            )],
            Some(&faucet.pubkey()),
            &blockhash,
        );
        let tx = VersionedTransaction::try_new(
            solana_message::VersionedMessage::Legacy(message),
            &[faucet.as_ref()],
        )
        .map_err(|e| SurfpoolError::internal(format!("sign historical airdrop: {e}")))?;
        let fee = self
            .vm
            .fee_for_message(&tx.message.serialize())?
            .ok_or_else(|| {
                SurfpoolError::historical(
                    "BLOCKHASH_NOT_FOUND",
                    "historical airdrop blockhash has no fee",
                    None,
                )
            })?;
        let balance = lamports.checked_add(fee).ok_or_else(|| {
            SurfpoolError::invalid_params("airdrop amount plus fee overflows lamports")
        })?;
        self.vm.set_balance(&faucet.pubkey(), balance)?;
        self.faucet = Some((faucet, tx));
        Ok(())
    }

    /// Executes the prepared transfer with normal signature, lifetime and duplicate checks.
    pub(crate) fn airdrop(
        &mut self,
        pubkey: &Pubkey,
        lamports: u64,
    ) -> SurfpoolResult<TransactionResult> {
        if self.faucet.is_none() {
            self.prepare_airdrop(pubkey, lamports)?;
        }
        let (_, tx) = self
            .faucet
            .take()
            .ok_or_else(|| SurfpoolError::internal("historical faucet missing"))?;
        self.vm.send(&tx)
    }

    /// The account as SVM Replay holds it at this branch, acquiring it when needed; `None` is
    /// a proven absence.
    pub(crate) fn get_account(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Account>> {
        self.vm.account(pubkey)
    }

    pub(crate) fn get_account_result(&self, pubkey: &Pubkey) -> SurfpoolResult<GetAccountResult> {
        Ok(match self.vm.account(pubkey)? {
            Some(account) => GetAccountResult::FoundAccount(*pubkey, account, AccountSource::Svm),
            None => GetAccountResult::None(*pubkey),
        })
    }

    /// What the branch already holds for `pubkey`, acquiring nothing.
    pub(crate) fn peek(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Option<Account>>> {
        self.vm.peek(pubkey)
    }

    pub(crate) fn set_account(&self, pubkey: &Pubkey, account: &Account) -> SurfpoolResult<()> {
        self.vm.set_account(pubkey, account)
    }

    /// Replaces accounts atomically: SVM Replay validates them together and applies all or none.
    pub(crate) fn set_accounts(&self, accounts: &[(Pubkey, Account)]) -> SurfpoolResult<()> {
        self.vm.set_accounts(accounts)
    }

    /// Historically a deletion is an explicit absence on the branch.
    pub(crate) fn delete_account(&self, pubkey: &Pubkey) -> SurfpoolResult<()> {
        self.vm.set_account(pubkey, &Account::default())
    }

    /// Puts accounts back to their boundary images, discarding local edits on this branch.
    pub(crate) fn restore(&self, pubkeys: &[Pubkey]) -> SurfpoolResult<()> {
        self.vm.restore(pubkeys)
    }

    /// The accounts this branch loaded (SVM Replay's inventory), sorted; not every account on
    /// the chain.
    pub(crate) fn get_all_accounts(&self) -> SurfpoolResult<Vec<(Pubkey, AccountSharedData)>> {
        Ok(self
            .vm
            .known_accounts()?
            .into_iter()
            .filter_map(|(pubkey, account)| Some((pubkey, AccountSharedData::from(account?))))
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect())
    }
}

/// A historical fork's session handle for Surfpool's lifecycle and journal coordination.
#[derive(Clone)]
pub(crate) struct HistoricalSession {
    session: Arc<Session>,
    pins: Arc<Pins>,
}

impl HistoricalSession {
    /// The fork's pins Surfpool reads: boundary, fork position, network and runtime.
    pub(crate) fn pins(&self) -> &Pins {
        &self.pins
    }

    /// How many operations SVM Replay's journal holds; none without a journal.
    pub(crate) fn journaled(&self) -> Option<u64> {
        self.session.journaled()
    }

    /// The newest store revision among the journal's records when the session started.
    pub(crate) fn journal_revision(&self) -> Option<u64> {
        self.session.journal_revision()
    }

    /// Sends `revision` with each following operation (none: outside the store's writes).
    pub(crate) fn set_revision(&self, revision: Option<std::num::NonZeroU64>) {
        self.session.set_revision(revision)
    }

    /// Whether the backend still serves: a backend that exited or failed is a typed error.
    pub(crate) fn check(&self) -> SurfpoolResult<()> {
        self.session.check()
    }

    /// Ends the backend now, even while a request waits on it.
    pub(crate) fn terminate(&self) {
        self.session.terminate()
    }
}

/// Starts SVM Replay's backend and waits until it is ready or `cancelled` turns true.
pub(crate) fn launch(
    launch: &HistoricalLaunch,
    log: HistoricalLog,
    cancelled: impl FnMut() -> bool,
) -> SurfpoolResult<HistoricalSession> {
    let session = client::HistoricalBackend::launch(launch, log, cancelled)?;
    // A backend whose pins Surfpool cannot read is not started: dropping it ends it.
    let pins = Arc::new(Pins::parse(session.pins())?);
    Ok(HistoricalSession { session, pins })
}

impl HistoricalSession {
    /// The fork on this session's main branch.
    pub(crate) fn attach(&self) -> SurfpoolResult<HistoricalFork> {
        HistoricalFork::attach(self)
    }
}

/// Ends every running historical backend of this process.
pub(crate) fn terminate_all() {
    client::terminate_all()
}

/// History before a historical fork's position as SVM Replay reads it (finalized, the same for
/// every branch): the typed queries Surfpool's archive and local-history policies make. Each
/// query is built with M63's exact keys and key order (serde_json preserves insertion order here),
/// so the private wire is unchanged; see the golden tests below.
#[derive(Clone)]
pub struct ArchiveReader(HistoricalVm);

mod query {
    use serde::Serialize;
    use serde_json::{Value, json};
    use solana_pubkey::Pubkey;

    pub fn signatures_before(address: &Pubkey, before: Option<&String>, limit: usize) -> Value {
        json!({"kind":"signatures","address":address.to_string(),"before":before,"limit":limit})
    }

    pub fn signatures(
        address: &Pubkey,
        before: Option<&String>,
        until: Option<&String>,
        limit: Option<usize>,
    ) -> Value {
        json!({"kind":"signatures","address":address.to_string(),
            "before":before,"until":until,"limit":limit})
    }

    pub fn transaction(signature: &str) -> Value {
        json!({"kind":"transaction","signature":signature})
    }

    pub fn transaction_with_history(signature: &str, history: bool) -> Value {
        json!({"kind":"transaction","signature":signature,"history":history})
    }

    pub fn block(slot: u64) -> Value {
        json!({"kind":"block","slot":slot})
    }

    pub fn blocks(start: u64, end: u64) -> Value {
        json!({"kind":"blocks","start":start,"end":end})
    }

    pub fn boundary_block() -> Value {
        json!({"kind":"boundaryBlock"})
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transactions_for_address(
        address: &Pubkey,
        sort_order: impl Serialize,
        token_accounts: impl Serialize,
        status: impl Serialize,
        limit: usize,
        pagination_token: Option<String>,
        slot: Value,
        block_time: Value,
    ) -> Value {
        json!({"kind":"transactionsForAddress","address":address.to_string(),
            "sortOrder":sort_order,"tokenAccounts":token_accounts,
            "status":status,"limit":limit,
            "paginationToken":pagination_token,"slot":slot,
            "blockTime":block_time})
    }

    pub fn token_accounts_by_owner(
        owner: &Pubkey,
        mint: Option<String>,
        program_id: Option<String>,
    ) -> Value {
        json!({"kind":"tokenAccountsByOwner","owner":owner.to_string(),
            "mint":mint,"programId":program_id})
    }

    pub fn token_holders(mint: &Pubkey, limit: u64) -> Value {
        json!({"kind":"tokenHolders","mint":mint.to_string(),"limit":limit})
    }
}

impl ArchiveReader {
    pub(crate) fn signatures_before(
        &self,
        address: &Pubkey,
        before: Option<&String>,
        limit: usize,
    ) -> SurfpoolResult<Value> {
        self.0
            .archive(query::signatures_before(address, before, limit))
    }

    pub(crate) fn signatures(
        &self,
        address: &Pubkey,
        before: Option<&String>,
        until: Option<&String>,
        limit: Option<usize>,
    ) -> SurfpoolResult<Value> {
        self.0
            .archive(query::signatures(address, before, until, limit))
    }

    /// An archived transaction record, `null` when unknown.
    pub(crate) fn transaction(&self, signature: &str) -> SurfpoolResult<Value> {
        self.0.archive(query::transaction(signature))
    }

    /// An archived transaction record; `history` reaches before the boundary block.
    pub(crate) fn transaction_with_history(
        &self,
        signature: &str,
        history: bool,
    ) -> SurfpoolResult<Value> {
        self.0
            .archive(query::transaction_with_history(signature, history))
    }

    pub(crate) fn block(&self, slot: u64) -> SurfpoolResult<Value> {
        self.0.archive(query::block(slot))
    }

    pub(crate) fn blocks(&self, start: u64, end: u64) -> SurfpoolResult<Value> {
        self.0.archive(query::blocks(start, end))
    }

    /// The boundary block's archived transactions before the fork position.
    pub(crate) fn boundary_block(&self) -> SurfpoolResult<Value> {
        self.0.archive(query::boundary_block())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn transactions_for_address(
        &self,
        address: &Pubkey,
        sort_order: impl serde::Serialize,
        token_accounts: impl serde::Serialize,
        status: impl serde::Serialize,
        limit: usize,
        pagination_token: Option<String>,
        slot: Value,
        block_time: Value,
    ) -> SurfpoolResult<Value> {
        self.0.archive(query::transactions_for_address(
            address,
            sort_order,
            token_accounts,
            status,
            limit,
            pagination_token,
            slot,
            block_time,
        ))
    }

    pub(crate) fn token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: Option<String>,
        program_id: Option<String>,
    ) -> SurfpoolResult<Value> {
        self.0
            .archive(query::token_accounts_by_owner(owner, mint, program_id))
    }

    pub(crate) fn token_holders(&self, mint: &Pubkey, limit: u64) -> SurfpoolResult<Value> {
        self.0.archive(query::token_holders(mint, limit))
    }

    /// Account images at the fork's branch, for policies that read outside the SVM lock.
    pub(crate) fn accounts(&self, pubkeys: &[Pubkey]) -> SurfpoolResult<Vec<Option<Account>>> {
        self.0.accounts(pubkeys)
    }

    pub(crate) fn peek_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> SurfpoolResult<Vec<Option<Option<Account>>>> {
        self.0.peek_accounts(pubkeys)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use solana_pubkey::Pubkey;

    use super::{Pins, Position, query};

    #[cfg(unix)]
    mod security_admission {
        use super::*;

        // A deterministic IPC peer exercises Surfpool's real admission paths without native
        // execution or provider traffic. Every request is retained to prove refusal ordering.
        fn ipc_fixture(count: usize) -> (super::super::HistoricalFork, tempfile::TempDir) {
            use std::sync::Arc;

            use super::super::{HistoricalFork, HistoricalLaunch, HistoricalVm, StateView, client};
            let dir = tempfile::tempdir().unwrap();
            let keys: Vec<String> = (0..count)
                .map(|_| Pubkey::new_unique().to_string())
                .collect();
            let script = r#"
import json, sys
keys = json.loads(sys.argv[1])
log = sys.argv[2]
installs = 0
print(json.dumps({'schema':'svm-historical-backend/v1','kind':'ready','pins':{}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    op = req['op']
    if op == 'shutdown':
        print(json.dumps({'id':req['id'],'result':None}), flush=True)
        break
    with open(log, 'a') as out: out.write(json.dumps(req) + '\n')
    if isinstance(op, dict) and 'archive' in op:
        kind = op['archive']['kind']
        if kind == 'tokenAccountsByOwner': result = {'accounts':keys}
        elif kind == 'tokenHolders': result = {'holders':[], 'touched':keys, 'mintAtParent':False, 'complete':True}
        else: result = None
    elif isinstance(op, dict) and 'peek' in op:
        result = [{'loaded':False} for _ in op['peek']['pubkeys']]
    elif isinstance(op, dict) and op.get('command', {}).get('method') == 'accounts':
        result = [None for _ in op['command']['params']['pubkeys']]
    elif isinstance(op, dict) and op.get('command', {}).get('method') == 'latestBlockhash':
        result = {'blockhash':'11111111111111111111111111111111', 'lastValidBlockHeight':150}
    elif isinstance(op, dict) and op.get('command', {}).get('method') == 'feeForMessage':
        result = 5000
    elif isinstance(op, dict) and op.get('command', {}).get('method') == 'setBalance':
        result = None
    elif isinstance(op, dict) and op.get('command', {}).get('method') in ('send', 'rpcSend', 'overrideAccounts'):
        if op['command']['method'] == 'overrideAccounts':
            installs += 1
            if len(keys) == 1 and installs > 1:
                print(json.dumps({'id':req['id'],'result':None}), flush=True)
                continue
        print(json.dumps({'id':req['id'],'error':{'code':'TEST_RUNTIME_REFUSAL','message':'recorded attempt refused'}}), flush=True)
        continue
    else: raise RuntimeError('unexpected request: ' + repr(req))
    print(json.dumps({'id':req['id'],'result':result}), flush=True)
"#;
            let launch = HistoricalLaunch {
                program: "/usr/bin/python3".into(),
                args: vec![
                    "-u".into(),
                    "-c".into(),
                    script.into(),
                    serde_json::to_string(&keys).unwrap().into(),
                    dir.path().join("requests.jsonl").into(),
                ],
                envs: vec![],
            };
            let backend = client::HistoricalBackend::launch(
                &launch,
                Arc::new(|line| eprintln!("{line}")),
                || false,
            )
            .unwrap();
            let fork = HistoricalFork {
                vm: HistoricalVm::main(backend),
                view: StateView {
                    clock: Default::default(),
                    epoch_schedule: Default::default(),
                    rent: Default::default(),
                },
                faucet: None,
                pins: Arc::new(Pins {
                    genesis_hash: Default::default(),
                    slot: 123,
                    parent_slot: 122,
                    block_height: 42,
                    transaction_index: 0,
                    execution_blockhash: Default::default(),
                    runtime: None,
                }),
            };
            (fork, dir)
        }

        fn ipc_requests(dir: &tempfile::TempDir) -> Vec<serde_json::Value> {
            match std::fs::read_to_string(dir.path().join("requests.jsonl")) {
                Ok(lines) => lines
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
                Err(error) => panic!("cannot read IPC transcript: {error}"),
            }
        }

        #[test]
        fn review_live_signature_skip_refuses_before_execution_ipc() {
            use crate::surfnet::backend::{Admission, Backend};
            let (fork, dir) = ipc_fixture(0);
            let mut backend = Backend::historical(fork);
            let error = backend
                .send_transaction(
                    solana_transaction::versioned::VersionedTransaction::default(),
                    &Admission {
                        sigverify: None,
                        skip_blockhash_check: false,
                    },
                )
                .unwrap_err();
            assert_eq!(
                error.historical_code(),
                Some("UNSUPPORTED_IN_HISTORICAL_MODE")
            );
            assert!(
                ipc_requests(&dir).is_empty(),
                "live signature skipping must not become an unverified send"
            );
            let error = backend
                .send_profile_prefix(
                    solana_transaction::versioned::VersionedTransaction::default(),
                    false,
                )
                .unwrap_err();
            assert_eq!(error.historical_code(), Some("TEST_RUNTIME_REFUSAL"));
            assert_eq!(
                ipc_requests(&dir)[0]["op"]["command"]["method"],
                "send",
                "instruction-prefix profiling still uses the disposable executor"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn review_historical_genesis_override_refused_before_runloop_start() {
            use crate::{rpc::full::SurfpoolFullRpc, tests::helpers::TestSetup};
            let setup = TestSetup::new(SurfpoolFullRpc);
            let (fork, dir) = ipc_fixture(0);
            setup.context.svm_locker.with_svm_writer(|svm| {
                svm.inner = crate::surfnet::backend::Backend::historical(fork)
            });
            let (commands, receiver) = crossbeam_channel::unbounded();
            commands
                .send(surfpool_types::SimnetCommand::Terminate(None))
                .unwrap();
            let (_geyser, geyser_rx) = crossbeam_channel::unbounded();
            let result = crate::runloops::start_local_surfnet_runloop(
                setup.context.svm_locker,
                surfpool_types::SurfpoolConfig {
                    simnets: vec![surfpool_types::SimnetConfig {
                        genesis_hash: Some(solana_hash::Hash::new_unique()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                commands,
                receiver,
                geyser_rx,
            )
            .await;
            let error = result.expect_err("a historical genesis hash is source-pinned");
            assert!(
                error.to_string().contains("UNSUPPORTED_IN_HISTORICAL_MODE"),
                "{error}"
            );
            assert!(
                ipc_requests(&dir).is_empty(),
                "invalid configuration must not reach the backend"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn review_upload_holes_wait_and_rejected_upload_keeps_its_bytes() {
            use crate::{rpc::full::SurfpoolFullRpc, tests::helpers::TestSetup};
            let setup = TestSetup::new(SurfpoolFullRpc);
            let (fork, dir) = ipc_fixture(0);
            setup.context.svm_locker.with_svm_writer(|svm| {
                svm.inner = crate::surfnet::backend::Backend::historical(fork)
            });
            let locker = &setup.context.svm_locker;
            let program = Pubkey::new_unique();
            let elf = crate::surfnet::noop_program::NOOP_PROGRAM_ELF;
            locker
                .write_program(program, None, 0, &elf[..64], &None)
                .await
                .unwrap();
            locker
                .write_program(program, None, elf.len() - 1, &elf[elf.len() - 1..], &None)
                .await
                .expect("the unreceived middle must not be installed");
            assert!(
                !ipc_requests(&dir)
                    .iter()
                    .any(|r| r["op"]["command"]["method"] == "overrideAccounts")
            );
            let error = locker
                .write_program(program, None, 64, &elf[64..elf.len() - 1], &None)
                .await
                .unwrap_err();
            assert_eq!(error.historical_code(), Some("TEST_RUNTIME_REFUSAL"));
            assert!(
                locker.with_svm_reader(|svm| svm.historical_uploads.contains_key(&program)),
                "keep staged bytes after refusal"
            );
            let retry = locker
                .write_program(program, None, 64, &elf[64..65], &None)
                .await
                .unwrap_err();
            assert_eq!(retry.historical_code(), Some("TEST_RUNTIME_REFUSAL"));
            assert_eq!(
                ipc_requests(&dir)
                    .iter()
                    .filter(|r| r["op"]["command"]["method"] == "overrideAccounts")
                    .count(),
                2,
                "retry a corrected chunk without reuploading the rest"
            );
        }

        #[test]
        fn review_airdrop_preparation_replaces_an_interrupted_request() {
            let (mut fork, dir) = ipc_fixture(0);
            let previous = Pubkey::new_unique();
            let recipient = Pubkey::new_unique();
            fork.prepare_airdrop(&previous, 1_000_000).unwrap();
            let abandoned_faucet = fork.airdrop_pubkey().unwrap();
            // Pre-airdrop account capture can fail before the prepared transfer is sent.
            fork.prepare_airdrop(&recipient, 2_000_000).unwrap();
            assert_ne!(fork.airdrop_pubkey().unwrap(), abandoned_faucet);
            let (_, tx) = fork.faucet.as_ref().unwrap();
            assert!(tx.message.static_account_keys().contains(&recipient));
            assert!(!tx.message.static_account_keys().contains(&previous));
            assert_eq!(
                fork.airdrop(&recipient, 2_000_000)
                    .unwrap_err()
                    .historical_code(),
                Some("TEST_RUNTIME_REFUSAL")
            );
            assert_eq!(
                ipc_requests(&dir)
                    .iter()
                    .filter(|r| r["op"]["command"]["method"] == "setBalance")
                    .count(),
                2,
                "send must consume the latest preparation, not create a third"
            );
            assert!(fork.faucet.is_none());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn review_upload_is_removed_only_after_successful_install() {
            use crate::{rpc::full::SurfpoolFullRpc, tests::helpers::TestSetup};
            let setup = TestSetup::new(SurfpoolFullRpc);
            let (fork, dir) = ipc_fixture(1);
            setup.context.svm_locker.with_svm_writer(|svm| {
                svm.inner = crate::surfnet::backend::Backend::historical(fork)
            });
            let locker = &setup.context.svm_locker;
            let program = Pubkey::new_unique();
            let elf = crate::surfnet::noop_program::NOOP_PROGRAM_ELF;
            assert_eq!(
                locker
                    .write_program(program, None, 0, elf, &None)
                    .await
                    .unwrap_err()
                    .historical_code(),
                Some("TEST_RUNTIME_REFUSAL")
            );
            locker
                .write_program(program, None, 0, &elf[..1], &None)
                .await
                .unwrap();
            assert!(locker.with_svm_reader(|svm| svm.historical_uploads.is_empty()));
            assert_eq!(
                ipc_requests(&dir)
                    .iter()
                    .filter(|r| r["op"]["command"]["method"] == "overrideAccounts")
                    .count(),
                2
            );
        }

        #[test]
        fn security_historical_owner_candidates_refuse_101_before_accounts_ipc() {
            use solana_client::{
                rpc_config::RpcAccountInfoConfig, rpc_request::TokenAccountsFilter,
            };

            use crate::{rpc::full::SurfpoolFullRpc, tests::helpers::TestSetup};
            let setup = TestSetup::new(SurfpoolFullRpc);
            for count in [100, 101] {
                let (fork, dir) = ipc_fixture(count);
                let result = setup.context.svm_locker.historical_token_accounts_by_owner(
                    &fork.archive_reader(),
                    Pubkey::new_unique(),
                    &TokenAccountsFilter::ProgramId(spl_token_interface::id()),
                    &RpcAccountInfoConfig::default(),
                );
                if count == 100 {
                    result.unwrap();
                    assert_eq!(
                        ipc_requests(&dir)
                            .iter()
                            .filter(|r| r["op"]["command"]["method"] == "accounts")
                            .count(),
                        100
                    );
                } else {
                    let error = result.err().expect("101 candidates must be refused");
                    assert_eq!(error.historical_code(), Some("HISTORICAL_DATA_UNAVAILABLE"));
                    assert!(
                        !ipc_requests(&dir)
                            .iter()
                            .any(|r| r["op"]["command"]["method"] == "accounts")
                    );
                }
            }
        }

        #[test]
        fn security_historical_holders_refuse_101_before_accounts_ipc() {
            use crate::{rpc::full::SurfpoolFullRpc, tests::helpers::TestSetup};
            let setup = TestSetup::new(SurfpoolFullRpc);
            for count in [100, 101] {
                let (fork, dir) = ipc_fixture(count);
                let result = setup.context.svm_locker.historical_token_largest_accounts(
                    &fork.archive_reader(),
                    &Pubkey::new_unique(),
                );
                if count == 100 {
                    result.unwrap();
                    assert_eq!(
                        ipc_requests(&dir)
                            .iter()
                            .filter(|r| r["op"]["command"]["method"] == "accounts")
                            .count(),
                        101,
                        "100 holders plus one mint"
                    );
                } else {
                    let error = result.err().expect("101 unloaded holders must be refused");
                    assert_eq!(error.historical_code(), Some("HISTORICAL_DATA_UNAVAILABLE"));
                    assert!(
                        !ipc_requests(&dir)
                            .iter()
                            .any(|r| r["op"]["command"]["method"] == "accounts")
                    );
                }
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn security_historical_statuses_refuse_257_before_archive_ipc() {
            use crate::{
                rpc::full::{Full, SurfpoolFullRpc},
                tests::helpers::TestSetup,
            };
            let setup = TestSetup::new(SurfpoolFullRpc);
            let (fork, dir) = ipc_fixture(0);
            setup.context.svm_locker.with_svm_writer(|svm| {
                svm.inner = crate::surfnet::backend::Backend::historical(fork)
            });
            for count in [256, 257] {
                let result = setup
                    .rpc
                    .get_signature_statuses(
                        Some(setup.context.clone()),
                        vec![solana_signature::Signature::new_unique().to_string(); count],
                        Some(solana_client::rpc_config::RpcSignatureStatusConfig {
                            search_transaction_history: true,
                        }),
                    )
                    .await;
                if count == 256 {
                    assert_eq!(result.unwrap().value.len(), 256);
                    assert_eq!(ipc_requests(&dir).len(), 256);
                } else {
                    let error = result.expect_err("257 signatures must be refused");
                    assert_eq!(error.code, jsonrpc_core::ErrorCode::InvalidParams);
                    assert!(error.message.contains("256"), "{error}");
                    assert_eq!(
                        ipc_requests(&dir).len(),
                        256,
                        "refusal must not query archive"
                    );
                }
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn security_program_upload_refused_before_historical_acquisition() {
            use crate::{rpc::full::SurfpoolFullRpc, tests::helpers::TestSetup};
            let setup = TestSetup::new(SurfpoolFullRpc);
            let (fork, dir) = ipc_fixture(0);
            setup.context.svm_locker.with_svm_writer(|svm| {
                svm.inner = crate::surfnet::backend::Backend::historical(fork)
            });
            let error = setup
                .context
                .svm_locker
                .write_program(
                    Pubkey::new_unique(),
                    None,
                    solana_system_interface::MAX_PERMITTED_DATA_LENGTH as usize,
                    &[1],
                    &None,
                )
                .await
                .expect_err("oversized upload must fail");
            assert!(error.to_string().contains("program write"), "{error}");
            assert!(
                ipc_requests(&dir).is_empty(),
                "oversized upload must not acquire accounts"
            );
            assert!(
                setup
                    .context
                    .svm_locker
                    .with_svm_reader(|svm| svm.historical_uploads.is_empty())
            );
        }
    }

    /// SVM Replay's position and pins reach Surfpool as typed facts: a missing or malformed chain
    /// pin or position field is a typed backend error, never a default.
    #[test]
    fn native_position_and_pins_are_typed_facts() {
        let pins = json!({"genesisHash": "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d",
            "slot": 9, "parentSlot": 8, "blockHeight": 7, "transactionIndex": 5,
            "executionBlockhash": "CxvhuTN6ym1wmcchbrqySxr7Qpfs5Zj9qLi7zB18cyEz",
            "runtime": {"agave": "3.0.1"}, "workers": ["kept by SVM Replay"]});
        let parsed = Pins::parse(&pins).unwrap();
        assert_eq!(
            (
                parsed.slot,
                parsed.parent_slot,
                parsed.block_height,
                parsed.transaction_index
            ),
            (9, 8, 7, 5)
        );
        assert_eq!(parsed.runtime.as_deref(), Some("3.0.1"));
        for field in [
            "genesisHash",
            "slot",
            "parentSlot",
            "blockHeight",
            "transactionIndex",
            "executionBlockhash",
        ] {
            for bad in [None, Some(json!("x")), Some(json!(-1))] {
                let mut broken = pins.clone();
                match &bad {
                    None => drop(broken.as_object_mut().unwrap().remove(field)),
                    Some(value) => broken[field] = value.clone(),
                }
                let error = Pins::parse(&broken).err().unwrap().to_string();
                assert!(
                    error.contains("HISTORICAL_BACKEND") && error.contains(field),
                    "{field} {bad:?}: {error}"
                );
            }
        }
        let mut beyond = pins.clone();
        beyond["transactionIndex"] = json!(u64::from(u32::MAX) + 1);
        assert!(Pins::parse(&beyond).is_err());
        let mut unnamed = pins.clone();
        unnamed.as_object_mut().unwrap().remove("runtime");
        assert_eq!(Pins::parse(&unnamed).unwrap().runtime, None);
        let mut garbled = pins.clone();
        garbled["runtime"]["agave"] = json!(3);
        assert!(Pins::parse(&garbled).is_err());

        assert_eq!(
            Position::parse(&json!({"slot": 3, "blockHeight": 2, "epoch": 1})).unwrap(),
            Position {
                slot: 3,
                block_height: 2
            }
        );
        for state in [
            json!({"blockHeight": 2}),
            json!({"slot": "3", "blockHeight": 2}),
            json!({"slot": 3}),
        ] {
            let error = Position::parse(&state).err().unwrap().to_string();
            assert!(error.contains("HISTORICAL_BACKEND"), "{state}: {error}");
        }
    }

    /// The archive queries keep M63's exact wire text (keys and their order). The expected text
    /// is read through the same JSON map type: with serde_json's `preserve_order` (as the CLI
    /// builds it) the key order is checked too; without it both sides print sorted keys.
    #[test]
    fn archive_queries_keep_m63_wire_text() {
        let address = Pubkey::new_from_array([7; 32]);
        let a = address.to_string();
        let cursor = "5x".to_string();
        let cases = [
            (
                query::signatures_before(&address, Some(&cursor), 10),
                format!(r#"{{"kind":"signatures","address":"{a}","before":"5x","limit":10}}"#),
            ),
            (
                query::signatures(&address, None, Some(&cursor), Some(3)),
                format!(
                    r#"{{"kind":"signatures","address":"{a}","before":null,"until":"5x","limit":3}}"#
                ),
            ),
            (
                query::transaction("sig"),
                r#"{"kind":"transaction","signature":"sig"}"#.to_string(),
            ),
            (
                query::transaction_with_history("sig", true),
                r#"{"kind":"transaction","signature":"sig","history":true}"#.to_string(),
            ),
            (query::block(9), r#"{"kind":"block","slot":9}"#.to_string()),
            (
                query::blocks(1, 2),
                r#"{"kind":"blocks","start":1,"end":2}"#.to_string(),
            ),
            (
                query::boundary_block(),
                r#"{"kind":"boundaryBlock"}"#.to_string(),
            ),
            (
                query::transactions_for_address(
                    &address,
                    "asc",
                    "none",
                    "any",
                    5,
                    None,
                    json!({}),
                    json!(null),
                ),
                format!(
                    r#"{{"kind":"transactionsForAddress","address":"{a}","sortOrder":"asc","tokenAccounts":"none","status":"any","limit":5,"paginationToken":null,"slot":{{}},"blockTime":null}}"#
                ),
            ),
            (
                query::token_accounts_by_owner(&address, Some("m".into()), None),
                format!(
                    r#"{{"kind":"tokenAccountsByOwner","owner":"{a}","mint":"m","programId":null}}"#
                ),
            ),
            (
                query::token_holders(&address, 1000),
                format!(r#"{{"kind":"tokenHolders","mint":"{a}","limit":1000}}"#),
            ),
        ];
        for (built, wire) in cases {
            let expected: serde_json::Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(
                serde_json::to_string(&built).unwrap(),
                serde_json::to_string(&expected).unwrap()
            );
        }
    }
}
