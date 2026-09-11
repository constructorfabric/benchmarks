//! OAGW — the outbound API gateway gear.
//!
//! This crate is the gear foundation (DECOMPOSITION entry 2.1,
//! FEATURE `gears/system/oagw/docs/features/gear-foundation.md`): the domain
//! model, the repository boundary, the effective-configuration merge engine,
//! the in-memory persistence, the GTS type provisioning and the gear wiring.
//! The REST handlers, the proxy data plane and the plugin runtime are added
//! by later entries on top of this skeleton.
//!
//! # Layering (DDD-Light, `cpt-cf-oagw-dod-gear-foundation-layer-boundaries`)
//!
//! | layer | modules | may depend on |
//! |---|---|---|
//! | API | [`api`] | domain |
//! | domain | [`domain`] | nothing outside the domain |
//! | infra | [`infra`] | domain |
//!
//! # Persistence
//!
//! Persistence is **in-memory** (graded deviation 5): no `toolkit-db`, no
//! `sea_orm`, no migrations. What the substitution must preserve is the
//! documented schema contract — the DESIGN §3.6 table shapes, which
//! [`infra::storage`] keeps as one child table per documented table.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::{OagwConfig, SsrfPolicy, TokenCacheConfig};
pub use domain::{
    AuthConfig, CorsConfig, DomainError, Endpoint, EndpointScheme, EffectiveConfig,
    GrpcMatch, HeadersConfig, HeaderPassthrough, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode,
    Plugin, PluginsConfig, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    RequestHeaders, ResponseHeaders, Route, RouteMatchType, ServerConfig, SharingMode,
    SustainedRate, Upstream,
};
pub use domain::services::{ControlPlaneService, ControlPlaneServiceImpl};
pub use gear::OagwGear;

/// Client fakes and a `ConfigProvider` for the gear tests.
///
/// Compiled for the crate's own unit tests and, through the `test-utils`
/// feature (which the crate's dev-dependency enables), for the `tests/`
/// targets.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

/// The GTS identifier constants and helpers of the OAGW namespace.
pub mod gts {
    pub use crate::domain::gts_helpers::*;
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod gear_tests;
