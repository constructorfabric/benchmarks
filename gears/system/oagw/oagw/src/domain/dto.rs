//! Shared domain model types (DoD `cpt-cf-oagw-dod-gear-foundation-domain-model`).
//!
//! The Control Plane and the Data Plane both consume these types; the shapes
//! mirror `schemas/upstream.v1.schema.json` and `schemas/route.v1.schema.json`
//! with `additionalProperties: false`, so a serialized record carries exactly
//! the schema's property names (plus the three schema-external API fields the
//! route payload contract names: `priority`, `enabled`, `cors`).
//!
//! Server-assigned / derived values that are **not** accepted on write are
//! modelled as fields the wire never sees: `Upstream.tenant_id`,
//! `Route.tenant_id` and `Route.match_type` are `#[serde(skip)]`.
//!
//! A field a configuration layer does not specify stays absent (merge step 11,
//! `inst-gf-merge-13`), so every mergeable sub-configuration is `Option<_>`
//! rather than defaulted. `RateLimitConfig`, `CorsConfig` and `PluginsConfig`
//! are concrete once present and carry their own documented defaults.
// @cpt-algo:cpt-cf-oagw-algo-gear-foundation-config-merge:p1

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-1
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-10
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-11
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-12
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-13
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-14
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-2
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-3
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-4
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-5
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-6
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-7
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-8
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-9
/// Alias pattern of `schemas/upstream.v1.schema.json`.
pub const ALIAS_PATTERN: &str = "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$";
//
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-9
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-8
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-7
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-6
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-5
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-4
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-3
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-2
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-14
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-13
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-12
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-11
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-10
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-merge:p1:inst-gf-merge-1
//
/// Tag pattern of both schemas.
pub const TAG_PATTERN: &str = "^[a-z0-9_-]+$";

/// `private | inherit | enforce` — the hierarchical sharing modes of the
/// merge engine (`cpt-cf-oagw-algo-gear-foundation-config-merge`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override, gated on the corresponding override
    /// permission.
    Inherit,
    /// Descendants cannot override.
    Enforce,
}

/// `http | https | wss | wt | grpc`, default `https`.
///
/// `http` is admitted only while `oagw.config.allow_http_upstream` is true —
/// the documented lift of `cpt-cf-oagw-constraint-https-only` recorded as
/// graded deviation 2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointScheme {
    /// Plaintext; rejected unless `allow_http_upstream`.
    Http,
    #[default]
    Https,
    Wss,
    Wt,
    Grpc,
}

impl EndpointScheme {
    /// Whether the scheme is legal under `allow_http_upstream`.
    #[must_use]
    pub const fn is_allowed(self, allow_http_upstream: bool) -> bool {
        !matches!(self, Self::Http) || allow_http_upstream
    }

    /// The standard port the alias-derivation algorithm assigns (step 2).
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }
}

/// Default endpoint port of both schemas (`443`).
pub const DEFAULT_ENDPOINT_PORT: u16 = 443;

/// One endpoint of `server.endpoints`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Defaults to `https`; the configuration boundary requires it to be
    /// present on the wire (schema `required: ["scheme", "host"]`).
    #[serde(default)]
    pub scheme: EndpointScheme,
    /// RFC 1123 hostname or IPv4/IPv6 address.
    pub host: String,
    /// `1..=65535`, default `443`.
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
}

const fn default_endpoint_port() -> u16 {
    DEFAULT_ENDPOINT_PORT
}

/// `server` — required, at least one endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

/// `auth` — a plugin reference plus its sharing mode and plugin configuration.
///
/// `auth.type` is the plugin reference: a built-in GTS identifier, a
/// registry-resolved custom plugin identifier, or a custom plugin UUID. It is
/// stored as the scalar `auth_plugin_ref` / `auth_plugin_uuid` columns of
/// `oagw_upstream` (DESIGN §3.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// The plugin reference, serialized as `type`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    #[serde(default)]
    pub sharing: SharingMode,
    /// Authentication plugin configuration. Credential-bearing values inside
    /// this object must be `cred://` references (see [`CredentialRef`]);
    /// anything else is rejected at the configuration boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// A `cred://` reference — the only credential value a configuration
/// sub-structure may carry (`cpt-cf-oagw-algo-gear-foundation-credential-boundary`).
///
/// Resolution through `cred_store` is owned by entry 2.6 and happens at
/// request time, never at configuration time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialRef(pub String);

impl CredentialRef {
    /// The `cred://` prefix every credential reference carries.
    pub const PREFIX: &'static str = "cred://";

