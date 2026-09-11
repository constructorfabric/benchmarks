//! The infrastructure layer (DDD-Light): storage, GTS type provisioning and
//! plugin plumbing — everything that touches something outside the domain
//! (`cpt-cf-oagw-dod-gear-foundation-layer-boundaries`).
//!
//! | module | owns |
//! |---|---|
//! | [`authorization`] | the management authorization and tenant-hierarchy adapters |
//! | [`storage`] | the in-memory repository implementations (graded deviation 5) |
//! | [`proxy`] | the proxy data plane of entry 2.4 |
//! | [`type_provisioning`] | the idempotent base GTS-type registration |
//! | [`plugin`] | plugin registries and built-ins (filled by entry 2.6) |
//! | [`metrics`] | the metric registry of entry 2.9 |
//! | [`audit`] | the structured JSON audit emitter of entry 2.9 |
//! | [`cp_cache`] | the Control Plane L1 cache of entry 2.9 |
//! | [`dp_cache`] | the Data Plane L1 hot-config cache of entry 2.9 |
//! | [`health`] | the health and readiness surface of entry 2.9 |
//! | [`observability`] | the write-side invalidation hook that joins them |

pub mod audit;
pub mod authorization;
pub mod cp_cache;
pub mod dp_cache;
pub mod health;
pub mod metrics;
pub mod observability;
pub mod plugin;
pub mod proxy;
pub mod storage;
pub mod type_provisioning;
