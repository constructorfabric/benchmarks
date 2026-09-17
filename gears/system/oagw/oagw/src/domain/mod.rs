//! Domain layer — entities, repository contracts, service boundaries, and
//! business rules.  Delivered by
//! `cpt-cf-oagw-feature-domain-model-repositories` (p2).
//!
//! DDD-Light layering: `api/rest` → `domain` → `infra`.  This module holds
//! business logic free of infrastructure concerns (DoD
//! `cpt-cf-oagw-dod-gear-foundation-skeleton`):
//!
//! - [`entity`] — the §3.1 domain entities mirroring the §3.7 table shapes
//!   (DoD `cpt-cf-oagw-dod-domain-model-repositories-entities`);
//! - [`repo`] — repository trait contracts for upstreams, routes, plugins
//!   (DoD `cpt-cf-oagw-dod-domain-model-repositories-repo-traits`);
//! - [`merge`] — per-field sharing-mode merge across the tenant hierarchy
//!   (DoD `cpt-cf-oagw-dod-domain-model-repositories-merge`);
//! - [`error`] — the central [`DomainError`] model and GTS instance catalog
//!   (DoD `cpt-cf-oagw-dod-error-semantics-catalog`);
//! - [`service`] — the Control Plane / Data Plane boundaries;
//! - [`state`] — the process-wide [`GearState`] shared with the REST layer.
//!
//! Tenant scoping (DoD `cpt-cf-oagw-dod-domain-model-repositories-tenant-scope`,
//! algorithm `cpt-cf-oagw-algo-domain-model-repositories-tenant-scope`) and
//! the SSRF guard (DoD
//! `cpt-cf-oagw-dod-domain-model-repositories-ssrf`, alias SSRF algorithm
//! `cpt-cf-oagw-algo-domain-model-repositories-ssrf-validate`) are enforced by
//! the repository layer and the entity validators.

pub mod cors;
pub mod entity;
pub mod error;
pub mod merge;
pub mod plugin;
pub mod rate;
pub mod repo;
pub mod service;
pub mod state;

pub use error::{DomainError, ErrorSource, Retriability};
pub use merge::UpstreamConfig;
pub use state::GearState;
