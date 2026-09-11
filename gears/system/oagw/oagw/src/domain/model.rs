//! Domain model for the OAGW control plane.
//!
//! These types mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`. They carry no infrastructure
//! dependency and no secret material: credentials are referenced indirectly
//! through `secret_ref` values, resolved at request time.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// RFC 3339 serde helpers, used for `created_at` / `updated_at`.
pub mod rfc3339 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    /// Serialize an instant as an RFC 3339 string.
    ///
    /// # Errors
    /// Returns a serde error when the instant cannot be formatted.
    pub fn serialize<S: Serializer>(v: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        v.format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?
            .serialize(s)
    }

    /// Parse an RFC 3339 string into an instant.
    ///
    /// # Errors
    /// Returns a serde error when the string is not valid RFC 3339.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let raw = String::deserialize(d)?;
        OffsetDateTime::parse(&raw, &Rfc3339).map_err(serde::de::Error::custom)
    }
}

/// Current wall-clock instant.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Upstream endpoint scheme.
///
/// `http` is a legal scheme in this deployment: `allow_http_upstream` governs
/// whether a plaintext *connection* is made, not which schemes the model
/// accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// HTTPS.
    #[default]
    Https,
    /// Plaintext HTTP.
    Http,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC.
    Grpc,
}

impl Scheme {
    /// Default port for the scheme.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Whether the scheme implies a WebSocket upgrade.
    #[must_use]
    pub fn is_websocket(self) -> bool {
        matches!(self, Self::Wss)
    }

    /// The scheme as it appears in an endpoint URI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Endpoint {
    /// URI scheme.
    pub scheme: Scheme,
    /// DNS name, IPv4 or bracketed IPv6 literal.
    pub host: String,
    /// Port; defaults to the scheme's standard port.
    pub port: Option<u16>,
    /// Path prefix prepended to the forwarded path.
    pub path_prefix: String,
    /// Load-balancing weight, 1..=100.
    pub weight: u32,
    /// Selection priority; lower is preferred.
    pub priority: u32,
    /// Free-form metadata.
    pub metadata: std::collections::HashMap<String, String>,
}

impl Endpoint {
    /// Effective port, falling back to the scheme default.
    #[must_use]
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// `host:port` when the port is not the scheme default, else `host`.
    #[must_use]
    pub fn host_authority(&self) -> String {
        let host = self.host.trim_end_matches('.');
        let port = self.port.unwrap_or_else(|| self.scheme.default_port());
        if port == self.scheme.default_port() {
            host.to_owned()
        } else {
            format!("{host}:{port}")
        }
    }

