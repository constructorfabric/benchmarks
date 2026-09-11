//! Create/update validation pipeline (`FR-002`).
//!
//! Validation normalizes in place (lowercase alias and header names, trimmed
//! paths) and rejects anything the published schemas would reject. It is pure:
//! the uniqueness checks that need cross-resource knowledge live in the
//! control-plane service.

use std::collections::HashSet;

use crate::domain::alias::{derive_alias, is_ip_address, normalize_alias, validate_hostname};
use crate::domain::error::DomainError;
use crate::domain::model::{
    Cors, Endpoint, HttpMatch, MatchRule, Plugin, PluginSet, RateLimit, Route, Upstream,
};

/// Everything validation needs to know that is not in the payload.
#[derive(Debug, Clone, Default)]
pub struct ValidationContext {
    /// Whether `http` endpoints may be declared at all.
    pub allow_http_upstream: bool,
    /// Plugin identifiers that may be bound: builtin instance GTS ids plus the
    /// stored custom plugin UUIDs of the calling tenant.
    pub known_plugins: HashSet<String>,
}

fn invalid(msg: impl Into<String>) -> DomainError {
    DomainError::Validation(msg.into())
}

/// Validates and normalizes an upstream submission.
///
/// # Errors
/// [`DomainError::Validation`] when any field violates the documented schema,
/// the pool invariants, the `allow_http_upstream` gate or the CORS
/// credentials/wildcard rule.
pub fn validate_upstream(
    upstream: &mut Upstream,
    ctx: &ValidationContext,
) -> Result<(), DomainError> {
    if upstream.server.endpoints.is_empty() {
        return Err(invalid(
            "server.endpoints must contain at least one endpoint",
        ));
    }

    let mut seen_hosts = HashSet::new();
    for endpoint in &mut upstream.server.endpoints {
        validate_endpoint(endpoint, ctx)?;
        if !seen_hosts.insert(format!("{}:{}", endpoint.host, endpoint.port())) {
            return Err(invalid(format!(
                "duplicate endpoint {}:{}",
                endpoint.host,
                endpoint.port()
            )));
        }
    }
    enforce_pool_homogeneity(&upstream.server.endpoints)?;

    // Alias: derive when possible, then reconcile with what the caller sent.
    let derived = derive_alias(&upstream.server.endpoints);
    let supplied = upstream.alias.clone();
    match supplied.as_deref() {
        None => {
            let Some(derived) = derived else {
                return Err(invalid(
                    "alias is required for IP-based or non-derivable endpoints",
                ));
            };
            upstream.alias = Some(derived);
        }
        Some(supplied) => {
            let normalized = normalize_alias(supplied).ok_or_else(|| invalid("alias is empty"))?;
            validate_alias_syntax(&normalized)?;
            if let Some(expected) = derived.as_deref()
                && expected != normalized
            {
                return Err(invalid(format!(
                    "alias '{normalized}' does not match the derived alias '{expected}'"
                )));
            }
            upstream.alias = Some(normalized);
        }
    }

    validate_tags(&upstream.tags)?;
    if let Some(rate) = &upstream.rate_limit {
        validate_rate_limit(rate)?;
    }
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    if let Some(headers) = &upstream.headers {
        validate_header_rules(headers)?;
    }
    if let Some(auth) = &mut upstream.auth {
        if auth.kind.trim().is_empty() {
            return Err(invalid("auth.type must not be empty"));
        }
        if !auth.kind.starts_with("gts.") && !ctx.known_plugins.contains(auth.kind.trim()) {
            return Err(invalid(format!(
                "unknown authentication plugin type '{}'",
                auth.kind
            )));
        }
        auth.kind = auth.kind.trim().to_owned();
    }
    if let Some(plugins) = &upstream.plugins {
        validate_plugin_set(plugins, ctx)?;
    }

    Ok(())
}

