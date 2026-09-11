//! The `oagw` domain model (`cpt-cf-oagw-dod-domain-types`).
//!
//! The `Upstream`, `Route` and `Plugin` aggregates with their configuration
//! value objects — [`ServerConfig`] with [`Endpoint`], [`MatchConfig`],
//! [`RateLimitConfig`], [`CorsConfig`], [`HeadersConfig`], [`AuthConfig`] and
//! [`PluginsConfig`] — as plain serde types. The field sets are the authoritative
//! field lists of `schemas/upstream.v1.schema.json` and `schemas/route.v1.schema.json`
//! (the `Route` additionally carries the declared field-set extension `enabled`
//! and `priority` of FEATURE §1.5), with the endpoint `scheme` enum of
//! DECOMPOSITION correction 2 (`http`, `https`, `wss`, `grpc`, `wt`) applied.
//!
//! Every type is `deny_unknown_fields` and applies the schema defaults, so a
//! payload is only ever shaped by the fields the schemas declare; the *values*
//! of those fields are validated by [`crate::domain::validation`], which is also
//! where a payload that violates several rules reports them all at once.
// @cpt-begin:cpt-cf-oagw-dod-domain-types:p1:inst-full

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// GTS type identifier of the upstream identifier family.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS type identifier of the route identifier family.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// GTS type identifier of the protocol identifier family.
pub const PROTOCOL_TYPE_ID: &str = "gts.cf.core.oagw.protocol.v1~";

/// GTS type identifiers of the plugin identifier family
/// (`cpt-cf-oagw-dod-plugin-identity`): the `gts.cf.core.oagw.{type}_plugin.v1~`
/// family the `Plugin` aggregate and every plugin reference belong to.
pub const PLUGIN_TYPE_IDS: &[&str] = &[
    "gts.cf.core.oagw.auth_plugin.v1~",
    "gts.cf.core.oagw.guard_plugin.v1~",
    "gts.cf.core.oagw.transform_plugin.v1~",
];

/// GTS instance identifier of the HTTP protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// GTS instance identifier of the gRPC protocol.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// The closed protocol instance set of `schemas/upstream.v1.schema.json`.
pub const PROTOCOL_INSTANCES: &[&str] = &[PROTOCOL_HTTP, PROTOCOL_GRPC];

/// Endpoint scheme: plaintext HTTP (DECOMPOSITION correction 2).
pub const SCHEME_HTTP: &str = "http";
/// Endpoint scheme: HTTPS, the schema default.
pub const SCHEME_HTTPS: &str = "https";
/// Endpoint scheme: WebSocket over TLS.
pub const SCHEME_WSS: &str = "wss";
/// Endpoint scheme: gRPC.
pub const SCHEME_GRPC: &str = "grpc";
/// Endpoint scheme: WebTransport, valid configuration that is not proxied
/// (DECOMPOSITION correction 7).
pub const SCHEME_WT: &str = "wt";

/// The accepted endpoint `scheme` enum of correction 2, which extends the
/// `https`, `wss`, `wt`, `grpc` enum of `schemas/upstream.v1.schema.json`.
///
/// Which schemes a field accepts and whether a plaintext connection is actually
/// established are two distinct concerns: the latter is the `allow_http_upstream`
/// policy knob of the gear wiring feature, which this layer never consults.
pub const ENDPOINT_SCHEMES: &[&str] = &[
    SCHEME_HTTP,
    SCHEME_HTTPS,
    SCHEME_WSS,
    SCHEME_GRPC,
    SCHEME_WT,
];

/// Default endpoint `scheme` (`schemas/upstream.v1.schema.json`).
pub const DEFAULT_ENDPOINT_SCHEME: &str = SCHEME_HTTPS;
/// Default endpoint `port` (`schemas/upstream.v1.schema.json`).
pub const DEFAULT_ENDPOINT_PORT: i64 = 443;
/// The lowest legal endpoint `port`.
pub const MIN_PORT: i64 = 1;
/// The highest legal endpoint `port`.
pub const MAX_PORT: i64 = 65_535;

/// Sharing mode: not visible to descendants.
pub const SHARING_PRIVATE: &str = "private";
/// Sharing mode: descendants can override.
pub const SHARING_INHERIT: &str = "inherit";
/// Sharing mode: descendants cannot override.
pub const SHARING_ENFORCE: &str = "enforce";

/// The closed sharing-mode enum shared by the value objects that carry one.
pub const SHARING_MODES: &[&str] = &[SHARING_PRIVATE, SHARING_INHERIT, SHARING_ENFORCE];

/// Rate-limit algorithm: token bucket, exact (DECOMPOSITION correction 8).
pub const ALGORITHM_TOKEN_BUCKET: &str = "token_bucket";
/// Rate-limit algorithm: enforced as a fixed-window approximation of the
/// sliding window (DECOMPOSITION correction 8).
pub const ALGORITHM_SLIDING_WINDOW: &str = "sliding_window";

/// The closed rate-limit `algorithm` enum.
pub const RATE_ALGORITHMS: &[&str] = &[ALGORITHM_TOKEN_BUCKET, ALGORITHM_SLIDING_WINDOW];

/// Rate-limit window: second, the schema default.
pub const WINDOW_SECOND: &str = "second";
/// Rate-limit window: minute.
pub const WINDOW_MINUTE: &str = "minute";
/// Rate-limit window: hour.
pub const WINDOW_HOUR: &str = "hour";
/// Rate-limit window: day.
pub const WINDOW_DAY: &str = "day";

/// The closed rate-limit `window` enum.
pub const RATE_WINDOWS: &[&str] = &[WINDOW_SECOND, WINDOW_MINUTE, WINDOW_HOUR, WINDOW_DAY];

