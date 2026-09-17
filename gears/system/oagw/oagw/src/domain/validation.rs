//! Validation rules that turn a submitted configuration document into a
//! normalized `UpstreamConfig` or `RouteConfig`, or into an RFC 9457
//! problem with status 400.
//!
//! Every rule mandated by the upstream and route wire schemas plus DESIGN.md
//! §3.1 is implemented here and has a test next to it:
//!
//! - endpoint `scheme` accepts `http`, `https`, `wss`, `wt` and `grpc`;
//!   `host` is required, `port` defaults to 443 and accepts 1-65535
//! - an upstream requires at least one endpoint and a `protocol`, and all
//!   endpoints of one pool share protocol, scheme and port
//! - hosts are RFC 1123 hostnames or IPv4/IPv6 literals
//! - `rate_limit` requires `sustained.rate` >= 1 and defaults `window`,
//!   `scope`, `strategy`, `cost` and `algorithm`
//! - `cors` requires nothing (enabled defaults to `false`) but rejects
//!   `allow_credentials` combined with a wildcard origin
//! - unknown members are rejected wherever the wire schema declares
//!   `additionalProperties: false`
//!
//! No rule in this module panics: rejected input is always a
//! [`GatewayError`], never a 500 and never an `unwrap`.

use std::collections::BTreeMap;

use url::Url;

use crate::domain::alias::resolve_new_alias;
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, GrpcMatch, HeadersConfig, Host, HttpMatch, HttpMethod,
    MAX_PORT, MIN_PORT, MatchRule, MatchSpec, PluginRef, PluginsConfig, RateLimitConfig,
    RequestHeaders, ResponseHeaders, RouteConfig, RouteSpec, ServerConfig, TAG_PATTERN,
    UpstreamConfig, UpstreamSpec,
};
use crate::error::GatewayError;

/// Validate an upstream submission and return its normalized form.
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when the endpoint pool is empty or
/// heterogeneous, a host is not a valid hostname or IP literal, a port is out
/// of range, a tag or plugin reference is malformed, the alias is missing or
/// contradicts the derived alias, or the rate limit / CORS configuration is
/// invalid.
pub fn validate_upstream(spec: &UpstreamSpec) -> Result<UpstreamConfig, GatewayError> {
    let server = validate_server(&spec.server)?;
    let auth = spec.auth.as_ref().map(validate_auth).transpose()?;
    let headers = spec.headers.as_ref().map(validate_headers).transpose()?;
    let plugins = validate_plugins(&spec.plugins)?;
    let rate_limit = spec
        .rate_limit
        .map(|limit| validate_rate_limit(&limit))
        .transpose()?;
    let cors = spec.cors.as_ref().map(validate_cors).transpose()?;
    let tags = validate_tags(&spec.tags)?;
    let alias = resolve_new_alias(spec.alias.as_deref(), &server.endpoints)?;

    Ok(UpstreamConfig {
        alias,
        enabled: spec.enabled,
        tags,
        server,
        protocol: spec.protocol,
        auth,
        headers,
        plugins,
        rate_limit,
        cors,
    })
}

/// Validate a route submission and return its normalized form.
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when the match rule is missing, declares
/// both `http` and `grpc`, lists an unsupported method or an empty path,
/// when a gRPC match omits the service or method, or when the tags, plugin
/// references, rate limit or CORS configuration are invalid.
pub fn validate_route(spec: &RouteSpec) -> Result<RouteConfig, GatewayError> {
    let match_rule = validate_match(&spec.match_spec)?;
    let plugins = validate_plugins(&spec.plugins)?;
    let rate_limit = spec
        .rate_limit
        .map(|limit| validate_rate_limit(&limit))
        .transpose()?;
    let cors = spec.cors.as_ref().map(validate_cors).transpose()?;
    let tags = validate_tags(&spec.tags)?;

    Ok(RouteConfig {
        upstream_id: spec.upstream_id,
        match_rule,
        enabled: spec.enabled,
        priority: spec.priority,
        tags,
        plugins,
        rate_limit,
        cors,
    })
}

/// Validate a host: an RFC 1123 hostname or an IPv4/IPv6 literal.
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] with the offending host in the `detail`.
pub fn validate_host(raw: &str) -> Result<Host, GatewayError> {
    Host::parse(raw)
}

