//! Write-time validation for upstreams, routes and plugin bindings.
//!
//! Validation is deliberately split from the schemas' *shape* checks (serde
//! does those): this module owns the cross-field rules the JSON Schemas
//! cannot express — pool homogeneity, protocol/scheme agreement, plugin
//! resolvability, and the CORS credential restriction.

use std::collections::BTreeSet;

use crate::domain::error::OagwError;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    CorsConfig, Endpoint, MatchConfig, PluginKind, PluginsConfig, RateLimitConfig, ServerConfig,
};
use crate::domain::ports::PluginCatalog;

/// Schemes an endpoint may declare.
///
/// The TLS family is the documented *default posture*; plaintext is admitted
/// here because whether a plaintext connection is actually opened is a
/// separate, connection-time decision governed by
/// `OagwConfig::allow_http_upstream`. Keeping the two apart is what lets an
/// operator run `{"scheme": "http", "port": 80}` in a deployment that has
/// opted in, without the type system rejecting the request first.
pub const ALLOWED_SCHEMES: &[&str] = &["https", "http", "wss", "ws", "wt", "grpc"];

/// Schemes that belong to the HTTP-family protocol.
const HTTP_FAMILY: &[&str] = &["https", "http", "wss", "ws", "wt"];

/// HTTP methods a route may declare.
pub const ALLOWED_ROUTE_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH"];

/// Methods a CORS policy may allow.
pub const ALLOWED_CORS_METHODS: &[&str] =
    &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Validate a tag against `^[a-z0-9_-]+$`.
///
/// # Errors
///
/// Returns a validation error naming the offending tag.
pub fn validate_tags(tags: &[String]) -> Result<(), OagwError> {
    for tag in tags {
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(OagwError::validation(format!(
                "tag '{tag}' must match ^[a-z0-9_-]+$"
            )));
        }
    }
    Ok(())
}

