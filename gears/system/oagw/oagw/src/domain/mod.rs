//! The domain layer (DDD-Light): pure model, invariants and algorithms with
//! **no** infrastructure dependency (`cpt-cf-oagw-dod-gear-foundation-layer-boundaries`).
//!
//! | module | owns |
//! |---|---|
//! | [`dto`] | the shared Control-Plane / Data-Plane model |
//! | [`error`] | the single [`DomainError`] taxonomy |
//! | [`validation`] | structural invariant helpers |
//! | [`list_query`] | the OData list-query interpretation |
//! | [`credential`] | the credential-reference boundary |
//! | [`alias`] | alias derivation and alias immutability |
//! | [`cors`] | the built-in CORS handler of entry 2.8 |
//! | [`proxy`] | the Data-Plane proxy values and lifecycle records |
//! | [`rate_limit`] | the rate-limit algorithms of entry 2.7 |
//! | [`route_matcher`] | the proxy-time route selection |
//! | [`headers`] | the request and response header pipeline |
//! | [`endpoints`] | endpoint selection and target-host resolution |
//! | [`merge`] | the effective-configuration merge engine |
//! | [`repo`] | the repository boundary traits |
//! | [`services`] | the `ControlPlaneService` aggregate |
//! | [`plugin`] | the plugin-execution traits and payloads |
//! | [`type_catalog`] | the plugin catalog the types-registry holds |
//! | [`gts_helpers`] | GTS identifier constants and helpers |
//!
//! Nothing here may depend on `axum`, on storage, or on any SDK client; the
//! merge engine receives its override permissions as an input rather than
//! resolving them itself.

pub mod alias;
pub mod cors;
pub mod credential;
pub mod dto;
pub mod endpoints;
pub mod error;
pub mod gts_helpers;
pub mod headers;
pub mod list_query;
pub mod merge;
pub mod plugin;
pub mod proxy;
pub mod rate_limit;
pub mod repo;
pub mod route_matcher;
pub mod services;
pub mod type_catalog;
pub mod validation;

pub use dto::{
    AuthConfig, Budget, BurstCapacity, CorsConfig, CredentialRef, Endpoint, EndpointScheme,
    GrpcMatch, HeadersConfig, HeaderPassthrough, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode,
    Plugin, PluginsConfig, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    RequestHeaders, ResponseHeaders, Route, RouteMatchType, ServerConfig, SharingMode,
    SustainedRate, Upstream,
};
pub use credential::reject_non_cred_reference_values;
pub use cors::{CorsObservation, CorsOutcome, OriginVerdict, PreflightCors};
pub use error::DomainError;
pub use merge::{EffectiveConfig, LayerSharing, OverridePermissions, Sharing};
pub use rate_limit::{
    ClockReading, CounterPhase, CounterSpec, CounterState, RateClock, RateDecision,
    RateLimitResource, SharedClock, SystemClock,
};
pub use repo::{PluginBinding, PluginRepository, RouteRecord, RouteRepository, UpstreamRecord, UpstreamRepository};
pub use services::{ControlPlaneService, ControlPlaneServiceImpl, ResolvedProxyTarget};
pub use services::proxy::DataPlaneService;

#[cfg(test)]
#[path = "cors_tests.rs"]
mod cors_tests;

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod proxy_tests;

#[cfg(test)]
#[path = "rate_limit_tests.rs"]
mod rate_limit_tests;

#[cfg(test)]
#[path = "route_matcher_tests.rs"]
mod route_matcher_tests;

#[cfg(test)]
#[path = "header_transform_tests.rs"]
mod header_transform_tests;

#[cfg(test)]
#[path = "endpoint_selector_tests.rs"]
mod endpoint_selector_tests;

#[cfg(test)]
#[path = "layer_boundary_tests.rs"]
mod layer_boundary_tests;
