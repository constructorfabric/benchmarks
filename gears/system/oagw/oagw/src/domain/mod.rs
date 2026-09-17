//! Domain layer (DESIGN.md §1.3, §3.1): the typed configuration model for
//! upstreams and routes, the validation rules that turn a submitted document
//! into either a normalized configuration or an RFC 9457 problem, alias
//! derivation, the in-memory configuration store and the hierarchical merge.
//!
//! The layer is free of HTTP concerns: every entry point is a plain function
//! or a service method operating on the types in [`model`], and every rejection
//! is a [`GatewayError`](crate::error::GatewayError) (which the transport layer
//! renders as `application/problem+json`).
//!
//! ## Module map
//!
//! - [`model`] — wire and stored types for upstreams, endpoints, routes,
//!   plugins, rate limits and CORS
//! - [`validation`] — the rules that normalize a submitted document
//! - [`alias`] — alias pattern, derivation (PSL) and immutability
//! - [`store`] — the in-memory configuration store and the control-plane
//!   service that owns it
//! - [`merge`] — hierarchical merge (tenant -> route) semantics

// Validation failures are rich RFC 9457 problems (detail, field, extensions), so
// `GatewayError` is larger than clippy's `result_large_err` threshold and every
// domain entry point that rejects input trips it. The error type is shared with
// the whole crate (see `crate::error`), so the domain layer accepts the size
// rather than boxing a problem it always hands to the transport layer whole.
#![allow(clippy::result_large_err)]

// === MODEL ===
pub mod alias;
pub mod merge;
pub mod model;
pub mod store;
pub mod validation;

// === RE-EXPORTS ===
pub use alias::{
    compute_derived_alias, enforce_alias_update, is_standard_port, normalize_alias,
    resolve_new_alias,
};
pub use merge::{
    effective_rate_limit, merge_chain, merge_cors, merge_plugins, merge_rate_limit, merge_route,
    merge_route_chain, merge_tags, merge_upstream, plugin_chain,
};
pub use model::{
    ALIAS_PATTERN, AuthConfig, BurstCapacity, CorsConfig, DEFAULT_ENDPOINT_PORT, Endpoint,
    GrpcMatch, HeadersConfig, Host, HttpMatch, HttpMethod, MAX_PORT, MIN_PORT, MatchRule,
    MatchSpec, PROTOCOL_GRPC, PROTOCOL_HTTP, PassthroughMode, PathSuffixMode, PluginRef,
    PluginsConfig, Protocol, ROUTE_BASE_TYPE, RateLimitAlgorithm, RateLimitConfig, RatePerSecond,
    RateScope, RateStrategy, RateWindow, RequestHeaders, ResponseHeaders, Route, RouteConfig,
    RouteSpec, STANDARD_HTTP_PORT, STANDARD_TLS_PORT, Scheme, ServerConfig, SharingMode,
    SustainedRate, TAG_PATTERN, UPSTREAM_BASE_TYPE, Upstream, UpstreamConfig, UpstreamSpec,
};
pub use store::{ConfigService, OagwStore};
pub use validation::{
    problem_from_deserialize, validate_cors, validate_host, validate_plugins, validate_rate_limit,
    validate_route, validate_tags, validate_upstream,
};