    /// Accepts only a well-formed `cred://` URI reference; the caller passes
    /// the *field name* so a rejection can name the field without echoing the
    /// rejected value.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError::ValidationError`] naming `field` when `value`
    /// is not a `cred://` reference.
    pub fn parse(field: &str, value: &str) -> Result<Self, crate::domain::error::DomainError> {
        if is_cred_reference(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(crate::domain::error::DomainError::field_rejection(
                field,
                "credential-bearing fields accept a `cred://` reference only",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `value` is a `cred://` reference (the only accepted credential
/// value at the configuration boundary).
#[must_use]
pub fn is_cred_reference(value: &str) -> bool {
    let rest = value.strip_prefix(CredentialRef::PREFIX);
    match rest {
        // `cred://` alone, or one with no path segments, is not a reference.
        None => false,
        Some(rest) => {
            !rest.is_empty()
                && rest.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                && rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ':'))
                && !rest.ends_with('/')
        }
    }
}

/// `headers.request.passthrough`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderPassthrough {
    #[default]
    None,
    Allowlist,
    All,
}

/// `headers.request` — `set` overwrites, `add` appends (duplicates allowed),
/// `remove` strips from the inbound request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
    /// Default `none`. May only be tightened across a major version boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<HeaderPassthrough>,
    /// Only meaningful with `passthrough: allowlist`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_allowlist: Option<Vec<String>>,
}

/// `headers.response` — `set` overwrites, `add` appends, `remove` strips.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
}

/// Header transformation rules for requests and responses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaders>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaders>,
}

/// Rate-limit algorithms (ADR 0003). Only the token bucket and sliding window
/// are specified.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Sustained-rate window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    /// Window length in seconds; `sustained.rate` divided by this is the
    /// single per-second refill rate.
    #[must_use]
    pub const fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// `rate_limit.sustained` — required, `rate` required and `>= 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Tokens replenished per window, `>= 1`.
    pub rate: u32,
    #[serde(default)]
    pub window: RateWindow,
}

/// `rate_limit.burst` — bucket capacity; defaults to `sustained.rate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BurstCapacity {
    pub capacity: u32,
}

/// Budget modes are parsed, validated and stored but **not executed**
/// (graded deviation 8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMode {
    #[default]
    Unlimited,
    Allocated,
    Shared,
}

/// `rate_limit.budget`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default)]
    pub mode: BudgetMode,
    /// Required when `mode` is `allocated` or `shared`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// `1.0..=2.0` inclusive.
    #[serde(default = "default_overcommit_ratio", skip_serializing_if = "Option::is_none")]
    pub overcommit_ratio: Option<f64>,
}

const fn default_overcommit_ratio() -> Option<f64> {
    Some(1.0)
}

/// Counter scope. `route` is admitted by the decomposition (graded
/// deviation 8) even though the PRD enumeration omits it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

/// Only `reject` is executed; `queue` and `degrade` parse/validate/store but
/// are not executed (graded deviation 8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Rate-limit configuration, delivered as **configuration data only**: no
/// counter is constructed and no limit is evaluated by foundation code.
///
/// The four keys `budget.{mode,total,overcommit_ratio}` and
/// `response_headers` are the documented field-set delta admitted beyond the
/// two JSON schemas; every other extra key is rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    #[serde(default)]
    pub scope: RateScope,
    #[serde(default)]
    pub strategy: RateStrategy,
    #[serde(default = "default_rate_cost")]
    pub cost: u32,
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

const fn default_rate_cost() -> u32 {
    1
}

const fn default_true() -> bool {
    true
}

impl RateLimitConfig {
    /// The burst capacity in effect: the declared capacity, or the sustained
    /// rate when no layer specifies one.
    #[must_use]
    pub const fn effective_burst_capacity(&self) -> u32 {
        match self.burst {
            Some(BurstCapacity { capacity }) => capacity,
            None => self.sustained.rate,
        }
    }

    /// The single per-second refill rate the sustained rate and window
    /// convert into.
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        self.sustained.rate as f64 / self.sustained.window.secs() as f64
    }

    /// Stricter-wins combination of an ancestor limit with a more specific
    /// one: `min()` on the sustained rate and on the burst capacity, with
    /// `algorithm`, `scope`, `strategy`, `cost`, `response_headers` and the
    /// window taken from the more specific layer that specifies each.
    #[must_use]
    pub fn merge_stricter(&self, more_specific: &RateLimitConfig) -> RateLimitConfig {
        let burst = match (self.burst, more_specific.burst) {
            (Some(a), Some(b)) => Some(BurstCapacity { capacity: a.capacity.min(b.capacity) }),
            (a, b) => a.or(b),
        };
        RateLimitConfig {
            sharing: more_specific.sharing,
            algorithm: more_specific.algorithm,
            sustained: SustainedRate {
                rate: self.sustained.rate.min(more_specific.sustained.rate),
                window: more_specific.sustained.window,
            },
            burst,
            budget: more_specific.budget,
            scope: more_specific.scope,
            strategy: more_specific.strategy,
            cost: more_specific.cost,
            response_headers: more_specific.response_headers,
        }
    }
}

