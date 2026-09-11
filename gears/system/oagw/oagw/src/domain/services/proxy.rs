//! Data-plane request planning (`contracts/proxy-api.md` § 1).
//!
//! Pure functions: given the resolved upstream, the route table and the inbound
//! request line, they decide which route answers, which endpoint serves it and
//! what the outbound request line looks like. The transport lives in
//! `infra::proxy`; everything here is testable without a socket.

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, HttpMatch, Route, Upstream};

/// A route that matched a proxy request, with the part of the path that the
/// route did not consume.
#[derive(Debug, Clone)]
pub struct RouteMatch {
    /// The matching route.
    pub route: Route,
    /// Path portion beyond the route prefix.
    pub suffix: String,
}

/// Splits a proxy URL into its alias and the remainder.
///
/// `/v1/proxy/api.openai.com/v1/chat` yields `(api.openai.com, /v1/chat)`.
#[must_use]
pub fn split_proxy_path(rest: &str) -> (String, String) {
    let rest = rest.trim_start_matches('/');
    match rest.find('/') {
        Some(idx) => {
            let (alias, suffix) = rest.split_at(idx);
            (alias.to_owned(), suffix.to_owned())
        }
        None => (rest.to_owned(), String::new()),
    }
}

/// Whether a request path matches a route prefix on segment boundaries.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Resolves the route for a proxy request.
///
/// Disabled routes are skipped. The longest matching prefix wins; a matching
/// path with a method outside `match.methods[]` is a `400`, and no matching
/// path at all is a `404`.
///
/// # Errors
/// [`DomainError::Validation`] on a method mismatch or a rejected path suffix,
/// [`DomainError::RouteNotFound`] when nothing matches.
pub fn match_http_route(
    routes: &[Route],
    method: &str,
    path: &str,
) -> Result<RouteMatch, DomainError> {
    let mut candidates: Vec<(&Route, String)> = routes
        .iter()
        .filter(|r| r.is_enabled())
        .filter_map(|r| {
            let http = r.match_rule.http.as_ref()?;
            prefix_matches(&http.path, path).then(|| (r, http.path.clone()))
        })
        .collect();
    candidates.sort_by_key(|(_, prefix)| std::cmp::Reverse(prefix.len()));

    let Some((route, prefix)) = candidates.first() else {
        return Err(DomainError::RouteNotFound);
    };
    let http = route
        .match_rule
        .http
        .as_ref()
        .expect("candidate carries an http match");
    let suffix = if prefix == "/" {
        path.to_owned()
    } else {
        path.strip_prefix(prefix).unwrap_or("").to_owned()
    };

    if !http.methods.iter().any(|m| m.as_str() == method) {
        return Err(DomainError::Validation(format!(
            "method {method} is not accepted by this route"
        )));
    }
    if http.path_suffix_mode == crate::domain::model::PathSuffixMode::Disabled && !suffix.is_empty()
    {
        return Err(DomainError::Validation(
            "path_suffix_mode is disabled for this route".to_owned(),
        ));
    }

    Ok(RouteMatch {
        route: (*route).clone(),
        suffix,
    })
}

/// Validates the query string against the route allow-list.
///
/// # Errors
/// [`DomainError::Validation`] when a parameter is outside the allow-list, or
/// when any parameter is present and the allow-list is empty.
pub fn validate_query(http: &HttpMatch, query: &str) -> Result<(), DomainError> {
    if query.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, _)| k.into_owned())
        .collect();
    if http.query_allowlist.is_empty() {
        return Err(DomainError::Validation(
            "the route does not accept query parameters".to_owned(),
        ));
    }
    for name in names {
        if !http.query_allowlist.iter().any(|allowed| allowed == &name) {
            return Err(DomainError::Validation(format!(
                "query parameter '{name}' is not in the route allow-list"
            )));
        }
    }
    Ok(())
}

