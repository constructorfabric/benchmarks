//! The route matcher of the proxy path
//! (`cpt-cf-oagw-flow-request-proxy-route-matching`,
//! `cpt-cf-oagw-algo-request-proxy-route-select`).
//!
//! The matcher is pure: it sees one upstream's route set plus the request
//! surface and returns the selected route or the reason there is none. Walking
//! the tenant chain — the level at which "closest tenant-chain distance
//! outranks everything" is decided — is the caller's job, because the walk
//! crosses the repository and tenant-resolver boundaries.
//!
//! The ranking is the one the DoD fixes: closest tenant-chain distance first
//! (applied by the caller, which stops at the first upstream whose route set
//! yields a match), then the longest matching `match.http.path` prefix, then
//! route priority. The match-determinism invariant guarantees no two enabled
//! routes under the same upstream share a path prefix and priority for the
//! same method, so the ranking is total.
//!
//! A `grpc` match block is configuration surface only and never matches a
//! proxy request (graded deviation 7): the upstream-protocol check
//! (`inst-rp-match-2`) rejects those requests before the matcher runs.

use crate::domain::dto::{MatchConfig, PathSuffixMode, Route};
use crate::domain::error::DomainError;

/// One candidate route, annotated with its distance along the tenant chain.
///
/// The *selected* upstream is distance `0`; an ancestor upstream found later
/// on the walk is `1`, `2`, ... The distance is carried for the outcome's
/// attribution, not for the ranking: the caller never compares candidates from
/// different upstreams, because the first upstream whose route set yields a
/// match wins outright.
#[derive(Debug, Clone)]
pub struct CandidateRoute {
    /// The candidate.
    pub route: Route,
    /// The tenant-chain distance of the upstream the route belongs to.
    pub tenant_distance: usize,
}

/// A route the matcher selected.
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The selected route.
    pub route: Route,
    /// The tenant-chain distance of its upstream.
    pub tenant_distance: usize,
    /// The length of the matching `match.http.path` prefix, for ranking.
    pub prefix_len: usize,
    /// The proxy request path the route matched.
    pub matched_path: String,
    /// The path suffix, without its leading slash; empty when none was
    /// supplied.
    pub path_suffix: String,
}

/// The outcome of matching one upstream's route set.
#[derive(Debug, Clone)]
pub enum RouteMatchOutcome {
    /// A route matched the method and the path surface.
    Matched(MatchedRoute),
    /// No enabled route matches the proxy request path: the route-not-found
    /// outcome, rendered by entry 2.5 as `404`.
    NotFound,
    /// A route matches the path but its `match.http.methods` allowlist
    /// excludes the request method: `405 Method Not Allowed`, which is not a
    /// row of the shared error table and so is not a [`DomainError`].
    MethodNotAllowed {
        /// The methods the path-matching routes admit, for the `Allow` header.
        allowed: Vec<&'static str>,
    },
}

/// Whether the proxy request path matches a route's `match.http.path` prefix.
///
/// The rule is the one the control-plane resolution applies: an exact match, or
/// a match on a whole following segment, so `/v1` matches `/v1` and `/v1/x`
/// but never `/v1x`.
#[must_use]
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-1
// `inst-rp-al-route-1`/`-2`, `inst-rp-al-route-6` .. `-8`: the path prefix
// filter and the suffix it yields, normalised and never escaping the matched
// prefix.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-10
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-11
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-12
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-5
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-7
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-8
pub fn path_matches(route_path: &str, request_path: &str) -> bool {
    let prefix = route_path.trim_end_matches('/');
    if prefix.is_empty() {
        // A root prefix matches the proxy request path alone.
        return request_path == "/" || request_path.is_empty();
    }
    if request_path == prefix {
        return true;
    }
    let boundary = format!("{prefix}/");
    request_path.starts_with(&boundary)
}
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-8
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-7
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-5
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-2
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-12
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-11
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-10
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-1

