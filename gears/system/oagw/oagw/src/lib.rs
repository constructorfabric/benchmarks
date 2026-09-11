//! OAGW gear — foundation.
//!
//! The root of the OAGW feature graph: the registered ToolKit gear, the
//! `OagwConfig` surface, the DDD-Light crate skeleton, the `Upstream` / `Route`
//! / `Plugin` domain model, the GTS identifier catalogue with its
//! types-registry provisioning, and the canonical [`DomainError`] mapped to
//! RFC 9457.
//!
//! ## Layering
//!
//! - [`domain`] — business vocabulary, free of transport and persistence types
//! - [`api`] — transport layer; the only module allowed to touch `axum`/`http`
//! - [`gts`] — GTS identifier catalogue and its types-registry provisioning
//! - [`plugins`] — the built-in plugin implementations, the `cred://` routine,
//!   and the OAuth2 token cache
//! - [`config`] — the gear configuration surface
//! - [`store`] — the in-process transactional configuration store
//! - [`control_plane`] — the management business logic, one module per CDSL
//!   routine, free of transport types
//! - [`gear`] — ToolKit gear wiring

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

pub mod api;
pub mod config;
pub mod control_plane;
pub mod data_plane;
pub mod domain;
pub mod gear;
pub mod gts;
pub mod plugins;
pub mod store;

pub use api::rest::dto;
pub use api::rest::params;
pub use api::rest::problem;
pub use api::rest::state::OagwState;
pub use config::{ConfigError, OagwConfig, SsrfPolicy};
pub use domain::{
    Algorithm, Alias, AliasError, AuthConfig, Burst, CorsConfig, DomainError, Endpoint,
    EndpointHost, ErrorContext, ErrorKind, ErrorSource, GearFoundationState, GrpcMatch,
    HeadersConfig, Hostname, HttpMatch, InvalidTransition, MatchConfig, ModelError, Passthrough,
    PathSuffixMode, Plugin, PluginsConfig, RateLimitConfig, RateLimitScope, RequestHeaderRules,
    ResponseHeaderRules, Route, Scheme, ServerConfig, SharingMode, Strategy, Sustained, Upstream,
    Window,
};
pub use control_plane::cache::{ControlPlaneCache, DeletionObservers, RateLimitCleanup};
pub use control_plane::odata::{ListQuery, Page};
pub use control_plane::service::{Lifecycle, ManagementService, ServiceError};
pub use gear::OagwGear;
pub use store::{RouteRow, UpstreamRow};
pub use gts::{
    AUTH_PLUGIN_TYPE, ERR_ALIAS_CONFLICT, ERR_AUTH_FAILED, ERR_CIRCUIT_BREAKER_OPEN,
    ERR_DOWNSTREAM_ERROR, ERR_INVALID_TARGET_HOST, ERR_LINK_UNAVAILABLE, ERR_MATCH_CONFLICT,
    ERR_MISSING_TARGET_HOST, ERR_PAYLOAD_TOO_LARGE, ERR_PLUGIN_IN_USE, ERR_PLUGIN_NOT_FOUND,
    ERR_PROTOCOL_ERROR, ERR_RATE_LIMIT_EXCEEDED, ERR_ROUTE_NOT_FOUND, ERR_SECRET_NOT_FOUND,
    ERR_STREAM_ABORTED, ERR_TIMEOUT_CONNECTION, ERR_TIMEOUT_IDLE, ERR_TIMEOUT_REQUEST,
    ERR_UNKNOWN_TARGET_HOST, ERR_VALIDATION, GUARD_PLUGIN_TYPE, PROTOCOL_GRPC, PROTOCOL_HTTP,
    PROTOCOL_TYPE, ROUTE_TYPE, TRANSFORM_PLUGIN_TYPE, UPSTREAM_TYPE,
};
