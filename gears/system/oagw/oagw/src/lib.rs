//! # cf-gears-oagw — Outbound API Gateway (OAGW)
//!
//! OAGW is the centralized outbound API gateway for Gears: it manages upstream
//! configuration (control plane) and proxies requests to external services
//! (data plane) through one gear, enforcing tenant-scoped configuration,
//! plugin-based authentication and RFC 9457 problem+json errors (DESIGN §1.1).
//!
//! Layout (DESIGN §3.2, DDD-Light):
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`config`] | `gears.oagw.config` surface |
//! | [`error`] | [`OagwError`], the GTS error catalogue and the problem+json mapping |
//! | [`tenant_context`] | authenticated caller → tenant binding |
//! | [`domain`] | domain model, in-memory tenant-scoped storage, control-plane and data-plane services, policies |
//! | [`api`] | REST transport (DTOs, handlers, route registration, error surface) |
//! | [`infra`] | platform integrations (GTS type provisioning, tenant hierarchy, plugin engine) |
//! | [`gear`] | ToolKit gear registration |
//!
//! Slice S1 delivered the foundation: configuration, the error surface, the
//! domain model with in-memory storage, and complete upstream CRUD at
//! `/oagw/v1/upstreams`. Slice S2 adds the data plane: alias resolution, route
//! matching, guards, header transformation and forwarding at
//! `/oagw/v1/proxy/{alias}/{*path}`. Slice S2b completes the control plane with
//! route and plugin management (`/oagw/v1/routes`, `/oagw/v1/plugins`). Slice
//! S5 fills the seams S2 left: rate limiting ([`domain::policy::rate_limit`]),
//! the CORS handler ([`domain::policy::cors`]) and the plugin engine
//! ([`infra::plugin`]), all installed by the gear as data-plane hooks. Slice S7
//! adds streaming ([`streaming`]): the WebSocket handshake the proxy splices
//! through, and the SSE responses it passes through without buffering. Slice S8
//! adds the observability of the proxy: the OpenTelemetry instruments of
//! [`domain::metrics`] (DESIGN §4.2) and the DEBUG records of the plugin chain
//! (DESIGN §4.3).

pub mod api;
pub mod config;
pub mod domain;
pub mod error;
pub mod gear;
pub mod infra;
pub mod streaming;
pub mod tenant_context;

pub use crate::api::rest::dto::{
    CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, PluginDto, ReplaceRouteRequest,
    ReplaceUpstreamRequest, RouteDto, UpstreamDto,
};
pub use crate::config::{OagwConfig, SsrfPolicy};
pub use crate::domain::metrics::{
    DEFAULT_METER_SCOPE, EndpointSelectionMethod, OagwMetrics, RequestPhase,
};
pub use crate::domain::policy::{
    CorsPreflight, CorsRequest, CorsService, PreflightResponse, RateLimitDecision,
    RateLimitLimiter, RateLimitService, cors_response_headers, effective_cors,
    effective_rate_limit, is_preflight, preflight_response,
};
pub use crate::domain::routing::TenantHierarchy;
pub use crate::domain::services::control_plane::{ControlPlaneService, DEFAULT_TENANT_ID};
pub use crate::domain::services::data_plane::{
    CorsHook, DataPlaneService, PluginEngine, ProxyContext, ProxyHooks, ProxyRequest, RateLimitHook,
};
pub use crate::domain::storage::{PluginStore, RouteStore, UpstreamStore};
pub use crate::domain::types::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, Plugin, PluginRef, PluginSpec, PluginsConfig,
    Protocol, RateLimitScope, Route, RouteSpec, Scheme, ServerConfig, SharingMode, Upstream,
    UpstreamSpec,
};
pub use crate::error::{OagwError, OagwErrorKind};
pub use crate::gear::OagwGear;
pub use crate::infra::plugin::{
    ApiKeyAuthPlugin, AuthPlugin, AuthPluginRegistry, GuardPlugin, GuardPluginRegistry,
    NoopAuthPlugin, OAuth2ClientCredAuthPlugin, PluginEngineService, PluginRegistries,
    RequestIdTransformPlugin, RequiredHeadersGuardPlugin, TokenCacheConfig, TransformPlugin,
    TransformPluginRegistry,
};
