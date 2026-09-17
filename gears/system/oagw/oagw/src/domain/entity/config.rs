//! Hierarchical configuration value types for upstream/route entities.
//!
//! These value types mirror the JSON shape of the upstream/route schemas and
//! the `JSONB` configuration blobs of the §3.7 relational tables.  They carry
//! the per-field sharing modes that drive the hierarchical configuration merge
//! (DoD `cpt-cf-oagw-dod-domain-model-repositories-merge`).

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

/// Sharing mode for a hierarchical configuration field
/// (`private` | `inherit` | `enforce`).
///
/// - `private`:  not visible to descendants;
/// - `inherit`:  descendants can override (per field-specific rule);
/// - `enforce`:  descendants cannot override the ancestor value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    #[default]
    Private,
    Inherit,
    Enforce,
}

impl SharingMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        }
    }
}

/// Protocol used to connect to the upstream service.
///
/// Only HTTP is reachable in this delivery; gRPC is Phase 3 and reserved
/// (DoD `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpstreamProtocol {
    #[default]
    Http,
    /// Reserved (Phase 3); no gRPC proxy code path is implemented or reachable.
    Grpc,
}

impl UpstreamProtocol {
    /// The canonical GTS protocol identifier.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            Self::Grpc => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        }
    }
}

impl std::fmt::Display for UpstreamProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.gts_id())
    }
}

impl std::str::FromStr for UpstreamProtocol {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" => Ok(Self::Http),
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1" => Ok(Self::Grpc),
            other => Err(format!("unknown upstream protocol: {other}")),
        }
    }
}

impl<'de> Deserialize<'de> for UpstreamProtocol {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for UpstreamProtocol {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.gts_id())
    }
}

/// Endpoint scheme of an upstream server pool member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EndpointScheme {
    #[default]
    Https,
    /// Plaintext HTTP — only accepted at the configuration boundary when
    /// `allow_http_upstream` is enabled (testing only).
    Http,
    Wss,
    /// WebTransport (TLS-based).
    Wt,
    /// gRPC (TLS-based; reserved for Phase 3).
    Grpc,
}

impl EndpointScheme {
    /// Whether this scheme is a TLS-protected scheme always in the SSRF
    /// scheme allowlist.
    #[must_use]
    pub const fn is_tls(self) -> bool {
        matches!(self, Self::Https | Self::Wss | Self::Wt | Self::Grpc)
    }
}

/// Time window for a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Allows bursts.
    #[default]
    TokenBucket,
    /// Prevents boundary bursts.
    SlidingWindow,
}

/// Scope for rate-limit counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    #[default]
    Global,
    Tenant,
    User,
    Ip,
    Route,
}

/// Behavior when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Sustained rate: tokens replenished per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Time window for the sustained rate.
    pub window: RateLimitWindow,
}

impl Default for SustainedRate {
    fn default() -> Self {
        Self {
            rate: 0,
            window: RateLimitWindow::Second,
        }
    }
}

/// Burst capacity for the token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BurstConfig {
    /// Maximum burst size (bucket capacity). Defaults to `sustained.rate`
    /// when not specified.
    pub capacity: Option<u64>,
}

/// Dual-rate token-bucket configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    pub sharing: SharingMode,
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedRate,
    pub burst: Option<BurstConfig>,
    pub scope: RateLimitScope,
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request (weighted endpoints).
    pub cost: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 1,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        }
    }
}

/// CORS configuration (per upstream/route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CorsConfig {
    pub sharing: SharingMode,
    pub enabled: bool,
    /// Allowed origins: `*` or URI origins.
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods (e.g. `GET`, `POST`).
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond CORS-safelisted headers.
    pub expose_headers: Vec<String>,
    /// Allow credentials (requires specific origins, not `*`).
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// Inbound header forwarding mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    /// Forward no inbound headers beyond defaults.
    #[default]
    None,
    Allowlist,
    All,
}

/// Header transformation rules for inbound requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestHeadersConfig {
    /// Headers to set (overwrite if exists).
    pub set: BTreeMap<String, String>,
    /// Headers to add (append; duplicates allowed).
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    pub passthrough: PassthroughMode,
    /// Headers to forward when `passthrough` is `allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

/// Header transformation rules for outbound responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponseHeadersConfig {
    /// Headers to set on the response to the client.
    pub set: BTreeMap<String, String>,
    /// Headers to add to the response.
    pub add: BTreeMap<String, String>,
    /// Headers to strip from the upstream response.
    pub remove: Vec<String>,
}

/// Header transformation rules for requests/responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadersConfig {
    pub request: RequestHeadersConfig,
    pub response: ResponseHeadersConfig,
}