/// Rate-limit scope: global.
pub const SCOPE_GLOBAL: &str = "global";
/// Rate-limit scope: tenant, the schema default.
pub const SCOPE_TENANT: &str = "tenant";
/// Rate-limit scope: user.
pub const SCOPE_USER: &str = "user";
/// Rate-limit scope: ip.
pub const SCOPE_IP: &str = "ip";
/// Rate-limit scope: route.
pub const SCOPE_ROUTE: &str = "route";

/// The closed rate-limit `scope` enum.
pub const RATE_SCOPES: &[&str] = &[
    SCOPE_GLOBAL,
    SCOPE_TENANT,
    SCOPE_USER,
    SCOPE_IP,
    SCOPE_ROUTE,
];

/// Rate-limit strategy: reject, the only behaviour implemented this release.
pub const STRATEGY_REJECT: &str = "reject";
/// Rate-limit strategy: accepted configuration that resolves to `reject`
/// behaviour (DECOMPOSITION correction 6).
pub const STRATEGY_QUEUE: &str = "queue";
/// Rate-limit strategy: accepted configuration that resolves to `reject`
/// behaviour (DECOMPOSITION correction 6).
pub const STRATEGY_DEGRADE: &str = "degrade";

/// The closed rate-limit `strategy` enum (DECOMPOSITION correction 6).
pub const RATE_STRATEGIES: &[&str] = &[STRATEGY_REJECT, STRATEGY_QUEUE, STRATEGY_DEGRADE];

/// Default tokens consumed per request (`schemas/upstream.v1.schema.json`).
pub const DEFAULT_RATE_COST: i64 = 1;

/// CORS wildcard origin, which `allow_credentials` may never be combined with
/// (ADR 0004).
pub const CORS_WILDCARD_ORIGIN: &str = "*";

/// The closed CORS `allowed_methods` enum.
pub const CORS_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Default CORS `allowed_methods` (`schemas/upstream.v1.schema.json`).
pub const DEFAULT_CORS_METHODS: &[&str] = &["GET", "POST"];

/// Header passthrough mode: forward no inbound header, the schema default.
pub const PASSTHROUGH_NONE: &str = "none";
/// Header passthrough mode: forward only the `passthrough_allowlist` entries.
pub const PASSTHROUGH_ALLOWLIST: &str = "allowlist";
/// Header passthrough mode: forward every inbound header.
pub const PASSTHROUGH_ALL: &str = "all";

/// The closed header `passthrough` enum.
pub const PASSTHROUGH_MODES: &[&str] = &[PASSTHROUGH_NONE, PASSTHROUGH_ALLOWLIST, PASSTHROUGH_ALL];

/// The well-known headers the headers block may set but never remove
/// (FEATURE §1.5, the feature-local headers rule).
pub const PROTECTED_HEADERS: &[&str] = &["content-length", "content-type"];

/// Scheme of the secret references the auth `config` object carries
/// (DESIGN "Secret Access Control"), resolved at request time by the cred-store
/// actor.
pub const CRED_REF_SCHEME: &str = "cred://";

/// The `auth.config` keys that carry secret material: a value under one of them
/// must be a `cred://` reference, never inline material (FEATURE §1.5).
pub const INLINE_SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "secret",
    "password",
    "token",
    "access_token",
    "client_secret",
    "private_key",
];

/// HTTP method the route `match.http` block may carry.
pub const METHOD_GET: &str = "GET";
/// HTTP method the route `match.http` block may carry.
pub const METHOD_POST: &str = "POST";
/// HTTP method the route `match.http` block may carry.
pub const METHOD_PUT: &str = "PUT";
/// HTTP method the route `match.http` block may carry.
pub const METHOD_DELETE: &str = "DELETE";
/// HTTP method the route `match.http` block may carry.
pub const METHOD_PATCH: &str = "PATCH";

/// The closed route `match.http.methods` enum
/// (`schemas/route.v1.schema.json`).
pub const HTTP_METHODS: &[&str] = &[
    METHOD_GET,
    METHOD_POST,
    METHOD_PUT,
    METHOD_DELETE,
    METHOD_PATCH,
];

/// Path-suffix mode: reject `/{path_suffix}` usage.
pub const PATH_SUFFIX_DISABLED: &str = "disabled";
/// Path-suffix mode: append `/{path_suffix}` to the matched path, the default.
pub const PATH_SUFFIX_APPEND: &str = "append";

/// The closed route `path_suffix_mode` enum.
pub const PATH_SUFFIX_MODES: &[&str] = &[PATH_SUFFIX_DISABLED, PATH_SUFFIX_APPEND];

/// The alias pattern of `schemas/upstream.v1.schema.json`, as the character
/// classes it is built from: lowercase, starting and ending alphanumeric, with
/// dots, colons and hyphens allowed inside.
pub const ALIAS_INNER_CHARS: &[char] = &['.', ':', '-'];

/// The tag pattern of both JSON Schemas: lowercase alphanumerics with
/// underscores and hyphens.
pub const TAG_EXTRA_CHARS: &[char] = &['_', '-'];

/// One upstream server endpoint (`schemas/upstream.v1.schema.json`).
///
/// `scheme` defaults to `https` and `port` to 443; `host` is required and is
/// validated as an RFC 1123 hostname or an IP literal carrying no embedded port
/// and no path segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Endpoint scheme, defaulting to `https` (correction 2 adds `http`).
    #[serde(default = "default_endpoint_scheme")]
    pub scheme: String,
    /// Hostname or IP literal of the upstream service.
    #[serde(default)]
    pub host: Option<String>,
    /// Endpoint port, defaulting to 443.
    #[serde(default = "default_endpoint_port")]
    pub port: i64,
}

