use solana_pubkey::Pubkey;

use crate::cheatcodes::builders::CheatcodeBuilder;

/// Builder for `surfnet_setTokenAccount`.
///
/// The required inputs are the token owner wallet and mint. Optional methods
/// can then be used to set token-account fields before execution.
///
/// ```rust
/// use surfpool_sdk::{Pubkey, Surfnet};
/// use surfpool_sdk::cheatcodes::builders::SetTokenAccount;
///
/// # async fn example() {
/// let surfnet = Surfnet::start().await.unwrap();
/// let cheats = surfnet.cheatcodes();
/// let owner = Pubkey::new_unique();
/// let mint = Pubkey::new_unique();
///
/// cheats
///     .execute(SetTokenAccount::new(owner, mint).amount(1_000))
///     .unwrap();
/// # }
/// ```
pub struct SetTokenAccount {
    owner: Pubkey,
    mint: Pubkey,
    amount: Option<u64>,
    delegate: Option<Option<Pubkey>>,
    state: Option<String>,
    delegated_amount: Option<u64>,
    close_authority: Option<Option<Pubkey>>,
    token_program: Option<Pubkey>,
}

impl SetTokenAccount {
    /// Create a new token-account update builder for the given owner and mint.
    pub fn new(owner: Pubkey, mint: Pubkey) -> Self {
        Self {
            owner,
            mint,
            amount: None,
            delegate: None,
            state: None,
            delegated_amount: None,
            close_authority: None,
            token_program: None,
        }
    }

    /// Set the token amount for the associated token account.
    pub fn amount(mut self, amount: u64) -> Self {
        self.amount = Some(amount);
        self
    }

    /// Set a delegate authority on the token account.
    pub fn delegate(mut self, delegate: Pubkey) -> Self {
        self.delegate = Some(Some(delegate));
        self
    }

    /// Clear any existing delegate authority.
    pub fn clear_delegate(mut self) -> Self {
        self.delegate = Some(None);
        self
    }

    /// Set the token account state string expected by Surfnet RPC.
    pub fn state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    /// Set the delegated token amount.
    pub fn delegated_amount(mut self, delegated_amount: u64) -> Self {
        self.delegated_amount = Some(delegated_amount);
        self
    }

    /// Set the close authority on the token account.
    pub fn close_authority(mut self, close_authority: Pubkey) -> Self {
        self.close_authority = Some(Some(close_authority));
        self
    }

    /// Clear any existing close authority.
    pub fn clear_close_authority(mut self) -> Self {
        self.close_authority = Some(None);
        self
    }

    /// Override the token program id.
    ///
    /// If omitted, the classic SPL Token program is used.
    pub fn token_program(mut self, token_program: Pubkey) -> Self {
        self.token_program = Some(token_program);
        self
    }
}

impl CheatcodeBuilder for SetTokenAccount {
    const METHOD: &'static str = "surfnet_setTokenAccount";

    /// Build the JSON-RPC parameter array for `surfnet_setTokenAccount`.
    fn build(self) -> serde_json::Value {
        let mut update = serde_json::Map::new();
        if let Some(amount) = self.amount {
            update.insert("amount".to_string(), amount.into());
        }
        if let Some(delegate) = self.delegate {
            update.insert(
                "delegate".to_string(),
                delegate
                    .map(|pubkey| pubkey.to_string().into())
                    .unwrap_or_else(|| "null".into()),
            );
        }
        if let Some(state) = self.state {
            update.insert("state".to_string(), state.into());
        }
        if let Some(delegated_amount) = self.delegated_amount {
            update.insert("delegatedAmount".to_string(), delegated_amount.into());
        }
        if let Some(close_authority) = self.close_authority {
            update.insert(
                "closeAuthority".to_string(),
                close_authority
                    .map(|pubkey| pubkey.to_string().into())
                    .unwrap_or_else(|| "null".into()),
            );
        }

        let mut params = vec![
            self.owner.to_string().into(),
            self.mint.to_string().into(),
            update.into(),
        ];

        if let Some(token_program) = self.token_program {
            params.push(token_program.to_string().into());
        }

        serde_json::Value::Array(params)
    }
}
