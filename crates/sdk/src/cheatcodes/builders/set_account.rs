use solana_pubkey::Pubkey;

use crate::cheatcodes::builders::CheatcodeBuilder;

/// Builder for `surfnet_setAccount`.
///
/// This builder starts with the required account address and lets tests add
/// whichever optional account fields they want to override before execution.
///
/// ```rust
/// use surfpool_sdk::{Pubkey, Surfnet};
/// use surfpool_sdk::cheatcodes::builders::SetAccount;
///
/// # async fn example() {
/// let surfnet = Surfnet::start().await.unwrap();
/// let cheats = surfnet.cheatcodes();
/// let address = Pubkey::new_unique();
/// let owner = Pubkey::new_unique();
///
/// cheats
///     .execute(
///         SetAccount::new(address)
///             .lamports(500_000)
///             .owner(owner)
///             .data(vec![0xAA, 0xBB]),
///     )
///     .unwrap();
/// # }
/// ```
pub struct SetAccount {
    address: Pubkey,
    lamports: Option<u64>,
    data: Option<Vec<u8>>,
    owner: Option<Pubkey>,
    rent_epoch: Option<u64>,
    executable: Option<bool>,
}

impl SetAccount {
    /// Create a new account-update builder for the given address.
    pub fn new(address: Pubkey) -> Self {
        Self {
            address,
            lamports: None,
            data: None,
            owner: None,
            rent_epoch: None,
            executable: None,
        }
    }

    /// Set the target lamport balance for the account.
    pub fn lamports(mut self, lamports: u64) -> Self {
        self.lamports = Some(lamports);
        self
    }

    /// Set raw account data bytes.
    ///
    /// The builder hex-encodes these bytes when producing the final RPC payload.
    pub fn data(mut self, data: Vec<u8>) -> Self {
        self.data = Some(data);
        self
    }

    /// Set the owning program for the account.
    pub fn owner(mut self, owner: Pubkey) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Set the account rent epoch.
    pub fn rent_epoch(mut self, rent_epoch: u64) -> Self {
        self.rent_epoch = Some(rent_epoch);
        self
    }

    /// Set whether the account is executable.
    pub fn executable(mut self, executable: bool) -> Self {
        self.executable = Some(executable);
        self
    }
}

impl CheatcodeBuilder for SetAccount {
    const METHOD: &'static str = "surfnet_setAccount";

    /// Build the JSON-RPC parameter array for `surfnet_setAccount`.
    fn build(self) -> serde_json::Value {
        let mut account_info = serde_json::Map::new();
        if let Some(lamports) = self.lamports {
            account_info.insert("lamports".to_string(), lamports.into());
        }
        if let Some(data) = self.data {
            account_info.insert("data".to_string(), hex::encode(data).into());
        }
        if let Some(owner) = self.owner {
            account_info.insert("owner".to_string(), owner.to_string().into());
        }
        if let Some(rent_epoch) = self.rent_epoch {
            account_info.insert("rentEpoch".to_string(), rent_epoch.into());
        }
        if let Some(executable) = self.executable {
            account_info.insert("executable".to_string(), executable.into());
        }

        serde_json::json!([self.address.to_string(), account_info])
    }
}
