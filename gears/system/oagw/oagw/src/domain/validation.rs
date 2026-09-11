//! Request shapes and their validation.
//!
//! The transport binds a request body to one of the `*Spec` types here and then
//! calls the matching validator, which resolves every recorded default and
//! returns the typed entity of [`crate::domain::model`] — or the *first*
//! validation failure with the offending field named, for the entry-2.1 mapping
//! layer. The validators never touch the store and never read a credential:
//! secret references are checked for well-formedness only
//! (`cpt-cf-oagw-principle-cred-isolation`).
//!
//! Unknown members are rejected wherever the shipped schema sets
//! `additionalProperties: false`, so a typo becomes a `400` instead of a silently
//! ignored field.

use std::collections::BTreeMap;

use serde::Deserialize;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::plugin::{self, NAMED_INSTANCE_PREFIX};
use crate::domain::model::{
    AuthConfig, BurstRate, CorsConfig, CorsMethod, Endpoint, GrpcMatch, HeaderPassthrough,
    HeadersConfig, HttpMatch, HttpMethod, MatchRule, MatchType, PluginBinding, PluginsConfig,
    Protocol, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow, RequestHeaders,
    ResponseHeaders, Route, Scheme, ServerConfig, Sharing, SustainedRate, SuffixMode, Timestamp,
    Upstream,
};

/// GTS identifier of every builtin auth plugin type (`auth.type`).
///
/// Derived from the resolvable names the domain fixes for the `auth` type, so
/// the validator and the builtin registry can never disagree
/// (`cpt-cf-oagw-algo-plugin-catalog-register`, `inst-pcrg-08`). `basic` and
/// `bearer` stay catalog-only and fail with `unknown auth plugin`.
#[must_use]
pub fn builtin_auth_plugins() -> Vec<String> {
    plugin::RESOLVABLE_AUTH
        .map(|name| {
            format!(
                "{}~{NAMED_INSTANCE_PREFIX}{name}.v1",
                plugin::AUTH_PLUGIN_BASE
            )
        })
        .to_vec()
}

/// Alias pattern the schema fixes (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`).
pub const ALIAS_PATTERN: &str = "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$";

/// Tag pattern the schema fixes (`^[a-z0-9_-]+$`).
pub const TAG_PATTERN: &str = "^[a-z0-9_-]+$";

// @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-01
/// Request shape of `POST /oagw/v1/upstreams` and
/// `PUT /oagw/v1/upstreams/{id}`.
///
/// Mirrors `docs/schemas/upstream.v1.schema.json`: `id` is accepted and ignored
/// (the schema marks it `readOnly`), every optional member is `Option` so the
/// validator can tell *absent* from *defaulted*, and the root rejects unknown
/// members.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSpec {
    /// `id` — read-only; a supplied value is ignored.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// `enabled` — absent means `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `alias` — absent means derived (`cpt-cf-oagw-algo-alias-derive`).
    #[serde(default)]
    pub alias: Option<String>,
    /// `tags`.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// `server` — required.
    #[serde(default)]
    pub server: Option<ServerSpec>,
    /// `protocol` — required.
    #[serde(default)]
    pub protocol: Option<String>,
    /// `auth`.
    #[serde(default)]
    pub auth: Option<AuthSpec>,
    /// `headers`.
    #[serde(default)]
    pub headers: Option<HeadersSpec>,
    /// `rate_limit`.
    #[serde(default)]
    pub rate_limit: Option<RateLimitSpec>,
    /// `cors`.
    #[serde(default)]
    pub cors: Option<CorsSpec>,
    /// `plugins`.
    #[serde(default)]
    pub plugins: Option<PluginsSpec>,
}

/// `server` of an [`UpstreamSpec`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSpec {
    /// `server.endpoints` — required, at least one entry.
    #[serde(default)]
    pub endpoints: Option<Vec<EndpointSpec>>,
}

/// One `server.endpoints[]` entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSpec {
    /// `scheme` — required by the schema member set, defaults to `https`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// `host` — required.
    #[serde(default)]
    pub host: Option<String>,
    /// `port` — defaults to the scheme's standard port.
    #[serde(default)]
    pub port: Option<u16>,
}

/// `auth` of an [`UpstreamSpec`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSpec {
    /// `auth.type` — required.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// `auth.sharing` — defaults to `private`.
    #[serde(default)]
    pub sharing: Option<String>,
    /// `auth.config` — string values only; a `cred://` value is checked for
    /// well-formedness, never dereferenced.
    #[serde(default)]
    pub config: Option<BTreeMap<String, String>>,
}

/// `headers` of an [`UpstreamSpec`] or a [`RouteSpec`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersSpec {
    /// `headers.request`.
    #[serde(default)]
    pub request: Option<RequestHeadersSpec>,
    /// `headers.response`.
    #[serde(default)]
    pub response: Option<ResponseHeadersSpec>,
}

/// `headers.request`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHeadersSpec {
    /// `request.set`.
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    /// `request.add`.
    #[serde(default)]
    pub add: Option<BTreeMap<String, String>>,
    /// `request.remove`.
    #[serde(default)]
    pub remove: Option<Vec<String>>,
    /// `request.passthrough` — `none|allowlist|all`, defaults to `none`.
    #[serde(default)]
    pub passthrough: Option<String>,
    /// `request.passthrough_allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Option<Vec<String>>,
}

/// `headers.response`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeadersSpec {
    /// `response.set`.
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    /// `response.add`.
    #[serde(default)]
    pub add: Option<BTreeMap<String, String>>,
    /// `response.remove`.
    #[serde(default)]
    pub remove: Option<Vec<String>>,
}

/// `rate_limit` of an [`UpstreamSpec`] or a [`RouteSpec`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitSpec {
    /// `rate_limit.sharing` — defaults to `private`.
    #[serde(default)]
    pub sharing: Option<String>,
    /// `rate_limit.algorithm` — defaults to `token_bucket`.
    #[serde(default)]
    pub algorithm: Option<String>,
    /// `rate_limit.sustained` — required.
    #[serde(default)]
    pub sustained: Option<SustainedRateSpec>,
    /// `rate_limit.burst`.
    #[serde(default)]
    pub burst: Option<BurstRateSpec>,
    /// `rate_limit.scope` — defaults to `tenant`.
    #[serde(default)]
    pub scope: Option<String>,
    /// `rate_limit.strategy` — defaults to `reject`.
    #[serde(default)]
    pub strategy: Option<String>,
    /// `rate_limit.cost` — defaults to `1`.
    #[serde(default)]
    pub cost: Option<u64>,
}

/// `rate_limit.sustained`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedRateSpec {
    /// `rate` — required, at least `1`.
    #[serde(default)]
    pub rate: Option<u64>,
    /// `window` — defaults to `second`.
    #[serde(default)]
    pub window: Option<String>,
}

/// `rate_limit.burst`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstRateSpec {
    /// `capacity` — at least `1`, defaults to `sustained.rate`.
    #[serde(default)]
    pub capacity: Option<u64>,
}

