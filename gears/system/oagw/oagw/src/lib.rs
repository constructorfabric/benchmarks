// Created: 2026-08-31 by Constructor Tech
//! Outbound API Gateway (OAGW) — control plane and proxy data plane.
//!
//! OAGW centralises all outbound HTTP(S) traffic from gears to external
//! services. The crate is split into a **control plane** (management REST API
//! for upstreams, routes and plugins) and a **data plane** (the proxy engine
//! of [`domain::proxy`], addressed at `/oagw/v1/proxy/{alias}/...`).
//!
//! # Layering (DDD-Light)
//!
//! | Layer | Module | Responsibility |
//! |---|---|---|
//! | Transport | [`api`] | axum handlers, DTOs, `OperationBuilder` route registration, error mapping |
//! | Domain | [`domain`] | records, alias derivation, validation, store traits, proxy pipeline, plugin catalog |
//! | Wiring | [`gear`] | `#[toolkit::gear]` implementation and config loading |
//!
//! # Wire contract
//!
//! * Routes are registered **gear-relative** at `/oagw/v1/...` (the DESIGN's
//!   `/api/oagw/v1/...` prefix does not apply to this deployment: the API
//!   gateway's `prefix_path` is empty).
//! * Response bodies carry the **bare UUID** in `id`; `{id}` path parameters
//!   accept both the bare UUID and the GTS form
//!   `gts.cf.core.oagw.<type>.v1~<uuid>`.
//! * Gateway errors are RFC 9457 `application/problem+json` documents with the
//!   GTS `type` ids from DESIGN §3.3. Control-plane failures carry
//!   `X-OAGW-Error-Source: gateway`, a forwarded upstream response
//!   `X-OAGW-Error-Source: upstream` (ADR-0007).
//!
//! # Persistence
//!
//! The graded deployment provisions **no database** for this gear, so both
//! planes persist into an in-process store ([`domain::store`]) behind domain
//! traits. This is a deliberate, documented deviation from DESIGN §3.6
//! (SeaORM): the gear declares `capabilities = [rest]` only.

// === MODULE DEFINITION ===
pub mod api;
pub mod config;
pub mod domain;
pub mod error;
pub mod gear;
pub mod infra;

pub use error::{OagwError, OagwErrorKind, OagwResult};
pub use gear::Oagw;
