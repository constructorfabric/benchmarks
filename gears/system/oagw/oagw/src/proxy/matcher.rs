//! Route matching for the proxy data plane (R3 step 2, R4).
//!
//! The route a request takes is the one whose `match.http` rule declares the
//! request method and the longest matching path prefix (DESIGN.md §3.2
//! "Shadowing Behavior"). Once a route is selected the match rule doubles as
//! the guard table of DESIGN.md §3.2 "Guard Rules": an unexpected path suffix
//! and an unexpected query parameter both reject the request before anything is
//! forwarded, and the query string is reduced to the allowlisted parameters.
//!
//! Matching never answers 405 (R1): a method no route allows is simply a
//! request no route matches, i.e. 404 `cf.oagw.route.not_found.v1`.

use axum::http::Method;

use crate::domain::merge::merge_route_chain;
use crate::domain::model::{HttpMethod, PathSuffixMode, Route};

#[cfg(test)]
use crate::domain::model::RouteSpec;
use crate::error::{GatewayError, GatewayErrorKind};

/// A route selected for a request, with the path and query to forward.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteMatch {
    /// The effective route (ancestor chain folded in when the same
    /// `(path, priority)` is declared more than once).
    pub route: Route,
    /// The path forwarded upstream: `match.http.path` with the request's path
    /// suffix appended (`path_suffix_mode: append`).
    pub upstream_path: String,
    /// The forwarded query string, reduced to the allowlisted parameters.
    /// `None` when nothing is forwarded.
    pub query: Option<String>,
}

/// The [`HttpMethod`] a wire method spells, or `None` when OAGW has no name for
/// it (`TRACE`, `CONNECT`, `WebDAV` verbs, …).
///
/// An unnameable method can never appear in a route's method allowlist —
/// `HttpMethod::ROUTE_MATCH_ALLOWED` is drawn from the same enum — so such a
/// request matches no route at all.
#[must_use]
pub fn http_method(method: &Method) -> Option<HttpMethod> {
    if method == Method::GET {
        Some(HttpMethod::Get)
    } else if method == Method::POST {
        Some(HttpMethod::Post)
    } else if method == Method::PUT {
        Some(HttpMethod::Put)
    } else if method == Method::PATCH {
        Some(HttpMethod::Patch)
    } else if method == Method::DELETE {
        Some(HttpMethod::Delete)
    } else if method == Method::HEAD {
        Some(HttpMethod::Head)
    } else if method == Method::OPTIONS {
        Some(HttpMethod::Options)
    } else {
        None
    }
}

/// Selects the route a request is served by.
///
/// `routes` are the routes declared for the resolved upstream, ordered root
/// first (ancestor before descendant) so that two declarations of the same
/// `(path, priority)` fold into one effective route through
/// [`merge_route_chain`]. Disabled routes are excluded from matching (R4).
///
/// `path` is the request path relative to the proxy alias (`""` when the proxy
/// URL carries no suffix) and `query` the raw query string.
///
/// # Errors
///
/// Returns 404 `cf.oagw.route.not_found.v1` when no route matches, and a 400
/// validation error when the selected route's `path_suffix_mode` forbids the
/// request's path suffix or when the query string carries a parameter outside
/// `query_allowlist`.
pub fn find_route(
    routes: &[Route],
    method: &Method,
    path: &str,
    query: &str,
) -> Result<RouteMatch, GatewayError> {
    let Some(method) = http_method(method) else {
        return Err(route_not_found(method.as_str(), path));
    };

    let request_path = normalize_path(path);

    // Fold the declarations of the same `(path, priority)` across the tenant
    // chain, so an ancestor's enforced policy stays active on a descendant's
    // route (DESIGN.md §3.2 "Shadowing Behavior").
    let mut groups: Vec<(String, u32, Vec<Route>)> = Vec::new();
    for route in routes {
        let Some(matched) = route.config.match_rule.as_http() else {
            continue;
        };

        match groups.iter_mut().find(|(existing, priority, _)| {
            *existing == matched.path && *priority == route.config.priority
        }) {
            Some((_, _, chain)) => chain.push(route.clone()),
            None => groups.push((
                matched.path.clone(),
                route.config.priority,
                vec![route.clone()],
            )),
        }
    }

    let candidates = groups.iter().filter_map(|(path, _priority, chain)| {
        let route = merge_route_chain(chain)?;
        let matched = route.config.match_rule.as_http()?.clone();
        if !route.is_enabled() || !matched.methods.contains(&method) {
            return None;
        }
        let suffix = prefix_suffix(&request_path, path)?;
        Some((path.clone(), route, matched, suffix))
    });

    let best = candidates.max_by(
        |(left_path, left_route, _, _), (right_path, right_route, _, _)| {
            prefix_length(left_path)
                .cmp(&prefix_length(right_path))
                .then_with(|| left_route.config.priority.cmp(&right_route.config.priority))
        },
    );

    let Some((route_path, route, matched, suffix)) = best else {
        return Err(route_not_found(method.as_str(), path));
    };

    if matched.path_suffix_mode == PathSuffixMode::Disabled && !suffix.is_empty() {
        return Err(GatewayError::validation(
            format!(
                "path suffix `{suffix}` is not accepted by this route because \
                 `match.http.path_suffix_mode` is `disabled`"
            ),
            "path",
        )
        .with_path(request_path));
    }

    let query = forward_query(&matched.query_allowlist, query)?;

    Ok(RouteMatch {
        route,
        upstream_path: append_suffix(&route_path, &suffix),
        query,
    })
}

