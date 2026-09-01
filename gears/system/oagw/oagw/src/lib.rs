//! OAGW — Outbound API Gateway gear.
//!
//! Routes outbound HTTP(S)/WebSocket traffic from services to upstream APIs:
//!
//! - control plane: tenant-scoped upstream / route / custom-plugin CRUD
//!   (in-memory repositories, no database);
//! - data plane: `/oagw/v1/proxy/{alias}/{*suffix}` — plugin chain
//!   (auth, guards, transforms), rate limiting, CORS, streaming proxy,
//!   WebSocket bridging;
//! - GTS type provisioning via the link-time `toolkit-gts` inventory.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
