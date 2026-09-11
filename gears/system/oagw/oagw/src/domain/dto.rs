//! Write models and internal transfer objects.
//!
//! The write models are the exact shape of the management API request bodies
//! (`docs/schemas/*.schema.json`): `additionalProperties: false` is expressed
//! as `deny_unknown_fields`, and the read-only `id` member is accepted and
//! ignored so a caller can round-trip a `GET` response back through `PUT`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PluginBinding, PluginsConfig, Protocol,
    RateLimitConfig, Route, ServerConfig, Upstream,
};

/// `POST` / `PUT` body for `/oagw/v1/upstreams`.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSpec {
    /// Server-generated; accepted and ignored on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub id: Option<Value>,
    /// Whether the upstream accepts proxy requests. Defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Routing alias. Only accepted for non-derivable endpoint pools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Wire protocol.
    pub protocol: Protocol,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Guard / transform chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// `POST` / `PUT` body for `/oagw/v1/routes`.
///
/// `upstream_id` is immutable: it is required on create and, on replace, may
/// only repeat the value the route already has.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// Server-generated; accepted and ignored on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub id: Option<Value>,
    /// Owning upstream, as a UUID or an anonymous GTS identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Derived from `match`; accepted and ignored on write so a `GET`
    /// response can be edited and sent straight back through `PUT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_type: Option<String>,
    /// Whether the route participates in matching. Defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Tie-breaker between equally specific paths. Defaults to `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Route-level rate limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// `POST` body for `/oagw/v1/plugins`.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    /// Server-generated; accepted and ignored on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub id: Option<Value>,
    /// Unique-per-tenant plugin name.
    pub name: String,
    /// Free-text description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `auth`, `guard` or `transform` (a full GTS base type is also accepted).
    pub plugin_type: String,
    /// Declared phases (`on_request`, `on_response`, `on_error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<Vec<String>>,
    /// JSON Schema for this plugin's configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub config_schema: Option<Value>,
    /// Starlark source.
    pub source_code: String,
}

/// The fully merged configuration the Data Plane executes a request against.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// The upstream selected by alias resolution (closest tenant wins).
    pub upstream: Upstream,
    /// The matched route.
    pub route: Route,
    /// Effective auth binding after applying ancestor sharing modes.
    pub auth: Option<AuthConfig>,
    /// Effective rate limit: the strictest of upstream, route and every
    /// enforcing ancestor.
    pub rate_limit: Option<RateLimitConfig>,
    /// Effective CORS policy.
    pub cors: Option<CorsConfig>,
    /// Effective header transformation rules.
    pub headers: HeadersConfig,
    /// Plugin chain: ancestors first, then the selected upstream, then the
    /// route (`[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`).
    pub plugin_chain: Vec<PluginBinding>,
    /// Union of the tags visible along the chain.
    pub tags: Vec<String>,
    /// Tenant that owns the selected upstream. Differs from the caller's
    /// tenant when the alias resolved through the hierarchy.
    pub owner_tenant_id: Uuid,
}
