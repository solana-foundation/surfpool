//! Keep-alive covers what a protocol's own off-chain actor rewrites every slot, such as a market
//! maker republishing its quote. Nobody runs those actors against a surfnet, so once a scenario's
//! override touches an account, the surfnet applies the keep-alive templates its template imports
//! every slot, as ordinary overrides, until the network is reset.

use std::collections::HashMap;

use solana_pubkey::Pubkey;

/// An account a scenario's override wrote through a template that imports keep-alive.
#[derive(Clone, Debug)]
pub struct KeepAliveSource {
    pub template: String,
    pub account: Pubkey,
    /// The override's values, which input and PDA addresses resolve against.
    pub values: HashMap<String, serde_json::Value>,
}

/// Binds a template's keep-alive import to the account it rewrites every slot, resolved when an
/// override of that template first touched it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeepAliveBinding {
    /// The importing template's ID.
    pub template: String,
    /// The import's index in that template's `keep_alive` list.
    pub import: usize,
    pub account: Pubkey,
}