/// Selects the endpoint to dial, honouring `X-OAGW-Target-Host`.
///
/// # Errors
/// [`DomainError::MissingTargetHost`], [`DomainError::InvalidTargetHost`],
/// [`DomainError::UnknownTargetHost`].
pub fn select_target(
    upstream: &Upstream,
    target_header: Option<&str>,
) -> Result<Endpoint, DomainError> {
    let endpoints = &upstream.server.endpoints;
    let valid_hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();

    if endpoints.len() == 1 {
        let only = endpoints[0].clone();
        if let Some(value) = target_header.map(str::trim).filter(|v| !v.is_empty())
            && value != only.host
            && value != only.authority()
        {
            return Err(DomainError::UnknownTargetHost {
                value: value.to_owned(),
                valid_hosts,
            });
        }
        return Ok(only);
    }

    let Some(value) = target_header.map(str::trim).filter(|v| !v.is_empty()) else {
        return Err(DomainError::MissingTargetHost { valid_hosts });
    };
    if !is_bare_host(value) {
        return Err(DomainError::InvalidTargetHost {
            value: value.to_owned(),
        });
    }
    endpoints
        .iter()
        .find(|e| e.host == value || e.authority() == value)
        .cloned()
        .ok_or(DomainError::UnknownTargetHost {
            value: value.to_owned(),
            valid_hosts,
        })
}

/// Whether a value is a bare hostname or IP literal: no scheme, port, path or
/// separator characters.
fn is_bare_host(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    if value.contains("://")
        || value.contains('/')
        || value.contains('?')
        || value.contains('#')
        || value.contains('@')
        || value.contains(':')
        || value.starts_with('.')
        || value.ends_with('.')
        || value.starts_with('-')
    {
        return false;
    }
    value
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_')
}

/// Builds the outbound path from the route prefix and the request suffix.
#[must_use]
pub fn outbound_path(route_path: &str, suffix: &str) -> String {
    let route_path = route_path.trim_end_matches('/');
    if suffix.is_empty() {
        if route_path.is_empty() {
            return "/".to_owned();
        }
        return route_path.to_owned();
    }
    if route_path.is_empty() {
        return suffix.to_owned();
    }
    format!("{route_path}{suffix}")
}

/// Whether an inbound request asks for an upgrade.
#[must_use]
pub fn wants_upgrade(headers: &http::HeaderMap) -> bool {
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    connection.split(',').any(|t| t.trim() == "upgrade") && !upgrade.is_empty()
}