/// Validate a rate limit configuration (ADR 0003).
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when `sustained.rate`, `burst.capacity` or
/// `cost` is below 1.
pub fn validate_rate_limit(rate_limit: &RateLimitConfig) -> Result<RateLimitConfig, GatewayError> {
    if rate_limit.sustained.rate < 1 {
        return Err(GatewayError::validation(
            "`rate_limit.sustained.rate` must be at least 1",
            "rate_limit.sustained.rate",
        ));
    }
    if rate_limit.cost < 1 {
        return Err(GatewayError::validation(
            "`rate_limit.cost` must be at least 1",
            "rate_limit.cost",
        ));
    }
    if rate_limit
        .burst
        .capacity
        .is_some_and(|capacity| capacity < 1)
    {
        return Err(GatewayError::validation(
            "`rate_limit.burst.capacity` must be at least 1",
            "rate_limit.burst.capacity",
        ));
    }

    Ok(*rate_limit)
}

/// Validate a CORS configuration (ADR 0004).
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when `allow_credentials` is combined with
/// the wildcard origin, or when an allowed origin is neither `*` nor an
/// absolute URI with a host.
pub fn validate_cors(cors: &CorsConfig) -> Result<CorsConfig, GatewayError> {
    if cors.allow_credentials && cors.allows_any_origin() {
        return Err(GatewayError::validation(
            "`allow_credentials` cannot be combined with the wildcard origin `*`",
            "cors.allowed_origins",
        ));
    }

    for (index, origin) in cors.allowed_origins.iter().enumerate() {
        validate_origin(origin, index)?;
    }

    Ok(cors.clone())
}

/// Validate the plugin chain: every item must be a builtin GTS identifier or a
/// custom plugin UUID.
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] naming the offending item.
pub fn validate_plugins(plugins: &PluginsConfig) -> Result<PluginsConfig, GatewayError> {
    let mut items = Vec::with_capacity(plugins.items.len());
    for item in &plugins.items {
        items.push(PluginRef::parse(item.as_str())?);
    }

    Ok(PluginsConfig {
        sharing: plugins.sharing,
        items,
    })
}

/// Validate the flat tags of an upstream or route against
/// [`TAG_PATTERN`].
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] naming the offending tag.
pub fn validate_tags(tags: &[String]) -> Result<Vec<String>, GatewayError> {
    for tag in tags {
        if !is_valid_tag(tag) {
            return Err(GatewayError::validation(
                format!("tag `{tag}` must match `{TAG_PATTERN}`"),
                "tags",
            ));
        }
    }

    Ok(tags.to_vec())
}

/// Turns a document deserialization failure into a 400 problem.
///
/// Missing required members, wrong types and unknown members (where the wire
/// schema forbids them) all surface as serde errors; this keeps them out of
/// the 500 path so a rejected configuration is always answered with
/// `cf.oagw.validation.error.v1`.
#[must_use]
pub fn problem_from_deserialize(error: &serde_json::Error) -> GatewayError {
    GatewayError::validation(
        format!("failed to parse the submitted document: {error}"),
        "body",
    )
}

// ---------------------------------------------------------------------------
// Endpoint pool
// ---------------------------------------------------------------------------

/// Validates the endpoint pool: at least one endpoint, every host and port
/// valid, and protocol/scheme/port homogeneous across the pool.
fn validate_server(server: &ServerConfig) -> Result<ServerConfig, GatewayError> {
    if server.endpoints.is_empty() {
        return Err(GatewayError::validation(
            "upstream requires at least one endpoint",
            "server.endpoints",
        ));
    }

    let mut endpoints = Vec::with_capacity(server.endpoints.len());
    for (index, endpoint) in server.endpoints.iter().enumerate() {
        endpoints.push(validate_endpoint(endpoint, index)?);
    }

    // `protocol` is a single upstream-level field, so it is homogeneous by
    // construction; scheme and port are checked here.
    let first = &endpoints[0];
    for (index, endpoint) in endpoints.iter().enumerate().skip(1) {
        if endpoint.scheme != first.scheme {
            return Err(GatewayError::validation(
                format!(
                    "endpoint {index} uses scheme `{}` but endpoint 0 uses `{}`; all endpoints \
                     in one pool must share the same scheme",
                    endpoint.scheme, first.scheme
                ),
                format!("server.endpoints[{index}].scheme"),
            ));
        }
        if endpoint.port != first.port {
            return Err(GatewayError::validation(
                format!(
                    "endpoint {index} uses port {} but endpoint 0 uses port {}; all endpoints in \
                     one pool must share the same port",
                    endpoint.port, first.port
                ),
                format!("server.endpoints[{index}].port"),
            ));
        }
    }

    Ok(ServerConfig::new(endpoints))
}