impl Endpoint {
    /// Builds an endpoint from its three schema fields.
    #[must_use]
    pub fn new(scheme: &str, host: &str, port: i64) -> Self {
        Self {
            scheme: scheme.to_owned(),
            host: Some(host.to_owned()),
            port,
        }
    }

    /// The parsed scheme of the endpoint, `None` when the value is outside the
    /// correction-2 enum.
    #[must_use]
    pub fn scheme_enum(&self) -> Option<EndpointScheme> {
        EndpointScheme::parse(&self.scheme)
    }

    /// Whether the gateway actually proxies to this endpoint.
    ///
    /// A `wt` endpoint is valid configuration that is not proxied
    /// (DECOMPOSITION correction 7): a proxy request routed to it is answered
    /// with the gateway `RouteError`/`ProtocolError` semantics instead.
    #[must_use]
    pub fn is_proxied(&self) -> bool {
        self.scheme_enum().is_some_and(EndpointScheme::is_proxied)
    }

    /// The host value, with the trailing dot of an FQDN form stripped.
    #[must_use]
    pub fn host_stripped(&self) -> Option<&str> {
        self.host.as_deref().map(strip_trailing_dot)
    }
}

/// An accepted endpoint scheme (DECOMPOSITION correction 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointScheme {
    /// Plaintext HTTP, whose establishment is gated by `allow_http_upstream`.
    Http,
    /// HTTPS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// gRPC.
    Grpc,
    /// WebTransport, not proxied this release.
    Wt,
}

impl EndpointScheme {
    /// Parses an accepted endpoint scheme.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            SCHEME_HTTP => Some(Self::Http),
            SCHEME_HTTPS => Some(Self::Https),
            SCHEME_WSS => Some(Self::Wss),
            SCHEME_GRPC => Some(Self::Grpc),
            SCHEME_WT => Some(Self::Wt),
            _ => None,
        }
    }

    /// The canonical wire name of the scheme.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => SCHEME_HTTP,
            Self::Https => SCHEME_HTTPS,
            Self::Wss => SCHEME_WSS,
            Self::Grpc => SCHEME_GRPC,
            Self::Wt => SCHEME_WT,
        }
    }

    /// Whether the gateway proxies to a scheme of this kind.
    #[must_use]
    pub const fn is_proxied(self) -> bool {
        !matches!(self, Self::Wt)
    }
}

/// The endpoint pool of an upstream (`schemas/upstream.v1.schema.json`).
///
/// All endpoints of one pool share the owning upstream's single `protocol`
/// value and must be homogeneous in scheme and port
/// (`cpt-cf-oagw-algo-endpoint-validation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The endpoint pool, at least one entry (`minItems: 1`).
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// Protocol-scoped inbound matching rules of a route
/// (`schemas/route.v1.schema.json`): exactly one of `http` or `grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// The HTTP match rules, present when the upstream protocol is HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// The gRPC match rules, present when the upstream protocol is gRPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    /// The configured HTTP path pattern, the path prefix of the route-match
    /// determinism invariant of `cpt-cf-oagw-db-schema`.
    #[must_use]
    pub fn http_path(&self) -> Option<&str> {
        self.http.as_ref().and_then(|http| http.path.as_deref())
    }

    /// The configured HTTP methods, empty for a gRPC match.
    #[must_use]
    pub fn http_methods(&self) -> &[String] {
        self.http
            .as_ref()
            .map_or(&[], |http| http.methods.as_slice())
    }
}

/// HTTP match rules (`schemas/route.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// HTTP methods this route answers, at least one.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path pattern of the route, non-empty.
    #[serde(default)]
    pub path: Option<String>,
    /// Allowed query-parameter names; if empty, allow none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How `/{path_suffix}` of the proxy URL is treated, defaulting to `append`.
    #[serde(default = "default_path_suffix_mode")]
    pub path_suffix_mode: String,
}

/// gRPC match rules (`schemas/route.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name, e.g. `foo.v1.UserService`.
    #[serde(default)]
    pub service: Option<String>,
    /// RPC method name, e.g. `GetUser`.
    #[serde(default)]
    pub method: Option<String>,
}

/// The sustained rate of a rate-limit block
/// (`cpt-cf-oagw-algo-shape-validation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per window, at least 1.
    #[serde(default)]
    pub rate: Option<i64>,
    /// Time window of the sustained rate, defaulting to `second`.
    #[serde(default = "default_window")]
    pub window: String,
}

/// The burst capacity of a rate-limit block, defaulting to `sustained.rate`
/// when absent (`schemas/upstream.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BurstCapacity {
    /// Maximum burst size, at least 1.
    #[serde(default)]
    pub capacity: Option<i64>,
}

/// Rate-limiting configuration shared by upstreams and routes
/// (`cpt-cf-oagw-adr-rate-limiting`, ADR 0003).
///
/// The value object carries exactly the fields the two JSON Schemas share: the
/// `budget` and `response_headers` extensions of ADR 0003 are not carried as
/// required fields (FEATURE §1.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode, defaulting to `private`.
    #[serde(default = "default_private")]
    pub sharing: String,
    /// Algorithm, `token_bucket` exact and `sliding_window` approximated.
    #[serde(default = "default_token_bucket")]
    pub algorithm: String,
    /// The sustained rate, required by both schemas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sustained: Option<SustainedRate>,
    /// The burst capacity, defaulting to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacity>,
    /// Scope of the rate-limit counters, defaulting to `tenant`.
    #[serde(default = "default_tenant_scope")]
    pub scope: String,
    /// Behaviour when the limit is exceeded, defaulting to `reject`; `queue`
    /// and `degrade` validate and behave as `reject` (correction 6).
    #[serde(default = "default_reject")]
    pub strategy: String,
    /// Tokens consumed per request, defaulting to 1.
    #[serde(default = "default_cost")]
    pub cost: Option<i64>,
}