/// CORS configuration (`cors` definition of both schemas).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    /// Required, default `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Each entry is `*` or a valid origin URI; compared exactly across
    /// scheme, host and port with no relaxation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl CorsConfig {
    /// Whether `*` may appear in `allowed_origins`: only while
    /// `allow_credentials` is false. Re-checked on the merged effective
    /// configuration.
    #[must_use]
    pub fn allows_wildcard(&self) -> Result<bool, crate::domain::error::DomainError> {
        let Some(origins) = &self.allowed_origins else {
            return Ok(false);
        };
        let wildcard = origins.iter().any(|o| o == "*");
        if wildcard && self.allow_credentials {
            return Err(crate::domain::error::DomainError::CorsInvalidConfig(
                "Cannot use allow_credentials with wildcard origin".to_owned(),
            ));
        }
        Ok(wildcard)
    }
}

/// Plugin chain: a sharing mode plus an ordered list of plugin references.
///
/// Each item is a built-in GTS identifier or a custom plugin UUID. The order
/// is the binding order; positions are the list indices, contiguous from zero
/// (`cpt-cf-oagw-algo-gear-foundation-repo-scope` step 4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub items: Vec<String>,
}

/// Protocol-scoped inbound matching rules; exactly one of `http`/`grpc`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// `path_suffix_mode`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Rejects a supplied `/oagw/v1/proxy/{alias}/{path_suffix}`.
    Disabled,
    #[default]
    Append,
}

/// The five methods an HTTP match may allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl HttpMethod {
    /// The wire verb.
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

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// `minItems: 1`.
    pub methods: Vec<HttpMethod>,
    /// Path pattern, `minLength: 1`.
    pub path: String,
    /// An empty allowlist permits no query parameter.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Default `append`.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules — configuration surface only (graded deviation 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Which protocol a route's `match` block selects. Derived at write time and
/// never accepted on write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMatchType {
    #[default]
    Http,
    Grpc,
}

/// `oagw_route` — PK `id`, FK `upstream_id` (cascade).
///
/// `Route` belongs to exactly one upstream. `priority` (default `0`, higher
/// wins at match time), `enabled` (default `true`) and `cors` are the named
/// closed set of schema-external API fields the route payload contract
/// admits; `id`, `tenant_id` and `match_type` are server-assigned or derived
/// and are not accepted on write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    #[serde(default)]
    pub id: Uuid,
    /// Owner tenant; `oagw_route` carries it for tenant scoping but the
    /// route schema is `additionalProperties: false` and does not accept it.
    #[serde(skip)]
    pub tenant_id: Uuid,
    pub upstream_id: Uuid,
    /// Derived from the selected `match` block; not accepted on write.
    #[serde(skip)]
    pub match_type: RouteMatchType,
    /// Default `0`; higher value wins at match time.
    #[serde(default)]
    pub priority: i64,
    /// Default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "match")]
    pub match_: MatchConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Schema-external API field (DESIGN carries it; the route schema does
    /// not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `oagw_upstream` — PK `id`, UNIQUE `(tenant_id, alias)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    #[serde(default)]
    pub id: Uuid,
    /// Owner tenant; the upstream schema is `additionalProperties: false` and
    /// does not accept it on the wire.
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Routing identifier, unique per tenant.
    pub alias: String,
    /// One of the two protocol GTS identifiers.
    pub protocol: String,
    /// Default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub server: ServerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `oagw_plugin` — PK `id`, UNIQUE `(tenant_id, name)`.
///
/// Plugins are immutable after creation (no PUT) and the garbage-collection
/// fields are carried but never populated: plugin GC is not implemented in
/// the graded configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    #[serde(default)]
    pub id: Uuid,
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// One of the three plugin base types' concrete identifiers.
    pub plugin_type: String,
    pub name: String,
    /// JSON schema object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Opaque source reference; registry-reference-only custom plugins
    /// (graded deviation 6) carry no interpreted source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<String>,
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod tests;