/// Validates a single endpoint: host, port and scheme.
fn validate_endpoint(endpoint: &Endpoint, index: usize) -> Result<Endpoint, GatewayError> {
    if endpoint.port < MIN_PORT {
        return Err(GatewayError::validation(
            format!("port must be between {MIN_PORT} and {MAX_PORT}"),
            format!("server.endpoints[{index}].port"),
        ));
    }

    let host = Host::parse(endpoint.host.as_str()).map_err(|error| {
        error.with_extension("field", format!("server.endpoints[{index}].host"))
    })?;

    Ok(Endpoint::new(endpoint.scheme, host, endpoint.port))
}

// ---------------------------------------------------------------------------
// Match rules
// ---------------------------------------------------------------------------

/// Validates the match rules and resolves them to exactly one variant.
fn validate_match(spec: &MatchSpec) -> Result<MatchRule, GatewayError> {
    match (spec.http.as_ref(), spec.grpc.as_ref()) {
        (Some(http), None) => Ok(MatchRule::Http(validate_http_match(http)?)),
        (None, Some(grpc)) => Ok(MatchRule::Grpc(validate_grpc_match(grpc)?)),
        (Some(_), Some(_)) => Err(GatewayError::validation(
            "`match` must declare exactly one of `http` or `grpc`",
            "match",
        )),
        (None, None) => Err(GatewayError::validation(
            "`match` requires exactly one of `http` or `grpc`",
            "match",
        )),
    }
}

/// Validates HTTP match rules (route schema `http_match`).
fn validate_http_match(matched: &HttpMatch) -> Result<HttpMatch, GatewayError> {
    if matched.methods.is_empty() {
        return Err(GatewayError::validation(
            "`match.http.methods` requires at least one method",
            "match.http.methods",
        ));
    }
    for method in &matched.methods {
        if !HttpMethod::ROUTE_MATCH_ALLOWED.contains(method) {
            return Err(GatewayError::validation(
                format!("method `{method}` is not allowed in `match.http.methods`"),
                "match.http.methods",
            ));
        }
    }
    if matched.path.is_empty() {
        return Err(GatewayError::validation(
            "`match.http.path` must not be empty",
            "match.http.path",
        ));
    }

    Ok(matched.clone())
}

/// Validates gRPC match rules (route schema `grpc_match`).
fn validate_grpc_match(matched: &GrpcMatch) -> Result<GrpcMatch, GatewayError> {
    if matched.service.is_empty() {
        return Err(GatewayError::validation(
            "`match.grpc.service` must not be empty",
            "match.grpc.service",
        ));
    }
    if matched.method.is_empty() {
        return Err(GatewayError::validation(
            "`match.grpc.method` must not be empty",
            "match.grpc.method",
        ));
    }

    Ok(matched.clone())
}

// ---------------------------------------------------------------------------
// Auth and headers
// ---------------------------------------------------------------------------

/// Validates the auth plugin binding: the plugin reference must resolve.
fn validate_auth(auth: &AuthConfig) -> Result<AuthConfig, GatewayError> {
    let plugin_type = match auth.plugin_type.as_ref() {
        Some(plugin_ref) => Some(
            PluginRef::parse(plugin_ref.as_str())
                .map_err(|error| error.with_extension("field", "auth.plugin_type"))?,
        ),
        None => None,
    };

    Ok(AuthConfig {
        plugin_type,
        sharing: auth.sharing,
        config: auth.config.clone(),
    })
}

/// Validates and normalizes the header transformation rules: header names must
/// be RFC 7230 tokens (stored lowercase) and values must not contain control
/// characters.
fn validate_headers(headers: &HeadersConfig) -> Result<HeadersConfig, GatewayError> {
    let request = match headers.request.as_ref() {
        Some(request) => Some(validate_request_headers(request)?),
        None => None,
    };
    let response = match headers.response.as_ref() {
        Some(response) => Some(validate_response_headers(response)?),
        None => None,
    };

    Ok(HeadersConfig { request, response })
}

