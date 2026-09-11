//! Validation of the domain types, mirroring the JSON Schemas.

use super::error::DomainError;
use super::gts_helpers;
use super::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, Plugin, PluginRef, PluginsConfig,
    RateLimitConfig, Route, RouteMatch, Upstream,
};

/// The alias production of the schemas: `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
pub const ALIAS_PATTERN: &str = "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$";
/// The tag production of the schemas: `^[a-z0-9_-]+$`.
pub const TAG_PATTERN: &str = "^[a-z0-9_-]+$";

/// Methods a route may declare.
pub const ROUTE_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// Default-on helper for the `enabled` flag.
#[must_use]
pub fn default_true() -> bool {
    true
}

/// True when `value` matches the alias production.
#[must_use]
pub fn is_valid_alias(value: &str) -> bool {
    valid_alias_len(value)
}

fn valid_alias_len(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 253 {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    let last = bytes.len() - 1;
    if !bytes[last].is_ascii_lowercase() && !bytes[last].is_ascii_digit() {
        return false;
    }
    bytes.iter().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b':' || *b == b'-'
    })
}

/// True when `value` matches the tag production.
#[must_use]
pub fn is_valid_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// True when `host` is a valid RFC 1123 hostname or an IP literal.
#[must_use]
pub fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    is_rfc1123_hostname(host)
}

fn is_rfc1123_hostname(host: &str) -> bool {
    let stripped = host.strip_suffix('.').unwrap_or(host);
    if stripped.is_empty() || stripped.len() > 253 {
        return false;
    }
    stripped.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// True when `value` looks like a GTS identifier (`gts.…~…`) or a UUID.
#[must_use]
pub fn is_identifier(value: &str) -> bool {
    value.starts_with("gts.") || value.parse::<uuid::Uuid>().is_ok()
}

fn invalid(field: &str, detail: impl std::fmt::Display) -> DomainError {
    DomainError::Validation(format!("{field}: {detail}"))
}

/// Validate an upstream's endpoint list.
///
/// `allow_http` lifts the HTTPS-only default posture (DESIGN `cpt-cf-oagw-constraint-https-only`).
pub fn validate_endpoints(endpoints: &[Endpoint], allow_http: bool) -> Result<(), DomainError> {
    if endpoints.is_empty() {
        return Err(invalid("server.endpoints", "at least one endpoint is required"));
    }
    if endpoints.len() > 64 {
        return Err(invalid("server.endpoints", "at most 64 endpoints are allowed"));
    }
    for (idx, ep) in endpoints.iter().enumerate() {
        let field = format!("server.endpoints[{idx}]");
        if !is_valid_host(&ep.host) {
            return Err(invalid(&format!("{field}.host"), "not a valid hostname or IP address"));
        }
        if let Some(port) = ep.port
            && port == 0 {
                return Err(invalid(&format!("{field}.port"), "must be between 1 and 65535"));
            }
        if ep.scheme.is_plain_http() && !allow_http {
            return Err(invalid(
                &format!("{field}.scheme"),
                "plaintext http upstreams are disabled by policy",
            ));
        }
    }
    // A pool is load-balanced as one unit, so its members have to agree on how to reach the
    // upstream (PRD §Upstream, DESIGN §Multi-Endpoint Load Balancing).
    let first = &endpoints[0];
    for (idx, ep) in endpoints.iter().enumerate().skip(1) {
        let field = format!("server.endpoints[{idx}]");
        if ep.scheme != first.scheme {
            return Err(invalid(&field, format_args!("all endpoints in a pool must share the same scheme ({} vs {})", first.scheme.as_str(), ep.scheme.as_str())));
        }
        if ep.port.unwrap_or(ep.scheme.default_port()) != first.port.unwrap_or(first.scheme.default_port()) {
            return Err(invalid(&field, format_args!("all endpoints in a pool must use the same port ({} vs {})", first.port.unwrap_or(first.scheme.default_port()), ep.port.unwrap_or(ep.scheme.default_port()))));
        }
    }
    Ok(())
}

/// Validate the auth plugin binding.
pub fn validate_auth(auth: &AuthConfig) -> Result<(), DomainError> {
    if !gts_helpers::is_known_protocol(&auth.plugin_type) && !auth_plugin_known(&auth.plugin_type)
    {
        return Err(invalid(
            "auth.type",
            format_args!("unknown auth plugin '{}'", auth.plugin_type),
        ));
    }
    Ok(())
}

fn auth_plugin_known(id: &str) -> bool {
    matches!(
        id,
        gts_helpers::AUTH_NOOP
            | gts_helpers::AUTH_APIKEY
            | gts_helpers::AUTH_OAUTH2_CC
            | gts_helpers::AUTH_OAUTH2_CC_BASIC
    )
}

/// Validate header rules.
pub fn validate_headers(headers: &HeadersConfig) -> Result<(), DomainError> {
    for (name, _) in headers.request.as_ref().map(|r| r.set.iter()).into_iter().flatten() {
        validate_header_name(name)?;
    }
    for (name, _) in headers.request.as_ref().map(|r| r.add.iter()).into_iter().flatten() {
        validate_header_name(name)?;
    }
    for (name, _) in headers.response.as_ref().map(|r| r.set.iter()).into_iter().flatten() {
        validate_header_name(name)?;
    }
    for (name, _) in headers.response.as_ref().map(|r| r.add.iter()).into_iter().flatten() {
        validate_header_name(name)?;
    }
    Ok(())
}

fn validate_header_name(name: &str) -> Result<(), DomainError> {
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        })
    {
        return Err(invalid("headers", format_args!("invalid header name '{name}'")));
    }
    Ok(())
}

