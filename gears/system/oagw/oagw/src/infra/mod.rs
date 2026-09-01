//! Infrastructure layer: implements the domain repository contracts and hosts
//! the data-plane seams.
//!
//! * [`metrics`] — data-plane metrics registry (DESIGN §4.2).
//! * [`ratelimit`] — per-alias limiter registry (ADR 0003).
//! * [`storage`] — in-memory control-plane persistence (the gear has no
//!   `database:` block; DESIGN §3.2).
//! * [`plugin`] — built-in plugin registry seam.
//! * [`proxy`] — outbound proxy engine (DESIGN §3.5 "Proxy Request Flow").
//! * [`transport`] — outbound HTTP transport.
//! * [`tenant`] — tenant-chain resolution for the data plane.

pub mod metrics;
pub mod plugin;
pub mod ratelimit;
pub mod proxy;
pub mod storage;
pub mod tenant;
pub mod transport;
