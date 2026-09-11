// @cpt-begin:cpt-cf-oagw-dod-resource-model-schema-validation:p1:inst-validate
//! Schema-level validation for upstream and route documents.
//!
//! Enforces the required fields, enums, patterns and ranges the two supplied
//! JSON Schemas declare. Enum membership is already enforced by serde; this
//! module covers the constraints serde cannot express.

use super::alias::validate_hostname;
use super::error::{DomainError, DomainResult};
use super::model::{
    CorsConfig, HttpMatch, MatchConfig, PROTOCOL_GRPC, PROTOCOL_HTTP, PluginsConfig,
    RateLimitConfig, Route, ServerConfig, Upstream,
};
use uuid::Uuid;

/// Methods the route schema permits.
const ALLOWED_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// Methods a cross-origin policy may permit.
const CORS_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Validate a tag against the schema's pattern.
fn validate_tag(tag: &str) -> DomainResult<()> {
    let ok = !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(DomainError::validation(format!(
            "tag `{tag}` must match ^[a-z0-9_-]+$"
        )))
    }
}

/// Validate the endpoint pool of an upstream.
///
/// # Errors
/// Returns a validation error when the pool is empty, a hostname is malformed,
/// a port is out of range, or the pool mixes scheme or port.
pub fn validate_server(server: &ServerConfig) -> DomainResult<()> {
    if server.endpoints.is_empty() {
        return Err(DomainError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    }
    for endpoint in &server.endpoints {
        validate_hostname(&endpoint.host)?;
        if endpoint.port == 0 {
            return Err(DomainError::validation(
                "endpoint port must be between 1 and 65535",
            ));
        }
    }
    // Endpoints in a pool must agree on scheme and port.
    let first = &server.endpoints[0];
    for endpoint in &server.endpoints[1..] {
        if endpoint.scheme != first.scheme {
            return Err(DomainError::validation(
                "all endpoints in a pool must share the same scheme",
            ));
        }
        if endpoint.port != first.port {
            return Err(DomainError::validation(
                "all endpoints in a pool must share the same port",
            ));
        }
    }
    Ok(())
}

/// Validate a rate-limit block against the schema's enums and ranges.
///
/// # Errors
/// Returns a validation error when a numeric field is below its minimum.
pub fn validate_rate_limit(rate_limit: &RateLimitConfig) -> DomainResult<()> {
    if rate_limit.sustained.rate < 1 {
        return Err(DomainError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if let Some(burst) = &rate_limit.burst
        && burst.capacity < 1
    {
        return Err(DomainError::validation(
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    if rate_limit.cost < 1 {
        return Err(DomainError::validation(
            "rate_limit.cost must be at least 1",
        ));
    }
    Ok(())
}

/// Validate a cross-origin block.
///
/// # Errors
/// Returns a validation error when credentials are combined with a wildcard
/// origin, or when a method is outside the permitted set.
pub fn validate_cors(cors: &CorsConfig) -> DomainResult<()> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::validation(
            "cors.allow_credentials must not be combined with a wildcard origin",
        ));
    }
    for method in &cors.allowed_methods {
        if !CORS_METHODS.contains(&method.as_str()) {
            return Err(DomainError::validation(format!(
                "cors.allowed_methods contains unsupported method `{method}`"
            )));
        }
    }
    Ok(())
}

/// Which shapes a plugin binding entry may take, per the owning document.
///
/// The upstream schema allows a global-type-system identifier or a bare
/// UUID; the route schema allows only a global-type-system identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRefContext {
    /// An upstream's `plugins.items`: a global type system identifier or a
    /// bare UUID are both accepted.
    Upstream,
    /// A route's `plugins.items`: only a global type system identifier is
    /// accepted.
    Route,
}

/// Whether a string is a plausible anonymous global type system identifier.
///
/// This is a shape check, not a registry lookup: it looks for the `gts.`
/// prefix and the `~` instance separator that every global type system
/// identifier carries.
fn looks_like_gts_identifier(item: &str) -> bool {
    item.starts_with("gts.") && item.contains('~')
}

/// Validate one plugin binding entry against the shapes `context` permits.
///
/// # Errors
/// Returns a validation error when the entry is empty, or when it is neither
/// a plausible global type system identifier nor (for
/// [`PluginRefContext::Upstream`]) a bare UUID.
fn validate_plugin_ref(item: &str, context: PluginRefContext) -> DomainResult<()> {
    if item.trim().is_empty() {
        return Err(DomainError::validation(
            "plugins.items entries must not be empty",
        ));
    }
    let is_gts = looks_like_gts_identifier(item);
    let is_bare_uuid = Uuid::parse_str(item).is_ok();
    let accepted = match context {
        PluginRefContext::Upstream => is_gts || is_bare_uuid,
        PluginRefContext::Route => is_gts,
    };
    if accepted {
        Ok(())
    } else {
        match context {
            PluginRefContext::Upstream => Err(DomainError::validation(format!(
                "plugins.items entry `{item}` must be a global type system identifier \
                 (`gts.<...>~<...>`) or a bare UUID"
            ))),
            PluginRefContext::Route => Err(DomainError::validation(format!(
                "plugins.items entry `{item}` must be a global type system identifier \
                 (`gts.<...>~<...>`)"
            ))),
        }
    }
}

/// Validate a plugin binding block.
///
/// # Errors
/// Returns a validation error when an entry is empty or has a shape `context`
/// does not permit; see [`PluginRefContext`].
pub fn validate_plugins(plugins: &PluginsConfig, context: PluginRefContext) -> DomainResult<()> {
    for item in &plugins.items {
        validate_plugin_ref(item, context)?;
    }
    Ok(())
}

/// Validate an upstream document.
///
/// # Errors
/// Returns a validation error when any schema constraint is violated.
pub fn validate_upstream(upstream: &Upstream) -> DomainResult<()> {
    validate_server(&upstream.server)?;
    if upstream.protocol != PROTOCOL_HTTP && upstream.protocol != PROTOCOL_GRPC {
        return Err(DomainError::validation(format!(
            "protocol `{}` must be one of `{PROTOCOL_HTTP}` or `{PROTOCOL_GRPC}`",
            upstream.protocol
        )));
    }
    for tag in &upstream.tags {
        validate_tag(tag)?;
    }
    if let Some(rate_limit) = &upstream.rate_limit {
        validate_rate_limit(rate_limit)?;
    }
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    if let Some(plugins) = &upstream.plugins {
        validate_plugins(plugins, PluginRefContext::Upstream)?;
    }
    Ok(())
}

/// Validate the match rule of a route.
///
/// # Errors
/// Returns a validation error when the rule does not carry exactly one of
/// `http` or `grpc`, or when a member of the chosen rule is malformed.
pub fn validate_match(match_config: &MatchConfig) -> DomainResult<()> {
    match (&match_config.http, &match_config.grpc) {
        (Some(http), None) => validate_http_match(http),
        (None, Some(grpc)) => {
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(DomainError::validation(
                    "match.grpc.service and match.grpc.method must not be empty",
                ));
            }
            Ok(())
        }
        (Some(_), Some(_)) => Err(DomainError::validation(
            "match must carry exactly one of `http` or `grpc`, not both",
        )),
        (None, None) => Err(DomainError::validation(
            "match must carry exactly one of `http` or `grpc`",
        )),
    }
}

fn validate_http_match(http: &HttpMatch) -> DomainResult<()> {
    if http.methods.is_empty() {
        return Err(DomainError::validation(
            "match.http.methods must contain at least one method",
        ));
    }
    for method in &http.methods {
        if !ALLOWED_METHODS.contains(&method.as_str()) {
            return Err(DomainError::validation(format!(
                "match.http.methods contains unsupported method `{method}`"
            )));
        }
    }
    if http.path.is_empty() {
        return Err(DomainError::validation("match.http.path must not be empty"));
    }
    Ok(())
}

/// Validate a route document.
///
/// # Errors
/// Returns a validation error when any schema constraint is violated.
pub fn validate_route(route: &Route) -> DomainResult<()> {
    validate_match(&route.match_config)?;
    for tag in &route.tags {
        validate_tag(tag)?;
    }
    if let Some(rate_limit) = &route.rate_limit {
        validate_rate_limit(rate_limit)?;
    }
    if let Some(plugins) = &route.plugins {
        validate_plugins(plugins, PluginRefContext::Route)?;
    }
    if let Some(cors) = &route.cors {
        validate_cors(cors)?;
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-resource-model-schema-validation:p1:inst-validate

#[cfg(test)]
mod tests {
    use super::{
        PluginRefContext, validate_cors, validate_match, validate_plugins, validate_rate_limit,
        validate_server,
    };
    use crate::domain::model::{
        BurstRate, CorsConfig, Endpoint, GrpcMatch, HttpMatch, MatchConfig, PluginsConfig,
        RateLimitConfig, RateLimitWindow, Scheme, ServerConfig, SharingMode, SustainedRate,
    };
    use uuid::Uuid;

    fn plugins(items: &[&str]) -> PluginsConfig {
        PluginsConfig {
            sharing: SharingMode::Private,
            items: items.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn server(endpoints: Vec<Endpoint>) -> ServerConfig {
        ServerConfig { endpoints }
    }

    fn endpoint(host: &str, scheme: Scheme, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn an_empty_pool_is_rejected() {
        assert!(validate_server(&server(vec![])).is_err());
    }

    #[test]
    fn a_plaintext_endpoint_on_port_eighty_is_accepted() {
        let cfg = server(vec![endpoint("stub.local", Scheme::Http, 80)]);
        assert!(validate_server(&cfg).is_ok());
    }

    #[test]
    fn a_pool_must_not_mix_scheme_or_port() {
        let mixed_scheme = server(vec![
            endpoint("a.vendor.com", Scheme::Https, 443),
            endpoint("b.vendor.com", Scheme::Wss, 443),
        ]);
        assert!(validate_server(&mixed_scheme).is_err());

        let mixed_port = server(vec![
            endpoint("a.vendor.com", Scheme::Https, 443),
            endpoint("b.vendor.com", Scheme::Https, 8443),
        ]);
        assert!(validate_server(&mixed_port).is_err());
    }

    #[test]
    fn match_requires_exactly_one_rule() {
        let both = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: Some(GrpcMatch {
                service: "S".to_owned(),
                method: "M".to_owned(),
            }),
        };
        assert!(validate_match(&both).is_err());
        assert!(validate_match(&MatchConfig::default()).is_err());
    }

    #[test]
    fn an_unsupported_method_is_rejected() {
        let cfg = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["TRACE".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&cfg).is_err());
    }

    #[test]
    fn rate_limit_minimums_are_enforced() {
        let mut cfg = RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 0,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: crate::domain::model::RateLimitScope::Tenant,
            strategy: crate::domain::model::RateLimitStrategy::Reject,
            cost: 1,
        };
        assert!(validate_rate_limit(&cfg).is_err());
        cfg.sustained.rate = 5;
        assert!(validate_rate_limit(&cfg).is_ok());
        cfg.burst = Some(BurstRate { capacity: 0 });
        assert!(validate_rate_limit(&cfg).is_err());
    }

    #[test]
    fn credentials_with_a_wildcard_origin_are_rejected() {
        let cfg = CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec![],
            allow_credentials: true,
        };
        assert!(validate_cors(&cfg).is_err());
    }

    #[test]
    fn upstream_plugin_bindings_accept_a_gts_identifier_or_a_bare_uuid() {
        let id = Uuid::new_v4().to_string();
        let gts = format!("gts.cf.core.oagw.guard_plugin.v1~{id}");
        assert!(validate_plugins(&plugins(&[&gts]), PluginRefContext::Upstream).is_ok());
        assert!(validate_plugins(&plugins(&[&id]), PluginRefContext::Upstream).is_ok());
    }

    #[test]
    fn route_plugin_bindings_reject_a_bare_uuid() {
        let id = Uuid::new_v4().to_string();
        let gts = format!("gts.cf.core.oagw.guard_plugin.v1~{id}");
        assert!(validate_plugins(&plugins(&[&gts]), PluginRefContext::Route).is_ok());
        assert!(validate_plugins(&plugins(&[&id]), PluginRefContext::Route).is_err());
    }

    #[test]
    fn a_plugin_binding_that_is_neither_shape_is_rejected_for_both_contexts() {
        let bogus = plugins(&["not-a-uuid-or-gts-id"]);
        assert!(validate_plugins(&bogus, PluginRefContext::Upstream).is_err());
        assert!(validate_plugins(&bogus, PluginRefContext::Route).is_err());
    }

    #[test]
    fn an_empty_plugin_binding_entry_is_rejected() {
        let empty = plugins(&[""]);
        assert!(validate_plugins(&empty, PluginRefContext::Upstream).is_err());
        assert!(validate_plugins(&empty, PluginRefContext::Route).is_err());
    }
}
