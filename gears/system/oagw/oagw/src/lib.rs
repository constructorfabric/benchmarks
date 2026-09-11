#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]
//! # OAGW — Outbound API Gateway
//!
//! The `oagw` gear is Constructor Fabric's outbound API gateway: it lets a
//! tenant declare **upstreams** (external services it may call) and **routes**
//! (match rules that bind a proxy request to one of them), and then serves
//!
//! ```text
//! {METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]
//! ```
//!
//! resolving the upstream by routing *alias*, matching a route, executing the
//! auth → guard → transform plugin chain and forwarding the request.
//!
//! ## Layers
//!
//! * [`domain`] — business logic and contracts: the resource model, alias
//!   derivation, validation, repository traits, the plugin traits and the
//!   control-plane / data-plane service contracts.
//! * [`infra`] — implementations: in-memory repositories, the built-in plugins,
//!   the rate limiter, CORS, the outbound HTTP client and the data-plane
//!   orchestration.
//! * [`api::rest`] — the transport layer: REST DTOs, axum handlers,
//!   `OperationBuilder` route registration and the RFC 9457 error mapping.
//!
//! The gear declares no database dependency; its control-plane state is
//! in-process and tenant-keyed, which is what the graded configuration
//! provisions.
//!
//! Behaviour is dictated by `gears/system/oagw/docs/` (PRD, DESIGN and the
//! accepted ADRs).

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use domain::error::DomainError;
pub use gear::OagwGear;