/// Validate a rate limit configuration.
pub fn validate_rate_limit(config: &RateLimitConfig) -> Result<(), DomainError> {
    if config.sustained.rate == 0 {
        return Err(invalid("rate_limit.sustained.rate", "must be at least 1"));
    }
    if let Some(burst) = &config.burst
        && burst.capacity == 0 {
            return Err(invalid("rate_limit.burst.capacity", "must be at least 1"));
        }
    if config.cost == 0 {
        return Err(invalid("rate_limit.cost", "must be at least 1"));
    }
    Ok(())
}

/// Validate a CORS configuration.
pub fn validate_cors(config: &CorsConfig) -> Result<(), DomainError> {
    if !config.enabled {
        return Ok(());
    }
    if config.allow_credentials && config.allowed_origins.iter().any(|o| o == "*") {
        return Err(invalid(
            "cors.allowed_origins",
            "'*' is not allowed together with allow_credentials",
        ));
    }
    Ok(())
}

/// Validate a plugin reference list.
pub fn validate_plugin_refs(refs: &[PluginRef], plugins: &[Plugin]) -> Result<(), DomainError> {
    for r in refs {
        let id = match r {
            PluginRef::Bare(s) => s,
            PluginRef::Detailed { plugin_ref, .. } => plugin_ref,
        };
        if !is_identifier(id) {
            return Err(invalid(
                "plugins.items",
                format_args!("'{}' is neither a GTS identifier nor a plugin UUID", id),
            ));
        }
        if let Ok(uuid) = id.parse::<uuid::Uuid>()
            && !plugins.iter().any(|p| p.id == uuid) {
                return Err(invalid("plugins.items", format_args!("plugin '{}' does not exist", id)));
            }
    }
    Ok(())
}

/// Validate the `plugins` block of an upstream or route.
pub fn validate_plugins(plugins: &PluginsConfig, plugins_store: &[Plugin]) -> Result<(), DomainError> {
    validate_plugin_catalogue(plugins)?;
    validate_plugin_refs(&plugins.items, plugins_store)
}

fn validate_plugin_catalogue(plugins: &PluginsConfig) -> Result<(), DomainError> {
    for r in &plugins.items {
        let id = match r {
            PluginRef::Bare(s) => s.as_str(),
            PluginRef::Detailed { plugin_ref, .. } => plugin_ref.as_str(),
        };
        if id.parse::<uuid::Uuid>().is_ok() {
            continue;
        }
        let known = gts_helpers::auth_plugin_catalog().contains(&id)
            || gts_helpers::guard_plugin_catalog().contains(&id)
            || gts_helpers::transform_plugin_catalog().contains(&id);
        if !known {
            return Err(invalid("plugins.items", format_args!("unknown plugin '{}'", id)));
        }
    }
    Ok(())
}