/// CORS configuration of an upstream (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode, defaulting to `private`.
    #[serde(default = "default_private")]
    pub sharing: String,
    /// Whether CORS is enabled, required by `schemas/upstream.v1.schema.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Allowed origins, `*` or an absolute origin URI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods, defaulting to `GET` and `POST`.
    #[serde(
        default = "default_cors_methods",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed, which rejects a wildcard origin.
    #[serde(default)]
    pub allow_credentials: bool,
}

/// Header transformation rules of an upstream
/// (`schemas/upstream.v1.schema.json` definitions/headers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Rules applied to the inbound request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaders>,
    /// Rules applied to the response to the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaders>,
}

/// Inbound-request header rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// Headers to set, overwriting an existing one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add, allowing duplicates.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded, defaulting to `none`.
    #[serde(default = "default_passthrough")]
    pub passthrough: String,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header rules (`schemas/upstream.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    /// Headers to set on the response to the client.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Authentication configuration of an upstream
/// (`schemas/upstream.v1.schema.json`).
///
/// The `config` object is opaque to this layer except for its `secret_ref` key,
/// which must carry a `cred://` reference that the cred-store actor resolves at
/// request time; inline secret material is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin this upstream authenticates with.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Sharing mode, defaulting to `private`.
    #[serde(default = "default_private")]
    pub sharing: String,
    /// Opaque authentication plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, Value>>,
}

impl AuthConfig {
    /// The `secret_ref` value of the `config` object, when it carries one.
    #[must_use]
    pub fn secret_ref(&self) -> Option<&str> {
        let config = self.config.as_ref()?;
        match config.get("secret_ref") {
            Some(Value::String(reference)) => Some(reference.as_str()),
            _ => None,
        }
    }
}

// @cpt-begin:cpt-cf-oagw-dod-plugin-identity:p1:inst-full
/// The plugin block of an upstream or a route
/// (`cpt-cf-oagw-dod-plugin-identity`).
///
/// The `items` entries are the plain identifier strings of the two JSON Schemas;
/// the persisted binding projection with position, `plugin_ref`, `plugin_uuid`
/// and `config` is derived from them on persist and is never a second payload
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Sharing mode, defaulting to `private`.
    #[serde(default = "default_private")]
    pub sharing: String,
    /// Builtin plugins referenced by GTS identifier, custom plugins by UUID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

impl PluginsConfig {
    /// Derives the persisted binding projection from the identifier items.
    ///
    /// `plugin_ref` is always present and `plugin_uuid` is derived only when the
    /// reference is UUID-backed, so a named plugin keeps `plugin_uuid` absent.
    #[must_use]
    pub fn bindings(&self) -> Vec<PluginBinding> {
        self.items
            .iter()
            .enumerate()
            .map(|(position, plugin_ref)| PluginBinding {
                position,
                plugin_ref: plugin_ref.clone(),
                plugin_uuid: plugin_ref_uuid(plugin_ref),
                config: None,
            })
            .collect()
    }
}

/// One persisted plugin binding
/// (`cpt-cf-oagw-dod-plugin-identity`, DESIGN "Plugin Identification Model").
///
/// `plugin_ref` is always stored; `plugin_uuid` is derived only when the
/// reference is UUID-backed, keeping it absent for named plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBinding {
    /// Position of the binding in the chain, contiguous from 0.
    pub position: usize,
    /// The canonical plugin reference, always present.
    pub plugin_ref: String,
    /// The extracted UUID, present only for a UUID-backed reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    /// The per-binding plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, Value>>,
}

/// A tenant-defined Starlark custom plugin (`cpt-cf-oagw-dod-plugin-identity`).
///
/// Named builtin plugins are resolved through an in-process registry and are
/// never persisted, so every stored `Plugin` is a Starlark custom plugin: it
/// carries a JSON-Schema-shaped `config_schema` and a non-empty `source_code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    /// Server-generated identifier of the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// The tenant the plugin is defined in; carried by the aggregate and never
    /// part of a payload, whose field set the schemas close.
    #[serde(skip)]
    pub tenant_id: Option<Uuid>,
    /// GTS type identifier of the plugin, a member of the
    /// `gts.cf.core.oagw.{type}_plugin.v1~` family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Unique plugin name per tenant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free-form description of the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON-Schema-shaped configuration schema of the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// The Starlark source of the plugin, non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
}

impl Plugin {
    /// Declared field order of the plugin payload, the order violations are
    /// reported in.
    pub const FIELD_ORDER: &'static [&'static str] = &[
        "id",
        "plugin_type",
        "name",
        "description",
        "config_schema",
        "source_code",
    ];

    /// The canonical GTS reference of the plugin:
    /// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`.
    #[must_use]
    pub fn gts_ref(&self) -> Option<String> {
        let plugin_type = self.plugin_type.as_deref()?;
        let id = self.id?;
        Some(format!("{plugin_type}{id}"))
    }
}

