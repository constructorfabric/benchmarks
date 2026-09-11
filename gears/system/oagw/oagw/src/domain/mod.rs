//! Domain layer of the OAGW gear.
//!
//! - [`model`] — the resource model shaped after
//!   `docs/schemas/upstream.v1.schema.json` / `route.v1.schema.json` and the
//!   pure business rules (alias derivation, endpoint validation, alias
//!   immutability).
//! - [`control_plane`] — the tenant-scoped store behind the management CRUD,
//!   plus the round-robin endpoint cursor the data plane shares with it.
//! - [`resolution`] — alias lookup, route matching and endpoint selection
//!   (ADR-0001): everything the proxy decides *before* it opens a connection.
//! - [`proxy`] — the data plane itself: body validation, request/ header
//!   transformation, the streaming pass-through and transport error mapping.
//! - [`rate_limit`] — the in-memory rate limiter of the proxy path (ADR-0003):
//!   the token bucket and sliding window, the counter scopes and the 429
//!   problem document with its headers.
//! - [`cors`] — the cross-origin behaviour of the proxy path (ADR-0004):
//!   preflight short-circuit and origin/method enforcement.
//! - [`plugins`] — the plugin traits, the execution order and the builtin
//!   plugins the proxy path runs (ADR-0002, ADR-0008, ADR-0009).
//!
//! The layer has no transport or persistence dependency: it works on plain data
//! types and signals failures with the shared [`crate::OagwError`].

pub mod control_plane;
pub mod cors;
pub mod model;
pub mod plugins;
pub mod proxy;
pub mod rate_limit;
pub mod resolution;
