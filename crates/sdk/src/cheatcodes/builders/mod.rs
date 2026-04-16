//! Builder types for constructing Surfnet cheatcode RPC payloads.
//!
//! These builders are useful when tests need to express optional parameters
//! incrementally and then execute the request through
//! [`crate::Cheatcodes::execute`].
//!
//! ```rust
//! use surfpool_sdk::{Pubkey, Surfnet};
//! use surfpool_sdk::cheatcodes::builders::SetAccount;
//!
//! # async fn example() {
//! let surfnet = Surfnet::start().await.unwrap();
//! let cheats = surfnet.cheatcodes();
//! let address = Pubkey::new_unique();
//! let owner = Pubkey::new_unique();
//!
//! cheats
//!     .execute(
//!         SetAccount::new(address)
//!             .lamports(1_000_000)
//!             .owner(owner)
//!             .data(vec![1, 2, 3, 4]),
//!     )
//!     .unwrap();
//! # }
//! ```
mod deploy_program;
mod reset_account;
mod set_account;
mod set_token_account;
mod stream_account;

pub use deploy_program::DeployProgram;
pub use reset_account::ResetAccount;
pub use set_account::SetAccount;
pub use set_token_account::SetTokenAccount;
pub use stream_account::StreamAccount;

/// Trait implemented by typed cheatcode builders.
///
/// `METHOD` is the target Surfnet RPC method, and [`Self::build`] returns
/// the JSON-RPC parameter array for that method.
pub trait CheatcodeBuilder {
    const METHOD: &'static str;
    fn build(self) -> serde_json::Value;
}