    /// Whether the host is an IP literal rather than a DNS name.
    #[must_use]
    pub fn is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
            || (self.host.starts_with('[') && self.host.ends_with(']'))
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Upstream {
    /// A minimal upstream for tests, with one HTTPS endpoint.
    #[must_use]
    pub fn for_test() -> Self {
        let now = now();
        Self {
            id: Uuid::new_v4(),
            alias: "api.partner.com".to_owned(),
            name: "Partner API".to_owned(),
            description: String::new(),
            tenant_id: Uuid::nil(),
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: "api.partner.com".to_owned(),
                ..Endpoint::default()
            }],
            load_balancing: LoadBalancing::RoundRobin,
            auth_methods: Vec::new(),
            tags: Vec::new(),
            sharing: SharingMode::Private,
            rate_limit: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Route {
    /// A minimal route for tests, forwarding `/v1` to the test upstream.
    #[must_use]
    pub fn for_test() -> Self {
        let now = now();
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            path: "/v1".to_owned(),
            methods: vec!["*".to_owned()],
            target_alias: "api.partner.com".to_owned(),
            target_path_prefix: String::new(),
            strip_prefix: true,
            preserve_host: false,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            timeout_secs: None,
            rate_limit: None,
            plugins: Vec::new(),
            cors: None,
            priority: 0,
            enabled: true,
            path_suffix_mode: PathSuffixMode::Append,
            passthrough: Passthrough::default(),
            passthrough_allowlist: Vec::new(),
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

/// Load-balancing strategy across an upstream's endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancing {
    /// Round robin across the pool.
    #[default]
    RoundRobin,
    /// Uniform random choice.
    Random,
    /// Fewest in-flight requests.
    LeastConnections,
}

/// Where an API key is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    /// Request header (default).
    #[default]
    Header,
    /// Query parameter.
    Query,
}

/// Credential injection for an upstream (see `docs/DESIGN.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthMethod {
    /// Static API key injected from the credential store.
    ApiKey {
        /// Credential-store reference, e.g. `cred://partner-openai-key`.
        secret_ref: String,
        /// Header or query parameter carrying the key.
        #[serde(default = "default_api_key_header")]
        header_name: String,
        /// Where the key is placed.
        #[serde(default)]
        in_: ApiKeyLocation,
    },
    /// `OAuth2` client-credentials grant (ADR 0008).
    OAuth2ClientCredentials {
        /// Token endpoint URL.
        token_url: String,
        /// Reference to the client identifier.
        client_id_ref: String,
        /// Reference to the client secret.
        client_secret_ref: String,
        /// Scopes requested from the authorization server.
        #[serde(default)]
        scopes: Vec<String>,
        /// Header carrying the bearer token.
        #[serde(default = "default_bearer_header")]
        header_name: String,
    },
}

fn default_api_key_header() -> String {
    "Authorization".to_owned()
}

impl AuthMethod {
    /// The header this method injects its credential into.
    #[must_use]
    pub fn header_name(&self) -> &str {
        match self {
            Self::ApiKey { header_name, .. }
            | Self::OAuth2ClientCredentials { header_name, .. } => header_name,
        }
    }
}

fn default_bearer_header() -> String {
    "Authorization".to_owned()
}

/// Whether an upstream is visible to descendants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Only the owning tenant may use it.
    #[default]
    Private,
    /// Descendants may bind to it.
    Inherit,
    /// Descendants must use it and may not override credentials.
    Enforce,
}

/// Rate-limit sharing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSharing {
    /// Shared across the tenant.
    #[default]
    Shared,
    /// A separate bucket per route.
    PerRoute,
    /// A separate bucket per upstream.
    PerUpstream,
}

/// Rate-limit algorithm (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Token bucket, refilled at the sustained rate (default).
    #[default]
    TokenBucket,
    /// Sliding window over the sustained rate's window.
    SlidingWindow,
}

/// Rate-limit scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    /// One bucket per tenant (default).
    #[default]
    Tenant,
    /// One bucket per authenticated subject.
    Subject,
    /// One bucket per client IP.
    Ip,
}

/// What happens when the bucket is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    /// Reject with 429 (default).
    #[default]
    Reject,
    /// Queue the request until a token is available.
    Queue,
}

/// Sustained rate portion of a dual-rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SustainedRate {
    /// Requests permitted per window.
    pub rate: u32,
    /// Window length in seconds.
    pub window_secs: u32,
}

impl Default for SustainedRate {
    fn default() -> Self {
        Self {
            rate: 10,
            window_secs: 1,
        }
    }
}

/// Rate limiting configuration for an upstream or a route (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimit {
    /// What the bucket is keyed on.
    pub sharing: RateLimitSharing,
    /// Which algorithm fills the bucket.
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to the sustained rate.
    pub burst_capacity: Option<u32>,
    /// Optional token budget drawn down over the window.
    pub budget: Option<u32>,
    /// Who the bucket is keyed on.
    pub scope: RateLimitScope,
    /// Reject or queue.
    pub strategy: RateLimitStrategy,
    /// Tokens each request consumes.
    pub cost: u32,
    /// Whether `X-RateLimit-*` headers are emitted.
    pub response_headers: bool,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            sharing: RateLimitSharing::Shared,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate::default(),
            burst_capacity: None,
            budget: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }
}

impl RateLimit {
    /// Effective burst capacity.
    #[must_use]
    pub fn effective_capacity(&self) -> u32 {
        self.burst_capacity.unwrap_or(self.sustained.rate).max(1)
    }

    /// Tokens added per second.
    #[must_use]
    pub fn refill_per_sec(&self) -> f64 {
        if self.sustained.rate == 0 || self.sustained.window_secs == 0 {
            return 0.0;
        }
        f64::from(self.sustained.rate) / f64::from(self.sustained.window_secs)
    }

