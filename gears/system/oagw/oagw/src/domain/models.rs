//! OAGW domain model.
//!
//! Mirrors the persistence-free projections of the `oagw_*` tables from
//! DESIGN.md §3.7 (`oagw_upstream`, `oagw_route`, `oagw_route_http_match`,
//! `oagw_plugin`, `oagw_upstream_plugin`, `oagw_route_plugin`, `oagw_config`).
//! Because this MVP is persistence-free (ADR 0010), these structs are the
//! single source of truth held by the in-memory repository; every field maps
//! one-to-one to a column of the corresponding table.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Upstream transport scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamScheme {
    /// Plaintext HTTP (only usable when `allow_http_upstream` is enabled).
    Http,
    /// TLS HTTP (default).
    Https,
}

impl UpstreamScheme {
    /// Parses a scheme string, defaulting to `Https`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            _ => None,
        }
    }

    /// Returns `true` when the scheme is TLS.
    #[must_use]
    pub const fn is_tls(self) -> bool {
        matches!(self, Self::Https)
    }
}

impl std::fmt::Display for UpstreamScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http => f.write_str("http"),
            Self::Https => f.write_str("https"),
        }
    }
}

/// Default upstream port when the caller does not specify one.
#[must_use]
pub const fn default_port_for(scheme: UpstreamScheme) -> u16 {
    match scheme {
        UpstreamScheme::Http => 80,
        UpstreamScheme::Https => 443,
    }
}

/// An outbound upstream target (`oagw_upstream`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// Unique alias (proxy path segment and binding key).
    pub alias: String,
    /// Human-readable name.
    pub name: String,
    /// Upstream host (DNS name or literal IP).
    pub host: String,
    /// Upstream TCP port.
    pub port: u16,
    /// Transport scheme.
    pub scheme: UpstreamScheme,
    /// Path prefix prepended to proxied request paths.
    pub path_prefix: String,
    /// Whether the upstream currently accepts traffic.
    pub enabled: bool,
    /// Per-upstream timeout override in seconds (0 = use global default).
    pub timeout_secs: u64,
}

/// One HTTP match rule for a route (`oagw_route_http_match`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RouteHttpMatch {
    /// Optional HTTP method this rule applies to.
    pub method: Option<String>,
    /// Path pattern (e.g. `/orders/{id}`).
    pub path_pattern: String,
}

/// An outbound route (`oagw_route`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Unique route alias.
    pub alias: String,
    /// Upstream the route forwards to (`None` = reserved).
    pub upstream_alias: Option<String>,
    /// Allowed HTTP methods (`None` = any).
    pub methods: Option<Vec<String>>,
    /// HTTP match rules applied by the data plane.
    pub http_matches: Vec<RouteHttpMatch>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS policy.
    pub cors: CorsConfig,
    /// Whether the route is enabled.
    pub enabled: bool,
    /// Priority used when multiple routes could match.
    pub priority: i32,
}

/// Token-bucket rate-limit configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimitConfig {
    /// Burst capacity (tokens).
    pub capacity: u64,
    /// Refill rate in tokens per second.
    pub refill_per_sec: f64,
}

/// CORS policy (`oagw_config`/route merged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CorsConfig {
    /// Whether CORS handling is enabled.
    pub enabled: bool,
    /// Allowed origins (`*` matches any origin).
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    pub allowed_methods: Vec<String>,
    /// Allowed request headers.
    pub allowed_headers: Vec<String>,
    /// Response headers exposed to the caller.
    pub expose_headers: Vec<String>,
    /// Preflight cache max-age in seconds.
    pub max_age_secs: u64,
    /// Whether to reflect `Access-Control-Allow-Credentials`.
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec![
                "GET".to_owned(),
                "POST".to_owned(),
                "PUT".to_owned(),
                "PATCH".to_owned(),
                "DELETE".to_owned(),
                "OPTIONS".to_owned(),
            ],
            allowed_headers: vec!["authorization".to_owned(), "content-type".to_owned()],
            expose_headers: Vec::new(),
            max_age_secs: 600,
            allow_credentials: false,
        }
    }
}

/// Plugin kinds understood by the data plane (catalog keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// No-op auth plugin (permissive).
    Noop,
    /// API-key header auth plugin.
    ApiKey,
    /// OAuth2 client-credentials plugin (form-encoded body auth).
    OAuth2ClientCred,
    /// OAuth2 client-credentials plugin (HTTP Basic auth on the token call).
    OAuth2ClientCredBasic,
    /// Guard plugin requiring a set of request headers.
    RequiredHeaders,
    /// Transform plugin injecting a request id.
    RequestId,
    /// Transform plugin emitting structured access logs.
    Logging,
    /// Transform plugin emitting request metrics.
    Metrics,
    /// Catalog-only auth plugin (Basic auth, not executed).
    Basic,
    /// Catalog-only auth plugin (Bearer passthrough, not executed).
    Bearer,
    /// Catalog-only guard/limit plugin (request timeout, not executed).
    Timeout,
    /// Catalog-only CORS plugin (CORS handled by the gateway policy).
    Cors,
}

impl PluginKind {
    /// Casts a kind into the catalog of its owning `gts` type id.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Noop
            | Self::ApiKey
            | Self::OAuth2ClientCred
            | Self::OAuth2ClientCredBasic
            | Self::Basic
            | Self::Bearer => "auth_plugin",
            Self::RequiredHeaders | Self::Timeout => "guard_plugin",
            Self::RequestId | Self::Logging | Self::Metrics | Self::Cors => "transform_plugin",
        }
    }

    /// Parses a plugin-kind string into the enum, or `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "noop" => Some(Self::Noop),
            "apikey" => Some(Self::ApiKey),
            "oauth2_client_cred" => Some(Self::OAuth2ClientCred),
            "oauth2_client_cred_basic" => Some(Self::OAuth2ClientCredBasic),
            "required_headers" => Some(Self::RequiredHeaders),
            "request_id" => Some(Self::RequestId),
            "logging" => Some(Self::Logging),
            "metrics" => Some(Self::Metrics),
            "basic" => Some(Self::Basic),
            "bearer" => Some(Self::Bearer),
            "timeout" => Some(Self::Timeout),
            "cors" => Some(Self::Cors),
            _ => None,
        }
    }
}

/// A bound plugin (`oagw_plugin` + binding tables).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// Unique plugin alias.
    pub alias: String,
    /// Plugin kind (catalog key).
    pub kind: PluginKind,
    /// Whether the plugin participates in the pipeline.
    pub enabled: bool,
    /// JSON configuration payload for the plugin.
    pub config: serde_json::Value,
}

/// The effective configuration resolved for one proxied request: the merged
/// hierarchy (route > upstream > global) projected onto what the data plane
/// needs. This is what the L1 config cache serves.
#[derive(Debug, Clone)]
pub struct EffectiveRouteConfig {
    /// Route alias this config belongs to.
    pub alias: String,
    /// Resolved upstream (None when the route is disabled/reserved).
    pub upstream: Option<Upstream>,
    /// Merged plugin list (route plugins first, then upstream plugins).
    pub plugins: Vec<Plugin>,
    /// Merged rate-limit configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// Merged CORS policy.
    pub cors: CorsConfig,
    /// Effective proxy timeout for this route.
    pub proxy_timeout: Duration,
    /// Allowed HTTP methods (`None` = any), enforced by the data plane.
    pub methods: Option<Vec<String>>,
    /// HTTP match rules enforced by the data plane.
    pub http_matches: Vec<RouteHttpMatch>,
    /// Whether an ancestor on the enablement tree disables this route.
    pub disabled: bool,
}