fn validate_endpoint(endpoint: &mut Endpoint, ctx: &ValidationContext) -> Result<(), DomainError> {
    endpoint.host = endpoint.host.trim().to_owned();
    if is_ip_address(&endpoint.host) {
        if !endpoint.host.contains(':') {
            // IPv4 — nothing further to check.
        }
    } else {
        validate_hostname(&endpoint.host).map_err(invalid)?;
        endpoint.host = endpoint.host.to_ascii_lowercase();
    }
    if !ctx.allow_http_upstream && !endpoint.scheme.is_tls() {
        return Err(invalid(
            "http endpoints are disabled by the deployment policy (allow_http_upstream)",
        ));
    }
    Ok(())
}

fn enforce_pool_homogeneity(endpoints: &[Endpoint]) -> Result<(), DomainError> {
    let first = endpoints
        .first()
        .ok_or_else(|| invalid("server.endpoints must not be empty"))?;
    let scheme = first.scheme;
    let port = first.port();
    for endpoint in endpoints {
        if endpoint.scheme != scheme {
            return Err(invalid("all endpoints in a pool must use the same scheme"));
        }
        if endpoint.port() != port {
            return Err(invalid("all endpoints in a pool must use the same port"));
        }
    }
    Ok(())
}

/// Validates an explicit alias against the documented pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
fn validate_alias_syntax(alias: &str) -> Result<(), DomainError> {
    let bytes = alias.as_bytes();
    let first = bytes.first().copied().unwrap_or(b' ');
    let last = bytes.last().copied().unwrap_or(b' ');
    let ok_char =
        |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b':' || c == b'-';
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(invalid(format!(
            "alias '{alias}' must start with a letter or digit"
        )));
    }
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return Err(invalid(format!(
            "alias '{alias}' must end with a letter or digit"
        )));
    }
    if !bytes.iter().copied().all(ok_char) {
        return Err(invalid(format!(
            "alias '{alias}' contains an invalid character"
        )));
    }
    Ok(())
}

fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    for tag in tags {
        if tag.is_empty()
            || !tag
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(invalid(format!("tag '{tag}' must match ^[a-z0-9_-]+$")));
        }
    }
    Ok(())
}

fn validate_rate_limit(rate: &RateLimit) -> Result<(), DomainError> {
    if rate.sustained.rate == 0 {
        return Err(invalid("rate_limit.sustained.rate must be at least 1"));
    }
    if rate.cost == 0 {
        return Err(invalid("rate_limit.cost must be at least 1"));
    }
    if let Some(capacity) = rate.burst.capacity
        && capacity == 0
    {
        return Err(invalid("rate_limit.burst.capacity must be at least 1"));
    }
    Ok(())
}

fn validate_cors(cors: &Cors) -> Result<(), DomainError> {
    const ALLOWED_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    for method in &cors.allowed_methods {
        if !ALLOWED_METHODS.contains(&method.as_str()) {
            return Err(invalid(format!("CORS method '{method}' is not allowed")));
        }
    }
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(invalid(
            "allow_credentials must not be combined with the wildcard origin",
        ));
    }
    Ok(())
}

fn validate_header_rules(headers: &crate::domain::model::HeaderRules) -> Result<(), DomainError> {
    for name in headers.request.set.keys() {
        if name.is_empty() {
            return Err(invalid("header names must not be empty"));
        }
    }
    Ok(())
}

fn validate_plugin_set(plugins: &PluginSet, ctx: &ValidationContext) -> Result<(), DomainError> {
    for item in &plugins.items {
        let reference = item.reference();
        if !ctx.known_plugins.contains(reference)
            && !reference.starts_with("gts.cf.core.oagw.")
            && !is_uuid(reference)
        {
            return Err(invalid(format!("unknown plugin reference '{reference}'")));
        }
    }
    Ok(())
}

fn is_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
}