/// The tenant-scoped root configuration object
/// (`cpt-cf-oagw-design-domain-model`), unique per `(tenant_id, alias)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Server-generated unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// The tenant the upstream belongs to; stamped by the caller's tenant
    /// context, never read from a payload.
    #[serde(skip)]
    pub tenant_id: Option<Uuid>,
    /// Whether the upstream is enabled, defaulting to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Human-readable routing identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags for categorization and discovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The endpoint pool of the upstream, required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerConfig>,
    /// GTS identifier of the protocol used to reach the upstream, required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Authentication configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin bindings of the upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate-limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Upstream {
    /// Declared field order of the upstream payload: the order
    /// `cpt-cf-oagw-flow-resource-validation` visits the fields
    /// (`inst-rv-02` identity, `inst-rv-03` endpoints, `inst-rv-09` alias and
    /// tags, `inst-rv-10` to `inst-rv-12` the blocks), which is the order
    /// violations are reported in.
    ///
    /// This is deliberately not the serde serialisation order, which follows
    /// the schema property order.
    pub const FIELD_ORDER: &'static [&'static str] = &[
        "id",
        "enabled",
        "protocol",
        "server",
        "alias",
        "tags",
        "rate_limit",
        "cors",
        "headers",
        "auth",
        "plugins",
    ];

    /// The alias the upstream is routed by, as a `&str`.
    #[must_use]
    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }

    /// The identifier of the upstream, if one was generated.
    #[must_use]
    pub fn id(&self) -> Option<Uuid> {
        self.id
    }
}

/// A route of an upstream (`cpt-cf-oagw-design-domain-model`).
///
/// The field set is `schemas/route.v1.schema.json` plus the declared field-set
/// extension `enabled` and `priority` of FEATURE §1.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Server-generated unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// The tenant the route belongs to; stamped by the caller's tenant context.
    #[serde(skip)]
    pub tenant_id: Option<Uuid>,
    /// Flat tags for categorization and discovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Reference to the upstream service this route belongs to, required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<Uuid>,
    /// Protocol-scoped inbound matching rules, required.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub match_config: Option<MatchConfig>,
    /// Plugin bindings of the route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate-limiting configuration of the route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Whether the route is enabled: a disabled route is excluded from
    /// matching (`cpt-cf-oagw-fr-enable-disable`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Ordering field of the route-match determinism invariant.
    #[serde(default)]
    pub priority: i64,
}

impl Route {
    /// Declared field order of the route payload: the order
    /// `cpt-cf-oagw-flow-resource-validation` visits the fields
    /// (`inst-rv-02` identity, `inst-rv-09` tags, `inst-rv-10` and
    /// `inst-rv-12` the blocks, `inst-rv-13` the match, `inst-rv-14` the
    /// upstream reference), which is the order violations are reported in.
    ///
    /// The three upstream-only blocks are listed where the same visiting order
    /// puts them for an upstream, so a shared block keeps one position; a route
    /// payload never carries them.
    pub const FIELD_ORDER: &'static [&'static str] = &[
        "id",
        "enabled",
        "priority",
        "tags",
        "rate_limit",
        "cors",
        "headers",
        "auth",
        "plugins",
        "match",
        "upstream_id",
    ];

    /// The upstream this route belongs to.
    #[must_use]
    pub fn upstream_id(&self) -> Option<Uuid> {
        self.upstream_id
    }
}