/// The path suffix a request carries beyond a route's path prefix, or `None`
/// when the route's path is not a prefix of the request path at all.
fn prefix_suffix(request_path: &str, route_path: &str) -> Option<String> {
    let route_path = route_path.trim_end_matches('/');

    if route_path.is_empty() {
        return Some(request_path.to_owned());
    }

    if !request_path.starts_with(route_path) {
        return None;
    }

    match request_path.get(route_path.len()..) {
        Some("" | "/") => Some(String::new()),
        Some(rest) if rest.starts_with('/') => Some(rest.to_owned()),
        _ => None,
    }
}

/// The matching weight of a route path: its length, so the longest prefix wins.
fn prefix_length(route_path: &str) -> usize {
    route_path.trim_end_matches('/').chars().count()
}

/// Normalizes a request path: absolute, without a trailing slash, without
/// repeated slashes. The root stays `/`.
fn normalize_path(path: &str) -> String {
    let mut normalized = path.trim().to_owned();
    if normalized.is_empty() {
        return String::from("/");
    }
    if !normalized.starts_with('/') {
        normalized.insert(0, '/');
    }
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    if normalized.len() > 1 {
        while normalized.ends_with('/') {
            normalized.pop();
        }
    }
    if normalized.is_empty() {
        String::from("/")
    } else {
        normalized
    }
}

/// Builds the upstream path: the route path with the request's suffix appended
/// (`path_suffix_mode: append`, DESIGN.md §3.2 "Transformation Rules").
fn append_suffix(route_path: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return normalize_path(route_path);
    }

    normalize_path(&format!("{route_path}{suffix}"))
}