    /// Tighter of two limits, element-wise — the hierarchical merge rule.
    #[must_use]
    pub fn merge_min(ancestor: Option<&Self>, descendant: Option<&Self>) -> Option<Self> {
        match (ancestor, descendant) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(*a),
            (Some(a), Some(d)) => Some(Self {
                sharing: d.sharing,
                algorithm: d.algorithm,
                sustained: SustainedRate {
                    rate: a.sustained.rate.min(d.sustained.rate),
                    window_secs: a.sustained.window_secs.max(d.sustained.window_secs),
                },
                burst_capacity: Some(a.effective_capacity().min(d.effective_capacity())),
                budget: match (a.budget, d.budget) {
                    (Some(x), Some(y)) => Some(x.min(y)),
                    (Some(x), None) | (None, Some(x)) => Some(x),
                    (None, None) => None,
                },
                scope: d.scope,
                strategy: d.strategy,
                cost: d.cost.max(1),
                response_headers: a.response_headers && d.response_headers,
            }),
        }
    }
}

/// CORS configuration for a route (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Cors {
    /// Origins allowed; `*` allows any origin.
    pub allow_origins: Vec<String>,
    /// Methods allowed; `*` allows any method.
    pub allow_methods: Vec<String>,
    /// Request headers allowed.
    pub allow_headers: Vec<String>,
    /// Whether credentials are allowed.
    pub allow_credentials: bool,
    /// Preflight cache duration in seconds.
    pub max_age_secs: u64,
}

/// Header transformation action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HeaderAction {
    /// Set the header, overwriting any existing value (default).
    #[default]
    Set,
    /// Set the header only when it is already present.
    Replace,
    /// Remove the header.
    Remove,
}

/// A single header transformation on a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HeaderTransform {
    /// Header name.
    pub name: String,
    /// Action to apply.
    pub action: HeaderAction,
    /// Literal value for `set` / `replace`.
    pub value: String,
    /// Credential-store reference resolved at request time.
    pub value_ref: String,
}

/// A plugin bound to a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PluginBinding {
    /// Plugin identifier in the catalog.
    pub plugin_id: String,
    /// Plugin configuration.
    pub config: serde_json::Map<String, serde_json::Value>,
    /// Whether the binding is active. Binding a plugin is the operator opting
    /// in, so an omitted field means "on" — a `false` default would silently
    /// no-op every binding written through the management API.
    #[serde(default = "default_binding_enabled")]
    pub enabled: bool,
    /// Ordering within its plugin class; lower runs first.
    pub priority: u32,
}

/// `true`, the default for a plugin binding the operator did not qualify.
fn default_binding_enabled() -> bool {
    true
}

/// A plugin an operator filed in the catalog (`PRD` 5.3: tenant-defined).
///
/// The gear stores it, lists it, reads it and serves its source; it does not
/// execute it in this increment, which the catalog's `built_in: false` label
/// states to any caller deciding what to bind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CustomPlugin {
    /// Identifier the operator chose; unique per tenant.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Class the plugin belongs to.
    pub plugin_type: crate::domain::plugin::PluginType,
    /// Operator-facing description.
    pub description: String,
    /// The plugin's source, served verbatim by `GET /plugins/{id}/source`.
    pub source: String,
    /// When it was filed.
    pub created_at: OffsetDateTime,
}

impl Default for CustomPlugin {
    fn default() -> Self {
        Self {
            id: String::new(),
            tenant_id: Uuid::nil(),
            plugin_type: crate::domain::plugin::PluginType::Transform,
            description: String::new(),
            source: String::new(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}

impl CustomPlugin {
    /// The identifier an operator may not take for a shipped plugin.
    #[must_use]
    pub fn valid_id(id: &str) -> bool {
        !id.trim().is_empty()
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '~' | ':'))
    }
}

/// An upstream: a named pool of endpoints identified by an immutable alias.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstream {
    /// Server-assigned identifier.
    pub id: Uuid,
    /// Routing key in `/oagw/v1/proxy/{alias}/...`; immutable once set.
    pub alias: String,
    /// Human label.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// Owning tenant; derived from the security context, never supplied.
    pub tenant_id: Uuid,
    /// Endpoint pool.
    pub endpoints: Vec<Endpoint>,
    /// Load-balancing strategy across the pool.
    pub load_balancing: LoadBalancing,
    /// Credential injection methods.
    pub auth_methods: Vec<AuthMethod>,
    /// Add-only label set.
    pub tags: Vec<String>,
    /// Visibility to descendant tenants.
    pub sharing: SharingMode,
    /// Rate limit for the pool.
    pub rate_limit: Option<RateLimit>,
    /// Whether the pool accepts traffic at all (PRD FR: `enabled`).
    pub enabled: bool,
    /// Creation instant.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last update instant.
    #[serde(with = "rfc3339")]
    pub updated_at: OffsetDateTime,
}

