//! Domain model for the `oagw` gear.
//!
//! These types are the canonical, in-memory representation of the OAGW
//! configuration. They are transport-agnostic: the REST layer converts
//! to/from them ([`crate::api::rest::dto`]) and the data plane (part 2)
//! consumes them directly.
//!
//! Shared configuration vocabulary (sharing modes, header rules, rate limits,
//! CORS, plugin chains) lives here in the module root; per-resource models are
//! in the child modules.

pub mod plugin;
pub mod route;
pub mod upstream;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use plugin::{Plugin, PluginKind, PluginSource};
pub use route::{GrpcMatch, HttpMatch, Route, RouteMatch};
pub use upstream::{AuthBinding, Endpoint, Protocol, Scheme, ServerConfig, Upstream};

/// Default HTTP methods for a CORS configuration.
pub const DEFAULT_CORS_METHODS: [&str; 2] = ["GET", "POST"];

/// How a configured block participates in tenant-hierarchy merging
/// (PRD "configuration layering": `Upstream < Route < Tenant`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendant tenants.
    #[default]
    Private,
    /// Visible to descendants, which may override it.
    Inherit,
    /// Visible to descendants, which may **not** override it.
    Enforce,
}

impl SharingMode {
    /// Wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        }
    }
}

/// Which inbound headers the gateway forwards upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderPassthrough {
    /// Forward nothing but the allowlist-independent set the data plane adds.
    #[default]
    None,
    /// Forward only `passthrough_allowlist` names.
    Allowlist,
    /// Forward everything.
    All,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite when present).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append; duplicates allowed).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the inbound request.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(skip_serializing_if = "header_passthrough_is_none")]
    pub passthrough: HeaderPassthrough,
    /// Headers forwarded when `passthrough` is [`HeaderPassthrough::Allowlist`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

impl HeaderPassthrough {
    /// True for [`HeaderPassthrough::None`].
    #[must_use]
    pub fn is_none(self) -> bool {
        self == Self::None
    }
}

/// `skip_serializing_if` helper for [`HeaderPassthrough`].
fn header_passthrough_is_none(passthrough: &HeaderPassthrough) -> bool {
    passthrough.is_none()
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response to the client.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation rules for a request/response exchange.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderRules {
    /// Rules applied to the request sent upstream.
    #[serde(skip_serializing_if = "RequestHeaderRules::is_empty")]
    pub request: RequestHeaderRules,
    /// Rules applied to the response returned downstream.
    #[serde(skip_serializing_if = "ResponseHeaderRules::is_empty")]
    pub response: ResponseHeaderRules,
}

impl HeaderRules {
    /// True when neither side declares any rule.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request == RequestHeaderRules::default()
            && self.response == ResponseHeaderRules::default()
    }
}

impl RequestHeaderRules {
    /// True when nothing is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

impl ResponseHeaderRules {
    /// True when nothing is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Sustained rate of a rate-limit configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SustainedRate {
    /// Tokens replenished per `window`.
    pub rate: u64,
    /// Window over which `rate` replenishes.
    pub window: RateWindow,
}

impl Default for SustainedRate {
    fn default() -> Self {
        Self {
            rate: 1,
            window: RateWindow::default(),
        }
    }
}

/// Burst capacity of a rate-limit configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstCapacity {
    /// Maximum burst size; defaults to `sustained.rate` when omitted.
    pub capacity: u64,
}

/// Time window for a sustained rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    /// One second.
    #[default]
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

impl RateWindow {
    /// Length of the window in seconds.
    #[must_use]
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket; allows bursts.
    #[default]
    TokenBucket,
    /// Sliding window; prevents boundary bursts.
    SlidingWindow,
}

/// Keying scope of a rate-limit counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant.
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    /// Queue the request until a token is available.
    Queue,
    /// Degrade (serve a reduced-fidelity response).
    Degrade,
}

/// Rate-limiting configuration (identical shape on upstream and route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// How the block participates in tenant-hierarchy merging.
    pub sharing: SharingMode,
    /// Rate-limiting algorithm.
    pub algorithm: RateAlgorithm,
    /// Sustained rate; required.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate` when omitted.
    pub burst: Option<BurstCapacity>,
    /// Counter keying scope.
    pub scope: RateScope,
    /// Behaviour when the limit is exhausted.
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    pub cost: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::default(),
            sustained: SustainedRate::default(),
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: 1,
        }
    }
}

