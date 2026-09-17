//! # Outbound API Gateway (OAGW)
//!
//! Centralized outbound API gateway gear. Gears reach external services
//! exclusively through OAGW, which resolves the target upstream by alias,
//! matches a route, injects authentication, applies guards/transforms, rate
//! limits, CORS, and forwards the request — collecting credentials, policies
//! and error semantics in one place.
//!
//! Layering (DDD-Light):
//! - `api` — transport: management CRUD + proxy handlers and [`api::routes`]
//!   `OperationBuilder` registration.
//! - `domain` — pure business logic: [`domain::model`] (configuration schemas),
//!   [`domain::alias`] (alias derivation/enforcement), [`domain::plugin`]
//!   (plugin traits + contexts), [`domain::service`] (control plane + data
//!   plane orchestration over an in-memory store).
//! - `infra` — adapter layer: [`infra::plugins`] (built-in plugin registry,
//!   apikey/noop/oauth2/required-headers/request-id), [`infra::ratelimit`]
//!   (token-bucket), [`infra::outbound`] (toolkit-http forwarding).
//!
//! All errors use RFC 9457 `application/problem+json` with GTS type
//! identifiers (see [`error`]) and every response carries the
//! `X-OAGW-Error-Source` header.

pub mod api;
pub mod config;
pub mod domain;
pub mod error;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use domain::service::OagwService;
pub use error::OagwError;
pub use gear::OagwGear;