// `#[serde(default)]` on the container needs a `Default`; `time`'s
// `OffsetDateTime` has none, so it is written by hand over the epoch.
impl Default for Upstream {
    fn default() -> Self {
        let now = OffsetDateTime::UNIX_EPOCH;
        Self {
            id: Uuid::nil(),
            alias: String::new(),
            name: String::new(),
            description: String::new(),
            tenant_id: Uuid::nil(),
            endpoints: Vec::new(),
            load_balancing: LoadBalancing::default(),
            auth_methods: Vec::new(),
            tags: Vec::new(),
            sharing: SharingMode::default(),
            rate_limit: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        }
    }
}

impl Upstream {
    /// Whether descendants may see this upstream.
    #[must_use]
    pub fn visible_to_descendants(&self) -> bool {
        matches!(self.sharing, SharingMode::Inherit | SharingMode::Enforce)
    }

    /// Whether descendants may override credentials on it.
    #[must_use]
    pub fn allows_override(&self) -> bool {
        matches!(self.sharing, SharingMode::Inherit)
    }

    /// Whether the pool takes traffic at all. An ancestor that disabled the
    /// pool disabled it for every descendant: the switch is read where the
    /// upstream is found, so the caller's own posture cannot lift it.
    #[must_use]
    pub fn accepts_traffic(&self) -> bool {
        self.enabled
    }

    /// The endpoint whose host (optionally `host:port`) equals `target_host`.
    #[must_use]
    pub fn endpoint_for_target_host(&self, target_host: &str) -> Option<&Endpoint> {
        let wanted = target_host.trim().to_ascii_lowercase();
        let with_port = self.endpoints.iter().find(|e| e.host_authority() == wanted);
        if with_port.is_some() {
            return with_port;
        }
        self.endpoints
            .iter()
            .find(|e| e.host.eq_ignore_ascii_case(&wanted))
    }
}

/// What a route does with the part of the request path that follows its own.
///
/// `append` (the default) forwards it, which is how a prefix route reaches
/// every resource behind it. `disabled` treats the route's path as the whole
/// address: a call that carries anything beyond it is refused rather than
/// silently aimed at a resource the operator never mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Forward the remainder of the path to the upstream.
    #[default]
    Append,
    /// Refuse a request whose path carries a suffix.
    Disabled,
}

impl PathSuffixMode {
    /// Whether a remainder may follow the route's path.
    #[must_use]
    pub const fn allows_suffix(self) -> bool {
        matches!(self, Self::Append)
    }
}

/// How much of the caller's own header set reaches the upstream.
///
/// `none` (the default) keeps the caller's message out of the forwarded
/// request entirely: the upstream sees what the route and the gateway put
/// there, which is what an outbound gateway is for. `allowlist` forwards the
/// named headers only, and `all` forwards everything that is not hop-by-hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Passthrough {
    /// Forward nothing the caller sent.
    #[default]
    None,
    /// Forward only the headers `passthrough_allowlist` names.
    Allowlist,
    /// Forward everything that is not hop-by-hop.
    All,
}

impl Passthrough {
    /// Whether a caller-sent header is forwarded under this mode.
    #[must_use]
    pub fn forwards(self, allowlist: &[String], name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allowlist => allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name)),
            Self::None => false,
        }
    }
}

