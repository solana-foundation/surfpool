use solana_pubkey::Pubkey;
use surfpool_types::{EpochStakeDelegation, EpochStakeEntry};

use crate::cheatcodes::builders::CheatcodeBuilder;

/// A stake account that [`SetEpochStakes`] writes as fully active and
/// delegated to its entry's vote account.
#[derive(Debug, Clone)]
pub struct StakeDelegation {
    stake_account: Pubkey,
    stake: u64,
    staker: Option<Pubkey>,
    withdrawer: Option<Pubkey>,
}

impl StakeDelegation {
    /// Delegate `stake` lamports from a stake account written at `stake_account`.
    ///
    /// The account also receives its rent-exempt reserve.
    pub fn new(stake_account: Pubkey, stake: u64) -> Self {
        Self {
            stake_account,
            stake,
            staker: None,
            withdrawer: None,
        }
    }

    /// Stake authority; defaults to the stake account's own address.
    pub fn staker(mut self, staker: Pubkey) -> Self {
        self.staker = Some(staker);
        self
    }

    /// Withdraw authority; defaults to the staker.
    pub fn withdrawer(mut self, withdrawer: Pubkey) -> Self {
        self.withdrawer = Some(withdrawer);
        self
    }
}

impl From<StakeDelegation> for EpochStakeDelegation {
    fn from(delegation: StakeDelegation) -> Self {
        EpochStakeDelegation {
            stake_account: delegation.stake_account.to_string(),
            stake: delegation.stake,
            staker: delegation.staker.map(|p| p.to_string()),
            withdrawer: delegation.withdrawer.map(|p| p.to_string()),
        }
    }
}

/// One vote account's entry in a [`SetEpochStakes`] request.
///
/// By itself an entry only sets the stake read by the `sol_get_epoch_stake`
/// syscall. Calling [`EpochStake::node_pubkey`] additionally writes a
/// rent-exempt vote account at the address, with one vote at the current
/// slot, which `getVoteAccounts` and on-chain programs can then read.
/// [`EpochStake::delegation`] writes fully active stake accounts delegated
/// to it.
///
/// While the surfnet's `alpenglow` feature is active, staked vote accounts
/// vote on every block: their last vote, root and timestamp follow the chain
/// tip, and their epoch credits grow by their share of the inflation reward
/// once a total supply is set. Without Alpenglow they keep the state they
/// were written with.
///
/// Read the result back with the [`RpcClient`](crate::RpcClient) from
/// [`Surfnet::rpc_client`](crate::Surfnet::rpc_client): `get_vote_accounts`
/// lists the stakes and `get_program_accounts` on the stake program lists
/// the delegations.
///
/// ```rust
/// use surfpool_sdk::{Pubkey, Surfnet};
/// use surfpool_sdk::cheatcodes::builders::{EpochStake, SetEpochStakes};
///
/// # async fn example() {
/// let surfnet = Surfnet::start().await.unwrap();
/// let cheats = surfnet.cheatcodes();
/// let vote_account = Pubkey::new_unique();
/// let identity = Pubkey::new_unique();
///
/// cheats
///     .execute(SetEpochStakes::new().entry(
///         EpochStake::new(vote_account, 1_000_000_000)
///             .node_pubkey(identity)
///             .commission(5),
///     ))
///     .unwrap();
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct EpochStake {
    vote_account: Pubkey,
    stake: Option<u64>,
    node_pubkey: Option<Pubkey>,
    authorized_voter: Option<Pubkey>,
    authorized_withdrawer: Option<Pubkey>,
    commission: Option<u8>,
    delegations: Vec<StakeDelegation>,
}

impl EpochStake {
    /// Stake `stake` lamports on `vote_account` without writing an account.
    ///
    /// With delegations added, `stake` must not be below their sum.
    pub fn new(vote_account: Pubkey, stake: u64) -> Self {
        Self {
            vote_account,
            stake: Some(stake),
            node_pubkey: None,
            authorized_voter: None,
            authorized_withdrawer: None,
            commission: None,
            delegations: Vec::new(),
        }
    }