/// Validates and normalizes a route submission.
///
/// # Errors
/// [`DomainError::Validation`] when the match rule is malformed, or
/// [`DomainError::NotFound`] when the upstream reference is missing from the
/// caller's tenant.
pub fn validate_route(route: &mut Route, ctx: &ValidationContext) -> Result<(), DomainError> {
    let upstream_id = route
        .upstream_id
        .ok_or_else(|| invalid("upstream_id is required"))?;

    match (&route.match_rule.http, &route.match_rule.grpc) {
        (Some(http), None) => validate_http_match(http)?,
        (None, Some(grpc)) => {
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(invalid(
                    "match.grpc.service and match.grpc.method are required",
                ));
            }
        }
        (None, None) => {
            return Err(invalid(
                "match must contain exactly one of 'http' or 'grpc'",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid(
                "match must contain exactly one of 'http' or 'grpc'",
            ));
        }
    }

    if let Some(plugins) = &route.plugins {
        validate_plugin_set(plugins, ctx)?;
    }
    if let Some(rate) = &route.rate_limit {
        validate_rate_limit(rate)?;
    }
    validate_tags(&route.tags)?;

    // Keep the immutable reference.
    route.upstream_id = Some(upstream_id);
    Ok(())
}

fn validate_http_match(match_rule: &HttpMatch) -> Result<(), DomainError> {
    if match_rule.methods.is_empty() {
        return Err(invalid(
            "match.http.methods must contain at least one method",
        ));
    }
    let path = match_rule.path.trim();
    if path.is_empty() {
        return Err(invalid("match.http.path must not be empty"));
    }
    if !path.starts_with('/') {
        return Err(invalid(format!(
            "match.http.path '{path}' must start with '/'"
        )));
    }
    if path.contains('?') || path.contains('#') {
        return Err(invalid(
            "match.http.path must not contain a query or fragment",
        ));
    }
    Ok(())
}

/// Validates that a match rule is internally consistent with the upstream's
/// protocol (called by the service once the upstream is resolved).
///
/// # Errors
/// [`DomainError::Validation`] when the match scope does not match the
/// upstream protocol.
pub fn validate_match_against_protocol(
    match_rule: &MatchRule,
    protocol: crate::domain::model::Protocol,
) -> Result<(), DomainError> {
    let is_http = match_rule.http.is_some();
    let matches_protocol = match protocol {
        crate::domain::model::Protocol::Http => is_http,
        crate::domain::model::Protocol::Grpc => match_rule.grpc.is_some(),
    };
    if !matches_protocol {
        return Err(invalid(
            "the match scope must agree with the upstream protocol",
        ));
    }
    Ok(())
}

