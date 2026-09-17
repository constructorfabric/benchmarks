//! # OAGW — Outbound API Gateway gear
//!
//! The OAGW gear provides an outbound API gateway: a management/control plane
//! that owns upstreams, routes and plugins (persistence-free, in-memory, per
//! ADR 0010's persistence-free MVP clause), a pingora-based data plane that
//! proxies outbound traffic through those upstreams, and a startup-time GTS
//! type-provisioning step that registers the OAGW type vocabulary with the
//! platform types-registry.
//!
//! ## Feature surface
//!
//! - **Control plane** — gear-relative REST surface `/oagw/v1/upstreams`,
//!   `/oagw/v1/routes`, `/oagw/v1/plugins` (CRUD) with authoritative
//!   management semantics: schema validation, binding rules, alias rules,
//!   cache invalidation and feedback to the data plane.
//! - **Data plane** — `{METHOD} /oagw/v1/proxy/{alias}/{*rest}` behind a
//!   pingora-based proxy engine with PEP gating, tenant resolution, auth and
//!   guard plugins, credential resolution, token-bucket rate limiting, CORS,
//!   SSRF protection, streaming and ADR 0007 error-source mapping.
//! - **Type provisioning** — registers the seven `gts.cf.core.oagw.*.v1`
//!   type ids and the plugin inventory instances, idempotently and with a
//!   bounded-failure contract (`infra/type_provisioning.rs`).
//!
//! ## Traceability
//!
//! Code carries `@cpt-*` markers referencing the pipeline feature documents
//! under `docs/pipeline/features/*/FEATURE.md` (flows, algorithms, states and
//! definitions of done), one `inst-*` step per marker block.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts;
pub mod infra;

pub use gear::OagwGear;