    /// An entry whose total stake is the sum of `delegations`, each written
    /// as a stake account delegated to `vote_account`.
    ///
    /// The surfnet rejects an entry with no delegations and no stake, so pass
    /// at least one.
    pub fn from_delegations(
        vote_account: Pubkey,
        delegations: impl IntoIterator<Item = StakeDelegation>,
    ) -> Self {
        Self {
            vote_account,
            stake: None,
            node_pubkey: None,
            authorized_voter: None,
            authorized_withdrawer: None,
            commission: None,
            delegations: delegations.into_iter().collect(),
        }
    }

    /// Write a fully active stake account delegated to this vote account.
    pub fn delegation(mut self, delegation: StakeDelegation) -> Self {
        self.delegations.push(delegation);
        self
    }

    /// Write several delegated stake accounts.
    pub fn delegations(mut self, delegations: impl IntoIterator<Item = StakeDelegation>) -> Self {
        self.delegations.extend(delegations);
        self
    }

    /// Write a vote account at the address with this validator identity.
    ///
    /// Required for the other vote-account fields to take effect.
    pub fn node_pubkey(mut self, node_pubkey: Pubkey) -> Self {
        self.node_pubkey = Some(node_pubkey);
        self
    }

    /// Signer for vote transactions; defaults to the node pubkey.
    pub fn authorized_voter(mut self, authorized_voter: Pubkey) -> Self {
        self.authorized_voter = Some(authorized_voter);
        self
    }

    /// Signer for withdrawals; defaults to the node pubkey.
    pub fn authorized_withdrawer(mut self, authorized_withdrawer: Pubkey) -> Self {
        self.authorized_withdrawer = Some(authorized_withdrawer);
        self
    }

    /// Inflation-reward commission as a percentage from 0 to 100; defaults to 0.
    pub fn commission(mut self, commission: u8) -> Self {
        self.commission = Some(commission);
        self
    }
}

impl From<EpochStake> for EpochStakeEntry {
    fn from(entry: EpochStake) -> Self {
        EpochStakeEntry {
            vote_account: entry.vote_account.to_string(),
            stake: entry.stake,
            node_pubkey: entry.node_pubkey.map(|p| p.to_string()),
            authorized_voter: entry.authorized_voter.map(|p| p.to_string()),
            authorized_withdrawer: entry.authorized_withdrawer.map(|p| p.to_string()),
            commission: entry.commission,
            delegations: if entry.delegations.is_empty() {
                None
            } else {
                Some(entry.delegations.into_iter().map(Into::into).collect())
            },
        }
    }
}

/// Builder for `surfnet_setEpochStakes`.
///
/// Replaces the whole epoch-stake snapshot with the given entries; an empty
/// builder clears it. See [`EpochStake`] for what each entry can set.
#[derive(Debug, Clone, Default)]
pub struct SetEpochStakes {
    entries: Vec<EpochStake>,
}

impl SetEpochStakes {
    /// Create an empty request, which clears the snapshot if executed as-is.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one vote account's entry.
    pub fn entry(mut self, entry: EpochStake) -> Self {
        self.entries.push(entry);
        self
    }

    /// Add several entries.
    pub fn entries(mut self, entries: impl IntoIterator<Item = EpochStake>) -> Self {
        self.entries.extend(entries);
        self
    }
}

impl CheatcodeBuilder for SetEpochStakes {
    const METHOD: &'static str = "surfnet_setEpochStakes";

    /// Build the JSON-RPC parameter array for `surfnet_setEpochStakes`.
    fn build(self) -> serde_json::Value {
        let entries: Vec<EpochStakeEntry> = self.entries.into_iter().map(Into::into).collect();
        serde_json::json!([entries])
    }
}
