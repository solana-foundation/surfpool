//! # Surfpool SDK
//!
//! Embed a full Surfpool instance directly in your Rust integration tests.
//! No external process needed — just spin up a `Surfnet`, point your RPC client at it,
//! and test against a real Solana-compatible runtime.
//!
//! ```rust
//! use surfpool_sdk::Surfnet;
//!
//! #[tokio::test]
//! async fn my_test() {
//!     let surfnet = Surfnet::start().await.unwrap();
//!
//!     let rpc = surfnet.rpc_client();
//!     let balance = rpc.get_balance(&surfnet.payer().pubkey()).unwrap();
//!     assert!(balance > 0);
//! }
//! ```

pub mod cheatcodes;
mod error;
mod surfnet;

pub use cheatcodes::Cheatcodes;
pub use error::{SurfnetError, SurfnetResult};
// Re-export key Solana types for convenience
pub use solana_keypair::Keypair;
pub use solana_pubkey::Pubkey;
pub use solana_rpc_client::rpc_client::RpcClient;
pub use solana_signer::Signer;
pub use surfnet::{Surfnet, SurfnetBuilder};
pub use surfpool_types::{BlockProductionMode, SimnetCommand, SimnetEvent};
