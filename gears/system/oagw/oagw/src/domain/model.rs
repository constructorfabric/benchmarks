//! Domain model: persisted configuration entities and their value types.
//!
//! The entities are tenant-scoped rows keyed by a server-generated UUID; their
//! GTS instance ids (`gts.cf.core.oagw.<type>.v1~<uuid>`) are derived on the
//! wire, never stored.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use toolkit_gts::gts_id;

// ── GTS base types ──────────────────────────────────────────────────────────

/// Base type of an upstream row.
pub const UPSTREAM_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// Base type of a route row.
pub const ROUTE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// Base type of a custom auth plugin row.
pub const AUTH_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// Base type of a custom guard plugin row.
pub const GUARD_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// Base type of a custom transform plugin row.
pub const TRANSFORM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");
/// Generic plugin base type, used only as a diagnostic fallback when a raw
/// resource id carries no plugin base-type prefix at all.
pub const PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.plugin.v1~");
/// Proxy resource type used for the `invoke` permission.
pub const PROXY_TYPE: &str = gts_id!("cf.core.oagw.proxy.v1~");

/// HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// gRPC upstream protocol (Phase 3; accepted but not proxied yet).
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

/// Canonical GTS ids for the management errors that have no `cf.oagw.*`
/// identity of their own. Spelled without the trailing type marker, matching
/// the `type` shape used by every `cf.oagw.*` identity in `DESIGN` §3.3.
pub mod canonical_types {
    use toolkit_gts::gts_id;

    /// Resource does not exist for the calling tenant.
    pub const NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.core.err.not_found.v1");
    /// Uniqueness conflict.
    pub const ALREADY_EXISTS: &str = gts_id!("cf.core.errors.err.v1~cf.core.err.already_exists.v1");
    /// Malformed or out-of-range query parameter.
    pub const INVALID_ARGUMENT: &str =
        gts_id!("cf.core.errors.err.v1~cf.core.err.invalid_argument.v1");
    /// Management permission missing.
    pub const PERMISSION_DENIED: &str =
        gts_id!("cf.core.errors.err.v1~cf.core.err.permission_denied.v1");
    /// A dependency is unavailable.
    pub const SERVICE_UNAVAILABLE: &str =
        gts_id!("cf.core.errors.err.v1~cf.core.err.service_unavailable.v1");
    /// Unexpected internal failure.
    pub const INTERNAL: &str = gts_id!("cf.core.errors.err.v1~cf.core.err.internal.v1");
}

/// Build the GTS instance id of a resource from its base type and UUID.
#[must_use]
pub fn resource_gts_id(base_type: &str, id: uuid::Uuid) -> String {
    format!("{base_type}{id}")
}

/// Extract the UUID tail of an anonymous GTS resource id.
///
/// `gts.cf.core.oagw.upstream.v1~3f2c1b2a-…` → the UUID. Returns `None` when
/// the instance part is not a UUID, which is how catalog-only identifiers are
/// told apart from row ids.
#[must_use]
pub fn parse_resource_uuid(id: &str) -> Option<uuid::Uuid> {
    let tail = id.rsplit('~').next().unwrap_or(id);
    if tail.len() != 36 {
        return None;
    }
    uuid::Uuid::parse_str(tail).ok()
}

/// The plugin base type a raw resource id carries, or the generic
/// [`PLUGIN_TYPE`] when it carries none at all.
///
/// Used to spell a `404`'s `resource` the way the caller addressed it, so a
/// missing guard plugin is not reported as an auth plugin.
#[must_use]
pub fn plugin_base_type_of(id: &str) -> &'static str {
    for base in [GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE, AUTH_PLUGIN_TYPE] {
        if id.starts_with(base) {
            return base;
        }
    }
    PLUGIN_TYPE
}

/// Parse a resource id that must carry a specific GTS base type.
///
/// # Errors
/// Returns [`crate::domain::error::DomainError::Validation`] when `id` is not
/// an anonymous GTS id of `base_type` with a UUID tail.
pub fn parse_typed_resource_id(
    id: &str,
    base_type: &str,
) -> Result<uuid::Uuid, crate::domain::error::DomainError> {
    let tail = id.strip_prefix(base_type).ok_or_else(|| {
        crate::domain::error::DomainError::validation(format!(
            "'{id}' is not an anonymous GTS id of the form {base_type}<uuid>"
        ))
    })?;
    uuid::Uuid::parse_str(tail).map_err(|_| {
        crate::domain::error::DomainError::validation(format!(
            "'{id}' is not an anonymous GTS id of the form {base_type}<uuid>"
        ))
    })
}

