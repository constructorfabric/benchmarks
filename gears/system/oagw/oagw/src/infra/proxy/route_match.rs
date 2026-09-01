//! Route selection for the data plane (`DESIGN` §3.3, "Find Matching Route").
//!
//! A proxy request is matched against the routes of the resolved upstream by
//!
//! 1. enabled,
//! 2. `match.http` present (a gRPC match rule can never satisfy an HTTP
//!    request — gRPC is Phase 3 and has no reachable proxy code path),
//! 3. method in `match.http.methods`,
//! 4. longest matching path prefix.
//!
//! Prefix ties are broken by the lower `priority` value first (the dispatch
//! contract for this data plane), then by route id so the outcome is total.
//!
//! The suffix that follows the matched prefix is spliced onto the outbound
//! path according to `path_suffix_mode`, and the query string is validated
//! against `query_allowlist` (empty allowlist ⇒ no query parameter is
//! accepted, per `route.v1.schema.json`).

use crate::domain::error::DomainError;
use crate::domain::model::{HttpMethod, PathSuffixMode, Route, Upstream};

/// The route a proxy request belongs to, and the outbound path it maps to.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedRoute {
    /// The winning route.
    pub route: Route,
    /// Proxy path after the matched route prefix; empty on an exact hit.
    pub remainder: String,
    /// Path forwarded upstream.
    pub outbound_path: String,
}

/// Pick the route of `upstream` that serves the request.
///
/// # Errors
/// Returns [`DomainError::RouteNotFound`] when no route matches, and
/// [`DomainError::Validation`] for a query parameter or path suffix the match
/// rule does not accept.
pub fn select_route(
    upstream: &Upstream,
    routes: &[Route],
    method: &http::Method,
    proxy_path: &str,
    query: &[(&str, &str)],
) -> Result<SelectedRoute, DomainError> {
    let matched = routes
        .iter()
        .filter(|route| route.enabled && route.upstream_id == upstream.id)
        .filter(|route| route.r#match.http.is_some())
        .filter(|route| {
            route
                .r#match
                .http
                .as_ref()
                .is_some_and(|http| allows(http, method))
        })
        .filter_map(|route| {
            let http = route.r#match.http.as_ref()?;
            prefix_len(proxy_path, &http.path).map(|len| (len, route))
        })
        .max_by(|left, right| {
            // Longest prefix first, then the lower `priority` value, then the
            // lower id, so the outcome is total and stable.
            left.0
                .cmp(&right.0)
                .then(right.1.priority.cmp(&left.1.priority))
                .then(left.1.id.cmp(&right.1.id))
        })
        .map(|(_, route)| route);

    let Some(route) = matched else {
        return Err(DomainError::RouteNotFound {
            alias: upstream.alias.clone(),
            path: proxy_path.to_owned(),
        });
    };
    let http = route
        .r#match
        .http
        .as_ref()
        .ok_or_else(|| DomainError::RouteNotFound {
            alias: upstream.alias.clone(),
            path: proxy_path.to_owned(),
        })?;

    enforce_query_allowlist(http, query)?;
    let prefix = http.path.trim_end_matches('/').len();
    let remainder = &proxy_path[prefix.min(proxy_path.len())..];
    let outbound_path = splice(&http.path, remainder, http.path_suffix_mode)?;

    Ok(SelectedRoute {
        route: route.clone(),
        remainder: remainder.to_owned(),
        outbound_path,
    })
}

/// `true` when the route's method allowlist admits `method`.
fn allows(http: &crate::domain::model::HttpMatch, method: &http::Method) -> bool {
    http.methods
        .iter()
        .any(|allowed| matches_method(*allowed, method))
}

/// Compare a configured [`HttpMethod`] with a wire method, case-insensitively
/// as RFC 9110 requires.
#[must_use]
pub fn matches_method(allowed: HttpMethod, method: &http::Method) -> bool {
    method.as_str().eq_ignore_ascii_case(allowed.as_str())
}

/// Length of `pattern` when it is a prefix of `path`, on a path boundary.
fn prefix_len(proxy_path: &str, pattern: &str) -> Option<usize> {
    let pattern = pattern.trim_end_matches('/');
    if pattern.is_empty() {
        return Some(0);
    }
    if proxy_path == pattern {
        return Some(pattern.len());
    }
    if proxy_path.starts_with(pattern) && proxy_path[pattern.len()..].starts_with('/') {
        Some(pattern.len())
    } else {
        None
    }
}

/// Splice the remainder onto the route path for the configured suffix mode.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the mode is
/// [`PathSuffixMode::Disabled`] and a suffix was supplied.
fn splice(pattern: &str, remainder: &str, mode: PathSuffixMode) -> Result<String, DomainError> {
    match mode {
        PathSuffixMode::Disabled if !remainder.is_empty() => Err(DomainError::validation(
            "path_suffix_mode 'disabled' rejects a path suffix",
        )),
        PathSuffixMode::Disabled => Ok(pattern.to_owned()),
        PathSuffixMode::Append => {
            let prefix = pattern.trim_end_matches('/');
            Ok(match (prefix, remainder) {
                ("", "") => "/".to_owned(),
                ("", suffix) => suffix.to_owned(),
                (prefix, "") => prefix.to_owned(),
                (route_path, suffix) => format!("{route_path}{suffix}"),
            })
        }
    }
}

/// Enforce `match.http.query_allowlist` (`DESIGN` §"Guard Rules").
///
/// # Errors
/// Returns [`DomainError::Validation`] for a parameter name the allowlist does
/// not accept; an empty allowlist accepts none.
fn enforce_query_allowlist(
    http: &crate::domain::model::HttpMatch,
    query: &[(&str, &str)],
) -> Result<(), DomainError> {
    for (name, _) in query {
        if !http.query_allowlist.iter().any(|allowed| allowed == name) {
            return Err(DomainError::validation(format!(
                "query parameter '{name}' is not allowed by the route"
            )));
        }
    }
    Ok(())
}
