//! OAGW infrastructure layer (DESIGN §3.2-3.5).
//!
//! * `storage` — in-memory control-plane store.
//! * `plugin` — built-in plugin implementations, registries and the
//!   resolving validator.
//! * `ratelimit` — token-bucket rate limiter (ADR-0003).
//! * `cors` — built-in CORS handling (ADR-0004).
//! * `metrics` — OTel data-plane instruments (DESIGN §4.2).
//! * `proxy` — the `DataPlaneService` implementation (DESIGN §3.5).
//! * `clients` — outbound HTTP + tenant-chain clients.

pub mod cors;
pub mod metrics;
pub mod plugin;
pub mod proxy;
pub mod ratelimit;
pub mod storage;