/// Default `enabled` flag of a create request.
#[must_use]
pub fn default_enabled() -> bool {
    true
}

// ── Timestamps ──────────────────────────────────────────────────────────────

/// Wall-clock seconds since the Unix epoch, saturating on clock error.
#[must_use]
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Render epoch seconds as an RFC 3339 UTC timestamp.
///
/// Ported date arithmetic: the divisions below are exact on their operands and
/// the day count never overflows `i64` for any representable epoch second.
#[allow(
    clippy::integer_division,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
#[must_use]
pub fn format_rfc3339(epoch_secs: u64) -> String {
    let (year, month, day) = civil_from_days((epoch_secs / 86_400) as i64);
    let secs_of_day = epoch_secs % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to `(y, m, d)`.
#[allow(
    clippy::integer_division,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

// ── Value types ─────────────────────────────────────────────────────────────

/// Endpoint scheme. Mirrors the `scheme` enum of `upstream.v1.schema.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum EndpointScheme {
    /// TLS HTTP (default).
    #[serde(rename = "https")]
    #[default]
    Https,
    /// TLS WebSocket.
    #[serde(rename = "wss")]
    Wss,
    /// TLS WebTransport.
    #[serde(rename = "wt")]
    Wt,
    /// TLS gRPC.
    #[serde(rename = "grpc")]
    Grpc,
    /// Plaintext HTTP. Only reachable on the data plane when the deployment
    /// sets `allow_http_upstream`; the management API keeps requiring `https`
    /// endpoints for `http` upstreams (`DESIGN` §2.2, HTTPS-only constraint).
    #[serde(rename = "http")]
    Http,
    /// Plaintext WebSocket. Same restriction as [`EndpointScheme::Http`].
    #[serde(rename = "ws")]
    Ws,
}

impl EndpointScheme {
    /// Port omitted from a derived alias for this scheme.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
            Self::Http | Self::Ws => 80,
        }
    }

    /// Wire spelling of the scheme, as it appears in an endpoint URL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
            Self::Http => "http",
            Self::Ws => "ws",
        }
    }

    /// `true` when the scheme speaks HTTP/1.1 or HTTP/2 cleartext framing over
    /// TLS, i.e. when an HTTP client can be used.
    #[must_use]
    pub const fn is_http(self) -> bool {
        matches!(self, Self::Https | Self::Wss | Self::Http | Self::Ws)
    }

    /// `true` when the scheme already implies a TLS session.
    #[must_use]
    pub const fn is_tls(self) -> bool {
        !matches!(self, Self::Http | Self::Ws)
    }
}

/// One pooled upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Wire scheme; defaults to `https`.
    #[serde(default = "default_scheme")]
    pub scheme: EndpointScheme,
    /// RFC 1123 hostname, or an IPv4/IPv6 literal.
    pub host: String,
    /// TCP port; defaults to `443`.
    #[serde(default = "default_port")]
    pub port: u16,
}

/// Default endpoint scheme.
#[must_use]
pub fn default_scheme() -> EndpointScheme {
    EndpointScheme::Https
}

/// Default endpoint port.
#[must_use]
pub fn default_port() -> u16 {
    443
}

/// Default tokens consumed per request.
#[must_use]
pub fn default_cost() -> u64 {
    1
}

/// Pool of endpoints an upstream resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoints forming the pool. `minItems: 1`.
    pub endpoints: Vec<Endpoint>,
}

/// Upstream protocol, carried on the wire as a GTS instance id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum Protocol {
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// The GTS instance id this protocol serializes to.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }
}

/// Hierarchical sharing mode (`DESIGN` §3.2, Hierarchical Configuration).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum SharingMode {
    /// Owner only.
    #[serde(rename = "private")]
    #[default]
    Private,
    /// Descendants may override.
    #[serde(rename = "inherit")]
    Inherit,
    /// Descendants inherit verbatim and may not override.
    #[serde(rename = "enforce")]
    Enforce,
}

/// Outbound request header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite when present).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append; duplicates allowed).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to strip from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<Passthrough>,

    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Inbound response header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response to the client.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Which inbound headers are forwarded upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum Passthrough {
    /// None (default).
    #[serde(rename = "none")]
    #[default]
    None,
    /// Only `passthrough_allowlist`.
    #[serde(rename = "allowlist")]
    Allowlist,
    /// All inbound headers except routing and hop-by-hop headers.
    #[serde(rename = "all")]
    All,
}