/// Validate the endpoint pool: at least one endpoint, well-formed hosts, and
/// an identical `scheme`/`port` across the pool (multi-endpoint pooling rule).
///
/// # Errors
///
/// Returns a validation error describing the first violated rule.
pub fn validate_server(server: &ServerConfig) -> Result<(), OagwError> {
    if server.endpoints.is_empty() {
        return Err(OagwError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    }
    for endpoint in &server.endpoints {
        validate_endpoint(endpoint)?;
    }
    let schemes: BTreeSet<&str> = server.endpoints.iter().map(|e| e.scheme.as_str()).collect();
    if schemes.len() > 1 {
        return Err(OagwError::validation(
            "all endpoints in a pool must share the same scheme",
        ));
    }
    let ports: BTreeSet<u16> = server.endpoints.iter().map(|e| e.port).collect();
    if ports.len() > 1 {
        return Err(OagwError::validation(
            "all endpoints in a pool must share the same port",
        ));
    }
    Ok(())
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<(), OagwError> {
    if !ALLOWED_SCHEMES.contains(&endpoint.scheme.as_str()) {
        return Err(OagwError::validation(format!(
            "endpoint scheme '{}' is not one of {}",
            endpoint.scheme,
            ALLOWED_SCHEMES.join(", ")
        )));
    }
    if endpoint.port == 0 {
        return Err(OagwError::validation("endpoint port must be 1..=65535"));
    }
    let host = crate::domain::alias::normalize_host(&endpoint.host);
    if host.is_empty() {
        return Err(OagwError::validation("endpoint host must not be empty"));
    }
    if crate::domain::alias::is_ip_literal(&host) {
        return Ok(());
    }
    crate::domain::alias::validate_hostname(&host)
}

/// Validate the protocol identifier and its agreement with the pool scheme.
///
/// # Errors
///
/// Returns a validation error for an unknown protocol or a scheme that does
/// not belong to the declared protocol family.
pub fn validate_protocol(protocol: &str, server: &ServerConfig) -> Result<(), OagwError> {
    if protocol != gts::PROTOCOL_HTTP && protocol != gts::PROTOCOL_GRPC {
        return Err(OagwError::validation(format!(
            "protocol '{protocol}' must be one of {} or {}",
            gts::PROTOCOL_HTTP,
            gts::PROTOCOL_GRPC
        )));
    }
    let Some(first) = server.endpoints.first() else {
        return Ok(());
    };
    let is_grpc_protocol = protocol == gts::PROTOCOL_GRPC;
    let is_grpc_scheme = first.scheme == "grpc";
    if is_grpc_protocol != is_grpc_scheme {
        return Err(OagwError::validation(format!(
            "protocol '{protocol}' does not admit endpoint scheme '{}'",
            first.scheme
        )));
    }
    if !is_grpc_protocol && !HTTP_FAMILY.contains(&first.scheme.as_str()) {
        return Err(OagwError::validation(format!(
            "endpoint scheme '{}' is not valid for the HTTP protocol",
            first.scheme
        )));
    }
    Ok(())
}

/// Validate a rate limit block.
///
/// # Errors
///
/// Returns a validation error for a non-positive rate, capacity or cost, or
/// an out-of-range overcommit ratio.
pub fn validate_rate_limit(rate_limit: &RateLimitConfig) -> Result<(), OagwError> {
    if rate_limit.sustained.rate == 0 {
        return Err(OagwError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if let Some(burst) = rate_limit.burst
        && burst.capacity == Some(0)
    {
        return Err(OagwError::validation(
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    if rate_limit.cost == 0 {
        return Err(OagwError::validation("rate_limit.cost must be at least 1"));
    }
    if let Some(budget) = rate_limit.budget
        && let Some(ratio) = budget.overcommit_ratio
        && !(1.0..=2.0).contains(&ratio)
    {
        return Err(OagwError::validation(
            "rate_limit.budget.overcommit_ratio must be within 1.0..=2.0",
        ));
    }
    Ok(())
}

/// Validate a CORS block.
///
/// # Errors
///
/// Returns a validation error when credentials are combined with a wildcard
/// origin, when an unknown method is listed, or when CORS is enabled without
/// any allowed origin.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(OagwError::validation(
            "cannot use allow_credentials with a wildcard origin",
        ));
    }
    if let Some(methods) = &cors.allowed_methods {
        for method in methods {
            if !ALLOWED_CORS_METHODS.contains(&method.to_ascii_uppercase().as_str()) {
                return Err(OagwError::validation(format!(
                    "cors.allowed_methods contains unsupported method '{method}'"
                )));
            }
        }
    }
    if cors.enabled && cors.allowed_origins.is_empty() {
        return Err(OagwError::validation(
            "cors.allowed_origins must list at least one origin when CORS is enabled",
        ));
    }
    Ok(())
}

/// Validate an HTTP/gRPC match block against the upstream's protocol.
///
/// # Errors
///
/// Returns a validation error when neither or both protocol blocks are
/// present, when they disagree with the upstream protocol, or when the HTTP
/// rules are malformed.
pub fn validate_match(match_config: &MatchConfig, protocol: &str) -> Result<(), OagwError> {
    match (&match_config.http, &match_config.grpc) {
        (Some(_), Some(_)) => {
            return Err(OagwError::validation(
                "match must carry exactly one of {http, grpc}",
            ));
        }
        (None, None) => {
            return Err(OagwError::validation(
                "match must carry exactly one of {http, grpc}",
            ));
        }
        (Some(http), None) => {
            if protocol != gts::PROTOCOL_HTTP {
                return Err(OagwError::validation(
                    "match.http is only valid for an HTTP upstream",
                ));
            }
            if http.methods.is_empty() {
                return Err(OagwError::validation(
                    "match.http.methods must list at least one method",
                ));
            }
            for method in &http.methods {
                if !ALLOWED_ROUTE_METHODS.contains(&method.to_ascii_uppercase().as_str()) {
                    return Err(OagwError::validation(format!(
                        "match.http.methods contains unsupported method '{method}'"
                    )));
                }
            }
            if http.path.trim().is_empty() {
                return Err(OagwError::validation("match.http.path must not be empty"));
            }
        }
        (None, Some(grpc)) => {
            if protocol != gts::PROTOCOL_GRPC {
                return Err(OagwError::validation(
                    "match.grpc is only valid for a gRPC upstream",
                ));
            }
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(OagwError::validation(
                    "match.grpc.service and match.grpc.method must not be empty",
                ));
            }
        }
    }
    Ok(())
}

/// Normalize a route path: trim, guarantee a leading `/`, drop a trailing `/`
/// (except for the root path itself).
#[must_use]
pub fn normalize_route_path(raw: &str) -> String {
    let trimmed = raw.trim();
    let with_slash = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    if with_slash.len() > 1 {
        with_slash.trim_end_matches('/').to_owned()
    } else {
        with_slash
    }
}

/// Outcome of resolving a plugin reference at write time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRefKind {
    /// A named, in-process plugin implementation.
    Named,
    /// A UUID-backed custom plugin that must exist in the plugin table.
    Custom(uuid::Uuid),
}

/// Validate the `auth.type` reference.
///
/// # Errors
///
/// Returns a validation error for a reference that is not an auth plugin or
/// that no registry can resolve. Catalog-only identifiers (`basic`,
/// `bearer`) fail here with `unknown auth plugin`.
pub fn validate_auth_ref(
    plugin_ref: &str,
    catalog: &dyn PluginCatalog,
) -> Result<PluginRefKind, OagwError> {
    let kind = PluginKind::from_plugin_ref(plugin_ref).ok_or_else(|| {
        OagwError::validation(format!(
            "auth.type '{plugin_ref}' is not a GTS plugin identifier"
        ))
    })?;
    if kind != PluginKind::Auth {
        return Err(OagwError::validation(format!(
            "auth.type '{plugin_ref}' is not an auth plugin"
        )));
    }
    if let Some(uuid) = gts::plugin_ref_uuid(plugin_ref) {
        return Ok(PluginRefKind::Custom(uuid));
    }
    if catalog.has_auth(plugin_ref) {
        Ok(PluginRefKind::Named)
    } else {
        Err(OagwError::validation(format!(
            "unknown auth plugin: {plugin_ref}"
        )))
    }
}

/// Validate one `plugins.items[]` binding.
///
/// # Errors
///
/// Returns a validation error when the reference is not a guard or transform
/// plugin, or when no registry can resolve it.
pub fn validate_chain_ref(
    plugin_ref: &str,
    catalog: &dyn PluginCatalog,
) -> Result<PluginRefKind, OagwError> {
    let kind = PluginKind::from_plugin_ref(plugin_ref).ok_or_else(|| {
        OagwError::validation(format!(
            "plugins.items[].plugin_ref '{plugin_ref}' is not a GTS plugin identifier"
        ))
    })?;
    if kind == PluginKind::Auth {
        return Err(OagwError::validation(format!(
            "auth plugin '{plugin_ref}' cannot be bound through plugins.items; use the `auth` \
             field instead"
        )));
    }
    if let Some(uuid) = gts::plugin_ref_uuid(plugin_ref) {
        return Ok(PluginRefKind::Custom(uuid));
    }
    let resolvable = match kind {
        PluginKind::Guard => catalog.has_guard(plugin_ref),
        PluginKind::Transform => catalog.has_transform(plugin_ref),
        PluginKind::Auth => false,
    };
    if resolvable {
        Ok(PluginRefKind::Named)
    } else {
        Err(OagwError::validation(format!(
            "unknown plugin: {plugin_ref}"
        )))
    }
}

/// Validate a whole plugin chain and return the custom plugin ids it
/// references, in binding order.
///
/// # Errors
///
/// Returns the first binding error encountered.
pub fn validate_plugins(
    plugins: &PluginsConfig,
    catalog: &dyn PluginCatalog,
) -> Result<Vec<uuid::Uuid>, OagwError> {
    let mut custom = Vec::new();
    for binding in &plugins.items {
        if let PluginRefKind::Custom(uuid) = validate_chain_ref(&binding.plugin_ref, catalog)? {
            custom.push(uuid);
        }
    }
    Ok(custom)
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_route_path, validate_auth_ref, validate_chain_ref, validate_cors, validate_match,
        validate_protocol, validate_rate_limit, validate_server, validate_tags,
    };
    use crate::domain::gts_helpers as gts;
    use crate::domain::model::{
        BudgetConfig, BurstRate, CorsConfig, Endpoint, GrpcMatch, HttpMatch, MatchConfig,
        PathSuffixMode, RateLimitConfig, ServerConfig, SustainedRate,
    };
    use crate::domain::ports::PluginCatalog;

    struct Catalog;

    impl PluginCatalog for Catalog {
        fn has_auth(&self, plugin_ref: &str) -> bool {
            plugin_ref == gts::APIKEY_AUTH_PLUGIN_ID || plugin_ref == gts::NOOP_AUTH_PLUGIN_ID
        }
        fn has_guard(&self, plugin_ref: &str) -> bool {
            plugin_ref == gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID
        }
        fn has_transform(&self, plugin_ref: &str) -> bool {
            plugin_ref == gts::REQUEST_ID_TRANSFORM_PLUGIN_ID
        }
    }

    fn server(endpoints: Vec<Endpoint>) -> ServerConfig {
        ServerConfig { endpoints }
    }

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn plaintext_scheme_is_accepted_by_the_field() {
        let s = server(vec![ep("http", "localhost", 80)]);
        validate_server(&s).expect("http is a legal endpoint scheme");
        validate_protocol(gts::PROTOCOL_HTTP, &s).expect("http belongs to the HTTP family");
    }

    #[test]
    fn tls_family_schemes_are_accepted() {
        for scheme in ["https", "wss", "wt"] {
            let s = server(vec![ep(scheme, "api.example.com", 443)]);
            validate_server(&s).expect("scheme accepted");
            validate_protocol(gts::PROTOCOL_HTTP, &s).expect("HTTP family");
        }
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let s = server(vec![ep("ftp", "files.example.com", 21)]);
        assert_eq!(validate_server(&s).expect_err("rejected").status, 400);
    }

    #[test]
    fn pool_must_be_homogeneous() {
        let mixed_scheme = server(vec![
            ep("https", "a.vendor.com", 443),
            ep("wss", "b.vendor.com", 443),
        ]);
        assert!(validate_server(&mixed_scheme).is_err());

        let mixed_port = server(vec![
            ep("https", "a.vendor.com", 443),
            ep("https", "b.vendor.com", 8443),
        ]);
        assert!(validate_server(&mixed_port).is_err());

        let ok = server(vec![
            ep("https", "a.vendor.com", 443),
            ep("https", "b.vendor.com", 443),
        ]);
        validate_server(&ok).expect("homogeneous");
    }

    #[test]
    fn empty_pool_is_rejected() {
        assert!(validate_server(&server(Vec::new())).is_err());
    }

    #[test]
    fn grpc_protocol_requires_grpc_scheme() {
        let http_pool = server(vec![ep("https", "api.example.com", 443)]);
        assert!(validate_protocol(gts::PROTOCOL_GRPC, &http_pool).is_err());
        let grpc_pool = server(vec![ep("grpc", "api.example.com", 443)]);
        validate_protocol(gts::PROTOCOL_GRPC, &grpc_pool).expect("agrees");
        assert!(validate_protocol(gts::PROTOCOL_HTTP, &grpc_pool).is_err());
    }

    #[test]
    fn unknown_protocol_is_rejected() {
        let pool = server(vec![ep("https", "api.example.com", 443)]);
        assert!(validate_protocol("gts.cf.core.oagw.protocol.v1~nope.v1", &pool).is_err());
    }

    #[test]
    fn cors_credentials_with_wildcard_is_rejected() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert_eq!(validate_cors(&cors).expect_err("rejected").status, 400);
    }

    #[test]
    fn cors_enabled_without_origins_is_rejected() {
        let cors = CorsConfig {
            enabled: true,
            ..CorsConfig::default()
        };
        assert!(validate_cors(&cors).is_err());
    }

    #[test]
    fn cors_unknown_method_is_rejected() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: Some(vec!["TRACE".to_owned()]),
            ..CorsConfig::default()
        };
        assert!(validate_cors(&cors).is_err());
    }

    #[test]
    fn rate_limit_bounds_are_enforced() {
        let base = RateLimitConfig {
            sharing: crate::domain::model::SharingMode::Private,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 10,
                window: crate::domain::model::RateWindow::Second,
            },
            burst: None,
            budget: None,
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        };
        validate_rate_limit(&base).expect("valid");

        let zero_rate = RateLimitConfig {
            sustained: SustainedRate {
                rate: 0,
                window: crate::domain::model::RateWindow::Second,
            },
            ..base.clone()
        };
        assert!(validate_rate_limit(&zero_rate).is_err());

        let zero_burst = RateLimitConfig {
            burst: Some(BurstRate { capacity: Some(0) }),
            ..base.clone()
        };
        assert!(validate_rate_limit(&zero_burst).is_err());

        let bad_ratio = RateLimitConfig {
            budget: Some(BudgetConfig {
                mode: crate::domain::model::BudgetMode::Allocated,
                total: Some(100),
                overcommit_ratio: Some(3.0),
            }),
            ..base
        };
        assert!(validate_rate_limit(&bad_ratio).is_err());
    }

    #[test]
    fn match_requires_exactly_one_protocol_block() {
        let http = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "/v1/models".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        validate_match(&http, gts::PROTOCOL_HTTP).expect("valid");
        assert!(validate_match(&http, gts::PROTOCOL_GRPC).is_err());

        let neither = MatchConfig::default();
        assert!(validate_match(&neither, gts::PROTOCOL_HTTP).is_err());

        let both = MatchConfig {
            http: http.http.clone(),
            grpc: Some(GrpcMatch {
                service: "s".to_owned(),
                method: "m".to_owned(),
            }),
        };
        assert!(validate_match(&both, gts::PROTOCOL_HTTP).is_err());
    }

    #[test]
    fn match_rejects_unsupported_method_and_empty_path() {
        let bad_method = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["TRACE".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&bad_method, gts::PROTOCOL_HTTP).is_err());

        let empty_methods = MatchConfig {
            http: Some(HttpMatch {
                methods: Vec::new(),
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&empty_methods, gts::PROTOCOL_HTTP).is_err());
    }

    #[test]
    fn catalog_only_auth_ids_are_unknown_plugins() {
        let catalog = Catalog;
        validate_auth_ref(gts::APIKEY_AUTH_PLUGIN_ID, &catalog).expect("implemented");
        let err = validate_auth_ref(gts::BASIC_AUTH_PLUGIN_ID, &catalog).expect_err("catalog only");
        assert!(err.detail.contains("unknown auth plugin"));
        assert!(validate_auth_ref(gts::BEARER_AUTH_PLUGIN_ID, &catalog).is_err());
    }

    #[test]
    fn catalog_only_chain_ids_are_not_bindable() {
        let catalog = Catalog;
        validate_chain_ref(gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID, &catalog).expect("bindable");
        validate_chain_ref(gts::REQUEST_ID_TRANSFORM_PLUGIN_ID, &catalog).expect("bindable");
        for id in [
            gts::TIMEOUT_GUARD_PLUGIN_ID,
            gts::CORS_GUARD_PLUGIN_ID,
            gts::LOGGING_TRANSFORM_PLUGIN_ID,
            gts::METRICS_TRANSFORM_PLUGIN_ID,
        ] {
            assert!(
                validate_chain_ref(id, &catalog).is_err(),
                "{id} must not be bindable"
            );
        }
    }

    #[test]
    fn auth_plugin_cannot_be_bound_in_the_chain() {
        assert!(validate_chain_ref(gts::APIKEY_AUTH_PLUGIN_ID, &Catalog).is_err());
    }

    #[test]
    fn tags_pattern_is_enforced() {
        validate_tags(&["openai".to_owned(), "llm-v2".to_owned(), "a_b".to_owned()])
            .expect("valid");
        assert!(validate_tags(&["Upper".to_owned()]).is_err());
        assert!(validate_tags(&["has space".to_owned()]).is_err());
        assert!(validate_tags(&[String::new()]).is_err());
    }

    #[test]
    fn route_path_normalization() {
        assert_eq!(normalize_route_path("v1/chat"), "/v1/chat");
        assert_eq!(normalize_route_path("/v1/chat/"), "/v1/chat");
        assert_eq!(normalize_route_path("  /v1  "), "/v1");
        assert_eq!(normalize_route_path("/"), "/");
    }
}