/// A route: how a method and path map onto an upstream.
///
/// The three switches are independent on the wire, exactly as the API contract
/// spells them, so they stay three fields rather than a fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct Route {
    /// Server-assigned identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Path pattern; starts with `/` and may contain `{param}` segments.
    pub path: String,
    /// Allowed methods, uppercase, or `*`.
    pub methods: Vec<String>,
    /// Upstream alias the route forwards to.
    pub target_alias: String,
    /// Prefix prepended to the forwarded path.
    pub target_path_prefix: String,
    /// Whether the matched prefix is stripped from the forwarded path.
    pub strip_prefix: bool,
    /// Whether the client's `Host` is forwarded.
    pub preserve_host: bool,
    /// Request header transformations.
    pub request_headers: Vec<HeaderTransform>,
    /// Response header transformations.
    pub response_headers: Vec<HeaderTransform>,
    /// Per-route timeout override in seconds.
    pub timeout_secs: Option<u64>,
    /// Per-route rate limit.
    pub rate_limit: Option<RateLimit>,
    /// Plugins bound to the route.
    pub plugins: Vec<PluginBinding>,
    /// CORS configuration.
    pub cors: Option<Cors>,
    /// Ordering among equally specific matches; higher wins.
    pub priority: u32,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// What happens to the path beyond the route's own.
    pub path_suffix_mode: PathSuffixMode,
    /// Which of the caller's own headers the upstream may see.
    pub passthrough: Passthrough,
    /// Headers forwarded when [`Passthrough::Allowlist`] is in force.
    pub passthrough_allowlist: Vec<String>,
    /// Add-only label set.
    pub tags: Vec<String>,
    /// Creation instant.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last update instant.
    #[serde(with = "rfc3339")]
    pub updated_at: OffsetDateTime,
}