/// Header transformation configuration of an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Request-side rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    /// Response-side rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// Sustained rate of a token bucket / sliding window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per `window`.
    pub rate: u64,
    /// Window the rate applies to.
    #[serde(default)]
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

/// Time unit of a sustained rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateWindow {
    /// One second (default).
    #[serde(rename = "second")]
    #[default]
    Second,
    /// One minute.
    #[serde(rename = "minute")]
    Minute,
    /// One hour.
    #[serde(rename = "hour")]
    Hour,
    /// One day.
    #[serde(rename = "day")]
    Day,
}

/// Burst capacity of a token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Burst {
    /// Maximum burst size; defaults to `sustained.rate`.
    pub capacity: u64,
}

/// Rate-limiting configuration (later slice; carried and validated here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode for hierarchical merge.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Behaviour when the limit is exceeded.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u64,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateAlgorithm {
    /// Token bucket (default).
    #[serde(rename = "token_bucket")]
    #[default]
    TokenBucket,
    /// Sliding window.
    #[serde(rename = "sliding_window")]
    SlidingWindow,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateScope {
    /// One global counter.
    #[serde(rename = "global")]
    Global,
    /// One counter per tenant (default).
    #[serde(rename = "tenant")]
    #[default]
    Tenant,
    /// One counter per subject.
    #[serde(rename = "user")]
    User,
    /// One counter per client IP.
    #[serde(rename = "ip")]
    Ip,
    /// One counter per route.
    #[serde(rename = "route")]
    Route,
}

/// Rate-limit overrun strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateStrategy {
    /// Reject the request (default).
    #[serde(rename = "reject")]
    #[default]
    Reject,
    /// Queue the request.
    #[serde(rename = "queue")]
    Queue,
    /// Degrade the response.
    #[serde(rename = "degrade")]
    Degrade,
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
            cost: default_cost(),
        }
    }
}

/// CORS configuration (later slice; carried and validated here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode for hierarchical merge.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Enable CORS for this upstream/route.
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials. Requires specific origins, never `*`.
    #[serde(default)]
    pub allow_credentials: bool,
}

/// One entry of a plugin chain.
///
/// Both wire spellings of the schemas are accepted:
///
/// * the bare id form — `"gts.cf.core.oagw.guard_plugin.v1~3f2c…"`;
/// * the binding object form of `ADR`-0009 —
///   `{"plugin_ref": "…", "config": {"required_request_headers": "…"}}`.
///
/// The object form is parsed by hand rather than derived: `#[serde(untagged)]`
/// would silently ignore unknown object keys, and a misspelled `config` key
/// must fail the request instead of being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub enum PluginRef {
    /// A bare plugin id.
    Id(String),
    /// A plugin id with its own configuration.
    Binding {
        /// GTS id of the bound plugin.
        plugin_ref: String,
        /// Per-binding configuration the plugin consumes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<serde_json::Value>,
    },
}

impl PluginRef {
    /// The GTS id of the bound plugin, whichever form carries it.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Id(id) | Self::Binding { plugin_ref: id, .. } => id,
        }
    }

    /// The per-binding configuration, when the entry is an object.
    #[must_use]
    pub const fn config(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Id(_) => None,
            Self::Binding { config, .. } => config.as_ref(),
        }
    }
}

impl From<String> for PluginRef {
    fn from(id: String) -> Self {
        Self::Id(id)
    }
}

impl From<&str> for PluginRef {
    fn from(id: &str) -> Self {
        Self::Id(id.to_owned())
    }
}

impl<'de> serde::Deserialize<'de> for PluginRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(id) => Ok(Self::Id(id)),
            serde_json::Value::Object(map) => {
                for key in map.keys() {
                    if key != "plugin_ref" && key != "config" {
                        return Err(D::Error::custom(format!(
                            "plugins.items binding accepts only 'plugin_ref' and 'config', \
                             got '{key}'"
                        )));
                    }
                }
                let reference = map.get("plugin_ref").ok_or_else(|| {
                    D::Error::custom("plugins.items binding requires a 'plugin_ref' string")
                })?;
                let plugin_ref = reference.as_str().ok_or_else(|| {
                    D::Error::custom("'plugin_ref' must be a GTS plugin identifier string")
                })?;
                Ok(Self::Binding {
                    plugin_ref: plugin_ref.to_owned(),
                    config: map.get("config").cloned(),
                })
            }
            other => Err(D::Error::custom(format!(
                "plugins.items must be a plugin GTS id or a {{plugin_ref, config}} binding, \
                 got {other}"
            ))),
        }
    }
}

