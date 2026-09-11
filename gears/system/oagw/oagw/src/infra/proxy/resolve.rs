//! Proxy-time route matching and endpoint selection.
//!
//! Pure functions: the async tenant walk lives in
//! [`crate::infra::proxy::service`].

use http::Method;

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, HttpMatch, HttpMethod, PathSuffixMode, Route, Upstream};

/// Everything after `/proxy/{alias}` in the request path.
pub type ProxyPath = String;

/// Result of route matching.
#[derive(Debug, Clone)]
pub struct Matched {
    /// The route configuration that was selected.
    pub route: Route,
    /// Path that will be sent upstream.
    pub outbound_path: String,
}

/// Select the endpoint that should receive the request.
///
/// # Errors
/// Returns [`DomainError::MissingTargetHost`],
/// [`DomainError::InvalidTargetHost`] or [`DomainError::UnknownTargetHost`]
/// per the ADR 0001 behaviour matrix.
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    target_host: Option<&str>,
) -> Result<&'a Endpoint, DomainError> {
    let endpoints = &upstream.server.endpoints;
    let alias_host = upstream.alias.split(':').next().unwrap_or_default();
    let is_common_suffix = endpoints.len() > 1
        && endpoints
            .iter()
            .all(|e| !e.host.eq_ignore_ascii_case(alias_host));

    let Some(requested) = target_host else {
        if is_common_suffix {
            return Err(DomainError::MissingTargetHost);
        }
        return endpoints
            .first()
            .ok_or_else(|| DomainError::validation("upstream has no configured endpoints"));
    };

    let normalized = requested.trim().trim_end_matches('.').to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.contains([':', '/', '@', '%', '?', '#'])
        || !crate::domain::alias::is_valid_host(&normalized)
    {
        return Err(DomainError::InvalidTargetHost(requested.to_owned()));
    }
    endpoints
        .iter()
        .find(|e| e.host.eq_ignore_ascii_case(&normalized))
        .ok_or_else(|| DomainError::UnknownTargetHost(requested.to_owned()))
}

/// Compute the outbound path for a route given the proxy path.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the route forbids a suffix.
pub fn outbound_path(match_rule: &HttpMatch, proxy_path: &str) -> Result<String, DomainError> {
    let route_path = match_rule.path.as_str();
    let extra = extra_of(route_path, proxy_path);
    match match_rule.path_suffix_mode {
        PathSuffixMode::Disabled => {
            if extra.is_empty() {
                Ok(route_path.to_owned())
            } else {
                Err(DomainError::validation(
                    "path suffix is not allowed for this route",
                ))
            }
        }
        PathSuffixMode::Append => Ok(format!("{route_path}{extra}")),
    }
}

/// The part of `proxy_path` beyond the route path.
#[must_use]
pub fn extra_of(route_path: &str, proxy_path: &str) -> String {
    if proxy_path.len() <= route_path.len() {
        return String::new();
    }
    let extra = &proxy_path[route_path.len()..];
    if extra.is_empty() {
        String::new()
    } else if extra.starts_with('/') {
        extra.to_owned()
    } else {
        String::new()
    }
}

/// Match a route for the resolved upstream.
///
/// Routes are ranked by priority, then by the length of their path prefix.
///
/// # Errors
/// Returns [`DomainError::RouteNotFound`] when no route path matches,
/// [`DomainError::Validation`] when the method is not in the allowlist, the
/// query is not allowlisted, or the path suffix is rejected.
pub fn match_route(
    candidates: &[Route],
    method: &Method,
    proxy_path: &str,
    query_names: &[String],
) -> Result<Option<Matched>, DomainError> {
    let parsed = HttpMethod::parse(method.as_str()).ok_or_else(|| {
        DomainError::validation(format!("unsupported method '{}'", method.as_str()))
    })?;

    let mut best: Option<(&Route, bool)> = None;
    let mut best_rank = (i64::MIN, 0_usize);
    for route in candidates {
        if !route.enabled {
            continue;
        }
        let Some(http) = route.match_rule.http.as_ref() else {
            continue;
        };
        if !matches_prefix(http, proxy_path) {
            continue;
        }
        let rank = (route.priority, http.path.len());
        if best.is_none_or(|_| rank > best_rank) {
            let suffix_ok = matches_suffix(http, proxy_path);
            best = Some((route, suffix_ok));
            best_rank = rank;
        }
    }

    let Some((route, suffix_ok)) = best else {
        return Ok(None);
    };
    if !suffix_ok {
        return Err(DomainError::validation(
            "the request path does not match the route's suffix rules",
        ));
    }
    let http = route
        .match_rule
        .http
        .as_ref()
        .ok_or_else(|| DomainError::validation("route has no HTTP match rule"))?;

    let allowed = http
        .methods
        .iter()
        .filter_map(|m| HttpMethod::parse(m))
        .any(|m| m == parsed);
    if !allowed {
        return Err(DomainError::validation(format!(
            "method '{}' is not allowed by the matched route",
            method.as_str()
        )));
    }
    if !http.query_allowlist.is_empty() {
        for name in query_names {
            if !http.query_allowlist.iter().any(|a| a == name) {
                return Err(DomainError::validation(format!(
                    "query parameter '{name}' is not allowed by this route"
                )));
            }
        }
    }
    Ok(Some(Matched {
        route: route.clone(),
        outbound_path: outbound_path(http, proxy_path)?,
    }))
}

