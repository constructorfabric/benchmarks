//! Outbound API Gateway (OAGW) gear.
//!
//! The OAGW sits between relying services and external upstream APIs.  It
//! exposes:
//!
//! * a **management API** (`/api/oagw/v1/upstreams|routes|plugins`) for
//!   tenant-scoped configuration of upstream services, routes and plugins;
//! * a **proxy API** (`/api/oagw/v1/proxy/{alias}/...`) that secures,
//!   transforms and forwards outbound traffic (HTTP, SSE, WebSocket).
//!
//! The implementation follows the DESIGN.md DDD-Light module layout:
//! `config`, `gear`, `api/rest/*`, `domain/*`, `infra/*`.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