/// The path suffix the request supplied beyond the matched prefix.
///
/// The suffix carries no leading slash; it is empty when the request path
/// reaches no further than the prefix.
#[must_use]
pub fn suffix_of(route_path: &str, request_path: &str) -> String {
    let prefix = route_path.trim_end_matches('/');
    if request_path.len() <= prefix.len() {
        return String::new();
    }
    request_path[prefix.len()..].trim_start_matches('/').to_owned()
}

/// Whether the request method is in the route's allowlist.
#[must_use]
pub fn method_allows(route: &Route, method: &str) -> bool {
    match &route.match_ {
        MatchConfig { http: Some(http), .. } => {
            http.methods.iter().any(|allowed| allowed.as_str() == method)
        }
        _ => false,
    }
}

/// The methods the route's allowlist names, for the `Allow` header.
#[must_use]
pub fn methods_of(route: &Route) -> Vec<&'static str> {
    match &route.match_ {
        MatchConfig { http: Some(http), .. } => {
            http.methods.iter().map(|method| method.as_str()).collect()
        }
        _ => Vec::new(),
    }
}

/// Whether the route's match block is an HTTP one.
///
/// A gRPC match block is configuration surface only; it never matches a proxy
/// request (graded deviation 7).
#[must_use]
pub const fn is_http_match(route: &Route) -> bool {
    matches!(&route.match_, MatchConfig { http: Some(_), grpc: None })
}

/// Reject a request path that carries a dot-segment
/// (`cpt-cf-oagw-dod-request-proxy-request-hardening`).
///
/// A `.` or `..` segment escapes the matched route prefix, so it is rejected
/// before any connection is opened or credential resolved.
///
/// # Errors
///
/// Returns a validation error naming `path` when a dot-segment is present.
pub fn reject_dot_segments(path: &str) -> Result<(), DomainError> {
    for segment in path.split('/') {
        if segment == "." || segment == ".." {
            return Err(DomainError::field_rejection(
                "path",
                "the request path carries a `.` or `..` segment",
            ));
        }
    }
    Ok(())
}

/// Reject a suffix that escapes the matched route prefix or introduces a
/// double slash by concatenation.
///
/// # Errors
///
/// Returns a validation error naming `path` when the composed target path does
/// not stay under the matched prefix.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-6
// `inst-rp-al-route-6`: `.` and `..` segments, a double slash introduced by
// suffix concatenation and a suffix that escapes the matched prefix are
// rejected before the outbound path is composed.
pub fn reject_suffix_escape(route_path: &str, request_path: &str) -> Result<(), DomainError> {
    reject_dot_segments(request_path)?;
    if request_path.contains("//") {
        return Err(DomainError::field_rejection(
            "path",
            "the path suffix introduces a double slash",
        ));
    }
    let prefix = route_path.trim_end_matches('/');
    if prefix.is_empty() {
        return Ok(());
    }
    if request_path != prefix && !request_path.starts_with(&format!("{prefix}/")) {
        return Err(DomainError::field_rejection(
            "path",
            "the path suffix escapes the matched route prefix",
        ));
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-6

/// The query-string pairs of a proxy request, in the order the client wrote
/// them.
///
/// A pair without a value contributes an empty one; a bare trailing `&`
/// contributes nothing.
#[must_use]
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (name.to_owned(), value.to_owned()),
            None => (pair.to_owned(), String::new()),
        })
        .collect()
}