/// Whether the proxy path starts on a route's path.
fn matches_prefix(http: &HttpMatch, proxy_path: &str) -> bool {
    let route_path = http.path.as_str();
    proxy_path == route_path || proxy_path.starts_with(route_path)
}

/// Whether the suffix after the route path is admissible.
fn matches_suffix(http: &HttpMatch, proxy_path: &str) -> bool {
    let extra = extra_of(http.path.as_str(), proxy_path);
    match http.path_suffix_mode {
        PathSuffixMode::Disabled => extra.is_empty(),
        PathSuffixMode::Append => extra.is_empty() || extra.starts_with('/'),
    }
}

/// Extract the query parameter names from a raw query string.
#[must_use]
pub fn query_names(query: Option<&str>) -> Vec<String> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .map(|(name, _)| name.into_owned())
        .collect()
}

/// Extract the alias and the proxy path from a proxy request path.
///
/// `full_path` starts after `/proxy/`.
#[must_use]
pub fn split_alias_and_path(full_path: &str) -> Option<(String, String)> {
    let (alias, rest) = full_path
        .split_once('/')
        .map_or((full_path, ""), |(alias, rest)| (alias, rest));
    let alias = crate::domain::alias::normalize(alias);
    if alias.is_empty() {
        return None;
    }
    let rest = if rest.is_empty() {
        "/"
    } else {
        &format!("/{rest}")
    };
    Some((alias, rest.to_owned()))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::model::{GrpcMatch, MatchConfig};
    use uuid::Uuid;

    /// A deterministic UUID for a test fixture.
    fn uuid_from_index(id: u64) -> Uuid {
        Uuid::from_fields(0, 0, 0, &id.to_be_bytes())
    }

    fn route(id: u64, path: &str, methods: &[&str]) -> Route {
        Route {
            id: uuid_from_index(id),
            tenant_id: Uuid::nil(),
            upstream_id: Uuid::nil(),
            match_rule: MatchConfig {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            priority: 0,
            enabled: true,
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: vec![],
        }
    }

    fn path_of(matched: &Matched) -> &str {
        &matched.route.match_rule.http.as_ref().unwrap().path
    }

    #[test]
    fn longest_prefix_wins() {
        let routes = vec![route(1, "/v1", &["GET"]), route(2, "/v1/chat", &["GET"])];
        let matched = match_route(&routes, &Method::GET, "/v1/chat/completions", &[])
            .unwrap()
            .unwrap();
        assert_eq!(path_of(&matched), "/v1/chat");
        assert_eq!(matched.outbound_path, "/v1/chat/completions");
    }

    #[test]
    fn higher_priority_wins_over_prefix_length() {
        let mut low = route(1, "/v1/chat", &["GET"]);
        low.priority = 0;
        let mut high = route(2, "/v1", &["GET"]);
        high.priority = 10;
        let matched = match_route(&[low, high], &Method::GET, "/v1/chat/completions", &[])
            .unwrap()
            .unwrap();
        assert_eq!(path_of(&matched), "/v1");
    }

    #[test]
    fn method_outside_the_allowlist_is_rejected() {
        let routes = vec![route(1, "/v1/x", &["GET"])];
        let err = match_route(&routes, &Method::POST, "/v1/x", &[]).unwrap_err();
        assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[test]
    fn method_is_case_insensitive() {
        let routes = vec![route(1, "/v1/x", &["get"])];
        assert!(
            match_route(&routes, &Method::GET, "/v1/x", &[])
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn no_matching_route_yields_none() {
        let routes = vec![route(1, "/v1/x", &["GET"])];
        assert!(
            match_route(&routes, &Method::GET, "/other", &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn disabled_routes_are_skipped() {
        let mut disabled = route(1, "/v1/x", &["GET"]);
        disabled.enabled = false;
        assert!(
            match_route(&[disabled], &Method::GET, "/v1/x", &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unknown_query_parameter_is_rejected() {
        let mut r = route(1, "/v1", &["GET"]);
        if let Some(http) = r.match_rule.http.as_mut() {
            http.query_allowlist = vec!["model".to_owned()];
        }
        let err = match_route(&[r], &Method::GET, "/v1", &["foo".to_owned()]).unwrap_err();
        assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn allowed_query_parameter_is_accepted() {
        let mut r = route(1, "/v1", &["GET"]);
        if let Some(http) = r.match_rule.http.as_mut() {
            http.query_allowlist = vec!["model".to_owned()];
        }
        assert!(
            match_route(&[r], &Method::GET, "/v1", &["model".to_owned()])
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn suffix_mode_disabled_rejects_suffixes() {
        let mut r = route(1, "/v1/models", &["GET"]);
        if let Some(http) = r.match_rule.http.as_mut() {
            http.path_suffix_mode = PathSuffixMode::Disabled;
        }
        let err = match_route(&[r], &Method::GET, "/v1/models/extra", &[]).unwrap_err();
        assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn suffix_mode_disabled_allows_exact_paths() {
        let mut r = route(1, "/v1/models", &["GET"]);
        if let Some(http) = r.match_rule.http.as_mut() {
            http.path_suffix_mode = PathSuffixMode::Disabled;
        }
        let matched = match_route(&[r], &Method::GET, "/v1/models", &[])
            .unwrap()
            .unwrap();
        assert_eq!(matched.outbound_path, "/v1/models");
    }

    #[test]
    fn grpc_only_routes_are_skipped() {
        let mut r = route(1, "/v1", &["GET"]);
        r.match_rule = MatchConfig {
            http: None,
            grpc: Some(GrpcMatch {
                service: "svc".to_owned(),
                method: "m".to_owned(),
            }),
        };
        assert!(
            match_route(&[r], &Method::GET, "/v1", &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn query_names_are_extracted() {
        assert_eq!(query_names(Some("a=1&b=2")), vec!["a", "b"]);
        assert_eq!(query_names(Some("dup=1&dup=2")), vec!["dup", "dup"]);
        assert!(query_names(None).is_empty());
    }

    #[test]
    fn proxy_paths_are_split_on_the_first_segment() {
        let (alias, path) = split_alias_and_path("api.vendor.com/v1/x").unwrap();
        assert_eq!(alias, "api.vendor.com");
        assert_eq!(path, "/v1/x");
        let (alias, path) = split_alias_and_path("api.vendor.com").unwrap();
        assert_eq!(alias, "api.vendor.com");
        assert_eq!(path, "/");
    }

    #[test]
    fn endpoint_selection_requires_the_header_for_common_suffix_aliases() {
        let upstream = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "vendor.com".to_owned(),
            protocol: "p".to_owned(),
            enabled: true,
            server: crate::domain::model::ServerConfig {
                endpoints: vec![
                    crate::domain::model::Endpoint {
                        scheme: crate::domain::model::EndpointScheme::Https,
                        host: "us.vendor.com".to_owned(),
                        port: 443,
                    },
                    crate::domain::model::Endpoint {
                        scheme: crate::domain::model::EndpointScheme::Https,
                        host: "eu.vendor.com".to_owned(),
                        port: 443,
                    },
                ],
            },
            auth: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: vec![],
        };
        assert!(matches!(
            select_endpoint(&upstream, None),
            Err(DomainError::MissingTargetHost)
        ));
        assert!(matches!(
            select_endpoint(&upstream, Some("us.vendor.com:8443")),
            Err(DomainError::InvalidTargetHost(_))
        ));
        assert!(matches!(
            select_endpoint(&upstream, Some("apac.vendor.com")),
            Err(DomainError::UnknownTargetHost(_))
        ));
        assert_eq!(
            select_endpoint(&upstream, Some("US.Vendor.com"))
                .unwrap()
                .host,
            "us.vendor.com"
        );
    }

    #[test]
    fn single_endpoint_ignores_the_header() {
        let upstream = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "api.vendor.com".to_owned(),
            protocol: "p".to_owned(),
            enabled: true,
            server: crate::domain::model::ServerConfig {
                endpoints: vec![crate::domain::model::Endpoint {
                    scheme: crate::domain::model::EndpointScheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: 8080,
                }],
            },
            auth: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: vec![],
        };
        assert_eq!(
            select_endpoint(&upstream, Some("127.0.0.1")).unwrap().host,
            "127.0.0.1"
        );
        assert_eq!(select_endpoint(&upstream, None).unwrap().host, "127.0.0.1");
    }
}