/// Plugin chain configuration of an upstream or route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Sharing mode for the plugin chain.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Bound plugins: built-in GTS ids, custom plugin references, or
    /// `{plugin_ref, config}` bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginRef>,
}

/// Authentication configuration of an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Auth plugin identity: a built-in GTS id or a custom plugin UUID.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    pub plugin_type: Option<String>,
    /// Sharing mode for hierarchical merge.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Auth plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

// ── Entities ────────────────────────────────────────────────────────────────

/// A tenant-scoped external service target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// Server-generated id.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Routing key; immutable once set.
    pub alias: String,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Disabled upstreams reject every request.
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Outbound auth configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Flat categorization tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation instant (RFC 3339, UTC).
    pub created_at: String,
    /// Last write instant (RFC 3339, UTC).
    pub updated_at: String,
}

/// Protocol-scoped inbound matching rules of a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// HTTP match rules (upstream protocol `http`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules (upstream protocol `grpc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Allowed methods; `minItems: 1`.
    pub methods: Vec<HttpMethod>,
    /// Path pattern.
    pub path: String,
    /// Allowed query parameters; an empty list allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How the `/{path_suffix}` of the proxy URL is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (Phase 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// HTTP methods a route can allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
pub enum HttpMethod {
    /// `GET`
    #[serde(rename = "GET")]
    Get,
    /// `POST`
    #[serde(rename = "POST")]
    Post,
    /// `PUT`
    #[serde(rename = "PUT")]
    Put,
    /// `DELETE`
    #[serde(rename = "DELETE")]
    Delete,
    /// `PATCH`
    #[serde(rename = "PATCH")]
    Patch,
}

impl HttpMethod {
    /// Uppercase wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

/// Treatment of the proxy path suffix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum PathSuffixMode {
    /// Reject a path suffix.
    #[serde(rename = "disabled")]
    Disabled,
    /// Append the suffix to the route path (default).
    #[serde(rename = "append")]
    #[default]
    Append,
}

/// A route: a match rule bound to an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Server-generated UUID.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Owning upstream; immutable.
    pub upstream_id: uuid::Uuid,
    /// Match rules; exactly one of `http`/`grpc`.
    pub r#match: MatchConfig,
    /// Higher priority wins when several routes match.
    pub priority: u32,
    /// Disabled routes never match.
    pub enabled: bool,
    /// Route-level rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Flat categorization tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation instant (RFC 3339, UTC).
    pub created_at: String,
    /// Last write instant (RFC 3339, UTC).
    pub updated_at: String,
}

/// A custom, tenant-defined plugin row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)] // `plugin_type` is the documented wire name
pub struct Plugin {
    /// Server-generated UUID.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Plugin kind.
    pub plugin_type: PluginType,
    /// Human-readable name, unique per tenant and type.
    pub name: String,
    /// JSON Schema the plugin `config` must satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source of the plugin.
    pub source_code: String,
    /// Phases the plugin declares.
    #[serde(default)]
    pub phases: Vec<PluginPhase>,
    /// Creation instant (RFC 3339, UTC).
    pub created_at: String,
    /// Last write instant (RFC 3339, UTC).
    pub updated_at: String,
    /// Last proxy request that executed this plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// Set when the plugin becomes unlinked; deleted once in the past.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<String>,
}

/// Plugin kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum PluginType {
    /// Credential injection; one per upstream.
    #[serde(rename = "auth")]
    Auth,
    /// Validation and policy enforcement; may reject.
    #[serde(rename = "guard")]
    Guard,
    /// Request/response mutation.
    #[serde(rename = "transform")]
    Transform,
}

impl PluginType {
    /// The GTS base type of a custom plugin of this kind.
    #[must_use]
    pub const fn gts_base_type(self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_TYPE,
            Self::Guard => GUARD_PLUGIN_TYPE,
            Self::Transform => TRANSFORM_PLUGIN_TYPE,
        }
    }
}

/// Phase a transform plugin participates in.
// The `on_` prefix is the documented wire spelling (`DESIGN` §3.2).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum PluginPhase {
    /// Before the upstream call.
    #[serde(rename = "on_request")]
    OnRequest,
    /// After the upstream response.
    #[serde(rename = "on_response")]
    OnResponse,
    /// On a proxy error.
    #[serde(rename = "on_error")]
    OnError,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "model_tests.rs"]
mod tests;