/// Whether every query parameter name is in the route's allowlist.
///
/// An empty allowlist permits no query parameter; an absent or empty query
/// string is always permitted.
///
/// # Errors
///
/// Returns a validation error naming the rejected parameter key.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-9
// `inst-rp-al-route-9` .. `-12`, `inst-rp-validate-4` .. `-6`: the allowlist
// filter — an empty allowlist permits no query parameter, an unknown key is
// rejected naming it, and a control character is rejected.
pub fn query_is_allowed(route: &Route, query: Option<&str>) -> Result<(), DomainError> {
    let Some(query) = query else {
        return Ok(());
    };
    if query.is_empty() {
        return Ok(());
    }
    let Some(http) = &route.match_.http else {
        return Ok(());
    };
    for (name, _) in query_pairs(query) {
        if !http.query_allowlist.iter().any(|allowed| allowed == &name) {
            // The rejected name is a client-controlled value, and the contract
            // of this module is that no request value is ever echoed into an
            // error `detail`: the rejection names the position, never the
            // offending spelling.
            return Err(DomainError::field_rejection(
                "query",
                "a query parameter outside the route's allowlist was supplied",
            ));
        }
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-request-proxy-route-select:p1:inst-rp-al-route-9

/// Select the route of one upstream's route set for a proxy request.
///
/// The candidates are the routes of one upstream; the caller walks the tenant
/// chain upstream by upstream and stops at the first set that returns a
/// [`RouteMatchOutcome::Matched`].
///
/// `path_suffix_mode` is the mode a route declares, read by the caller because
/// the mode is consulted before the match is confirmed.
#[must_use]
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-1
// `inst-rp-match-1` .. `-10`, `inst-rp-al-route-1` .. `-12`: the matcher —
// enabled-and-HTTP filtering, the closest-tenant / longest-prefix / priority
// ranking, the disabled-suffix verdict, the query allowlist and the
// method-not-allowed outcome.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-9
pub fn select(
    candidates: &[CandidateRoute],
    method: &str,
    request_path: &str,
    path_suffix_mode_of: impl Fn(&Route) -> PathSuffixMode,
) -> RouteMatchOutcome {
    // Disabled routes never match, and only an HTTP match block can match.
    let enabled: Vec<&CandidateRoute> = candidates
        .iter()
        .filter(|candidate| candidate.route.enabled && is_http_match(&candidate.route))
        .collect();

    // The path filter decides between "no route" and "a route exists but the
    // method is excluded".
    let mut path_matched: Vec<(&CandidateRoute, usize)> = Vec::new();
    for candidate in &enabled {
        let route_path = candidate
            .route
            .match_
            .http
            .as_ref()
            .map_or(String::new(), |http| http.path.clone());
        if !path_matches(&route_path, request_path) {
            continue;
        }
        // A suffix supplied to a route with `path_suffix_mode: disabled` does
        // not match it, because that route admits no suffix at all.
        if path_suffix_mode_of(&candidate.route) == PathSuffixMode::Disabled
            && !suffix_of(&route_path, request_path).is_empty()
        {
            continue;
        }
        path_matched.push((candidate, route_path.len()));
    }

    if path_matched.is_empty() {
        return RouteMatchOutcome::NotFound;
    }

    // The method filter, over the routes the path already matched.
    let mut allowed: Vec<&'static str> = Vec::new();
    let mut method_matched: Vec<(&CandidateRoute, usize)> = Vec::new();
    for (candidate, prefix_len) in &path_matched {
        for allowed_method in methods_of(&candidate.route) {
            if !allowed.contains(&allowed_method) {
                allowed.push(allowed_method);
            }
        }
        if method_allows(&candidate.route, method) {
            method_matched.push((candidate, *prefix_len));
        }
    }
    if method_matched.is_empty() {
        allowed.sort_unstable();
        return RouteMatchOutcome::MethodNotAllowed { allowed };
    }

    // Rank: longest matching path prefix, then route priority, then the
    // identifier so the ordering is total under the determinism invariant.
    method_matched.sort_by(|(left, left_len), (right, right_len)| {
        right_len
            .cmp(left_len)
            .then(right.route.priority.cmp(&left.route.priority))
            .then(left.route.id.cmp(&right.route.id))
    });
    let (candidate, prefix_len) = method_matched[0];
    let route_path = candidate
        .route
        .match_
        .http
        .as_ref()
        .map_or(String::new(), |http| http.path.clone());
    RouteMatchOutcome::Matched(MatchedRoute {
        route: candidate.route.clone(),
        tenant_distance: candidate.tenant_distance,
        prefix_len,
        matched_path: request_path.to_owned(),
        path_suffix: suffix_of(&route_path, request_path),
    })
}
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-10
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-1