/// An ordered plugin binding (mirrors `oagw_{upstream,route}_plugin` rows).
///
/// `plugin_ref` stores the canonical GTS plugin identifier; `plugin_uuid` is
/// set only for UUID-backed custom plugins.  Named (built-in) plugins have no
/// `oagw_plugin` row and therefore `plugin_uuid == None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginBinding {
    /// Execution position (contiguous from 0) within its parent.
    pub position: u32,
    /// Canonical GTS plugin identifier, e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`.
    pub plugin_ref: String,
    /// UUID of the custom plugin when `plugin_ref` names a UUID-backed
    /// custom plugin; `None` for named plugins.
    pub plugin_uuid: Option<uuid::Uuid>,
    /// Plugin configuration blob.
    pub config: JsonValue,
}

impl Default for PluginBinding {
    fn default() -> Self {
        Self {
            position: 0,
            plugin_ref: String::new(),
            plugin_uuid: None,
            config: JsonValue::Null,
        }
    }
}

/// Plugin chain configuration with a sharing mode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    pub sharing: SharingMode,
    /// The ordered plugin bindings (contiguous positions from 0).
    pub items: Vec<PluginBinding>,
}

impl PluginsConfig {
    /// Appends descendant bindings after ancestor bindings, preserving order
    /// (ancestor first).  Enforced ancestor chains are concatenated and never
    /// removed by descendants.
    #[must_use]
    pub fn concat_ancestor(&self, ancestor: &PluginsConfig) -> PluginsConfig {
        let mut items = ancestor.items.clone();
        // Re-derive contiguous positions across the concatenated chain.
        for (i, binding) in self.items.iter().enumerate() {
            let mut b = binding.clone();
            b.position = u32::try_from(items.len() + i).unwrap_or(u32::MAX);
            items.push(b);
        }
        PluginsConfig {
            sharing: if ancestor.sharing == SharingMode::Enforce {
                SharingMode::Enforce
            } else {
                self.sharing
            },
            items,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_mode_round_trips() {
        for (raw, mode) in [
            ("private", SharingMode::Private),
            ("inherit", SharingMode::Inherit),
            ("enforce", SharingMode::Enforce),
        ] {
            let v: SharingMode = serde_json::from_str(&format!("\"{raw}\"")).unwrap();
            assert_eq!(v, mode);
            assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{raw}\""));
        }
    }

    #[test]
    fn endpooint_scheme_tls_classification() {
        assert!(EndpointScheme::Https.is_tls());
        assert!(EndpointScheme::Wss.is_tls());
        assert!(EndpointScheme::Wt.is_tls());
        assert!(EndpointScheme::Grpc.is_tls());
        assert!(!EndpointScheme::Http.is_tls());
    }

    #[test]
    fn protocol_gts_ids_parse_and_serialize() {
        assert_eq!(
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
                .parse::<UpstreamProtocol>()
                .unwrap(),
            UpstreamProtocol::Http
        );
        assert_eq!(
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"
                .parse::<UpstreamProtocol>()
                .unwrap(),
            UpstreamProtocol::Grpc
        );
        assert!("bogus".parse::<UpstreamProtocol>().is_err());
        let json = serde_json::to_string(&UpstreamProtocol::Http).unwrap();
        assert_eq!(
            json,
            "\"gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1\""
        );
    }

    #[test]
    fn rate_limit_defaults_from_schema() {
        let cfg = RateLimitConfig::default();
        assert_eq!(cfg.sharing, SharingMode::Private);
        assert_eq!(cfg.algorithm, RateLimitAlgorithm::TokenBucket);
        assert_eq!(cfg.scope, RateLimitScope::Tenant);
        assert_eq!(cfg.strategy, RateLimitStrategy::Reject);
        assert_eq!(cfg.cost, 1);
        assert_eq!(cfg.sustained.window, RateLimitWindow::Second);
    }

    #[test]
    fn cors_defaults_from_schema() {
        let cfg = CorsConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.allowed_methods, vec!["GET", "POST"]);
        assert!(!cfg.allow_credentials);
    }

    #[test]
    fn plugin_concat_puts_ancestor_first_with_contiguous_positions() {
        let mut ancestor = PluginsConfig::default();
        ancestor.items.push(PluginBinding {
            position: 0,
            plugin_ref: "a".to_owned(),
            plugin_uuid: None,
            config: JsonValue::Null,
        });
        let mut descendant = PluginsConfig::default();
        descendant.items.push(PluginBinding {
            position: 0,
            plugin_ref: "b".to_owned(),
            plugin_uuid: None,
            config: JsonValue::Null,
        });

        let merged = descendant.concat_ancestor(&ancestor);
        assert_eq!(merged.items.len(), 2);
        assert_eq!(merged.items[0].plugin_ref, "a");
        assert_eq!(merged.items[0].position, 0);
        assert_eq!(merged.items[1].plugin_ref, "b");
        assert_eq!(merged.items[1].position, 1);
    }

    #[test]
    fn value_types_deserialize_from_object_with_defaults() {
        let json = serde_json::json!({});
        let cfg: RateLimitConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg, RateLimitConfig::default());
    }
}