/// `cors` of an [`UpstreamSpec`] or a [`RouteSpec`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsSpec {
    /// `cors.sharing` — defaults to `private`.
    #[serde(default)]
    pub sharing: Option<String>,
    /// `cors.enabled` — required.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `cors.allowed_origins` — `*` or a URI.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
    /// `cors.allowed_methods` — defaults to `GET` and `POST`.
    #[serde(default)]
    pub allowed_methods: Option<Vec<String>>,
    /// `cors.expose_headers` — defaults to empty.
    #[serde(default)]
    pub expose_headers: Option<Vec<String>>,
    /// `cors.allow_credentials` — defaults to `false`.
    #[serde(default)]
    pub allow_credentials: Option<bool>,
}

/// `plugins` of an [`UpstreamSpec`] or a [`RouteSpec`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsSpec {
    /// `plugins.sharing` — defaults to `private`.
    #[serde(default)]
    pub sharing: Option<String>,
    /// `plugins.items[]` — plugin bindings, positions assigned in order.
    #[serde(default)]
    pub items: Option<Vec<PluginItemSpec>>,
}

/// One `plugins.items[]` entry.
///
/// The shipped schemas fix the item as a reference string — a GTS identifier or,
/// on an upstream, a custom-plugin UUID. The binding row of the logical model
/// also carries a `config`, which has no string form, so an object form is
/// accepted beside it and stored verbatim
/// (`cpt-cf-oagw-algo-plugin-binding-validate`, `inst-pbnd-10`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PluginItemSpec {
    /// A bare reference: a GTS identifier or a custom-plugin UUID.
    Reference(String),
    /// A reference with its declared configuration.
    Bound(BoundPluginSpec),
}

/// A `plugins.items[]` entry that carries a `config`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundPluginSpec {
    /// `plugin_ref` — the plugin identifier.
    pub plugin_ref: String,
    /// `config` — declared configuration, stored verbatim and never validated
    /// against the referenced plugin's `config_schema`.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}
// @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-01

// @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-01
/// Request shape of `POST /oagw/v1/routes` and `PUT /oagw/v1/routes/{id}`.
///
/// The shipped route schema does not close the root object, so unknown root
/// members are tolerated; the nested `match`, `http_match` and `grpc_match`
/// objects reject them. `enabled` and `priority` are the recorded deviation of
/// section 1.2 — the shipped schema does not declare them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RouteSpec {
    /// `id` — read-only; a supplied value is ignored.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// `tags`.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// `upstream_id` — required.
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    /// `match` — required, exactly one of `http` and `grpc`.
    #[serde(default, rename = "match")]
    pub matches: Option<MatchSpec>,
    /// `enabled` (deviation) — defaults to `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `priority` (deviation) — defaults to `0`.
    #[serde(default)]
    pub priority: Option<i32>,
    /// `plugins`.
    #[serde(default)]
    pub plugins: Option<PluginsSpec>,
    /// `rate_limit`.
    #[serde(default)]
    pub rate_limit: Option<RateLimitSpec>,
    /// `cors` (deviation: routes carry their own CORS declaration).
    #[serde(default)]
    pub cors: Option<CorsSpec>,
}

/// `match` of a [`RouteSpec`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    /// `match.http`.
    #[serde(default)]
    pub http: Option<HttpMatchSpec>,
    /// `match.grpc`.
    #[serde(default)]
    pub grpc: Option<GrpcMatchSpec>,
}

/// `match.http`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatchSpec {
    /// `methods` — required, at least one of `GET|POST|PUT|DELETE|PATCH`.
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    /// `path` — required, at least one character.
    #[serde(default)]
    pub path: Option<String>,
    /// `query_allowlist` — defaults to empty.
    #[serde(default)]
    pub query_allowlist: Option<Vec<String>>,
    /// `path_suffix_mode` — `disabled|append`, defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: Option<String>,
}

/// `match.grpc`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatchSpec {
    /// `service` — required, at least one character.
    #[serde(default)]
    pub service: Option<String>,
    /// `method` — required, at least one character.
    #[serde(default)]
    pub method: Option<String>,
}
// @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-01

/// Build the `field: reason` detail the mapping layer renders.
fn invalid(field: &str, reason: &str) -> DomainError {
    DomainError::ValidationError {
        detail: format!("{field}: {reason}"),
    }
}

