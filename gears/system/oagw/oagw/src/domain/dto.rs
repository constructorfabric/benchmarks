//! Domain model for the `oagw` gear.
//!
//! Shapes follow `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` exactly; `http` is additionally
//! accepted as an endpoint scheme (the scheme *field* is a data question,
//! whether a plaintext connection is made is governed by
//! [`crate::config::OagwConfig::allow_http_upstream`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::gts_helpers;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Hierarchical sharing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    /// One counter for the whole gear.
    Global,
    /// One counter per tenant (the default).
    #[default]
    Tenant,
    /// One counter per authenticated user.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Behaviour when a limit is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    /// Refuse the request (429).
    #[default]
    Reject,
    /// Queue the request (treated as reject).
    Queue,
    /// Degrade the response (treated as reject).
    Degrade,
}

/// Refill window of a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    /// Window length in seconds.
    pub fn seconds(self) -> u64 {
        match self {
            RateWindow::Second => 1,
            RateWindow::Minute => 60,
            RateWindow::Hour => 3_600,
            RateWindow::Day => 86_400,
        }
    }
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Which inbound headers are forwarded upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderPassthrough {
    /// Drop every inbound header.
    #[default]
    None,
    /// Forward only the headers in `passthrough_allowlist`.
    Allowlist,
    /// Forward every inbound header.
    All,
}

/// Endpoint transport scheme.
///
/// `http` is a legal value at create time; whether a plaintext connection is
/// actually established is governed by the gear-level
/// `allow_http_upstream` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointScheme {
    Http,
    Https,
    Wss,
    Wt,
    Grpc,
}

impl EndpointScheme {
    /// Default port for the scheme when the endpoint does not state one.
    pub fn default_port(self) -> u16 {
        match self {
            EndpointScheme::Http => 80,
            EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt => 443,
            EndpointScheme::Grpc => 443,
        }
    }

    /// `true` for the TLS-carrying schemes.
    pub fn is_tls(self) -> bool {
        matches!(self, EndpointScheme::Https | EndpointScheme::Wss)
    }

    /// The wire representation.
    pub fn as_str(self) -> &'static str {
        match self {
            EndpointScheme::Http => "http",
            EndpointScheme::Https => "https",
            EndpointScheme::Wss => "wss",
            EndpointScheme::Wt => "wt",
            EndpointScheme::Grpc => "grpc",
        }
    }

    /// Parses a scheme, accepting the documented set plus `http`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "http" => Some(EndpointScheme::Http),
            "https" => Some(EndpointScheme::Https),
            "wss" => Some(EndpointScheme::Wss),
            "wt" => Some(EndpointScheme::Wt),
            "grpc" => Some(EndpointScheme::Grpc),
            _ => None,
        }
    }
}

impl Serialize for EndpointScheme {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EndpointScheme {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        EndpointScheme::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown endpoint scheme: {s}")))
    }
}

/// Upstream protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    #[default]
    Http,
    Grpc,
}

impl Protocol {
    /// The GTS protocol identifier.
    pub fn as_gts(self) -> &'static str {
        match self {
            Protocol::Http => gts_helpers::PROTOCOL_HTTP,
            Protocol::Grpc => gts_helpers::PROTOCOL_GRPC,
        }
    }

    /// Parses a protocol from its GTS identifier, tolerating the short
    /// `http` / `grpc` spellings as well.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            gts_helpers::PROTOCOL_HTTP => Some(Protocol::Http),
            gts_helpers::PROTOCOL_GRPC => Some(Protocol::Grpc),
            "http" => Some(Protocol::Http),
            "grpc" => Some(Protocol::Grpc),
            _ => None,
        }
    }
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_gts())
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Protocol::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown protocol: {s}")))
    }
}

/// HTTP method accepted by a route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    GET,
    POST,
    PUT,
    DELETE,
    PATCH,
}

impl HttpMethod {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::GET => "GET",
            HttpMethod::POST => "POST",
            HttpMethod::PUT => "PUT",
            HttpMethod::DELETE => "DELETE",
            HttpMethod::PATCH => "PATCH",
        }
    }

    /// Parses an HTTP method accepted by a route match.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "GET" => Some(HttpMethod::GET),
            "POST" => Some(HttpMethod::POST),
            "PUT" => Some(HttpMethod::PUT),
            "DELETE" => Some(HttpMethod::DELETE),
            "PATCH" => Some(HttpMethod::PATCH),
            _ => None,
        }
    }
}

/// How the proxy URL's path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject a supplied suffix.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

