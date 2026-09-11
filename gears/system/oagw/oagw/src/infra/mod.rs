//! Infrastructure: the outbound transport and the built-in plugin
//! implementations that the [`crate::domain::plugin::ControlPlane`] resolves.

pub mod outbound;
pub mod plugins;
pub mod token_cache;

pub use outbound::{Outbound, UpstreamConnection};
pub use token_cache::TokenCache;
