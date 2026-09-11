//! `oagw` — the Outbound API Gateway gear.
//!
//! OAGW manages all outbound API requests from gears to external services. It
//! is a single crate with an internal Control Plane / Data Plane split, laid
//! out in DDD-Light layers (see `docs/DESIGN.md` §3.2):
//!
//! * [`domain`] — models, service traits, plugin traits, validation. No
//!   infrastructure dependencies.
//! * [`infra`] — repositories, plugin registries, the proxy engine, metrics
//!   and types-registry provisioning.
//! * [`api`] — the Axum transport: management CRUD plus the proxy endpoint.
//!
//! # Route paths
//!
//! Every path registered here is **gear-relative** (`/oagw/v1/...`). The
//! api-gateway nests each gear's router under its own `prefix_path`, so the
//! absolute paths tabulated in `docs/PRD.md` / `docs/DESIGN.md`
//! (`/api/oagw/v1/...`) are what an operator gateway configured with
//! `prefix_path: /api` serves — the gear itself must not repeat the prefix.
//!
//! # Deviations from the design documents
//!
//! These are recorded where the shipped component differs from the prose, so
//! the gap is visible rather than surprising:
//!
//! * **Storage is in-process.** The gear declares no `db` capability and the
//!   crate has no `toolkit-db` dependency, so upstreams, routes and plugins
//!   live in the in-memory repositories in [`infra::storage`]. The repository
//!   traits in [`domain::repo`] are the seam a SeaORM implementation would
//!   slot into.
//! * **Custom (Starlark) plugins are stored, not executed.** There is no
//!   Starlark runtime in the workspace; UUID-backed plugin definitions are
//!   managed through the plugin CRUD surface and resolve at bind time, but
//!   their bodies are inert. Named built-in plugins execute normally.
//! * **gRPC proxying is not implemented** — `docs/DESIGN.md` puts it in
//!   Phase 3 and states no gRPC proxy code path is currently reachable.
//! * **Rate limiting is per-instance.** The MVP decision in
//!   `ADR/0003-rate-limiting.md` §4; no Redis sync layer exists.

#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

#[cfg(feature = "test-utils")]
pub mod test_utils;

pub use config::OagwConfig;
pub use gear::OagwGear;