// ---------------------------------------------------------------------------
// Value objects
// ---------------------------------------------------------------------------

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    #[serde(default = "Endpoint::default_scheme")]
    pub scheme: EndpointScheme,
    pub host: String,
    #[serde(default = "Endpoint::default_port")]
    pub port: u16,
}

impl Endpoint {
    fn default_scheme() -> EndpointScheme {
        EndpointScheme::Https
    }

    fn default_port() -> u16 {
        443
    }

    /// Effective port: the stated one, or the scheme default when zero.
    pub fn effective_port(&self) -> u16 {
        if self.port == 0 {
            self.scheme.default_port()
        } else {
            self.port
        }
    }

    /// `host[:port]` when the port differs from the scheme default.
    pub fn host_with_port(&self) -> String {
        if self.effective_port() == self.scheme.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.effective_port())
        }
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            scheme: EndpointScheme::Https,
            host: String::new(),
            port: 443,
        }
    }
}

/// The `server` object of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub endpoints: Vec<Endpoint>,
}

/// Credential injection configured on an upstream.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Request-side header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRequestRules {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, skip_serializing_if = "HeaderPassthrough::is_none")]
    pub passthrough: HeaderPassthrough,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

impl HeaderPassthrough {
    /// `true` for [`HeaderPassthrough::None`].
    pub fn is_none(&self) -> bool {
        matches!(self, HeaderPassthrough::None)
    }
}

/// Response-side header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderResponseRules {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<HeaderRequestRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<HeaderResponseRules>,
}

/// A bound plugin reference (built-in GTS id, or a persisted plugin UUID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRef {
    /// A named, built-in plugin identified by GTS id.
    Builtin(String),
    /// A tenant-defined plugin referenced by UUID.
    Custom(Uuid),
}

impl PluginRef {
    /// The reference as it appears on the wire.
    pub fn as_ref_str(&self) -> String {
        match self {
            PluginRef::Builtin(s) => s.clone(),
            PluginRef::Custom(u) => u.to_string(),
        }
    }
}

impl std::fmt::Display for PluginRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_ref_str())
    }
}

/// The `plugins` object of an upstream or a route.
///
/// An item is either a bare plugin reference — the schema's string form, which
/// carries no configuration — or the `{plugin_ref, config}` object ADR 0009
/// binds, whose configuration travels with the binding. Both are accepted, and
/// a binding without a configuration is rendered as the bare string again so
/// the two forms round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "PluginSetWire", into = "PluginSetWire")]
pub struct PluginSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// Configuration carried by a `{plugin_ref, config}` binding, keyed by the
    /// reference it belongs to.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub configs: BTreeMap<String, serde_json::Value>,
}

impl PluginSet {
    /// The configuration a binding carries, if it carried one.
    pub fn config_of(&self, plugin_ref: &str) -> Option<&serde_json::Value> {
        self.configs.get(plugin_ref)
    }
}

/// The wire shape of a plugin set: strings and binding objects may be mixed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct PluginSetWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sharing: Option<SharingMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    items: Vec<PluginBindingWire>,
}

/// One entry of `plugins.items` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PluginBindingWire {
    /// A bare reference: the schema's string form.
    Reference(String),
    /// A reference with the configuration bound to it (ADR 0009).
    Bound { plugin_ref: String, config: serde_json::Value },
}

impl<'de> Deserialize<'de> for PluginBindingWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PluginBindingWire;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a plugin reference or a `{plugin_ref, config}` object")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(PluginBindingWire::Reference(value.to_string()))
            }
            fn visit_map<A>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut plugin_ref: Option<String> = None;
                let mut config: Option<serde_json::Value> = None;
                while let Some(key) = access.next_key::<String>()? {
                    match key.as_str() {
                        "plugin_ref" => plugin_ref = Some(access.next_value()?),
                        "config" => config = Some(access.next_value()?),
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["plugin_ref", "config"],
                            ))
                        }
                    }
                }
                let plugin_ref = plugin_ref
                    .ok_or_else(|| serde::de::Error::missing_field("plugin_ref"))?;
                Ok(PluginBindingWire::Bound {
                    plugin_ref,
                    config: config.unwrap_or(serde_json::Value::Null),
                })
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for PluginBindingWire {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PluginBindingWire::Reference(reference) => serializer.serialize_str(reference),
            PluginBindingWire::Bound { plugin_ref, config } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("plugin_ref", plugin_ref)?;
                map.serialize_entry("config", config)?;
                map.end()
            }
        }
    }
}