/// Reduces the raw query string to the parameters `query_allowlist` names, in
/// request order and with their original encoding (DESIGN.md §3.2
/// "Transformation Rules": "Passthrough allowed params").
///
/// # Errors
///
/// Returns a 400 validation error naming the first parameter that is not
/// allowlisted.
fn forward_query(allowlist: &[String], raw_query: &str) -> Result<Option<String>, GatewayError> {
    if raw_query.is_empty() {
        return Ok(None);
    }

    let segments: Vec<&str> = raw_query
        .split('&')
        .filter(|part| !part.is_empty())
        .collect();
    let decoded: Vec<(String, String)> = url::form_urlencoded::parse(raw_query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    let mut forwarded = Vec::with_capacity(segments.len());
    for (index, segment) in segments.iter().enumerate() {
        let raw_key = segment.split_once('=').map_or(*segment, |(key, _)| key);
        let decoded_key = decoded.get(index).map_or(raw_key, |(key, _)| key.as_str());

        if !allowlist
            .iter()
            .any(|allowed| allowed == raw_key || allowed == decoded_key)
        {
            return Err(GatewayError::validation(
                format!(
                    "query parameter `{raw_key}` is not in this route's \
                     `match.http.query_allowlist`"
                ),
                "query",
            ));
        }

        forwarded.push((*segment).to_owned());
    }

    let query = forwarded.join("&");

    Ok((!query.is_empty()).then_some(query))
}

/// 404 `cf.oagw.route.not_found.v1`.
fn route_not_found(method: &str, path: &str) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::RouteNotFound,
        format!("no route of this upstream matches `{method} {path}`"),
    )
    .with_path(path.to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::json;

    use super::*;

    /// Builds a stored route from its JSON document.
    fn route(body: serde_json::Value) -> Route {
        let spec: RouteSpec = serde_json::from_value(body).expect("valid route spec");
        let config = crate::domain::validation::validate_route(&spec).expect("valid route");

        Route::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), config)
    }

    fn matching(path: &str, methods: &[&str]) -> Route {
        route(json!({
            "upstream_id": uuid::Uuid::nil(),
            "match": {
                "http": { "methods": methods, "path": path }
            }
        }))
    }

    #[test]
    fn test_wire_methods_map_onto_the_domain_enum() {
        for (wire, domain) in [
            (Method::GET, HttpMethod::Get),
            (Method::POST, HttpMethod::Post),
            (Method::PUT, HttpMethod::Put),
            (Method::PATCH, HttpMethod::Patch),
            (Method::DELETE, HttpMethod::Delete),
            (Method::HEAD, HttpMethod::Head),
            (Method::OPTIONS, HttpMethod::Options),
        ] {
            assert_eq!(http_method(&wire), Some(domain));
        }

        assert_eq!(http_method(&Method::TRACE), None);
        assert_eq!(http_method(&Method::CONNECT), None);
    }

    #[test]
    fn test_longest_path_prefix_wins() {
        let routes = vec![matching("/v1", &["GET"]), matching("/v1/chat", &["GET"])];

        let matched =
            find_route(&routes, &Method::GET, "/v1/chat/completions", "").expect("a route matches");

        assert_eq!(matched.upstream_path, "/v1/chat/completions");
        assert_eq!(
            matched.route.config.match_rule.as_http().unwrap().path,
            "/v1/chat"
        );
    }

    #[test]
    fn test_method_outside_the_allowlist_is_no_match() {
        let routes = vec![matching("/v1/chat", &["GET", "POST"])];

        let error = find_route(&routes, &Method::DELETE, "/v1/chat", "").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::RouteNotFound);
        assert_eq!(error.status(), 404);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        );
    }

    #[test]
    fn test_disabled_routes_are_excluded_from_matching() {
        let routes = vec![route(json!({
            "upstream_id": uuid::Uuid::nil(),
            "enabled": false,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }))];

        assert!(find_route(&routes, &Method::GET, "/v1/status", "").is_err());
    }

    #[test]
    fn test_grpc_routes_never_match_http_requests() {
        let routes = vec![route(json!({
            "upstream_id": uuid::Uuid::nil(),
            "match": { "grpc": { "service": "gts.cf.demo.v1.Echo", "method": "Send" } }
        }))];

        let error =
            find_route(&routes, &Method::POST, "/gts.cf.demo.v1.Echo/Send", "").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::RouteNotFound);
    }

    #[test]
    fn test_the_highest_priority_wins_at_equal_prefix_length() {
        let routes = vec![
            route(json!({
                "upstream_id": uuid::Uuid::nil(),
                "priority": 1,
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } }
            })),
            matching("/v1/chat", &["GET"]),
        ];

        let matched = find_route(&routes, &Method::GET, "/v1/chat", "").expect("a route matches");

        assert_eq!(matched.route.config.priority, 1);
    }

    #[test]
    fn test_a_disabled_mode_rejects_a_path_suffix() {
        let routes = vec![route(json!({
            "upstream_id": uuid::Uuid::nil(),
            "match": {
                "http": {
                    "methods": ["GET"],
                    "path": "/v1/status",
                    "path_suffix_mode": "disabled"
                }
            }
        }))];

        let matched = find_route(&routes, &Method::GET, "/v1/status", "").expect("exact match");
        assert_eq!(matched.upstream_path, "/v1/status");

        let error = find_route(&routes, &Method::GET, "/v1/status/history", "").unwrap_err();
        assert_eq!(error.kind(), GatewayErrorKind::Validation);
        assert_eq!(error.status(), 400);
        assert_eq!(
            error
                .extensions()
                .extra
                .get("field")
                .and_then(|value| value.as_str()),
            Some("path")
        );
    }

    #[test]
    fn test_the_suffix_is_appended_to_the_route_path() {
        let routes = vec![matching("/v1/chat", &["POST"])];

        let matched = find_route(&routes, &Method::POST, "/v1/chat/completions", "")
            .expect("a route matches");

        assert_eq!(matched.upstream_path, "/v1/chat/completions");
    }

    #[test]
    fn test_a_request_without_a_suffix_forwards_the_route_path() {
        let routes = vec![matching("/v1/chat", &["GET"])];

        let matched = find_route(&routes, &Method::GET, "/v1/chat", "").expect("a route matches");

        assert_eq!(matched.upstream_path, "/v1/chat");
        assert_eq!(matched.query, None);
    }

    #[test]
    fn test_the_proxy_alias_without_a_path_matches_the_root_route() {
        let routes = vec![matching("/", &["GET"])];

        let matched = find_route(&routes, &Method::GET, "", "").expect("a route matches");

        assert_eq!(matched.upstream_path, "/");
    }

    #[test]
    fn test_the_proxy_alias_without_a_path_needs_a_root_route() {
        let routes = vec![matching("/v1/chat", &["GET"])];

        let error = find_route(&routes, &Method::GET, "", "").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::RouteNotFound);
    }

    #[test]
    fn test_unknown_query_parameters_are_rejected() {
        let routes = vec![matching("/v1/chat", &["GET"])];

        let error = find_route(&routes, &Method::GET, "/v1/chat", "model=gpt&trace=1").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::Validation);
        assert!(error.detail().contains("model"), "{}", error.detail());
        assert_eq!(
            error
                .extensions()
                .extra
                .get("field")
                .and_then(|value| value.as_str()),
            Some("query")
        );
    }

    #[test]
    fn test_allowlisted_query_parameters_are_forwarded_verbatim() {
        let routes = vec![route(json!({
            "upstream_id": uuid::Uuid::nil(),
            "match": {
                "http": {
                    "methods": ["GET"],
                    "path": "/v1/chat",
                    "query_allowlist": ["model", "stream"]
                }
            }
        }))];

        let matched = find_route(&routes, &Method::GET, "/v1/chat", "stream=true&model=gpt-4")
            .expect("a route matches");

        assert_eq!(matched.query.as_deref(), Some("stream=true&model=gpt-4"));
    }

    #[test]
    fn test_an_empty_allowlist_forbids_every_query_parameter() {
        let routes = vec![matching("/v1/chat", &["GET"])];

        let error = find_route(&routes, &Method::GET, "/v1/chat", "model=gpt").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::Validation);
    }

    #[test]
    fn test_a_deeper_declaration_of_the_same_path_folds_onto_the_ancestor() {
        let ancestor = matching("/v1/chat", &["GET"]);
        let descendant = route(json!({
            "upstream_id": uuid::Uuid::nil(),
            "match": {
                "http": {
                    "methods": ["GET"],
                    "path": "/v1/chat",
                    "query_allowlist": ["model"]
                }
            }
        }));

        // Root first: the ancestor's declaration comes before the descendant's.
        let routes = vec![ancestor, descendant];

        let matched = find_route(&routes, &Method::GET, "/v1/chat", "").expect("a route matches");

        let allowlist = &matched
            .route
            .config
            .match_rule
            .as_http()
            .expect("http route")
            .query_allowlist;
        assert_eq!(allowlist, &["model".to_owned()]);
    }

    #[test]
    fn test_paths_are_normalized_before_matching() {
        let routes = vec![matching("/v1/chat/", &["GET"])];

        let matched = find_route(&routes, &Method::GET, "/v1/chat/completions/", "")
            .expect("a route matches");

        assert_eq!(matched.upstream_path, "/v1/chat/completions");
    }

    #[test]
    fn test_no_route_at_all_is_a_route_not_found_problem() {
        let error = find_route(&[], &Method::GET, "/v1/chat", "").unwrap_err();

        assert_eq!(error.kind(), GatewayErrorKind::RouteNotFound);
        assert_eq!(error.status(), 404);
        assert!(
            error.detail().contains("GET /v1/chat"),
            "{}",
            error.detail()
        );
    }
}