// @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-12
/// Validate an upstream request and resolve every recorded default.
///
/// Returns the typed entity, or the first failure with the offending field
/// named. `tenant_id` is the calling tenant and `now` the creation instant the
/// store stamps; both are copied verbatim into the result, which is why they are
/// arguments and not defaults.
pub fn validate_upstream(
    spec: &UpstreamSpec,
    tenant_id: Uuid,
    created_at: Timestamp,
) -> Result<Upstream, DomainError> {
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-12
    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-01
    // `server` and `protocol` are required; unknown members were already
    // rejected by the `deny_unknown_fields` attribute of the spec.
    let server = spec.server.as_ref().ok_or_else(|| invalid("server", "required"))?;
    let protocol = spec
        .protocol
        .as_deref()
        .ok_or_else(|| invalid("protocol", "required"))?;
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-01

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-03
    let protocol = Protocol::parse(protocol)
        .ok_or_else(|| invalid("protocol", "unknown protocol identifier"))?;
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-03

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-02
    let server = validate_server(server)?;
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-02

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-10
    let tags = validate_tags(spec.tags.as_deref(), "tags")?;
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-10

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-04
    let auth = match spec.auth.as_ref() {
        Some(auth) => Some(validate_auth(auth)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-04

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-05
    // The `cred://` well-formedness check is part of `validate_auth`; no
    // credential store is consulted here.
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-05

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-06
    let headers = match spec.headers.as_ref() {
        Some(headers) => Some(validate_headers(headers)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-06

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-07
    let rate_limit = match spec.rate_limit.as_ref() {
        Some(rate_limit) => Some(validate_rate_limit(rate_limit)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-07

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-08
    let cors = match spec.cors.as_ref() {
        Some(cors) => Some(validate_cors(cors)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-08

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-09
    let plugins = match spec.plugins.as_ref() {
        Some(plugins) => Some(validate_plugins(plugins, PluginRefKind::Upstream)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-09

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-11
    // Recorded defaults: `enabled: true` plus the schema defaults each block
    // validator resolved. Plugin positions are assigned here and re-checked by
    // the store's invariant set; UUID-backed plugin resolvability is entry 2.3.
    // The auth plugin identity is additionally carried on the scalar columns
    // the plugin-in-use scan reads, so no check depends on JSON scanning.
    let (auth_plugin_ref, auth_plugin_uuid) = auth_reference(&auth);
    let upstream = Upstream {
        id: spec.id.unwrap_or_default(),
        tenant_id,
        enabled: spec.enabled.unwrap_or(true),
        alias: spec.alias.clone().unwrap_or_default(),
        tags,
        server,
        protocol,
        auth,
        auth_plugin_ref,
        auth_plugin_uuid,
        headers,
        rate_limit,
        cors,
        plugins,
        created_at,
    };
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-11

    // @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-12
    Ok(upstream)
    // @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-12
}

/// Validate `server.endpoints[]` and apply the endpoint defaults.
fn validate_server(spec: &ServerSpec) -> Result<ServerConfig, DomainError> {
    let endpoints = spec
        .endpoints
        .as_ref()
        .ok_or_else(|| invalid("server.endpoints", "required"))?;
    if endpoints.is_empty() {
        return Err(invalid("server.endpoints", "at least one endpoint is required"));
    }

    let mut resolved = Vec::with_capacity(endpoints.len());
    for (index, endpoint) in endpoints.iter().enumerate() {
        let field = format!("server.endpoints[{index}]");
        let host = endpoint
            .host
            .as_deref()
            .ok_or_else(|| invalid(&field, "host is required"))?;
        let host = normalize_host(host)
            .ok_or_else(|| invalid(&format!("{field}.host"), "not a hostname or IP address"))?;
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-03
        let scheme = match endpoint.scheme.as_deref() {
            Some(raw) => Scheme::parse(raw)
                .ok_or_else(|| invalid(&format!("{field}.scheme"), "unknown scheme"))?,
            // The schema records `https` as the default scheme.
            None => Scheme::Https,
        };
        let port = endpoint.port.unwrap_or_else(|| scheme.default_port());
        if port == 0 {
            return Err(invalid(&format!("{field}.port"), "must be between 1 and 65535"));
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-03
        resolved.push(Endpoint {
            scheme,
            host,
            port,
        });
    }

    Ok(ServerConfig { endpoints: resolved })
}

/// Normalize a host to ASCII lowercase without a single trailing dot.
///
/// Returns `None` when the value is neither a hostname nor an IP literal, which
/// is the shape the schema's `host` member allows.
#[must_use]
pub fn normalize_host(host: &str) -> Option<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return None;
    }
    // An IPv6 literal may arrive bracketed, the URI form of the same address.
    let stripped = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .map_or(trimmed, |inner| inner);
    let lowered = stripped.to_ascii_lowercase();
    let lowered = lowered.strip_suffix('.').unwrap_or(&lowered);

    if let Ok(address) = lowered.parse::<std::net::IpAddr>() {
        return Some(address.to_string());
    }
    if is_valid_hostname(lowered) {
        return Some(lowered.to_string());
    }
    None
}

/// RFC 1123 hostname check: at most 253 characters, labels of 1 to 63
/// alphanumeric-or-hyphen characters that do not start or end with a hyphen.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Validate `tags[]` against the schema pattern.
fn validate_tags(tags: Option<&[String]>, field: &str) -> Result<Vec<String>, DomainError> {
    let Some(tags) = tags else {
        return Ok(Vec::new());
    };
    for tag in tags {
        if !valid_tag(tag) {
            return Err(invalid(field, &format!("tag `{tag}` must match {TAG_PATTERN}")));
        }
    }
    Ok(tags.to_vec())
}

/// Whether one tag matches `^[a-z0-9_-]+$`.
#[must_use]
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Validate `auth` and check its `cred://` references for well-formedness.
fn validate_auth(spec: &AuthSpec) -> Result<AuthConfig, DomainError> {
    let kind = spec
        .kind
        .as_deref()
        .ok_or_else(|| invalid("auth.type", "required"))?;
    // A builtin named plugin resolves through `AuthPluginRegistry`; a
    // catalog-only identifier (`basic`, `bearer`) is registered but has no
    // backing implementation and fails here; a UUID-backed custom plugin is
    // resolved against the stored definition by the write path
    // (`cpt-cf-oagw-algo-plugin-binding-validate`, `inst-pbnd-08`).
    let identifier = plugin::PluginIdentifier::parse(kind)
        .ok_or_else(|| invalid("auth.type", "unknown auth plugin"))?;
    let auth_plugin = match identifier.classify() {
        plugin::PluginClass::Named(plugin::PluginType::Auth, _) => Some(identifier.clone()),
        plugin::PluginClass::Custom(_) => Some(identifier.clone()),
        _ => None,
    };
    let base_is_auth = matches!(
        identifier.base,
        plugin::PluginBase::Typed(plugin::PluginType::Auth) | plugin::PluginBase::Bare
    );
    if auth_plugin.is_none() || !base_is_auth {
        return Err(invalid("auth.type", "unknown auth plugin"));
    }
    let sharing = parse_sharing(spec.sharing.as_deref(), "auth.sharing")?;

    let mut config = BTreeMap::new();
    if let Some(values) = spec.config.as_ref() {
        for (key, value) in values {
            if let Some(false) = value.strip_prefix("cred://").map(is_valid_cred_reference) {
                return Err(invalid(
                    &format!("auth.config.{key}"),
                    "not a well-formed cred:// reference",
                ));
            }
            config.insert(key.clone(), value.clone());
        }
    }

    Ok(AuthConfig {
        kind: kind.to_string(),
        sharing,
        config,
    })
}

/// Whether the instance part of a `cred://` reference is well-formed.
#[must_use]
pub fn is_valid_cred_reference(instance: &str) -> bool {
    !instance.is_empty()
        && instance
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
        && !instance.ends_with('/')
}

/// Validate `headers` and apply `passthrough`'s recorded default.
fn validate_headers(spec: &HeadersSpec) -> Result<HeadersConfig, DomainError> {
    let request = match spec.request.as_ref() {
        Some(request) => {
            let passthrough = match request.passthrough.as_deref() {
                Some(raw) => HeaderPassthrough::parse(raw)
                    .ok_or_else(|| invalid("headers.request.passthrough", "unknown passthrough mode"))?,
                None => HeaderPassthrough::None,
            };
            RequestHeaders {
                set: request.set.clone().unwrap_or_default(),
                add: request.add.clone().unwrap_or_default(),
                remove: request.remove.clone().unwrap_or_default(),
                passthrough,
                passthrough_allowlist: request.passthrough_allowlist.clone().unwrap_or_default(),
            }
        }
        None => RequestHeaders::default(),
    };
    let response = match spec.response.as_ref() {
        Some(response) => ResponseHeaders {
            set: response.set.clone().unwrap_or_default(),
            add: response.add.clone().unwrap_or_default(),
            remove: response.remove.clone().unwrap_or_default(),
        },
        None => ResponseHeaders::default(),
    };
    Ok(HeadersConfig { request, response })
}

/// Validate `rate_limit` and apply every recorded default.
fn validate_rate_limit(spec: &RateLimitSpec) -> Result<RateLimitConfig, DomainError> {
    let sustained = spec
        .sustained
        .as_ref()
        .ok_or_else(|| invalid("rate_limit.sustained", "required"))?;
    let rate = sustained
        .rate
        .ok_or_else(|| invalid("rate_limit.sustained.rate", "required"))?;
    if rate == 0 {
        return Err(invalid("rate_limit.sustained.rate", "must be at least 1"));
    }
    let window = match sustained.window.as_deref() {
        Some(raw) => RateWindow::parse(raw)
            .ok_or_else(|| invalid("rate_limit.sustained.window", "unknown window"))?,
        None => RateWindow::Second,
    };

    if Some(0) == spec.burst.as_ref().and_then(|burst| burst.capacity) {
        return Err(invalid("rate_limit.burst.capacity", "must be at least 1"));
    }

    let algorithm = match spec.algorithm.as_deref() {
        Some(raw) => RateAlgorithm::parse(raw)
            .ok_or_else(|| invalid("rate_limit.algorithm", "unknown algorithm"))?,
        None => RateAlgorithm::TokenBucket,
    };
    let scope = match spec.scope.as_deref() {
        Some(raw) => RateScope::parse(raw).ok_or_else(|| invalid("rate_limit.scope", "unknown scope"))?,
        None => RateScope::Tenant,
    };
    let strategy = match spec.strategy.as_deref() {
        Some(raw) => RateStrategy::parse(raw)
            .ok_or_else(|| invalid("rate_limit.strategy", "unknown strategy"))?,
        None => RateStrategy::Reject,
    };
    if spec.cost == Some(0) {
        return Err(invalid("rate_limit.cost", "must be at least 1"));
    }
    let sharing = parse_sharing(spec.sharing.as_deref(), "rate_limit.sharing")?;

    Ok(RateLimitConfig {
        sharing,
        algorithm,
        sustained: SustainedRate { rate, window },
        burst: spec.burst.as_ref().map(|burst| BurstRate {
            capacity: burst.capacity,
        }),
        scope,
        strategy,
        cost: spec.cost.unwrap_or(1),
    })
}

/// Validate `cors` and apply every recorded default.
fn validate_cors(spec: &CorsSpec) -> Result<CorsConfig, DomainError> {
    let enabled = spec
        .enabled
        .ok_or_else(|| invalid("cors.enabled", "required"))?;

    let mut origins = Vec::new();
    if let Some(allowed_origins) = spec.allowed_origins.as_ref() {
        for origin in allowed_origins {
            if origin != "*" && !is_valid_origin(origin) {
                return Err(invalid("cors.allowed_origins", &format!("`{origin}` is not a URI")));
            }
            origins.push(origin.clone());
        }
    }

    let mut methods = Vec::new();
    if let Some(allowed_methods) = spec.allowed_methods.as_ref() {
        for method in allowed_methods {
            let parsed = CorsMethod::parse(method)
                .ok_or_else(|| invalid("cors.allowed_methods", &format!("unknown method `{method}`")))?;
            if !methods.contains(&parsed) {
                methods.push(parsed);
            }
        }
    } else {
        // Recorded default of the schema.
        methods = vec![CorsMethod::Get, CorsMethod::Post];
    }

    let allow_credentials = spec.allow_credentials.unwrap_or(false);
    if allow_credentials && origins.iter().any(|origin| origin == "*") {
        return Err(invalid(
            "cors.allowed_origins",
            "the wildcard origin cannot be combined with allow_credentials",
        ));
    }

    Ok(CorsConfig {
        sharing: parse_sharing(spec.sharing.as_deref(), "cors.sharing")?,
        enabled,
        allowed_origins: origins,
        allowed_methods: methods.iter().map(|method| method.as_str().to_string()).collect(),
        expose_headers: spec.expose_headers.clone().unwrap_or_default(),
        allow_credentials,
    })
}

/// Whether an origin is `*` or an absolute URI with a scheme.
#[must_use]
pub fn is_valid_origin(origin: &str) -> bool {
    origin.contains("://") && !origin.contains(' ') && url::Url::parse(origin).is_ok()
}

/// The scalar auth plugin columns of an upstream, derived from `auth`.
///
/// `auth_plugin_ref` is the reference exactly as `auth.type` carries it and
/// `auth_plugin_uuid` is its instance UUID when the reference is UUID-backed, so
/// a named row keeps the UUID column unset
/// (`cpt-cf-oagw-algo-plugin-binding-validate`, `inst-pbnd-08`).
#[must_use]
fn auth_reference(auth: &Option<AuthConfig>) -> (Option<String>, Option<Uuid>) {
    match auth.as_ref() {
        Some(config) => (
            Some(config.kind.clone()),
            uuid_of_plugin_reference(&config.kind),
        ),
        None => (None, None),
    }
}

/// Validate `plugins` and assign the contiguous positions.
fn validate_plugins(spec: &PluginsSpec, kind: PluginRefKind) -> Result<PluginsConfig, DomainError> {
    let sharing = parse_sharing(spec.sharing.as_deref(), "plugins.sharing")?;
    let mut items = Vec::new();
    for (position, item) in spec.items.clone().unwrap_or_default().into_iter().enumerate() {
        // The reference is canonicalized and the declared configuration is kept
        // verbatim: no check against the referenced plugin's `config_schema`
        // happens here, per the recorded non-goal of section 1.2.
        let (raw, config) = match item {
            PluginItemSpec::Reference(raw) => (raw, None),
            PluginItemSpec::Bound(bound) => (bound.plugin_ref, bound.config),
        };
        let reference = normalize_plugin_reference(&raw, kind)
            .ok_or_else(|| invalid("plugins.items", &format!("`{raw}` is not a plugin reference")))?;
        items.push(PluginBinding {
            position: u32::try_from(position).unwrap_or(u32::MAX),
            plugin_uuid: uuid_of_plugin_reference(&reference),
            reference,
            config,
        });
    }
    Ok(PluginsConfig { sharing, items })
}

/// Which references a `plugins.items[]` entry may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginRefKind {
    /// An upstream accepts a GTS identifier or a bare custom-plugin UUID.
    Upstream,
    /// A route accepts a GTS identifier.
    Route,
}

/// Canonicalize one `plugins.items[]` entry.
fn normalize_plugin_reference(raw: &str, kind: PluginRefKind) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("gts.") {
        return Some(trimmed.to_string());
    }
    match kind {
        // The upstream schema allows a bare custom-plugin UUID.
        PluginRefKind::Upstream => Uuid::parse_str(trimmed)
            .ok()
            .map(|uuid| format!("{PLUGIN_BASE}~{uuid}")),
        PluginRefKind::Route => None,
    }
}

/// GTS base type of a bindable plugin reference.
const PLUGIN_BASE: &str = "gts.cf.core.oagw.plugin.v1";

/// The UUID of a UUID-backed plugin reference, `None` for a named plugin.
#[must_use]
pub fn uuid_of_plugin_reference(reference: &str) -> Option<Uuid> {
    let instance = reference.split('~').nth(1)?;
    Uuid::parse_str(instance).ok()
}

/// Parse a `sharing` member, applying the recorded `private` default.
fn parse_sharing(raw: Option<&str>, field: &str) -> Result<Sharing, DomainError> {
    match raw {
        Some(raw) => Sharing::parse(raw).ok_or_else(|| invalid(field, "unknown sharing mode")),
        None => Ok(Sharing::Private),
    }
}

// @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-13
/// Validate a route request and resolve every recorded default.
///
/// `tenant_id` and `created_at` are copied verbatim; `upstream_id` is validated
/// for shape here and resolved against the calling tenant by the service flow
/// (`inst-rval-09`), because resolution needs the store.
pub fn validate_route(
    spec: &RouteSpec,
    tenant_id: Uuid,
    created_at: Timestamp,
) -> Result<Route, DomainError> {
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-13
    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-01
    let upstream_id = spec
        .upstream_id
        .ok_or_else(|| invalid("upstream_id", "required"))?;
    let matches = spec.matches.as_ref().ok_or_else(|| invalid("match", "required"))?;
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-01

    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-02
    let (matches, match_type) = validate_match(matches)?;
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-02

    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-07
    let tags = validate_tags(spec.tags.as_deref(), "tags")?;
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-07

    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-05
    let rate_limit = match spec.rate_limit.as_ref() {
        Some(rate_limit) => Some(validate_rate_limit(rate_limit)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-05

    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-14
    let cors = match spec.cors.as_ref() {
        Some(cors) => Some(validate_cors(cors)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-14

    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-06
    let plugins = match spec.plugins.as_ref() {
        Some(plugins) => Some(validate_plugins(plugins, PluginRefKind::Route)?),
        None => None,
    };
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-06

    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-08
    let route = Route {
        id: spec.id.unwrap_or_default(),
        tenant_id,
        upstream_id,
        enabled: spec.enabled.unwrap_or(true),
        matches,
        match_type,
        priority: spec.priority.unwrap_or(0),
        plugins,
        rate_limit,
        cors,
        tags,
        created_at,
    };
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-08

    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-13
    Ok(route)
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-13
}

/// Validate `match`: exactly one of `http` and `grpc`, plus the derived kind.
fn validate_match(spec: &MatchSpec) -> Result<(MatchRule, MatchType), DomainError> {
    match (spec.http.as_ref(), spec.grpc.as_ref()) {
        (Some(http), None) => {
            let http = validate_http_match(http)?;
            Ok((MatchRule { http: Some(http), grpc: None }, MatchType::Http))
        }
        (None, Some(grpc)) => {
            let grpc = validate_grpc_match(grpc)?;
            Ok((MatchRule { http: None, grpc: Some(grpc) }, MatchType::Grpc))
        }
        (Some(_), Some(_)) => Err(invalid("match", "exactly one of http and grpc is allowed")),
        (None, None) => Err(invalid("match", "one of http and grpc is required")),
    }
}

/// Validate `match.http` and apply the recorded defaults.
fn validate_http_match(spec: &HttpMatchSpec) -> Result<HttpMatch, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-03
    let methods = spec
        .methods
        .as_ref()
        .ok_or_else(|| invalid("match.http.methods", "required"))?;
    if methods.is_empty() {
        return Err(invalid("match.http.methods", "at least one method is required"));
    }
    let mut resolved = Vec::with_capacity(methods.len());
    for method in methods {
        let parsed = HttpMethod::parse(method)
            .ok_or_else(|| invalid("match.http.methods", &format!("unknown method `{method}`")))?;
        if !resolved.contains(&parsed) {
            resolved.push(parsed);
        }
    }

    let path = spec
        .path
        .as_deref()
        .ok_or_else(|| invalid("match.http.path", "required"))?;
    if path.is_empty() {
        return Err(invalid("match.http.path", "must be at least one character"));
    }

    let suffix_mode = match spec.path_suffix_mode.as_deref() {
        Some(raw) => SuffixMode::parse(raw)
            .ok_or_else(|| invalid("match.http.path_suffix_mode", "unknown mode"))?,
        None => SuffixMode::Append,
    };

    Ok(HttpMatch {
        methods: resolved.iter().map(|method| method.as_str().to_string()).collect(),
        path: path.to_string(),
        query_allowlist: spec.query_allowlist.clone().unwrap_or_default(),
        path_suffix_mode: suffix_mode,
    })
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-03
}

/// Validate `match.grpc` (stored as declared; no gRPC path is served).
fn validate_grpc_match(spec: &GrpcMatchSpec) -> Result<GrpcMatch, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-04
    let service = spec
        .service
        .as_deref()
        .ok_or_else(|| invalid("match.grpc.service", "required"))?;
    let method = spec
        .method
        .as_deref()
        .ok_or_else(|| invalid("match.grpc.method", "required"))?;
    if service.is_empty() {
        return Err(invalid("match.grpc.service", "must be at least one character"));
    }
    if method.is_empty() {
        return Err(invalid("match.grpc.method", "must be at least one character"));
    }
    Ok(GrpcMatch {
        service: service.to_string(),
        method: method.to_string(),
    })
    // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-04
}

// @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-01
/// Request shape of `POST /oagw/v1/plugins`.
///
/// No shipped plugin schema exists, so the accepted field set is exactly the
/// stored definition's (`cpt-cf-oagw-dod-plugin-model`): the declared contract
/// members plus the server-managed storage metadata, which a body may carry but
/// which the store stamps (`inst-pdef-08`). Unknown properties are rejected, so
/// a typo becomes a `400` instead of a silently ignored field.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    /// `id` — read-only; a supplied value is ignored.
    #[serde(default)]
    pub id: Option<String>,
    /// `plugin_type` — required, one of `auth|guard|transform`.
    #[serde(default)]
    pub plugin_type: Option<String>,
    /// `name` — required and unique per tenant.
    #[serde(default)]
    pub name: Option<String>,
    /// `description`.
    #[serde(default)]
    pub description: Option<String>,
    /// `config_schema` — a JSON object when present, stored verbatim.
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,
    /// `phases` — every entry in `on_request|on_response|on_error`.
    #[serde(default)]
    pub phases: Option<Vec<String>>,
    /// `source_code` — the stored script, verbatim and unchecked.
    #[serde(default)]
    pub source_code: Option<String>,
    /// `tenant_id` — read-only; a supplied value is ignored.
    #[serde(default)]
    pub tenant_id: Option<Uuid>,
    /// `last_used_at` — read-only; a supplied value is ignored.
    #[serde(default)]
    pub last_used_at: Option<serde_json::Value>,
    /// `gc_eligible_at` — read-only; a supplied value is ignored.
    #[serde(default)]
    pub gc_eligible_at: Option<serde_json::Value>,
}

/// The declared plugin contract members of a [`PluginSpec`], after validation.
///
/// The server-managed members are the store's: `id` is generated, `tenant_id`
/// comes from the security context, and `last_used_at` / `gc_eligible_at` stay
/// unset because no usage tracking or GC job exists in this deployment.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginContract {
    /// Plugin type in `auth|guard|transform`.
    pub plugin_type: plugin::PluginType,
    /// Tenant-unique name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Declared configuration contract, verbatim.
    pub config_schema: serde_json::Value,
    /// Declared phase set.
    pub phases: Vec<plugin::Phase>,
    /// Stored script source, verbatim.
    pub source_code: String,
}

/// Validate a plugin create request and classify its `plugin_type`.
///
/// The definition is validated on create only, because no replace operation
/// exists (`inst-pdef-09`). `tenant_id` and `created_at` are not arguments: the
/// definition carries no server-managed instant of its own, so the store stamps
/// only the identity.
///
/// # Errors
///
/// Returns the first failure with the offending field named, for the entry-2.1
/// mapping layer.
pub fn validate_plugin(spec: &PluginSpec) -> Result<PluginContract, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-09
    // The validator is called on create only: a plugin definition is immutable
    // for its lifetime and no replace operation exists to re-run it.
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-09

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-02
    let plugin_type = spec
        .plugin_type
        .as_deref()
        .and_then(plugin::PluginType::parse)
        .ok_or_else(|| invalid("plugin_type", "must be one of `auth`, `guard`, `transform`"))?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-02

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-03
    let name = spec
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid("name", "must be a non-empty string"))?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-03

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-04
    if let Some(schema) = spec.config_schema.as_ref()
        && !schema.is_object()
    {
        return Err(invalid("config_schema", "must be a JSON object"));
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-04

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-05
    let mut phases = Vec::new();
    for raw in spec.phases.as_deref().unwrap_or(&[]) {
        let phase = plugin::Phase::parse(raw)
            .ok_or_else(|| invalid("phases", &format!("unknown phase `{raw}`")))?;
        phases.push(phase);
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-05

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-06
    // No syntax check and no execution: the source is a storage and contract
    // artifact only (DECOMPOSITION assumption 4). An absent `source_code`
    // stores an empty source, so a definition can describe a builtin-shaped
    // contract without a script body.
    let source_code = spec.source_code.clone().unwrap_or_default();
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-06

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-07
    // The definition and its `config_schema` hold configuration metadata only:
    // no `cred://` reference is resolved on this path and no credential
    // material is carried in either direction.
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-07

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-11
    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-12
    // A failure above leaves through the `?` of the `invalid` constructor, which
    // carries the offending field and the reason to the caller flow for the
    // entry-2.1 mapping layer, and no definition is built.
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-12
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-11

    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-10
    Ok(PluginContract {
        plugin_type,
        name: name.to_owned(),
        description: spec.description.clone().unwrap_or_default(),
        config_schema: spec.config_schema.clone().unwrap_or(serde_json::Value::Null),
        phases,
        source_code,
    })
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-10
    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-13
    // The typed plugin definition is what the create flow stores; the store
    // derives its GTS identifier from the UUID it generates.
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-13
}

/// Build the stored definition from a validated contract and the server-managed
/// identity.
///
/// The store generates the UUID, derives the GTS identifier from it and stamps
/// the tenant; `last_used_at` and `gc_eligible_at` stay unset
/// (`cpt-cf-oagw-dod-plugin-model`, `inst-full`).
#[must_use]
pub fn build_plugin(
    contract: PluginContract,
    tenant_id: Uuid,
    id: Uuid,
) -> crate::domain::model::Plugin {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-08
    // The server-managed fields: the store generates the `id` UUID and stamps
    // the `tenant_id` from the security context, while `last_used_at` and
    // `gc_eligible_at` stay unset because this deployment has no usage tracking
    // and no GC job (the deviation record of section 1.2).
    crate::domain::model::Plugin {
        id: format!("{}~{id}", contract.plugin_type.base_identifier()),
        tenant_id,
        plugin_type: contract.plugin_type,
        name: contract.name,
        description: contract.description,
        config_schema: contract.config_schema,
        phases: contract.phases,
        source_code: contract.source_code,
        last_used_at: None,
        gc_eligible_at: None,
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-08
}
// @cpt-end:cpt-cf-oagw-algo-plugin-definition-validate:p1:inst-pdef-01

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PROTOCOL_HTTP;
    use serde_json::json;

    const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000001");

    fn upstream_from(body: serde_json::Value) -> Result<Upstream, DomainError> {
        let spec: UpstreamSpec = serde_json::from_value(body).expect("bindable body");
        validate_upstream(&spec, TENANT, Timestamp::from_nanos(1))
    }

    fn route_from(body: serde_json::Value) -> Result<Route, DomainError> {
        let spec: RouteSpec = serde_json::from_value(body).expect("bindable body");
        validate_route(&spec, TENANT, Timestamp::from_nanos(1))
    }

    fn minimal_upstream() -> serde_json::Value {
        json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP
        })
    }

    #[test]
    fn a_minimal_upstream_resolves_the_recorded_defaults() {
        let upstream = upstream_from(minimal_upstream()).expect("valid");
        assert!(upstream.enabled, "recorded default of `enabled`");
        assert_eq!(upstream.tags, Vec::<String>::new());
        assert_eq!(upstream.server.endpoints[0].scheme, Scheme::Https);
        assert_eq!(upstream.server.endpoints[0].port, 443, "recorded default port");
        assert_eq!(upstream.server.endpoints[0].host, "api.vendor.com");
        assert_eq!(upstream.tenant_id, TENANT);
        assert_eq!(upstream.created_at, Timestamp::from_nanos(1));
        assert!(upstream.auth.is_none());
        assert!(upstream.headers.is_none());
        assert!(upstream.rate_limit.is_none());
        assert!(upstream.cors.is_none());
        assert!(upstream.plugins.is_none());
    }

    #[test]
    fn the_read_only_id_is_accepted_and_carried_through() {
        let id = uuid::uuid!("11111111-2222-3333-4444-555555555555");
        let upstream = upstream_from(json!({
            "id": id,
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect("valid");
        assert_eq!(upstream.id, id, "the store keeps the caller-chosen identity");
    }

    #[test]
    fn missing_required_members_name_the_offending_field() {
        let error = upstream_from(json!({ "server": { "endpoints": [ { "host": "h" } ] } }))
            .expect_err("protocol missing");
        assert_eq!(error.status(), 400);
        assert_eq!(error.detail(), "protocol: required");

        let error = upstream_from(json!({ "protocol": PROTOCOL_HTTP }))
            .expect_err("server missing");
        assert_eq!(error.detail(), "server: required");

        let error = upstream_from(json!({
            "server": { "endpoints": [] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect_err("no endpoints");
        assert_eq!(error.detail(), "server.endpoints: at least one endpoint is required");
    }

    #[test]
    fn an_unknown_root_member_is_rejected_at_binding() {
        // `additionalProperties: false` is enforced by the request shape, so the
        // transport rejects the body before the validator runs; the reason names
        // the offending member.
        let bound: Result<UpstreamSpec, _> = serde_json::from_value(json!({
            "server": { "endpoints": [ { "host": "h" } ] },
            "protocol": PROTOCOL_HTTP,
            "routable": true
        }));
        let message = bound.expect_err("unknown member").to_string();
        assert!(message.contains("routable"), "{message}");
    }

    #[test]
    fn endpoint_hosts_are_normalized_and_ports_bounded() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "API.Vendor.COM.", "port": 8443 } ] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect("valid");
        assert_eq!(upstream.server.endpoints[0].host, "api.vendor.com");
        assert_eq!(upstream.server.endpoints[0].port, 8443);

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "-bad-.example", "port": 0 } ] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect_err("invalid host");
        assert!(error.detail().contains("server.endpoints[0]"), "{}", error.detail());
    }

    #[test]
    fn every_recorded_endpoint_scheme_is_accepted() {
        for (scheme, port) in [("https", 443), ("wss", 443), ("wt", 443), ("grpc", 443), ("http", 80)] {
            let upstream = upstream_from(json!({
                "server": { "endpoints": [ { "scheme": scheme, "host": "api.vendor.com" } ] },
                "protocol": PROTOCOL_HTTP
            }))
            .unwrap_or_else(|error| panic!("{scheme} must be accepted: {error}"));
            assert_eq!(upstream.server.endpoints[0].port, port);
        }
    }

    #[test]
    fn an_unknown_scheme_is_rejected_with_the_field_named() {
        let error = upstream_from(json!({
            "server": { "endpoints": [ { "scheme": "ftp", "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect_err("unknown scheme");
        assert_eq!(error.detail(), "server.endpoints[0].scheme: unknown scheme");
    }

    #[test]
    fn the_protocol_must_be_one_of_the_two_gts_identifiers() {
        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.ftp.v1"
        }))
        .expect_err("unknown protocol");
        assert_eq!(error.detail(), "protocol: unknown protocol identifier");
        assert_eq!(error.gts_id(), "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");
    }

    #[test]
    fn an_unknown_auth_plugin_is_rejected() {
        for kind in [
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1",
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
        ] {
            let upstream = upstream_from(json!({
                "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
                "protocol": PROTOCOL_HTTP,
                "auth": { "type": kind }
            }))
            .unwrap_or_else(|error| panic!("{kind} must be accepted: {error}"));
            assert_eq!(upstream.auth.as_ref().expect("auth").kind, kind);
            assert_eq!(upstream.auth.as_ref().expect("auth").sharing, Sharing::Private);
        }

        for kind in [
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        ] {
            let error = upstream_from(json!({
                "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
                "protocol": PROTOCOL_HTTP,
                "auth": { "type": kind }
            }))
            .expect_err("catalog-only identifier");
            assert_eq!(error.detail(), "auth.type: unknown auth plugin");
        }
    }

    #[test]
    fn auth_config_is_never_dereferenced_but_checked_for_well_formedness() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "auth": {
                "type": builtin_auth_plugins()[2],
                "config": {
                    "token_url": "https://auth.vendor.com/token",
                    "client_id": "oagw-gateway",
                    "client_secret": "cred://vendor/oauth2-client-secret",
                    "audience": "payments-api"
                }
            }
        }))
        .expect("valid");
        let auth = upstream.auth.expect("auth");
        assert_eq!(auth.config.get("client_secret").map(String::as_str), Some("cred://vendor/oauth2-client-secret"));

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "auth": {
                "type": builtin_auth_plugins()[1],
                "config": { "api_key": "cred://" }
            }
        }))
        .expect_err("empty credential reference");
        assert_eq!(error.detail(), "auth.config.api_key: not a well-formed cred:// reference");
    }

    #[test]
    fn headers_accept_the_schema_member_set_and_default_passthrough() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "headers": {
                "request": {
                    "set": { "x-tenant": "acme" },
                    "add": { "x-forwarded-by": "oagw" },
                    "remove": [ "x-internal" ],
                    "passthrough": "allowlist",
                    "passthrough_allowlist": [ "x-request-id" ]
                },
                "response": { "remove": [ "server" ] }
            }
        }))
        .expect("valid");
        let headers = upstream.headers.expect("headers");
        assert_eq!(headers.request.passthrough, HeaderPassthrough::Allowlist);
        assert_eq!(headers.request.set.get("x-tenant").map(String::as_str), Some("acme"));
        assert_eq!(headers.response.remove, vec!["server".to_string()]);

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "headers": { "request": { "passthrough": "everything" } }
        }))
        .expect_err("unknown passthrough");
        assert_eq!(error.detail(), "headers.request.passthrough: unknown passthrough mode");

        let defaulted = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "headers": {}
        }))
        .expect("valid");
        assert_eq!(defaulted.headers.expect("headers").request.passthrough, HeaderPassthrough::None);
    }

    #[test]
    fn rate_limit_resolves_every_recorded_default() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "rate_limit": { "sustained": { "rate": 100 } }
        }))
        .expect("valid");
        let rate_limit = upstream.rate_limit.expect("rate limit");
        assert_eq!(rate_limit.algorithm, RateAlgorithm::TokenBucket);
        assert_eq!(rate_limit.scope, RateScope::Tenant);
        assert_eq!(rate_limit.strategy, RateStrategy::Reject);
        assert_eq!(rate_limit.cost, 1);
        assert_eq!(rate_limit.sharing, Sharing::Private);
        assert_eq!(rate_limit.sustained.window, RateWindow::Second);
        assert_eq!(rate_limit.effective_capacity(), 100, "burst defaults to sustained.rate");

        for (field, value) in [
            ("algorithm", json!("leaky_bucket")),
            ("scope", json!("cluster")),
            ("strategy", json!("throttle")),
            ("sharing", json!("public")),
        ] {
            let error = upstream_from(json!({
                "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
                "protocol": PROTOCOL_HTTP,
                "rate_limit": { "sustained": { "rate": 10 }, field: value }
            }))
            .expect_err("unknown enum value");
            assert!(error.detail().starts_with(&format!("rate_limit.{field}")), "{}", error.detail());
        }

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "rate_limit": { "sustained": { "rate": 10 }, "burst": { "capacity": 0 } }
        }))
        .expect_err("zero burst capacity");
        assert_eq!(error.detail(), "rate_limit.burst.capacity: must be at least 1");

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "rate_limit": { "burst": { "capacity": 5 } }
        }))
        .expect_err("missing sustained");
        assert_eq!(error.detail(), "rate_limit.sustained: required");

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "rate_limit": { "sustained": { "rate": 0 } }
        }))
        .expect_err("zero sustained rate");
        assert_eq!(error.detail(), "rate_limit.sustained.rate: must be at least 1");
    }

    #[test]
    fn cors_requires_enabled_and_rejects_the_wildcard_with_credentials() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "cors": { "enabled": true, "allowed_origins": ["https://app.acme.dev"] }
        }))
        .expect("valid");
        let cors = upstream.cors.expect("cors");
        assert!(cors.enabled);
        assert_eq!(cors.allowed_methods, vec!["GET".to_string(), "POST".to_string()]);
        assert!(!cors.allow_credentials);
        assert_eq!(cors.sharing, Sharing::Private);
        assert!(cors.expose_headers.is_empty());

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "cors": { "allowed_origins": ["*"] }
        }))
        .expect_err("enabled missing");
        assert_eq!(error.detail(), "cors.enabled: required");

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "cors": { "enabled": true, "allow_credentials": true, "allowed_origins": ["*"] }
        }))
        .expect_err("wildcard with credentials");
        assert!(error.detail().contains("cors.allowed_origins"), "{}", error.detail());

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "cors": { "enabled": true, "allowed_methods": ["TRACE"] }
        }))
        .expect_err("unknown method");
        assert_eq!(error.detail(), "cors.allowed_methods: unknown method `TRACE`");
    }

    #[test]
    fn plugins_accept_gts_identifiers_and_bare_uuids_on_upstreams() {
        let custom = uuid::uuid!("22222222-3333-4444-5555-666666666666");
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "plugins": {
                "items": [
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    custom
                ]
            }
        }))
        .expect("valid");
        let plugins = upstream.plugins.expect("plugins");
        assert_eq!(plugins.sharing, Sharing::Private);
        assert_eq!(plugins.items.len(), 2);
        assert_eq!(plugins.items[0].position, 0);
        assert_eq!(plugins.items[1].position, 1, "positions are contiguous from 0");
        assert!(!plugins.items[0].is_custom());
        assert!(plugins.items[1].is_custom());
        assert_eq!(plugins.items[1].plugin_uuid, Some(custom));

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "plugins": { "items": ["not-a-plugin"] }
        }))
        .expect_err("not a reference");
        assert!(error.detail().starts_with("plugins.items"), "{}", error.detail());
    }

    #[test]
    fn the_grpc_protocol_is_accepted_and_stored() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "scheme": "grpc", "host": "api.vendor.com" } ] },
            "protocol": crate::domain::model::PROTOCOL_GRPC
        }))
        .expect("valid");
        assert_eq!(upstream.protocol.as_str(), crate::domain::model::PROTOCOL_GRPC);
    }

    #[test]
    fn tags_must_match_the_schema_pattern() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "tags": ["llm", "openai"]
        }))
        .expect("valid");
        assert_eq!(upstream.tags, vec!["llm".to_string(), "openai".to_string()]);

        let error = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "tags": ["OpenAI"]
        }))
        .expect_err("uppercase tag");
        assert!(error.detail().starts_with("tags"), "{}", error.detail());
    }

    #[test]
    fn a_minimal_route_resolves_the_recorded_defaults() {
        let upstream_id = uuid::uuid!("33333333-3333-3333-3333-333333333333");
        let route = route_from(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/pay" } }
        }))
        .expect("valid");
        assert_eq!(route.upstream_id, upstream_id);
        assert_eq!(route.match_type, MatchType::Http);
        assert!(route.enabled, "recorded deviation default");
        assert_eq!(route.priority, 0, "recorded deviation default");
        let http = route.matches.http.expect("http match");
        assert_eq!(http.path_suffix_mode, SuffixMode::Append);
        assert_eq!(http.query_allowlist, Vec::<String>::new());
        assert_eq!(route.created_at, Timestamp::from_nanos(1));
    }

    #[test]
    fn a_route_requires_upstream_id_and_exactly_one_match() {
        let error = route_from(json!({
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }))
        .expect_err("upstream_id missing");
        assert_eq!(error.detail(), "upstream_id: required");

        let error = route_from(json!({ "upstream_id": TENANT }))
            .expect_err("match missing");
        assert_eq!(error.detail(), "match: required");

        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": {}
        }))
        .expect_err("no match member");
        assert_eq!(error.detail(), "match: one of http and grpc is required");

        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": {
                "http": { "methods": ["GET"], "path": "/v1" },
                "grpc": { "service": "foo.v1.UserService", "method": "GetUser" }
            }
        }))
        .expect_err("both match members");
        assert_eq!(error.detail(), "match: exactly one of http and grpc is allowed");
    }

    #[test]
    fn a_route_http_match_bounds_its_methods_and_path() {
        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["TRACE"], "path": "/v1" } }
        }))
        .expect_err("unsupported method");
        assert_eq!(error.detail(), "match.http.methods: unknown method `TRACE`");

        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": [], "path": "/v1" } }
        }))
        .expect_err("no methods");
        assert_eq!(error.detail(), "match.http.methods: at least one method is required");

        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["GET"], "path": "" } }
        }))
        .expect_err("empty path");
        assert_eq!(error.detail(), "match.http.path: must be at least one character");

        let route = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["PATCH"], "path": "/v1", "path_suffix_mode": "disabled" } }
        }))
        .expect("valid");
        assert_eq!(
            route.matches.http.expect("http").path_suffix_mode,
            SuffixMode::Disabled
        );
    }

    #[test]
    fn a_route_grpc_match_is_stored_as_declared() {
        let route = route_from(json!({
            "upstream_id": TENANT,
            "match": { "grpc": { "service": "foo.v1.UserService", "method": "GetUser" } }
        }))
        .expect("valid");
        assert_eq!(route.match_type, MatchType::Grpc);
        let grpc = route.matches.grpc.expect("grpc match");
        assert_eq!(grpc.service, "foo.v1.UserService");
        assert_eq!(grpc.method, "GetUser");

        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": { "grpc": { "service": "", "method": "GetUser" } }
        }))
        .expect_err("empty service");
        assert_eq!(error.detail(), "match.grpc.service: must be at least one character");
    }

    #[test]
    fn a_route_rejects_unknown_members_on_the_nested_match_object() {
        // `match`, `http_match` and `grpc_match` close their member sets.
        let bound: Result<RouteSpec, _> = serde_json::from_value(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["GET"], "path": "/v1", "weigth": 1 } }
        }));
        let message = bound.expect_err("unknown nested member").to_string();
        assert!(message.contains("weigth"), "{message}");
    }

    #[test]
    fn a_route_accepts_unknown_root_members_like_the_schema() {
        // The shipped route schema does not close the root object, so an
        // unknown root member is tolerated rather than a 400.
        let route = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "note": "tenant-local annotation"
        }))
        .expect("valid");
        assert_eq!(route.matches.http.expect("http").path, "/v1");
    }

    #[test]
    fn route_plugins_accept_only_gts_identifiers() {
        let route = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"] }
        }))
        .expect("valid");
        assert_eq!(route.plugins.expect("plugins").items.len(), 1);

        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "plugins": { "items": ["22222222-3333-4444-5555-666666666666"] }
        }))
        .expect_err("bare UUID on a route");
        assert!(error.detail().starts_with("plugins.items"), "{}", error.detail());
    }

    #[test]
    fn route_tags_and_rate_limit_share_the_upstream_rules() {
        let error = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "tags": ["Not A Tag"]
        }))
        .expect_err("invalid tag");
        assert!(error.detail().starts_with("tags"), "{}", error.detail());

        let route = route_from(json!({
            "upstream_id": TENANT,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "rate_limit": { "sustained": { "rate": 5, "window": "minute" }, "scope": "ip" }
        }))
        .expect("valid");
        let rate_limit = route.rate_limit.expect("rate limit");
        assert_eq!(rate_limit.sustained.window, RateWindow::Minute);
        assert_eq!(rate_limit.scope, RateScope::Ip);
    }

    #[test]
    fn host_normalization_accepts_ip_literals_and_rejects_garbage() {
        assert_eq!(
            normalize_host("192.168.0.1").as_deref(),
            Some("192.168.0.1"),
            "IPv4 literal"
        );
        assert_eq!(
            normalize_host("[2001:db8::1]").as_deref(),
            Some("2001:db8::1"),
            "IPv6 literal"
        );
        assert_eq!(normalize_host("2001:db8::1").as_deref(), Some("2001:db8::1"));
        assert_eq!(normalize_host(""), None);
        assert_eq!(normalize_host("not a host"), None);
        assert!(is_valid_hostname("api.vendor.com"));
        assert!(!is_valid_hostname("-api.vendor.com"));
        assert!(!is_valid_hostname("api..com"));
    }

    #[test]
    fn credential_references_are_checked_for_shape_only() {
        assert!(is_valid_cred_reference("vendor/oauth2-client-secret"));
        assert!(is_valid_cred_reference("vendor/db/primary:password"));
        assert!(!is_valid_cred_reference(""));
        assert!(!is_valid_cred_reference("a b"));
        assert!(!is_valid_cred_reference("trailing/"));
    }

    #[test]
    fn origins_must_be_the_wildcard_or_a_uri() {
        // The wildcard is handled by the CORS validator itself, not by the URI
        // check; a bare authority and a URI with a space are both rejected.
        assert!(!is_valid_origin("*"));
        assert!(is_valid_origin("https://app.acme.dev"));
        assert!(!is_valid_origin("app.acme.dev"));
        assert!(!is_valid_origin("https://app.acme.dev/with space"));
    }

    #[test]
    fn the_wildcard_origin_is_accepted_without_credentials() {
        let upstream = upstream_from(json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": PROTOCOL_HTTP,
            "cors": { "enabled": true, "allowed_origins": ["*"] }
        }))
        .expect("valid");
        assert_eq!(upstream.cors.expect("cors").allowed_origins, vec!["*".to_string()]);
    }

    #[test]
    fn plugin_reference_uuid_extraction() {
        let custom = uuid::uuid!("22222222-3333-4444-5555-666666666666");
        assert_eq!(
            uuid_of_plugin_reference(&format!("{PLUGIN_BASE}~{custom}")),
            Some(custom)
        );
        assert_eq!(
            uuid_of_plugin_reference("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
            None,
            "a named plugin has no UUID instance"
        );
    }
}
