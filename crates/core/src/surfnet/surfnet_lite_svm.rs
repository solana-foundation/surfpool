//! Former home of the local VM wrapper, kept so existing import paths still resolve: the
//! implementation lives in `surfnet::backend::local`.

pub use super::backend::local::{LAMPORTS_PER_SOL, LocalBackend as SurfnetLiteSvm};
