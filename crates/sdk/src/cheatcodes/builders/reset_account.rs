use solana_pubkey::Pubkey;

use crate::cheatcodes::builders::CheatcodeBuilder;

/// Builder for `surfnet_resetAccount`.
///
/// This builder starts with the required account address and exposes
/// additional reset options as fluent setters.
///
/// ```rust
/// use surfpool_sdk::{Pubkey, Surfnet};
/// use surfpool_sdk::cheatcodes::builders::ResetAccount;
///
/// # async fn example() {
/// let surfnet = Surfnet::start().await.unwrap();
/// let cheats = surfnet.cheatcodes();
/// let address = Pubkey::new_unique();
///
/// cheats
///     .execute(ResetAccount::new(address).include_owned_accounts(true))
///     .unwrap();
/// # }
/// ```
pub struct ResetAccount {
    address: Pubkey,
    include_owned_accounts: Option<bool>,
}

impl ResetAccount {
    /// Create a reset-account builder for the given address.
    pub fn new(address: Pubkey) -> Self {
        Self {
            address,
            include_owned_accounts: None,
        }
    }

    /// Include accounts owned by the target account in the reset operation.
    pub fn include_owned_accounts(mut self, include_owned_accounts: bool) -> Self {
        self.include_owned_accounts = Some(include_owned_accounts);
        self
    }
}

impl CheatcodeBuilder for ResetAccount {
    const METHOD: &'static str = "surfnet_resetAccount";

    /// Build the JSON-RPC parameter array for `surfnet_resetAccount`.
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
