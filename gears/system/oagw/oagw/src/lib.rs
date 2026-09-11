//! OAGW — the outbound API gateway gear.
//!
//! The gear owns two planes that share one in-memory configuration store:
//!
//! * the **control plane** — tenant-scoped CRUD over `upstream`, `route` and
//!   `plugin` definitions (`/oagw/v1/...`);
//! * the **data plane** — `POST/GET/... /oagw/v1/proxy/{alias}/{path}`, which
//!   resolves an alias, matches a route, merges configuration down the tenant
//!   hierarchy, runs the plugin chain and forwards the request to the external
//!   service.
//!
//! See `docs/PRD.md`, `docs/DESIGN.md` and the accepted ADRs for the
//! specification this implementation follows.

pub mod api;
pub mod config;
pub mod credstore_client;
pub mod domain;
pub mod error;
pub mod gear;
pub mod ids;
pub mod infra;

pub use gear::OagwGear;
