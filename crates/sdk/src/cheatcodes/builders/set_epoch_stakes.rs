use solana_pubkey::Pubkey;
use surfpool_types::EpochStakeEntry;

use crate::cheatcodes::builders::CheatcodeBuilder;

/// One vote account's entry in a [`SetEpochStakes`] request.
///
/// By itself an entry only sets the stake read by the `sol_get_epoch_stake`
/// syscall. Calling [`EpochStake::node_pubkey`] additionally writes a
/// rent-exempt, freshly initialized vote account at the address, which
/// `getVoteAccounts` and on-chain programs can then read.
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
    stake: u64,
    node_pubkey: Option<Pubkey>,
    authorized_voter: Option<Pubkey>,
    authorized_withdrawer: Option<Pubkey>,
    commission: Option<u8>,
}

impl EpochStake {
    /// Stake `stake` lamports on `vote_account` without writing an account.
    pub fn new(vote_account: Pubkey, stake: u64) -> Self {
        Self {
            vote_account,
            stake,
            node_pubkey: None,
            authorized_voter: None,
            authorized_withdrawer: None,
            commission: None,
        }
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
