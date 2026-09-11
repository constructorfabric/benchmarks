//! Route matching: method allowlist, longest path prefix, priority tiebreak.

use crate::domain::model::{GrpcMatch, HttpMatch, PathSuffixMode, Route};

/// The outcome of matching one route against a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    /// The route matches; carries the path suffix left after the prefix.
    Matched { suffix: String },
    /// The route is not a match for this request.
    NoMatch,
}

/// How a route's path is compared with the request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMatch {
    /// The request path must equal the configured prefix.
    Exact,
    /// The request path must start with the prefix; the rest is the suffix.
    Prefix,
}

/// Whether a request path matches a route's HTTP rule.
#[must_use]
pub fn matches_http(route: &Route, method: &str, path: &str) -> MatchOutcome {
    let Some(match_config) = route.http_match() else {
        return MatchOutcome::NoMatch;
    };
    if !route.enabled || !matches_method(match_config, method) {
        return MatchOutcome::NoMatch;
    }
    match_suffix(match_config, path)
}

fn matches_method(match_config: &HttpMatch, method: &str) -> bool {
    match_config
        .methods
        .iter()
        .any(|candidate| candidate.as_str().eq_ignore_ascii_case(method))
}

fn match_suffix(match_config: &HttpMatch, path: &str) -> MatchOutcome {
    let prefix = match_config.path.trim_end_matches('/');
    let request = path.trim_end_matches('/');
    let normalised_request = if request.is_empty() { "/" } else { request };
    let normalised_prefix = if prefix.is_empty() { "/" } else { prefix };
    if normalised_request != normalised_prefix
        && !normalised_request.starts_with(&format!("{normalised_prefix}/"))
    {
        return MatchOutcome::NoMatch;
    }
    match match_config.path_suffix_mode {
        PathSuffixMode::Disabled => {
            if normalised_request == normalised_prefix {
                MatchOutcome::Matched {
                    suffix: String::new(),
                }
            } else {
                MatchOutcome::NoMatch
            }
        }
        PathSuffixMode::Append => {
            let suffix = normalised_request
                .strip_prefix(normalised_prefix)
                .unwrap_or_default();
            MatchOutcome::Matched {
                suffix: suffix.trim_start_matches('/').to_owned(),
            }
        }
    }
}

/// Whether a gRPC route matches the requested service and method.
#[must_use]
pub fn matches_grpc(route: &Route, service: &str, method: &str) -> bool {
    route.enabled
        && route.grpc_match().is_some_and(
            |GrpcMatch {
                 service: s,
                 method: m,
             }| { s == service && (m.is_empty() || m == method) },
        )
}

/// Sorts routes into the order they should be tried: longest prefix first,
/// then the higher priority, then the lower route id for determinism.
pub fn order_candidates(routes: &mut [Route]) {
    routes.sort_by(|left, right| {
        let left_len = prefix_len(left);
        let right_len = prefix_len(right);
        right_len
            .cmp(&left_len)
            .then(right.priority.cmp(&left.priority))
            .then(left.id.cmp(&right.id))
    });
}

fn prefix_len(route: &Route) -> usize {
    route
        .http_match()
        .map(|match_config| match_config.path.trim_end_matches('/').len())
        .unwrap_or_default()
}

/// Picks the first matching route from an already-ordered candidate list.
#[must_use]
pub fn select<'a>(routes: &'a [Route], method: &str, path: &str) -> Option<(&'a Route, String)> {
    routes
        .iter()
        .find_map(|route| match matches_http(route, method, path) {
            MatchOutcome::Matched { suffix } => Some((route, suffix)),
            MatchOutcome::NoMatch => None,
        })
}

/// The methods a route accepts as wire strings.
#[must_use]
pub fn allowed_methods(route: &Route) -> Vec<&'static str> {
    route
        .http_match()
        .map(|match_config| {
            match_config
                .methods
                .iter()
                .map(|method| method.as_str())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "match_route_tests.rs"]
mod tests;
