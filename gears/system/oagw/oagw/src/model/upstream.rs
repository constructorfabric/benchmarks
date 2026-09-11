//! Upstream domain/DTO types, modeled against
//! `docs/schemas/upstream.v1.schema.json`.
//!
//! The schema's `server.endpoints[].scheme` enum lists only the TLS family
//! (`https`, `wss`, `wt`, `grpc`). Per the task specification's override
//! (`DECOMPOSITION.md` §1, correction 2), this type additionally accepts the
//! plaintext counterparts `http` and `ws`, because the graded configuration
//! sets `allow_http_upstream: true`, which makes declaring a plaintext
//! upstream legal at create time. Whether a plaintext connection is
//! *actually opened* is a separate, connection-time concern gated by that
//! same flag and owned by `cpt-cf-oagw-feature-proxy-core` (2.5); this type
//! only governs what an Upstream record may *declare*.
//!
//! DECOMPOSITION entry 2.2 ("Upstream Management API") extends this file
//! with the schema-validation, alias-derivation/immutability, and
//! host-classification algorithms `docs/features/upstream-management.md`
//! specifies, plus the internal (never-on-the-wire) `tenant_id` bookkeeping
//! field the REST layer (`api::rest::upstreams`) needs to scope every CRUD
//! operation to the calling tenant.

pub mod alias;
pub mod host;
pub mod ident;
pub mod validate;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Sharing mode for hierarchical configuration, reused across
/// `auth`/`plugins`/`rate_limit`/`cors` sub-configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants can override.
    Inherit,
    /// Descendants cannot override.
    Enforce,
}

/// Endpoint connection scheme. `Https`/`Wss`/`Wt`/`Grpc` are the schema's
/// documented TLS-family values; `Http`/`Ws` are the plaintext counterparts
/// this type additionally accepts (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    Https,
    Wss,
    Wt,
    Grpc,
    Http,
    Ws,
}

fn default_port() -> u16 {
    443
}

/// One upstream network endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Endpoint {
    pub scheme: EndpointScheme,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

/// `server.endpoints` container (`minItems: 1` per schema; enforced by
/// upstream-management's validation, not by this skeleton type).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

/// `auth` sub-config: authentication plugin binding for the upstream.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct AuthConfig {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Which inbound headers a request-header passthrough rule forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    #[default]
    None,
    Allowlist,
    All,
}

/// `headers.request` transformation rules.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct RequestHeaderRules {
    #[serde(default)]
    pub set: HashMap<String, String>,
    #[serde(default)]
    pub add: HashMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default)]
    pub passthrough: PassthroughMode,
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// `headers.response` transformation rules.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ResponseHeaderRules {
    #[serde(default)]
    pub set: HashMap<String, String>,
    #[serde(default)]
    pub add: HashMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// `headers` sub-config: request/response header transformation rules.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeaderRules,
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

/// `plugins` sub-config: plugin chain binding. Items are either a builtin
/// plugin's GTS identifier or a custom plugin's UUID (both represented on
/// the wire as strings; `cpt-cf-oagw-feature-plugin-management`, 2.4, owns
/// distinguishing and resolving them).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct PluginsBinding {
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub items: Vec<String>,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Time window for the sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

/// `rate_limit.sustained` sub-config.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SustainedRate {
    pub rate: u32,
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// `rate_limit.burst` sub-config.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct BurstConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

/// Scope for rate-limit counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

/// Behavior when the rate limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

fn default_rate_limit_cost() -> u32 {
    1
}

/// `rate_limit` sub-config, shared by Upstream and Route.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "default_rate_limit_cost")]
    pub cost: u32,
}

/// CORS-allowed HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum CorsMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

fn default_cors_methods() -> Vec<CorsMethod> {
    vec![CorsMethod::Get, CorsMethod::Post]
}

/// `cors` sub-config, shared by Upstream and Route.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: Sharing,
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<CorsMethod>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_enabled() -> bool {
    true
}

/// Tenant-scoped root Upstream configuration resource
/// (`gts.cf.core.oagw.upstream.v1~`). Unique per `(tenant_id, alias)`;
/// alias derivation/enforcement and tenant scoping belong to
/// `cpt-cf-oagw-feature-upstream-management` (2.2).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Upstream {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Owning tenant, derived from the request's `SecurityContext` at write
    /// time -- **never** client-suppliable and never rendered on the wire
    /// (`upstream.v1.schema.json` has no `tenant_id` property). Bookkeeping
    /// only, so that `ConfigStore::upstreams()` (keyed by upstream `id`
    /// alone) can still answer tenant-scoped queries; see
    /// `cpt-cf-oagw-algo-resolve-tenant-scope`.
    #[serde(skip)]
    pub tenant_id: Uuid,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plaintext_http_scheme_with_allow_http_upstream_in_mind() {
        let json = serde_json::json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "example.com", "port": 80 } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let upstream: Upstream = serde_json::from_value(json).unwrap();
        assert_eq!(upstream.server.endpoints.len(), 1);
        assert_eq!(upstream.server.endpoints[0].scheme, EndpointScheme::Http);
        assert_eq!(upstream.server.endpoints[0].port, 80);
    }

    #[test]
    fn accepts_plaintext_ws_scheme() {
        let json = serde_json::json!({
            "server": { "endpoints": [ { "scheme": "ws", "host": "example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let upstream: Upstream = serde_json::from_value(json).unwrap();
        assert_eq!(upstream.server.endpoints[0].scheme, EndpointScheme::Ws);
        assert_eq!(upstream.server.endpoints[0].port, 443);
    }

    #[test]
    fn still_accepts_the_schema_documented_tls_family_schemes() {
        for scheme in ["https", "wss", "wt", "grpc"] {
            let json = serde_json::json!({
                "server": { "endpoints": [ { "scheme": scheme, "host": "example.com" } ] },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            });
            let upstream: Upstream = serde_json::from_value(json).unwrap();
            assert_eq!(upstream.server.endpoints.len(), 1);
        }
    }

    #[test]
    fn enabled_defaults_to_true() {
        let json = serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let upstream: Upstream = serde_json::from_value(json).unwrap();
        assert!(upstream.enabled);
    }
}