/// Whether an inbound request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &http::Method, headers: &http::HeaderMap) -> bool {
    method == http::Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, HttpMatch, HttpMethod, MatchRule, Protocol, Scheme, ServerConfig,
    };
    use uuid::Uuid;

    fn upstream(hosts: &[&str]) -> Upstream {
        Upstream {
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|h| Endpoint {
                        scheme: Scheme::Https,
                        host: (*h).to_owned(),
                        port: Some(443),
                    })
                    .collect(),
            },
            protocol: Protocol::Http,
            ..Upstream::default()
        }
    }

    fn route(path: &str, methods: &[HttpMethod]) -> Route {
        Route {
            upstream_id: Some(Uuid::new_v4()),
            match_rule: MatchRule {
                http: Some(http_match(path, methods)),
                grpc: None,
            },
            ..Route::default()
        }
    }

    fn http_match(path: &str, methods: &[HttpMethod]) -> HttpMatch {
        HttpMatch {
            methods: methods.to_vec(),
            path: path.to_owned(),
            ..HttpMatch::default()
        }
    }

    #[test]
    fn splits_the_alias_from_the_suffix() {
        let (alias, suffix) = split_proxy_path("api.openai.com/v1/chat");
        assert_eq!(alias, "api.openai.com");
        assert_eq!(suffix, "/v1/chat");
        assert_eq!(split_proxy_path("api.openai.com").1, "");
    }

    #[test]
    fn the_longest_matching_prefix_wins() {
        let routes = vec![
            route("/v1", &[HttpMethod::Get]),
            route("/v1/chat", &[HttpMethod::Get]),
        ];
        let matched = match_http_route(&routes, "GET", "/v1/chat/completions").expect("matched");
        assert_eq!(
            matched.route.match_rule.http.expect("http").path,
            "/v1/chat"
        );
        assert_eq!(matched.suffix, "/completions");
    }

    #[test]
    fn a_disallowed_method_is_a_validation_error() {
        let routes = vec![route("/v1", &[HttpMethod::Get])];
        let err = match_http_route(&routes, "DELETE", "/v1/x").expect_err("rejected");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn an_unmatched_path_is_route_not_found() {
        let routes = vec![route("/v1", &[HttpMethod::Get])];
        let err = match_http_route(&routes, "GET", "/v2/x").expect_err("rejected");
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn disabled_routes_do_not_match() {
        let mut disabled = route("/v1", &[HttpMethod::Get]);
        disabled.enabled = false;
        let err = match_http_route(&[disabled], "GET", "/v1/x").expect_err("rejected");
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn a_disabled_suffix_mode_rejects_a_suffix() {
        let mut r = route("/v1/x", &[HttpMethod::Get]);
        r.match_rule.http.as_mut().expect("http").path_suffix_mode =
            crate::domain::model::PathSuffixMode::Disabled;
        let err = match_http_route(&[r], "GET", "/v1/x/y").expect_err("rejected");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn query_parameters_outside_the_allowlist_are_rejected() {
        let mut http = http_match("/v1", &[HttpMethod::Get]);
        http.query_allowlist = vec!["model".to_owned()];
        assert!(validate_query(&http, "model=gpt").is_ok());
        assert!(validate_query(&http, "model=gpt&evil=1").is_err());
        let open = http_match("/v1", &[HttpMethod::Get]);
        assert!(validate_query(&open, "model=gpt").is_err());
        assert!(validate_query(&open, "").is_ok());
    }

    #[test]
    fn a_single_endpoint_needs_no_target_header() {
        let upstream = upstream(&["api.openai.com"]);
        let target = select_target(&upstream, None).expect("selected");
        assert_eq!(target.host, "api.openai.com");
        assert!(select_target(&upstream, Some("api.openai.com:443")).is_ok());
        assert!(select_target(&upstream, Some("other.example.com")).is_err());
    }

    #[test]
    fn a_multi_endpoint_pool_requires_a_matching_header() {
        let upstream = upstream(&["us.vendor.com", "eu.vendor.com"]);
        assert_eq!(
            select_target(&upstream, None).expect_err("rejected"),
            DomainError::MissingTargetHost {
                valid_hosts: vec!["us.vendor.com".to_owned(), "eu.vendor.com".to_owned()],
            }
        );
        assert!(matches!(
            select_target(&upstream, Some("us.vendor.com:443/x")),
            Err(DomainError::InvalidTargetHost { .. })
        ));
        assert!(matches!(
            select_target(&upstream, Some("ap.vendor.com")),
            Err(DomainError::UnknownTargetHost { .. })
        ));
        assert_eq!(
            select_target(&upstream, Some("eu.vendor.com"))
                .expect("selected")
                .host,
            "eu.vendor.com"
        );
    }

    #[test]
    fn the_outbound_path_appends_the_suffix() {
        assert_eq!(
            outbound_path("/v1/chat", "/completions"),
            "/v1/chat/completions"
        );
        assert_eq!(outbound_path("/v1/chat", ""), "/v1/chat");
        assert_eq!(outbound_path("/", ""), "/");
        assert_eq!(outbound_path("/", "/x"), "/x");
    }

    #[test]
    fn upgrades_and_preflights_are_detected() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONNECTION,
            "keep-alive, Upgrade".parse().unwrap(),
        );
        headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(wants_upgrade(&headers));

        let mut preflight = http::HeaderMap::new();
        preflight.insert(http::header::ORIGIN, "https://app.example".parse().unwrap());
        preflight.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            "POST".parse().unwrap(),
        );
        assert!(is_preflight(&http::Method::OPTIONS, &preflight));
        assert!(!is_preflight(&http::Method::POST, &preflight));
    }
}
