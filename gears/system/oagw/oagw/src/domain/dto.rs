//! Internal domain data-transfer types shared between the control and data
//! planes (proxy context, resolved upstream, effective configuration).

use std::sync::Arc;

use http::{HeaderMap, Method};
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeaderRules, HeadersConfig, PluginBinding, Route, ServerConfig,
    TenantId, Upstream,
};

/// One plugin binding in the *effective* (merged) plugin chain.
#[derive(Debug, Clone)]
pub struct ActiveBinding {
    /// Full GTS plugin id (named or custom instance id).
    pub plugin_ref: String,
    /// Resolved custom-plugin row (present only for UUID-bound plugins).
    pub plugin_uuid: Option<Uuid>,
    /// Per-binding configuration.
    pub config: serde_json::Value,
}

impl From<&PluginBinding> for ActiveBinding {
    fn from(value: &PluginBinding) -> Self {
        Self {
            plugin_ref: value.plugin_ref.clone(),
            plugin_uuid: value.plugin_uuid,
            config: value.config.clone(),
        }
    }
}

/// Effective computed rate-limit (stricter-wins over the hierarchy).
#[derive(Debug, Clone)]
pub struct EffectiveRateLimit {
    /// Tokens replenished per second.
    pub refill_per_sec: f64,
    /// Bucket capacity (tokens).
    pub capacity: f64,
    /// Cost consumed per request.
    pub cost: u32,
    /// Strategy of the *winning* (most strict) contributor.
    pub strategy: crate::domain::model::RateLimitStrategy,
    /// Scope of the winning contributor.
    pub scope: crate::domain::model::RateLimitScope,
    /// Shorter-than-minute windows >1s skipped: window in seconds of the
    /// winning contributor (used for `X-RateLimit-Reset`).
    pub window_secs: u64,
}

/// The merged configuration that governs a single proxied request
/// (upstream < route < tenant; rate limits `min(parent, child)`).
#[derive(Debug, Clone, Default)]
pub struct EffectiveConfig {
    /// Effective auth (single source; enforced ancestors win).
    pub auth: AuthConfig,
    /// Effective request headers transformation.
    pub headers: HeadersConfig,
    /// Ordered guard bindings: [enforced-ancestors..., upstream..., route...].
    pub guards: Vec<ActiveBinding>,
    /// Ordered transform bindings (same composition rule).
    pub transforms: Vec<ActiveBinding>,
    /// Effective rate limit (`None` = unlimited).
    pub rate_limit: Option<EffectiveRateLimit>,
    /// Effective CORS configuration (origins unioned).
    pub cors: Option<CorsConfig>,
    /// Effective tags (add-only union).
    pub tags: Vec<String>,
    /// Effective enabled flag.
    pub enabled: bool,
}

impl EffectiveConfig {
    /// Whether the effective config enables CORS.
    #[must_use]
    pub fn cors_enabled(&self) -> bool {
        self.cors.as_ref().is_some_and(|c| c.enabled)
    }
}

/// A resolved upstream at data-plane time.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The selected (closest) upstream row.
    pub upstream: Arc<Upstream>,
    /// All same-alias upstreams found on the tenant chain, ordered
    /// descendant-first (index 0 = selected).
    pub chain: Vec<Arc<Upstream>>,
    /// Effective config merged from chain + route.
    pub effective: EffectiveConfig,
    /// Matched route, if any.
    pub route: Option<Arc<Route>>,
}

/// Endpoint pool selection result (multi-endpoint target-host routing).
#[derive(Debug, Clone)]
pub struct EndpointSelection {
    /// Chosen endpoint index into `server.endpoints`.
    pub index: usize,
    /// Whether the alias is a common-suffix alias requiring
    /// `X-OAGW-Target-Host`.
    pub requires_target_host: bool,
}

/// Execution context assembled by the data plane.
#[derive(Debug, Clone)]
pub struct ProxyContext {
    pub tenant_id: TenantId,
    pub alias: String,
    pub method: Method,
    /// Path suffix from the proxy URL (already appended for `append` mode).
    pub path: String,
    /// Raw query string (undecoded).
    pub query: String,
    /// Maintained for requests that must be authorized against
    /// `gts.cf.core.oagw.proxy.v1~:invoke`.
    pub invoke_authorized: bool,
}

/// Effective server + endpoint routing information for the forward proxy.
#[derive(Debug, Clone)]
pub struct ForwardTarget {
    /// Effective server config (endpoints) after resolution.
    pub server: ServerConfig,
    /// Index of the target endpoint within `server.endpoints`.
    pub endpoint_index: usize,
    /// Whether the caller must supply `X-OAGW-Target-Host`.
    pub requires_target_host: bool,
}

/// The effective header rules, separated for direct use by the data plane.
#[derive(Debug, Clone, Default)]
pub struct EffectiveHeaders {
    pub request: HeaderRules,
    pub response: HeaderRules,
    /// Final passthrough policy for inbound headers.
    pub passthrough: crate::domain::model::PassthroughMode,
    pub passthrough_allowlist: Vec<String>,
}

impl From<&EffectiveConfig> for EffectiveHeaders {
    fn from(value: &EffectiveConfig) -> Self {
        Self {
            request: value.headers.request.clone(),
            response: value.headers.response.clone(),
            passthrough: value.headers.request.passthrough,
            passthrough_allowlist: value.headers.request.passthrough_allowlist.clone(),
        }
    }
}

/// Converts HeaderRules + selected passthrough into the data-plane view.
#[doc(hidden)]
#[must_use]
pub fn headers_view(headers: &HeadersConfig) -> EffectiveHeaders {
    EffectiveHeaders {
        request: headers.request.clone(),
        response: headers.response.clone(),
        passthrough: headers.request.passthrough,
        passthrough_allowlist: headers.request.passthrough_allowlist.clone(),
    }
}

/// Placeholder used until a body is attached.
pub type ProxyHeaderMap = HeaderMap;