impl RateLimitConfig {
    /// Effective burst capacity: `burst.capacity` or `sustained.rate`.
    #[must_use]
    pub fn effective_burst(&self) -> u64 {
        self.burst.map_or(self.sustained.rate, |b| b.capacity)
    }
}

/// CORS configuration (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// How the block participates in tenant-hierarchy merging.
    pub sharing: SharingMode,
    /// Whether CORS is enabled. Required by the JSON Schema.
    pub enabled: bool,
    /// Allowed origins; `["*"]` means any origin.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentialed requests are allowed.
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: DEFAULT_CORS_METHODS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// A reference to a plugin: either a built-in/named GTS identifier or the UUID
/// of a tenant-defined plugin resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct PluginRef {
    /// The canonical plugin identifier string (full GTS identifier or UUID).
    pub plugin_ref: String,
    /// Extracted UUID when `plugin_ref` is UUID-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<uuid::Uuid>,
}

impl PluginRef {
    /// A named (built-in or gear-provided) plugin reference.
    #[must_use]
    pub fn named(gts_id: impl Into<String>) -> Self {
        Self {
            plugin_ref: gts_id.into(),
            plugin_uuid: None,
        }
    }

    /// A UUID-backed (tenant-defined) plugin reference.
    #[must_use]
    pub fn custom(id: uuid::Uuid) -> Self {
        Self {
            plugin_ref: id.to_string(),
            plugin_uuid: Some(id),
        }
    }
}

/// An entry of a `plugins.items` list: a plugin reference plus optional
/// per-binding configuration.
///
/// On the wire this accepts both the bare-string form (`"gts.…"` / UUID) and
/// the object form (`{"plugin_ref": "gts.…", "config": {…}}`) shown in
/// ADR 0009; serialisation always emits the object form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBinding {
    /// Bare plugin reference with no per-binding configuration.
    Bare(String),
    /// Plugin reference plus `ctx.config`.
    Detailed {
        /// Which plugin is bound.
        plugin_ref: String,
        /// Per-binding plugin configuration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<serde_json::Value>,
    },
}

impl PluginBinding {
    /// The canonical plugin identifier of this binding.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Bare(plugin_ref) | Self::Detailed { plugin_ref, .. } => plugin_ref,
        }
    }

    /// Per-binding configuration, if any.
    #[must_use]
    pub fn config(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Bare(_) => None,
            Self::Detailed { config, .. } => config.as_ref(),
        }
    }
}

/// An ordered plugin chain with its sharing mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginChain {
    /// How the chain participates in tenant-hierarchy merging.
    pub sharing: SharingMode,
    /// Plugins in execution order (upstream plugins run before route plugins).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_mode_wire_format() {
        assert_eq!(
            serde_json::to_value(SharingMode::Enforce).unwrap(),
            "enforce"
        );
        assert_eq!(SharingMode::default(), SharingMode::Private);
    }

    #[test]
    fn rate_limit_defaults() {
        let cfg: RateLimitConfig = serde_json::from_str(r#"{"sustained": {"rate": 5}}"#).unwrap();
        assert_eq!(cfg.sustained.window, RateWindow::Second);
        assert_eq!(cfg.algorithm, RateAlgorithm::TokenBucket);
        assert_eq!(cfg.scope, RateScope::Tenant);
        assert_eq!(cfg.strategy, RateStrategy::Reject);
        assert_eq!(cfg.cost, 1);
        assert_eq!(cfg.effective_burst(), 5);
    }

    #[test]
    fn plugin_bindings_accept_both_wire_forms() {
        let bare: PluginBinding = serde_json::from_str(
            r#""gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1""#,
        )
        .unwrap();
        assert_eq!(
            bare.plugin_ref(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert!(bare.config().is_none());

        let detailed: PluginBinding = serde_json::from_str(
            r#"{"plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                "config": {"required_request_headers": "x-correlation-id"}}"#,
        )
        .unwrap();
        assert_eq!(
            detailed
                .config()
                .and_then(|c| c.get("required_request_headers"))
                .and_then(|v| v.as_str()),
            Some("x-correlation-id")
        );
    }

    #[test]
    fn cors_defaults_use_schema_default_methods() {
        let cors: CorsConfig = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert_eq!(
            cors.allowed_methods,
            vec!["GET".to_owned(), "POST".to_owned()]
        );
    }
}