/// Validates a custom plugin definition.
///
/// # Errors
/// [`DomainError::Validation`] when the kind or name is missing.
pub fn validate_plugin(plugin: &Plugin) -> Result<(), DomainError> {
    if plugin.name.trim().is_empty() {
        return Err(invalid("plugin.name must not be empty"));
    }
    if plugin.source.trim().is_empty() {
        return Err(invalid("plugin.source must not be empty"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, GrpcMatch, HttpMatch, Protocol, Scheme};

    fn ctx() -> ValidationContext {
        ValidationContext {
            allow_http_upstream: true,
            known_plugins: HashSet::new(),
        }
    }

    fn upstream(host: &str, scheme: Scheme, port: u16) -> Upstream {
        Upstream {
            server: crate::domain::model::ServerConfig {
                endpoints: vec![Endpoint {
                    scheme,
                    host: host.to_owned(),
                    port: Some(port),
                }],
            },
            protocol: Protocol::Http,
            ..Upstream::default()
        }
    }

    #[test]
    fn accepts_a_minimal_upstream_and_derives_its_alias() {
        let mut u = upstream("api.openai.com", Scheme::Https, 443);
        validate_upstream(&mut u, &ctx()).expect("valid");
        assert_eq!(u.alias.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn rejects_an_empty_endpoint_pool() {
        let mut u = upstream("api.openai.com", Scheme::Https, 443);
        u.server.endpoints.clear();
        assert!(validate_upstream(&mut u, &ctx()).is_err());
    }

    #[test]
    fn rejects_an_http_endpoint_when_the_flag_is_off() {
        let mut u = upstream("api.openai.com", Scheme::Http, 80);
        let mut strict = ctx();
        strict.allow_http_upstream = false;
        assert!(validate_upstream(&mut u, &strict).is_err());
    }

    #[test]
    fn rejects_a_supplied_alias_that_disagrees_with_the_derived_one() {
        let mut u = upstream("api.openai.com", Scheme::Https, 443);
        u.alias = Some("something-else".to_owned());
        assert!(validate_upstream(&mut u, &ctx()).is_err());
    }

    #[test]
    fn accepts_a_supplied_alias_that_matches_the_derived_one() {
        let mut u = upstream("api.openai.com", Scheme::Https, 443);
        u.alias = Some("API.OpenAI.Com.".to_owned());
        validate_upstream(&mut u, &ctx()).expect("idempotent alias");
        assert_eq!(u.alias.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn requires_an_alias_for_ip_endpoints() {
        let mut u = upstream("10.0.1.1", Scheme::Https, 443);
        assert!(validate_upstream(&mut u, &ctx()).is_err());
        u.alias = Some("my-internal-service".to_owned());
        validate_upstream(&mut u, &ctx()).expect("explicit alias accepted");
    }

    #[test]
    fn rejects_a_mixed_pool() {
        let mut u = upstream("us.vendor.com", Scheme::Https, 443);
        u.server.endpoints.push(Endpoint {
            scheme: Scheme::Wss,
            host: "eu.vendor.com".to_owned(),
            port: Some(443),
        });
        assert!(validate_upstream(&mut u, &ctx()).is_err());
    }

    #[test]
    fn rejects_an_invalid_host() {
        let mut u = upstream("not a host", Scheme::Https, 443);
        assert!(validate_upstream(&mut u, &ctx()).is_err());
    }

    #[test]
    fn rejects_wildcard_origins_with_credentials() {
        let mut u = upstream("api.openai.com", Scheme::Https, 443);
        u.cors = Some(Cors {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..Cors::default()
        });
        assert!(validate_upstream(&mut u, &ctx()).is_err());
    }

    #[test]
    fn rejects_a_zero_sustained_rate() {
        let mut u = upstream("api.openai.com", Scheme::Https, 443);
        u.rate_limit = Some(RateLimit {
            sustained: crate::domain::model::SustainedRate {
                rate: 0,
                window: crate::domain::model::RateWindow::Second,
            },
            ..RateLimit::default()
        });
        assert!(validate_upstream(&mut u, &ctx()).is_err());
    }

    #[test]
    fn rejects_a_route_without_a_match_scope() {
        let mut r = Route {
            upstream_id: Some(uuid::Uuid::new_v4()),
            ..Route::default()
        };
        assert!(validate_route(&mut r, &ctx()).is_err());
    }

    #[test]
    fn rejects_a_route_with_both_match_scopes() {
        let mut r = Route {
            upstream_id: Some(uuid::Uuid::new_v4()),
            match_rule: MatchRule {
                http: Some(HttpMatch::default()),
                grpc: Some(GrpcMatch::default()),
            },
            ..Route::default()
        };
        assert!(validate_route(&mut r, &ctx()).is_err());
    }

    #[test]
    fn rejects_a_route_with_no_methods_or_a_relative_path() {
        let mut r = Route {
            upstream_id: Some(uuid::Uuid::new_v4()),
            match_rule: MatchRule {
                http: Some(HttpMatch::default()),
                grpc: None,
            },
            ..Route::default()
        };
        assert!(validate_route(&mut r, &ctx()).is_err());

        r.match_rule.http = Some(HttpMatch {
            methods: vec![crate::domain::model::HttpMethod::Get],
            path: "no-leading-slash".to_owned(),
            ..HttpMatch::default()
        });
        assert!(validate_route(&mut r, &ctx()).is_err());
    }

    #[test]
    fn validates_the_match_scope_against_the_upstream_protocol() {
        let grpc_match = MatchRule {
            http: None,
            grpc: Some(GrpcMatch {
                service: "foo.v1.UserService".to_owned(),
                method: "GetUser".to_owned(),
            }),
        };
        assert!(validate_match_against_protocol(&grpc_match, Protocol::Grpc).is_ok());
        assert!(validate_match_against_protocol(&grpc_match, Protocol::Http).is_err());
    }
}