impl From<PluginSetWire> for PluginSet {
    fn from(wire: PluginSetWire) -> Self {
        let mut configs = BTreeMap::new();
        let mut items = Vec::new();
        for binding in wire.items {
            match binding {
                PluginBindingWire::Reference(reference) => items.push(reference),
                PluginBindingWire::Bound { plugin_ref, config } => {
                    configs.insert(plugin_ref.clone(), config);
                    items.push(plugin_ref);
                }
            }
        }
        Self {
            sharing: wire.sharing,
            items,
            configs,
        }
    }
}

impl From<PluginSet> for PluginSetWire {
    fn from(set: PluginSet) -> Self {
        Self {
            sharing: set.sharing,
            items: set
                .items
                .into_iter()
                .map(|reference| match set.configs.get(&reference) {
                    Some(config) => PluginBindingWire::Bound {
                        plugin_ref: reference,
                        config: config.clone(),
                    },
                    None => PluginBindingWire::Reference(reference),
                })
                .collect(),
        }
    }
}

/// Sustained refill rate.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sustained {
    pub rate: u64,
    #[serde(default)]
    pub window: RateWindow,
}

/// Bucket capacity of a burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Burst {
    #[serde(default)]
    pub capacity: u64,
}

/// Rate-limiting configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    pub sustained: Sustained,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    #[serde(default)]
    pub scope: RateScope,
    #[serde(default)]
    pub strategy: RateStrategy,
    #[serde(default = "default_cost")]
    pub cost: u64,
    /// Whether proxied responses carry `X-RateLimit-*` headers.
    #[serde(default = "crate::domain::dto::default_true")]
    pub response_headers: bool,
}

fn default_cost() -> u64 {
    1
}

impl RateLimit {
    /// Bucket capacity: the burst capacity, or the sustained rate.
    pub fn capacity(&self) -> u64 {
        self.burst
            .and_then(|b| if b.capacity == 0 { None } else { Some(b.capacity) })
            .unwrap_or(self.sustained.rate.max(1))
    }

    /// Tokens replenished per second.
    pub fn refill_rate(&self) -> f64 {
        let secs = self.sustained.window.seconds().max(1) as f64;
        self.sustained.rate as f64 / secs
    }
}

/// CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cors {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_string(), "POST".to_string()]
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// A tenant-owned upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Upstream {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default = "crate::domain::dto::default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
}

pub(crate) fn default_true() -> bool {
    true
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: Server::default(),
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }
}

impl Upstream {
    /// The plugin binding list, empty when absent.
    pub fn plugin_refs(&self) -> Vec<PluginRef> {
        self.plugins
            .as_ref()
            .map(|p| p.items.iter().filter_map(|s| parse_plugin_ref(s)).collect())
            .unwrap_or_default()
    }

    /// The effective alias (already normalised by the service layer).
    pub fn alias_str(&self) -> &str {
        self.alias.as_deref().unwrap_or("")
    }
}

/// Parses a plugin reference into its built-in / custom variant.
pub fn parse_plugin_ref(value: &str) -> Option<PluginRef> {
    if value.is_empty() {
        return None;
    }
    if value.contains('~') {
        return Some(PluginRef::Builtin(value.to_string()));
    }
    Uuid::parse_str(value).ok().map(PluginRef::Custom)
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct HttpMatch {
    /// The method allowlist, as an upper-case verb. A method the gateway does
    /// not know is a validation error at the control plane, not a
    /// deserialisation failure, so this stays a string on the wire.
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: Vec::new(),
            path: String::new(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }
}

/// gRPC match rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub method: String,
}

/// Inbound matching rules for a route (exactly one of `http` / `grpc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MatchRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl Default for MatchRule {
    fn default() -> Self {
        Self { http: None, grpc: None }
    }
}

impl MatchRule {
    /// The single match variant, when exactly one is present.
    pub fn exactly_one(&self) -> Result<MatchKind, String> {
        match (&self.http, &self.grpc) {
            (Some(_), Some(_)) => Err("only one of `match.http` or `match.grpc` may be set".into()),
            (None, None) => Err("one of `match.http` or `match.grpc` is required".into()),
            (Some(_), None) => Ok(MatchKind::Http),
            (None, Some(_)) => Ok(MatchKind::Grpc),
        }
    }
}

/// Which match variant a route declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Http,
    Grpc,
}

/// A tenant-owned route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Whether the route participates in matching (PRD: a disabled route is
    /// excluded from route matching).
    #[serde(default = "crate::domain::dto::default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub upstream_id: String,
    #[serde(rename = "match", default)]
    pub match_rule: MatchRule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: None,
            enabled: true,
            tags: Vec::new(),
            upstream_id: String::new(),
            match_rule: MatchRule::default(),
            plugins: None,
            rate_limit: None,
        }
    }
}