/// Strips one trailing dot from a hostname, so the FQDN form
/// `api.openai.com.` is accepted and normalized to `api.openai.com`.
#[must_use]
pub fn strip_trailing_dot(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

/// Extracts the UUID of a UUID-backed plugin reference
/// (`cpt-cf-oagw-dod-plugin-identity`).
///
/// Both reference forms of `schemas/upstream.v1.schema.json` are accepted — a
/// custom-plugin UUID and a full GTS identifier whose instance part is a UUID —
/// and a named GTS identifier yields `None`.
#[must_use]
pub fn plugin_ref_uuid(plugin_ref: &str) -> Option<Uuid> {
    if let Ok(uuid) = Uuid::parse_str(plugin_ref) {
        return Some(uuid);
    }
    let (_, instance) = split_plugin_reference(plugin_ref)?;
    if instance.is_empty() {
        return None;
    }
    Uuid::parse_str(instance).ok()
}

/// Splits a plugin reference into its GTS family prefix and its instance part.
///
/// A reference that is not a member of the `gts.cf.core.oagw.{type}_plugin.v1~`
/// family yields `None`.
#[must_use]
pub fn split_plugin_reference(plugin_ref: &str) -> Option<(&'static str, &str)> {
    PLUGIN_TYPE_IDS.iter().find_map(|family| {
        plugin_ref
            .strip_prefix(*family)
            .map(|instance| (*family, instance))
    })
}
// @cpt-end:cpt-cf-oagw-dod-plugin-identity:p1:inst-full

fn default_true() -> bool {
    true
}

fn default_endpoint_scheme() -> String {
    DEFAULT_ENDPOINT_SCHEME.to_owned()
}

fn default_endpoint_port() -> i64 {
    DEFAULT_ENDPOINT_PORT
}

fn default_path_suffix_mode() -> String {
    PATH_SUFFIX_APPEND.to_owned()
}

fn default_passthrough() -> String {
    PASSTHROUGH_NONE.to_owned()
}

fn default_private() -> String {
    SHARING_PRIVATE.to_owned()
}

fn default_token_bucket() -> String {
    ALGORITHM_TOKEN_BUCKET.to_owned()
}

fn default_window() -> String {
    WINDOW_SECOND.to_owned()
}

fn default_tenant_scope() -> String {
    SCOPE_TENANT.to_owned()
}

fn default_reject() -> String {
    STRATEGY_REJECT.to_owned()
}

fn default_cost() -> Option<i64> {
    Some(DEFAULT_RATE_COST)
}

fn default_cors_methods() -> Vec<String> {
    DEFAULT_CORS_METHODS
        .iter()
        .map(|method| (*method).to_owned())
        .collect()
}

// @cpt-end:cpt-cf-oagw-dod-domain-types:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::type_provisioning::{
        PLUGIN_TYPE_IDS as PROVISIONED_PLUGIN_TYPES, PROTOCOL_TYPE_ID as PROVISIONED_PROTOCOL_TYPE,
        UPSTREAM_TYPE_ID as PROVISIONED_UPSTREAM_TYPE,
    };

    /// A complete, well-formed upstream payload — every schema field populated
    /// once — used as the documented happy-path example.
    fn full_upstream_json() -> Value {
        serde_json::json!({
            "id": "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
            "enabled": true,
            "alias": "payments.vendor.com",
            "tags": ["openai", "llm"],
            "server": {
                "endpoints": [
                    { "scheme": "https", "host": "api.vendor.com", "port": 443 },
                    { "scheme": "https", "host": "eu.vendor.com.", "port": 443 }
                ]
            },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "sharing": "private",
                "config": { "secret_ref": "cred://vendor/openai/api_key" }
            },
            "headers": {
                "request": {
                    "set": { "x-forwarded-by": "oagw" },
                    "add": { "x-request-id": "req-1" },
                    "remove": ["x-internal-token"],
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["accept", "content-type"]
                },
                "response": { "set": { "x-gateway": "oagw" }, "remove": ["server"] }
            },
            "plugins": {
                "sharing": "private",
                "items": [
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    "3f0a1b2c-3d4e-4f50-8617-8899aabbccdd"
                ]
            },
            "rate_limit": {
                "sharing": "private",
                "algorithm": "token_bucket",
                "sustained": { "rate": 100, "window": "minute" },
                "burst": { "capacity": 200 },
                "scope": "tenant",
                "strategy": "reject",
                "cost": 2
            },
            "cors": {
                "sharing": "private",
                "enabled": true,
                "allowed_origins": ["https://console.vendor.com"],
                "allowed_methods": ["GET", "POST"],
                "expose_headers": ["x-request-id"],
                "allow_credentials": false
            }
        })
    }

    /// A complete, well-formed route payload — the schema field set plus the
    /// declared `enabled` and `priority` extension.
    fn full_route_json() -> Value {
        serde_json::json!({
            "id": "1f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
            "tags": ["chat"],
            "upstream_id": "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
            "match": {
                "http": {
                    "methods": ["POST", "GET"],
                    "path": "/v1/chat",
                    "query_allowlist": ["model"],
                    "path_suffix_mode": "append"
                }
            },
            "plugins": {
                "sharing": "private",
                "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"]
            },
            "rate_limit": {
                "sharing": "private",
                "algorithm": "sliding_window",
                "sustained": { "rate": 10, "window": "second" },
                "scope": "user",
                "strategy": "queue",
                "cost": 1
            },
            "enabled": false,
            "priority": 10
        })
    }

    #[test]
    fn the_upstream_round_trips_the_documented_happy_path() {
        let payload = full_upstream_json();
        let upstream: Upstream = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(
            upstream.id(),
            Some(Uuid::parse_str("0f0a1b2c-3d4e-4f50-8617-8899aabbccdd").unwrap())
        );
        assert!(upstream.enabled);
        assert_eq!(upstream.alias(), Some("payments.vendor.com"));
        assert_eq!(upstream.tags, ["openai", "llm"]);
        let server = upstream.server.as_ref().unwrap();
        assert_eq!(server.endpoints.len(), 2);
        assert_eq!(upstream.protocol.as_deref(), Some(PROTOCOL_HTTP));

        let round_tripped = serde_json::to_value(&upstream).unwrap();
        assert_eq!(round_tripped, payload, "no additional or missing field");
        let again: Upstream = serde_json::from_value(round_tripped).unwrap();
        assert_eq!(again, upstream);
    }

    #[test]
    fn the_route_round_trips_the_documented_happy_path() {
        let payload = full_route_json();
        let route: Route = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(
            route.upstream_id(),
            Some(Uuid::parse_str("0f0a1b2c-3d4e-4f50-8617-8899aabbccdd").unwrap())
        );
        assert!(!route.enabled, "the declared extension field round-trips");
        assert_eq!(route.priority, 10);
        let round_tripped = serde_json::to_value(&route).unwrap();
        assert_eq!(round_tripped, payload, "no additional or missing field");
    }

    #[test]
    fn a_route_with_a_grpc_match_round_trips() {
        let payload = serde_json::json!({
            "upstream_id": "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
            "match": { "grpc": { "service": "foo.v1.UserService", "method": "GetUser" } }
        });
        let route: Route = serde_json::from_value(payload).unwrap();
        assert_eq!(route.priority, 0, "priority defaults to 0");
        assert!(route.enabled, "enabled defaults to true");
        let grpc = route.match_config.as_ref().unwrap().grpc.as_ref().unwrap();
        assert_eq!(grpc.service.as_deref(), Some("foo.v1.UserService"));
        assert_eq!(grpc.method.as_deref(), Some("GetUser"));

        let round_tripped = serde_json::to_value(&route).unwrap();
        let again: Route = serde_json::from_value(round_tripped).unwrap();
        assert_eq!(again, route, "the round trip is lossless for the aggregate");
    }

    #[test]
    fn schema_defaults_apply() {
        let upstream: Upstream = serde_json::from_value(serde_json::json!({
            "server": { "endpoints": [{ "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP
        }))
        .unwrap();
        let endpoint = &upstream.server.as_ref().unwrap().endpoints[0];
        assert_eq!(
            endpoint.scheme, DEFAULT_ENDPOINT_SCHEME,
            "scheme defaults to https"
        );
        assert_eq!(endpoint.port, DEFAULT_ENDPOINT_PORT, "port defaults to 443");
        assert!(upstream.enabled, "upstream enabled defaults to true");

        let rate_limit: RateLimitConfig =
            serde_json::from_value(serde_json::json!({ "sustained": { "rate": 5 } })).unwrap();
        assert_eq!(rate_limit.sharing, SHARING_PRIVATE);
        assert_eq!(rate_limit.algorithm, ALGORITHM_TOKEN_BUCKET);
        assert_eq!(rate_limit.sustained.as_ref().unwrap().window, WINDOW_SECOND);
        assert_eq!(rate_limit.scope, SCOPE_TENANT);
        assert_eq!(rate_limit.strategy, STRATEGY_REJECT);
        assert_eq!(rate_limit.cost, Some(DEFAULT_RATE_COST));

        let cors: CorsConfig =
            serde_json::from_value(serde_json::json!({ "enabled": true })).unwrap();
        assert_eq!(cors.allowed_methods, ["GET", "POST"], "cors defaults apply");
        assert!(cors.expose_headers.is_empty());
        assert!(!cors.allow_credentials);

        let http: HttpMatch =
            serde_json::from_value(serde_json::json!({ "methods": ["GET"], "path": "/v1" }))
                .unwrap();
        assert_eq!(http.path_suffix_mode, PATH_SUFFIX_APPEND);
        assert!(http.query_allowlist.is_empty());
    }

    #[test]
    fn an_unknown_field_is_rejected_at_every_level() {
        for payload in [
            serde_json::json!({ "nope": 1, "server": { "endpoints": [] }, "protocol": PROTOCOL_HTTP }),
            serde_json::json!({ "server": { "endpoints": [], "nope": 1 }, "protocol": PROTOCOL_HTTP }),
            serde_json::json!({ "server": { "endpoints": [{ "host": "h", "nope": 1 }] }, "protocol": PROTOCOL_HTTP }),
            serde_json::json!({ "server": { "endpoints": [{ "host": "h" }] }, "protocol": PROTOCOL_HTTP, "rate_limit": { "sustained": { "rate": 1 }, "nope": 1 } }),
        ] {
            let error = serde_json::from_value::<Upstream>(payload).unwrap_err();
            assert!(
                error.to_string().contains("unknown field"),
                "deny_unknown_fields must reject the payload: {error}"
            );
        }
    }

    #[test]
    fn a_route_rejects_an_unknown_field() {
        let error = serde_json::from_value::<Route>(serde_json::json!({
            "upstream_id": "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "alias": "routes-have-no-alias"
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn a_foreign_tenant_id_is_never_read_from_a_payload() {
        let payload = full_upstream_json();
        let mut object = payload.as_object().unwrap().clone();
        object.insert("tenant_id".to_owned(), Value::String("x".to_owned()));
        let error = serde_json::from_value::<Upstream>(Value::Object(object)).unwrap_err();
        assert!(
            error.to_string().contains("unknown field"),
            "tenant_id is carried by the aggregate, not by the schema field set: {error}"
        );
        assert!(serde_json::to_value(Upstream::default()).is_ok());
    }

    #[test]
    fn the_endpoint_scheme_enum_is_the_correction_2_set() {
        for scheme in ENDPOINT_SCHEMES {
            let parsed = EndpointScheme::parse(scheme)
                .unwrap_or_else(|| panic!("scheme {scheme} must be accepted by correction 2"));
            assert_eq!(parsed.as_str(), *scheme);
        }
        assert!(EndpointScheme::parse("ftp").is_none());
        assert_eq!(ENDPOINT_SCHEMES.len(), 5, "http, https, wss, grpc, wt");
    }

    #[test]
    fn a_wt_endpoint_is_valid_configuration_that_is_not_proxied() {
        for scheme in ENDPOINT_SCHEMES {
            let endpoint = Endpoint::new(scheme, "api.vendor.com", 443);
            assert_eq!(
                endpoint.is_proxied(),
                *scheme != SCHEME_WT,
                "{scheme} proxying must follow correction 7"
            );
        }
        let wt = Endpoint::new(SCHEME_WT, "api.vendor.com", 443);
        assert!(!wt.is_proxied());
        assert_eq!(EndpointScheme::Wt.as_str(), SCHEME_WT);
        assert!(!EndpointScheme::Wt.is_proxied());
    }

    #[test]
    fn a_trailing_dot_host_is_stripped_by_the_value_object() {
        let endpoint = Endpoint::new(SCHEME_HTTPS, "api.vendor.com.", 443);
        assert_eq!(endpoint.host_stripped(), Some("api.vendor.com"));
        assert_eq!(
            Endpoint::new(SCHEME_HTTPS, "api.vendor.com", 443).host_stripped(),
            Some("api.vendor.com")
        );
    }

    #[test]
    fn the_field_orders_are_the_declared_field_sets() {
        assert_eq!(
            Upstream::FIELD_ORDER,
            &[
                "id",
                "enabled",
                "protocol",
                "server",
                "alias",
                "tags",
                "rate_limit",
                "cors",
                "headers",
                "auth",
                "plugins"
            ],
            "the order the validation flow visits the fields"
        );
        assert_eq!(
            Route::FIELD_ORDER,
            &[
                "id",
                "enabled",
                "priority",
                "tags",
                "rate_limit",
                "cors",
                "headers",
                "auth",
                "plugins",
                "match",
                "upstream_id"
            ]
        );
        assert_eq!(Upstream::FIELD_ORDER.len(), 11);
        assert_eq!(Route::FIELD_ORDER.len(), 11);
        let shared = ["rate_limit", "cors", "headers", "auth", "plugins", "tags"];
        let ranks = |field_order: &[&str]| -> Vec<usize> {
            let ordered: Vec<usize> = shared
                .iter()
                .map(|block| {
                    field_order
                        .iter()
                        .position(|field| field == block)
                        .unwrap_or(usize::MAX)
                })
                .collect();
            let mut rank = ordered.clone();
            rank.sort_unstable();
            ordered
                .iter()
                .map(|position| {
                    rank.iter()
                        .position(|candidate| candidate == position)
                        .unwrap()
                })
                .collect()
        };
        assert_eq!(
            ranks(Upstream::FIELD_ORDER),
            ranks(Route::FIELD_ORDER),
            "the shared blocks keep one relative order in both aggregates"
        );
    }

    #[test]
    fn the_declared_enums_are_closed() {
        assert_eq!(SHARING_MODES, &["private", "inherit", "enforce"]);
        assert_eq!(RATE_ALGORITHMS, &["token_bucket", "sliding_window"]);
        assert_eq!(RATE_WINDOWS, &["second", "minute", "hour", "day"]);
        assert_eq!(RATE_SCOPES, &["global", "tenant", "user", "ip", "route"]);
        assert_eq!(RATE_STRATEGIES, &["reject", "queue", "degrade"]);
        assert_eq!(
            CORS_METHODS,
            &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
        );
        assert_eq!(HTTP_METHODS, &["GET", "POST", "PUT", "DELETE", "PATCH"]);
        assert_eq!(PATH_SUFFIX_MODES, &["disabled", "append"]);
        assert_eq!(PASSTHROUGH_MODES, &["none", "allowlist", "all"]);
        assert_eq!(PROTECTED_HEADERS, &["content-length", "content-type"]);
    }

    #[test]
    fn the_identifier_families_match_the_provisioned_ones() {
        assert_eq!(UPSTREAM_TYPE_ID, PROVISIONED_UPSTREAM_TYPE);
        assert_eq!(PROTOCOL_TYPE_ID, PROVISIONED_PROTOCOL_TYPE);
        assert_eq!(PLUGIN_TYPE_IDS, PROVISIONED_PLUGIN_TYPES);
        assert!(ROUTE_TYPE_ID.ends_with('~'));
    }

    #[test]
    fn a_plugin_ref_uuid_is_derived_only_from_a_uuid_backed_reference() {
        let uuid = Uuid::parse_str("3f0a1b2c-3d4e-4f50-8617-8899aabbccdd").unwrap();
        assert_eq!(
            plugin_ref_uuid("3f0a1b2c-3d4e-4f50-8617-8899aabbccdd"),
            Some(uuid),
            "the custom-plugin UUID form of the schema oneOf"
        );
        assert_eq!(
            plugin_ref_uuid(
                "gts.cf.core.oagw.guard_plugin.v1~3f0a1b2c-3d4e-4f50-8617-8899aabbccdd"
            ),
            Some(uuid),
            "the wrapped GTS form of DESIGN"
        );
        assert_eq!(
            plugin_ref_uuid("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
            None,
            "a named plugin keeps plugin_uuid absent"
        );
        assert_eq!(plugin_ref_uuid("gts.cf.core.oagw.guard_plugin.v1~"), None);
        assert_eq!(plugin_ref_uuid("not-a-plugin"), None);
    }

    #[test]
    fn a_plugin_reference_splits_into_family_and_instance() {
        let (family, instance) =
            split_plugin_reference("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.pii.v1")
                .unwrap();
        assert_eq!(family, "gts.cf.core.oagw.transform_plugin.v1~");
        assert_eq!(instance, "cf.core.oagw.pii.v1");
        assert!(split_plugin_reference("gts.cf.core.oagw.upstream.v1~x").is_none());
    }

    #[test]
    fn bindings_are_derived_with_contiguous_positions() {
        let plugins = PluginsConfig {
            sharing: SHARING_PRIVATE.to_owned(),
            items: vec![
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1".to_owned(),
                "3f0a1b2c-3d4e-4f50-8617-8899aabbccdd".to_owned(),
            ],
        };
        let bindings = plugins.bindings();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].position, 0);
        assert_eq!(bindings[1].position, 1);
        assert!(
            bindings[0].plugin_uuid.is_none(),
            "a named plugin keeps plugin_uuid absent"
        );
        assert!(bindings[1].plugin_uuid.is_some());
        assert_eq!(
            bindings[0].plugin_ref,
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert_eq!(bindings[0].config, None);
    }

    #[test]
    fn a_plugin_carries_its_gts_reference() {
        let plugin = Plugin {
            id: Some(Uuid::parse_str("3f0a1b2c-3d4e-4f50-8617-8899aabbccdd").unwrap()),
            plugin_type: Some("gts.cf.core.oagw.guard_plugin.v1~".to_owned()),
            ..Plugin::default()
        };
        assert_eq!(
            plugin.gts_ref().as_deref(),
            Some("gts.cf.core.oagw.guard_plugin.v1~3f0a1b2c-3d4e-4f50-8617-8899aabbccdd")
        );
        assert_eq!(Plugin::default().gts_ref(), None);
    }

    #[test]
    fn the_auth_config_exposes_its_secret_reference() {
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "config": { "secret_ref": "cred://tenant/openai" }
        }))
        .unwrap();
        assert_eq!(auth.secret_ref(), Some("cred://tenant/openai"));
        let auth: AuthConfig =
            serde_json::from_value(serde_json::json!({ "config": { "header": "x-api-key" } }))
                .unwrap();
        assert_eq!(auth.secret_ref(), None);
    }
}
