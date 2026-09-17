//! OAGW — Outbound API Gateway gear.
//!
//! Proxies tenant-scoped `.../oagw/v1/proxy/{alias}/{path}` requests to
//! configured upstream services, backed by a management API for
//! upstreams, routes and plugins.
//!
//! Architecture follows `docs/DESIGN.md`:
//! - **Control plane** — tenant-scoped CRUD for upstreams / routes /
//!   plugins under `/oagw/v1/...`.
//! - **Data plane** — alias resolution along the tenant hierarchy
//!   (descendant → root, closest wins), route matching (method
//!   allowlist + longest path prefix), effective config merging
//!   (`sharing: private | inherit | enforce`), the Auth → Guard →
//!   Transform(request) → upstream → Transform(response) plugin order,
//!   token-bucket rate limiting with `effective_rate = min(...)`, and
//!   RFC 9457 problem+json errors carrying GTS type identifiers plus
//!   the `X-OAGW-Error-Source` header.
//!
//! Storage is in-memory (dashmap) — the crate has no `database`
//! capability and the graded config runs without a DB. GTS Type
//! Schemas and the builtin-plugin catalogue are contributed to the
//! process-wide `toolkit-gts` inventory at link time, so
//! `types-registry` seeds them at boot with no runtime provisioning
//! code.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts;
pub mod infra;

pub use gear::OagwGear;