/// Validate a route's match rules.
pub fn validate_route_match(route_match: &RouteMatch) -> Result<(), DomainError> {
    match route_match {
        RouteMatch::Http(http) => {
            if http.methods.is_empty() {
                return Err(invalid("match.http.methods", "at least one method is required"));
            }
            for method in &http.methods {
                if !ROUTE_METHODS.contains(&method.as_str()) {
                    return Err(invalid(
                        "match.http.methods",
                        format_args!("'{}' is not a supported method", method),
                    ));
                }
            }
            if http.path.is_empty() {
                return Err(invalid("match.http.path", "must not be empty"));
            }
            if !http.path.starts_with('/') {
                return Err(invalid("match.http.path", "must start with '/'"));
            }
        }
        RouteMatch::Grpc(grpc) => {
            if grpc.service.is_empty() || grpc.method.is_empty() {
                return Err(invalid("match.grpc", "service and method are required"));
            }
        }
    }
    Ok(())
}

/// Validate an incoming upstream payload.
pub fn validate_upstream(upstream: &Upstream, plugins: &[Plugin]) -> Result<(), DomainError> {
    if !is_valid_alias(&upstream.alias) {
        return Err(invalid("alias", format_args!("'{}' does not match {}", upstream.alias, ALIAS_PATTERN_DISPLAY)));
    }
    for tag in &upstream.tags {
        if !is_valid_tag(tag) {
            return Err(invalid("tags", format_args!("'{}' does not match {}", tag, TAG_PATTERN_DISPLAY)));
        }
    }
    if !gts_helpers::is_known_protocol(&upstream.protocol) {
        return Err(invalid("protocol", format_args!("unknown protocol '{}'", upstream.protocol)));
    }
    validate_plugins(upstream.plugins.as_ref().unwrap_or(&PluginsConfig::default()), plugins)?;
    if let Some(auth) = &upstream.auth {
        validate_auth(auth)?;
    }
    if let Some(headers) = &upstream.headers {
        validate_headers(headers)?;
    }
    if let Some(limit) = &upstream.rate_limit {
        validate_rate_limit(limit)?;
    }
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    Ok(())
}

const ALIAS_PATTERN_DISPLAY: &str = "the alias pattern";
const TAG_PATTERN_DISPLAY: &str = "the tag pattern";

/// Validate an incoming route payload.
pub fn validate_route(route: &Route, upstream: Option<&Upstream>, plugins: &[Plugin]) -> Result<(), DomainError> {
    validate_route_match(&route.route_match)?;
    for tag in &route.tags {
        if !is_valid_tag(tag) {
            return Err(invalid("tags", format_args!("'{}' does not match {}", tag, TAG_PATTERN_DISPLAY)));
        }
    }
    let upstream = upstream.ok_or_else(|| {
        invalid("upstream_id", format_args!("upstream '{}' does not exist", route.upstream_id))
    })?;
    if route.tenant_id != upstream.tenant_id {
        return Err(invalid("upstream_id", "route and upstream must belong to the same tenant"));
    }
    if let Some(http) = route.route_match.as_http() {
        if upstream.protocol != gts_helpers::PROTOCOL_HTTP {
            return Err(invalid(
                "match.http",
                format_args!("upstream protocol '{}' does not carry HTTP routes", upstream.protocol),
            ));
        }
        if !http.path.starts_with('/') {
            return Err(invalid("match.http.path", "must start with '/'"));
        }
    } else if upstream.protocol != gts_helpers::PROTOCOL_GRPC {
        return Err(invalid(
            "match.grpc",
            format_args!("upstream protocol '{}' does not carry gRPC", upstream.protocol),
        ));
    }
    validate_plugins(route.plugins.as_ref().unwrap_or(&PluginsConfig::default()), plugins)?;
    if let Some(limit) = &route.rate_limit {
        validate_rate_limit(limit)?;
    }
    if let Some(cors) = &route.cors {
        validate_cors(cors)?;
    }
    Ok(())
}

/// Validate a stored plugin definition.
pub fn validate_plugin(plugin: &Plugin) -> Result<(), DomainError> {
    if plugin.name.trim().is_empty() {
        return Err(invalid("name", "must not be blank"));
    }
    if plugin.name.len() > 128 {
        return Err(invalid("name", "must be at most 128 characters"));
    }
    if let Some(t) = &plugin.plugin_type
        && !matches!(t.as_str(), "auth" | "guard" | "transform") {
            return Err(invalid(
                "type",
                format_args!("'{}' is not a plugin base type (auth, guard or transform)", t),
            ));
        }
    for phase in &plugin.phases {
        if !matches!(
            phase.as_str(),
            "request" | "response" | "auth" | "guard" | "transform" | "error"
        ) {
            return Err(invalid("phases", format_args!("unknown phase '{}'", phase)));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