// Same reason as [`Upstream`]: the container-level `#[serde(default)]`.
impl Default for Route {
    fn default() -> Self {
        let now = OffsetDateTime::UNIX_EPOCH;
        Self {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            path: String::new(),
            methods: Vec::new(),
            target_alias: String::new(),
            target_path_prefix: String::new(),
            strip_prefix: false,
            preserve_host: false,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            timeout_secs: None,
            rate_limit: None,
            plugins: Vec::new(),
            cors: None,
            priority: 0,
            enabled: true,
            path_suffix_mode: PathSuffixMode::default(),
            passthrough: Passthrough::default(),
            passthrough_allowlist: Vec::new(),
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

impl Route {
    /// Whether the route accepts `method`.
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        let method = method.to_ascii_uppercase();
        self.methods
            .iter()
            .any(|m| m == "*" || m.eq_ignore_ascii_case(&method))
    }

    /// Remaining path after the matched prefix is stripped.
    #[must_use]
    pub fn forward_path(&self, matched_prefix: &str, remaining: &str) -> String {
        if self.strip_prefix {
            let suffix = remaining.strip_prefix(matched_prefix).unwrap_or(remaining);
            let suffix = suffix.strip_prefix('/').unwrap_or(suffix);
            let prefix = self.target_path_prefix.trim_end_matches('/');
            if suffix.is_empty() {
                if prefix.is_empty() {
                    "/".to_owned()
                } else {
                    format!("{prefix}/")
                }
            } else if prefix.is_empty() {
                format!("/{suffix}")
            } else {
                format!("{prefix}/{suffix}")
            }
        } else {
            // An unstripped remainder already starts with `/`.
            let path = if remaining.starts_with('/') {
                remaining.to_owned()
            } else {
                format!("/{remaining}")
            };
            let prefix = self.target_path_prefix.trim_end_matches('/');
            if prefix.is_empty() {
                path
            } else {
                format!("{prefix}{path}")
            }
        }
    }
}

#[cfg(test)]
mod model_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_defaults_to_scheme_port() {
        let e: Endpoint =
            serde_json::from_value(json!({"scheme": "https", "host": "a.com"})).unwrap();
        assert_eq!(e.port_or_default(), 443);
        assert_eq!(e.host_authority(), "a.com");
        let e: Endpoint =
            serde_json::from_value(json!({"scheme": "https", "host": "a.com", "port": 8443}))
                .unwrap();
        assert_eq!(e.host_authority(), "a.com:8443");
    }

    #[test]
    fn http_is_a_legal_scheme() {
        let e: Endpoint =
            serde_json::from_value(json!({"scheme": "http", "host": "127.0.0.1"})).unwrap();
        assert_eq!(e.scheme, Scheme::Http);
        assert_eq!(e.port_or_default(), 80);
        assert!(e.is_ip());
    }

    #[test]
    fn ip_endpoints_are_detected() {
        for host in ["127.0.0.1", "10.0.0.1", "::1", "[fe80::1]"] {
            let e = Endpoint {
                host: host.to_owned(),
                ..Endpoint::default()
            };
            assert!(e.is_ip(), "{host} should be an IP endpoint");
        }
        let e = Endpoint {
            host: "api.partner.com".to_owned(),
            ..Endpoint::default()
        };
        assert!(!e.is_ip());
    }

    #[test]
    fn rate_limit_defaults_burst_to_sustained_rate() {
        let rl = RateLimit::default();
        assert_eq!(rl.effective_capacity(), rl.sustained.rate);
        assert!(
            (rl.refill_per_sec() - 10.0).abs() < f64::EPSILON,
            "the refill rate is 10/s"
        );
    }

    #[test]
    fn rate_limit_merge_is_elementwise_min() {
        let ancestor = RateLimit {
            sustained: SustainedRate {
                rate: 5,
                window_secs: 2,
            },
            burst_capacity: Some(50),
            budget: Some(100),
            ..RateLimit::default()
        };
        let descendant = RateLimit {
            sustained: SustainedRate {
                rate: 9,
                window_secs: 1,
            },
            burst_capacity: Some(80),
            ..RateLimit::default()
        };
        let merged = RateLimit::merge_min(Some(&ancestor), Some(&descendant)).unwrap();
        assert_eq!(merged.sustained.rate, 5);
        assert_eq!(merged.sustained.window_secs, 2);
        assert_eq!(merged.effective_capacity(), 50);
        assert_eq!(merged.budget, Some(100));
    }

    #[test]
    fn rate_limit_merge_with_absent_side_keeps_present_side() {
        let rl = RateLimit::default();
        assert_eq!(
            RateLimit::merge_min(Some(&rl), None),
            Some(rl),
            "absent ancestor must not tighten"
        );
        assert_eq!(RateLimit::merge_min(None, None), None);
    }

    #[test]
    fn endpoint_target_host_matches_with_and_without_port() {
        let upstream = Upstream {
            endpoints: vec![
                Endpoint {
                    scheme: Scheme::Https,
                    host: "us.vendor.com".to_owned(),
                    port: Some(443),
                    ..Endpoint::default()
                },
                Endpoint {
                    scheme: Scheme::Http,
                    host: "eu.vendor.com".to_owned(),
                    port: Some(8080),
                    ..Endpoint::default()
                },
            ],
            ..Upstream::for_test()
        };
        assert_eq!(
            upstream
                .endpoint_for_target_host("us.vendor.com")
                .unwrap()
                .host,
            "us.vendor.com"
        );
        assert_eq!(
            upstream
                .endpoint_for_target_host("eu.vendor.com:8080")
                .unwrap()
                .host,
            "eu.vendor.com"
        );
        assert!(upstream.endpoint_for_target_host("ap.vendor.com").is_none());
    }

    #[test]
    fn route_forward_path_applies_strip_and_prefix() {
        let stripped = Route {
            target_path_prefix: "/api".to_owned(),
            strip_prefix: true,
            ..Route::for_test()
        };
        assert_eq!(stripped.forward_path("/v1", "/v1/chat"), "/api/chat");
        assert_eq!(stripped.forward_path("/v1", "/v1"), "/api/");

        let kept = Route {
            target_path_prefix: String::new(),
            strip_prefix: false,
            ..Route::for_test()
        };
        assert_eq!(kept.forward_path("/v1", "/v1/chat"), "/v1/chat");
    }

    #[test]
    fn route_allows_method_with_wildcard() {
        let wildcard = Route {
            methods: vec!["*".to_owned()],
            ..Route::for_test()
        };
        assert!(wildcard.allows_method("PATCH"));
        let get_only = Route {
            methods: vec!["get".to_owned()],
            ..Route::for_test()
        };
        assert!(get_only.allows_method("GET"));
        assert!(!get_only.allows_method("POST"));
    }

    #[test]
    fn round_trips_rfc3339_timestamps() {
        let upstream = Upstream::for_test();
        let raw = serde_json::to_value(&upstream).unwrap();
        let text = raw["created_at"].as_str().unwrap();
        assert!(text.ends_with('Z'), "created_at should be RFC 3339: {text}");
        let back: Upstream = serde_json::from_value(raw).unwrap();
        assert_eq!(back.created_at, upstream.created_at);
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = json!({"scheme": "https", "host": "a.com", "bogus": 1});
        assert!(serde_json::from_value::<Endpoint>(raw).is_err());
    }
}