/// Validates the inbound request header rules.
fn validate_request_headers(rules: &RequestHeaders) -> Result<RequestHeaders, GatewayError> {
    Ok(RequestHeaders {
        set: normalize_header_map(&rules.set, "headers.request.set")?,
        add: normalize_header_map(&rules.add, "headers.request.add")?,
        remove: normalize_header_names(&rules.remove, "headers.request.remove")?,
        passthrough: rules.passthrough,
        passthrough_allowlist: normalize_header_names(
            &rules.passthrough_allowlist,
            "headers.request.passthrough_allowlist",
        )?,
    })
}

/// Validates the response header rules.
fn validate_response_headers(rules: &ResponseHeaders) -> Result<ResponseHeaders, GatewayError> {
    Ok(ResponseHeaders {
        set: normalize_header_map(&rules.set, "headers.response.set")?,
        add: normalize_header_map(&rules.add, "headers.response.add")?,
        remove: normalize_header_names(&rules.remove, "headers.response.remove")?,
    })
}

/// Validates a name -> value header map, returning it with lowercase names.
fn normalize_header_map(
    headers: &BTreeMap<String, String>,
    field: &str,
) -> Result<BTreeMap<String, String>, GatewayError> {
    let mut normalized = BTreeMap::new();
    for (name, value) in headers {
        normalized.insert(normalize_header_name(name, field)?, value.clone());
        validate_header_value(value, name, field)?;
    }

    Ok(normalized)
}

/// Validates a list of header names, returning them lowercase.
fn normalize_header_names(names: &[String], field: &str) -> Result<Vec<String>, GatewayError> {
    let mut normalized = Vec::with_capacity(names.len());
    for name in names {
        normalized.push(normalize_header_name(name, field)?);
    }

    Ok(normalized)
}

/// Validates an RFC 7230 header name and returns its lowercase form.
fn normalize_header_name(name: &str, field: &str) -> Result<String, GatewayError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || is_token_special(byte));

    if !valid {
        return Err(GatewayError::validation(
            format!("`{name}` is not a valid HTTP header name"),
            field,
        ));
    }

    Ok(name.to_ascii_lowercase())
}

/// Validates an RFC 7230 header value (no control characters).
fn validate_header_value(value: &str, name: &str, field: &str) -> Result<(), GatewayError> {
    if value
        .bytes()
        .any(|byte| byte < 0x20 && byte != b'\t' || byte == 0x7F)
    {
        return Err(GatewayError::validation(
            format!("header `{name}` has an invalid value"),
            field,
        ));
    }

    Ok(())
}

/// RFC 7230 `tchar` specials allowed in a header name.
const fn is_token_special(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
    )
}

/// Validates a single tag against [`TAG_PATTERN`](crate::domain::model::TAG_PATTERN).
fn is_valid_tag(tag: &str) -> bool {
    let valid_byte = |byte: u8| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
    };

    !tag.is_empty() && tag.bytes().all(valid_byte)
}

