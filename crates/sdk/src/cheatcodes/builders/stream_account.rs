use solana_pubkey::Pubkey;

use crate::cheatcodes::builders::CheatcodeBuilder;

/// Builder for `surfnet_streamAccount`.
///
/// This builder starts with the required account address and can be extended
/// with optional streaming configuration before execution.
///
/// ```rust
/// use surfpool_sdk::{Pubkey, Surfnet};
/// use surfpool_sdk::cheatcodes::builders::StreamAccount;
///
/// # async fn example() {
/// let surfnet = Surfnet::start().await.unwrap();
/// let cheats = surfnet.cheatcodes();
/// let address = Pubkey::new_unique();
///
/// cheats
///     .execute(StreamAccount::new(address).include_owned_accounts(true))
///     .unwrap();
/// # }
/// ```
pub struct StreamAccount {
    address: Pubkey,
    include_owned_accounts: Option<bool>,
}

impl StreamAccount {
    /// Create a stream-account builder for the given address.
    pub fn new(address: Pubkey) -> Self {
        Self {
            address,
            include_owned_accounts: None,
        }
    }

    /// Include owned accounts when registering the streamed account.
    pub fn include_owned_accounts(mut self, include_owned_accounts: bool) -> Self {
        self.include_owned_accounts = Some(include_owned_accounts);
        self
    }
}

impl CheatcodeBuilder for StreamAccount {
    const METHOD: &'static str = "surfnet_streamAccount";

    /// Build the JSON-RPC parameter array for `surfnet_streamAccount`.
    fn build(self) -> serde_json::Value {
        let mut params = vec![self.address.to_string().into()];

        if let Some(include_owned_accounts) = self.include_owned_accounts {
            params.push(serde_json::json!({
                "includeOwnedAccounts": include_owned_accounts
            }));
        }

        serde_json::Value::Array(params)
    }
}
