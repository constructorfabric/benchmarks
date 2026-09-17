//! Infrastructure adapters: persistence, transport, data-plane caches and
//! plugin implementations.

pub mod cache;
pub mod plugin;
pub mod rate_limit;
pub mod storage;
pub mod transport;

pub use cache::ConfigCache;
pub use rate_limit::{RateLimitIdentity, RateLimiter};
pub use transport::UpstreamTransport;