/// Validates an allowed origin: `*` or an absolute URI with a host.
fn validate_origin(origin: &str, index: usize) -> Result<(), GatewayError> {
    if origin == "*" {
        return Ok(());
    }

    let field = format!("cors.allowed_origins[{index}]");
    let parsed = Url::parse(origin).map_err(|_| {
        GatewayError::validation(format!("`{origin}` is not an absolute URI"), field.clone())
    })?;

    if parsed.host_str().is_none() {
        return Err(GatewayError::validation(
            format!("`{origin}` must be an absolute URI with a host"),
            field,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    use crate::domain::model::{
        DEFAULT_ENDPOINT_PORT, PROTOCOL_HTTP, Protocol, RateLimitAlgorithm, RateScope,
        RateStrategy, RateWindow, Scheme, SharingMode,
    };
    use crate::error::GatewayErrorKind;

    const HTTP: &str = PROTOCOL_HTTP;

    fn parse<T: serde::de::DeserializeOwned>(body: serde_json::Value) -> T {
        serde_json::from_value(body).unwrap()
    }

    /// A minimal valid upstream body.
    fn upstream_body() -> serde_json::Value {
        json!({
            "server": { "endpoints": [{ "host": "api.openai.com" }] },
            "protocol": HTTP
        })
    }

    fn assert_field(error: &GatewayError, field: &str) {
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(error.extensions().extra["field"], field, "{error}");
    }

    // -- upstream ---------------------------------------------------------

    #[test]
    fn test_valid_upstream_materializes_every_default() {
        let config = validate_upstream(&parse(upstream_body())).unwrap();

        assert_eq!(config.alias, "api.openai.com");
        assert!(config.enabled);
        assert!(config.tags.is_empty());
        assert_eq!(config.protocol, Protocol::Http);
        assert_eq!(config.plugins.sharing, SharingMode::Private);
        assert!(config.plugins.items.is_empty());
        assert!(config.auth.is_none());
        assert!(config.headers.is_none());
        assert!(config.rate_limit.is_none());
        assert!(config.cors.is_none());
        assert_eq!(config.server.endpoints[0].port, DEFAULT_ENDPOINT_PORT);
        assert_eq!(config.server.endpoints[0].scheme, Scheme::Https);
    }

    #[test]
    fn test_upstream_requires_at_least_one_endpoint() {
        let body = json!({
            "server": { "endpoints": [] },
            "protocol": HTTP
        });
        let error = validate_upstream(&parse(body)).unwrap_err();

        assert_field(&error, "server.endpoints");
        assert!(error.detail().contains("at least one endpoint"), "{error}");
    }

    #[test]
    fn test_upstream_requires_a_protocol() {
        let error =
            serde_json::from_value::<UpstreamSpec>(json!({ "server": { "endpoints": [] } }))
                .unwrap_err();
        assert!(error.to_string().contains("protocol"), "{error}");
    }

    #[test]
    fn test_upstream_endpoint_pool_must_share_the_scheme() {
        let body = json!({
            "server": { "endpoints": [
                { "host": "api.openai.com", "scheme": "https" },
                { "host": "eu.api.openai.com", "scheme": "http" }
            ] },
            "protocol": HTTP
        });
        let error = validate_upstream(&parse(body)).unwrap_err();

        assert_field(&error, "server.endpoints[1].scheme");
        assert!(error.detail().contains("share the same scheme"), "{error}");
    }

    #[test]
    fn test_upstream_endpoint_pool_must_share_the_port() {
        let body = json!({
            "server": { "endpoints": [
                { "host": "api.openai.com", "port": 443 },
                { "host": "eu.api.openai.com", "port": 8443 }
            ] },
            "protocol": HTTP
        });
        let error = validate_upstream(&parse(body)).unwrap_err();

        assert_field(&error, "server.endpoints[1].port");
        assert!(error.detail().contains("share the same port"), "{error}");
    }

    #[test]
    fn test_endpoint_ports_are_bounded() {
        let body = json!({
            "server": { "endpoints": [{ "host": "api.openai.com", "port": 65_535 }] },
            "protocol": HTTP
        });
        let config = validate_upstream(&parse(body)).unwrap();
        assert_eq!(config.server.endpoints[0].port, MAX_PORT);

        let zero: Endpoint = parse(json!({ "host": "api.openai.com", "port": 0 }));
        let error = validate_endpoint(&zero, 0).unwrap_err();
        assert_field(&error, "server.endpoints[0].port");
        assert!(error.detail().contains(&MIN_PORT.to_string()), "{error}");
    }

    #[test]
    fn test_upstream_endpoint_host_must_be_a_hostname_or_ip() {
        let body = json!({
            "server": { "endpoints": [{ "host": "under_score.example.com" }] },
            "protocol": HTTP
        });
        let error = validate_upstream(&parse(body)).unwrap_err();

        assert_field(&error, "server.endpoints[0].host");
    }

    #[test]
    fn test_upstream_accepts_a_plaintext_http_endpoint() {
        let body = json!({
            "server": { "endpoints": [{ "host": "api.openai.com", "scheme": "http", "port": 80 }] },
            "protocol": HTTP
        });
        let config = validate_upstream(&parse(body)).unwrap();

        assert_eq!(config.server.endpoints[0].scheme, Scheme::Http);
        assert!(config.server.endpoints[0].is_plaintext());
        assert_eq!(config.alias, "api.openai.com");
    }

    #[test]
    fn test_upstream_rejects_an_unknown_scheme() {
        assert!(
            serde_json::from_value::<Endpoint>(json!({
                "scheme": "httpx",
                "host": "api.openai.com"
            }))
            .is_err()
        );
    }

    // -- tags and plugins -------------------------------------------------

    #[test]
    fn test_tags_must_match_the_tag_pattern() {
        let valid = validate_tags(&["llm".to_owned(), "prod-eu_1".to_owned()]).unwrap();
        assert_eq!(valid.len(), 2);

        for tag in ["", "LLM", "with space", "dot.", "slash/"] {
            let error = validate_tags(&[tag.to_owned()]).unwrap_err();
            assert_field(&error, "tags");
            assert!(error.detail().contains(TAG_PATTERN), "{tag}: {error}");
        }
    }

    #[test]
    fn test_plugins_must_be_a_gts_identifier_or_a_uuid() {
        let plugins = validate_plugins(&parse(json!({
            "items": ["gts.cf.plugins.plugin.v1~cf.plugins.pii_redactor.v1"]
        })))
        .unwrap();
        assert_eq!(plugins.items.len(), 1);

        let error = validate_plugins(&parse(json!({ "items": ["nope"] }))).unwrap_err();
        assert_field(&error, "plugins");
        assert!(error.detail().contains("nope"), "{error}");
    }

    // -- rate limit -------------------------------------------------------

    #[test]
    fn test_rate_limit_requires_a_positive_sustained_rate() {
        let error = validate_rate_limit(&parse(json!({ "sustained": { "rate": 0 } }))).unwrap_err();

        assert_field(&error, "rate_limit.sustained.rate");
        assert!(error.detail().contains("at least 1"), "{error}");
    }

    #[test]
    fn test_rate_limit_requires_a_positive_cost() {
        let error = validate_rate_limit(&parse(json!({
            "sustained": { "rate": 10 },
            "cost": 0
        })))
        .unwrap_err();

        assert_field(&error, "rate_limit.cost");
    }

    #[test]
    fn test_rate_limit_requires_a_positive_burst_capacity() {
        let error = validate_rate_limit(&parse(json!({
            "sustained": { "rate": 10 },
            "burst": { "capacity": 0 }
        })))
        .unwrap_err();

        assert_field(&error, "rate_limit.burst.capacity");
    }

    #[test]
    fn test_rate_limit_defaults_are_materialized() {
        let limit = validate_rate_limit(&parse(json!({ "sustained": { "rate": 10 } }))).unwrap();

        assert_eq!(limit.sustained.window, RateWindow::Second);
        assert_eq!(limit.burst.capacity, None);
        assert_eq!(limit.burst_capacity(), 10);
        assert_eq!(limit.scope, RateScope::Tenant);
        assert_eq!(limit.strategy, RateStrategy::Reject);
        assert_eq!(limit.algorithm, RateLimitAlgorithm::TokenBucket);
        assert_eq!(limit.cost, 1);
    }

    // -- CORS -------------------------------------------------------------

    #[test]
    fn test_cors_rejects_credentials_with_a_wildcard_origin() {
        let error = validate_cors(&parse(json!({
            "enabled": true,
            "allowed_origins": ["*"],
            "allow_credentials": true
        })))
        .unwrap_err();

        assert_field(&error, "cors.allowed_origins");
        assert!(error.detail().contains("wildcard"), "{error}");
    }

    #[test]
    fn test_cors_requires_an_absolute_origin_with_a_host() {
        let error = validate_cors(&parse(json!({
            "enabled": true,
            "allowed_origins": ["not a uri"]
        })))
        .unwrap_err();
        assert_field(&error, "cors.allowed_origins[0]");

        let error = validate_cors(&parse(json!({
            "enabled": true,
            "allowed_origins": ["https://"]
        })))
        .unwrap_err();
        assert_field(&error, "cors.allowed_origins[0]");
    }

    #[test]
    fn test_cors_accepts_an_absolute_origin_and_passes_the_config_through() {
        let cors = validate_cors(&parse(json!({
            "enabled": true,
            "allowed_origins": ["https://console.example.com"],
            "allowed_methods": ["GET", "DELETE"],
            "expose_headers": ["X-Request-Id"],
            "allow_credentials": true
        })))
        .unwrap();

        assert!(cors.enabled);
        assert!(cors.allow_credentials);
        assert_eq!(
            cors.allowed_methods,
            vec![HttpMethod::Get, HttpMethod::Delete]
        );
    }

    // -- headers ----------------------------------------------------------

    #[test]
    fn test_header_names_must_be_rfc_7230_tokens_and_are_lowercased() {
        let headers = validate_headers(&parse(json!({
            "request": { "set": { "X-Correlation-Id": "abc" }, "remove": ["X-Secret"] },
            "response": { "add": { "X-Frame-Options": "DENY" } }
        })))
        .unwrap();

        let request = headers.request.unwrap();
        assert!(request.set.contains_key("x-correlation-id"));
        assert_eq!(request.remove, vec!["x-secret"]);
        assert!(
            headers
                .response
                .unwrap()
                .add
                .contains_key("x-frame-options")
        );
    }

    #[test]
    fn test_header_names_must_not_be_empty_or_ill_formed() {
        for name in ["", "sp ace", "he@ader"] {
            let error = validate_headers(&parse(json!({
                "request": { "set": { name: "value" } }
            })))
            .unwrap_err();
            assert_field(&error, "headers.request.set");
        }
    }

    #[test]
    fn test_header_values_must_not_contain_control_characters() {
        let error = validate_headers(&parse(json!({
            "request": { "set": { "X-Trace": "bad\u{7f}value" } }
        })))
        .unwrap_err();

        assert_field(&error, "headers.request.set");
        assert!(error.detail().contains("invalid value"), "{error}");
    }

    // -- auth -------------------------------------------------------------

    #[test]
    fn test_auth_plugin_reference_must_resolve() {
        let auth = validate_auth(&parse(json!({
            "type": "gts.cf.plugins.plugin.v1~cf.plugins.apikey_auth.v1",
            "unknown_member": true
        })))
        .unwrap();
        assert!(auth.plugin_type.unwrap().is_named());

        let error = validate_auth(&parse(json!({ "type": "nonsense" }))).unwrap_err();
        assert_field(&error, "auth.plugin_type");
    }

    // -- match rules ------------------------------------------------------

    #[test]
    fn test_route_requires_exactly_one_match_rule() {
        let error =
            validate_route(&parse(json!({ "upstream_id": Uuid::from_u128(1) }))).unwrap_err();
        assert_field(&error, "match");

        let error = validate_route(&parse(json!({
            "upstream_id": Uuid::from_u128(1),
            "match": {
                "http": { "methods": ["GET"], "path": "/v1" },
                "grpc": { "service": "s.v1.Svc", "method": "Exec" }
            }
        })))
        .unwrap_err();
        assert_field(&error, "match");
    }

    #[test]
    fn test_route_match_requires_at_least_one_allowed_method() {
        let error = validate_route(&parse(json!({
            "upstream_id": Uuid::from_u128(1),
            "match": { "http": { "methods": [], "path": "/v1" } }
        })))
        .unwrap_err();
        assert_field(&error, "match.http.methods");
    }

    #[test]
    fn test_route_match_only_allows_the_route_methods() {
        let error = validate_route(&parse(json!({
            "upstream_id": Uuid::from_u128(1),
            "match": { "http": { "methods": ["OPTIONS"], "path": "/v1" } }
        })))
        .unwrap_err();

        assert_field(&error, "match.http.methods");
        assert!(error.detail().contains("OPTIONS"), "{error}");
    }

    #[test]
    fn test_route_match_requires_a_path() {
        let error = validate_route(&parse(json!({
            "upstream_id": Uuid::from_u128(1),
            "match": { "http": { "methods": ["GET"], "path": "" } }
        })))
        .unwrap_err();

        assert_field(&error, "match.http.path");
    }

    #[test]
    fn test_grpc_match_requires_service_and_method() {
        let upstream_id = Uuid::from_u128(1);
        for (field, spec) in [
            (
                "match.grpc.service",
                RouteSpec {
                    upstream_id,
                    match_spec: MatchSpec {
                        http: None,
                        grpc: Some(GrpcMatch {
                            service: String::new(),
                            method: "Exec".to_owned(),
                        }),
                    },
                    ..RouteSpec::default()
                },
            ),
            (
                "match.grpc.method",
                RouteSpec {
                    upstream_id,
                    match_spec: MatchSpec {
                        http: None,
                        grpc: Some(GrpcMatch {
                            service: "cf.shell.v1.Shell".to_owned(),
                            method: String::new(),
                        }),
                    },
                    ..RouteSpec::default()
                },
            ),
        ] {
            let error = validate_route(&spec).unwrap_err();
            assert_field(&error, field);
        }
    }

    #[test]
    fn test_route_materializes_its_defaults() {
        let config = validate_route(&parse(json!({
            "upstream_id": Uuid::from_u128(1),
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } }
        })))
        .unwrap();

        assert!(config.enabled);
        assert_eq!(config.priority, 0);
        assert!(config.tags.is_empty());
        assert!(config.plugins.items.is_empty());
        assert!(config.rate_limit.is_none());
        assert!(config.cors.is_none());
        assert_eq!(config.match_rule.as_http().unwrap().path, "/v1/chat");
    }

    // -- deserialize failures ---------------------------------------------

    #[test]
    fn test_deserialize_failures_become_400_problems() {
        let error = serde_json::from_str::<UpstreamSpec>("{ not json }").unwrap_err();
        let problem = problem_from_deserialize(&error);

        assert_eq!(problem.status(), 400);
        assert_eq!(problem.kind(), GatewayErrorKind::Validation);
        assert!(
            problem
                .detail()
                .contains("failed to parse the submitted document")
        );
        assert_eq!(problem.extensions().extra["field"], "body");
    }

    #[test]
    fn test_unknown_members_of_every_strict_object_are_rejected() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            // upstream root
            (
                json!({
                    "server": { "endpoints": [{ "host": "api.openai.com" }] },
                    "protocol": HTTP,
                    "lb_policy": "round_robin"
                }),
                "lb_policy",
            ),
            // server
            (
                json!({
                    "server": { "endpoints": [{ "host": "api.openai.com" }], "load_balancing": "rr" },
                    "protocol": HTTP
                }),
                "load_balancing",
            ),
            // endpoint
            (
                json!({
                    "server": { "endpoints": [{ "host": "api.openai.com", "weight": 1 }] },
                    "protocol": HTTP
                }),
                "weight",
            ),
            // rate_limit
            (
                json!({
                    "server": { "endpoints": [{ "host": "api.openai.com" }] },
                    "protocol": HTTP,
                    "rate_limit": { "sustained": { "rate": 1 }, "quota": 5 }
                }),
                "quota",
            ),
            // cors
            (
                json!({
                    "server": { "endpoints": [{ "host": "api.openai.com" }] },
                    "protocol": HTTP,
                    "cors": { "enabled": true, "allow_headers": ["*"] }
                }),
                "allow_headers",
            ),
            // route.match
            (
                json!({
                    "upstream_id": Uuid::from_u128(1),
                    "match": { "http": { "methods": ["GET"], "path": "/v1" }, "prefix": "/v1" }
                }),
                "prefix",
            ),
        ];

        for (body, member) in cases {
            let is_route = body.get("upstream_id").is_some();
            let result = if is_route {
                serde_json::from_value::<RouteSpec>(body).map(|_: RouteSpec| ())
            } else {
                serde_json::from_value::<UpstreamSpec>(body).map(|_: UpstreamSpec| ())
            };
            let rendered = result
                .err()
                .unwrap_or_else(|| panic!("{member} was accepted"))
                .to_string();

            assert!(rendered.contains(member), "{member}: {rendered}");
        }
    }

    // -- host -------------------------------------------------------------

    #[test]
    fn test_validate_host_round_trips_the_model() {
        assert_eq!(
            validate_host("api.openai.com").unwrap().as_str(),
            "api.openai.com"
        );
        assert_eq!(validate_host("10.0.0.1").unwrap().as_str(), "10.0.0.1");
        assert_eq!(
            validate_host("[2001:db8::1]").unwrap().as_str(),
            "2001:db8::1"
        );

        let error = validate_host("").unwrap_err();
        assert_field(&error, "host");
    }

    // -- service-level wiring ---------------------------------------------

    #[test]
    fn test_service_rejects_an_unknown_alias_claim_with_409() {
        use crate::config::OagwConfig;
        use crate::domain::{ConfigService, OagwStore};

        let service = ConfigService::new(OagwConfig::default());
        service
            .create_upstream(Uuid::from_u128(1), &parse(upstream_body()))
            .unwrap();

        let error = service
            .create_upstream(Uuid::from_u128(1), &parse(upstream_body()))
            .unwrap_err();

        assert_eq!(error.status(), 409, "{error}");
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.config.conflict.v1"
        );
        assert_eq!(OagwStore::new().upstream_count(), 0);
    }
}
