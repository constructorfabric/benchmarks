//! Route matching and target-endpoint selection (ADR-0001).

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, GrpcMatch, HttpMatch, MatchConfig, PathSuffixMode};

/// A candidate route with the upstream it belongs to, in tenant-chain order
/// (root first, descendants last so descendant routes take priority).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCandidate<'a> {
    /// Tenant the route was configured on.
    pub tenant_id: uuid::Uuid,
    /// Owning upstream id.
    pub upstream_id: uuid::Uuid,
    /// The route.
    pub route: &'a crate::domain::model::Route,
}

/// Normalizes an HTTP method for matching: `HEAD` matches a route that allows
/// `GET`, everything else is compared case-insensitively.
#[must_use]
pub fn effective_methods(method: &str) -> Vec<String> {
    match method.to_ascii_uppercase().as_str() {
        "HEAD" => vec!["HEAD".to_owned(), "GET".to_owned()],
        other => vec![other.to_owned()],
    }
}

/// Whether a route's match rule can serve the given method.
#[must_use]
fn allows_method(match_config: &MatchConfig, method: &str) -> bool {
    let Some(http) = match_config.http.as_ref() else {
        return false;
    };
    effective_methods(method).iter().any(|m| {
        http.methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(m))
    })
}

/// Longest-prefix HTTP path match quality: the number of characters of the
/// route's `path` prefix consumed by the request path.
///
/// `path` is the request path relative to the proxy alias; the route's
/// `path` pattern must be a prefix of it (after `path_suffix_mode` handling).
#[must_use]
fn prefix_len(route_path: &str, request_path: &str) -> Option<usize> {
    let route_path = route_path.trim_end_matches('/');
    let request_path = request_path.trim_end_matches('/');
    if route_path.is_empty() {
        return Some(0);
    }
    if request_path == route_path {
        return Some(route_path.len());
    }
    if request_path.starts_with(route_path)
        && request_path.as_bytes().get(route_path.len()) == Some(&b'/')
    {
        return Some(route_path.len());
    }
    None
}

/// Selects the best matching route for an HTTP request.
///
/// Candidate routes are considered in the order given (root-first); the
/// winner is the route with the longest path prefix, ties broken by the
/// highest `priority`, then by the last (closest tenant) candidate.
///
/// # Errors
///
/// Returns [`DomainError::RouteNotFound`] when no candidate matches the
/// method and path.
pub fn match_http_route<'a>(
    candidates: &[RouteCandidate<'a>],
    method: &str,
    request_path: &str,
) -> Result<&'a crate::domain::model::Route, DomainError> {
    let mut best: Option<(usize, u32, usize, &'a crate::domain::model::Route)> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(http) = candidate.route.match_config.http.as_ref() else {
            continue;
        };
        if !candidate.route.enabled || !allows_method(&candidate.route.match_config, method) {
            continue;
        }
        if !path_matches(http, request_path) {
            continue;
        }
        let quality = prefix_len(&http.path, request_path).unwrap_or(0);
        let score = (quality, candidate.route.priority);
        if best
            .is_none_or(|(best_quality, best_priority, _, _)| score > (best_quality, best_priority))
        {
            best = Some((quality, candidate.route.priority, index, candidate.route));
        }
    }
    best.map(|(_, _, _, route)| route).ok_or_else(|| {
        DomainError::RouteNotFound(format!("no route matches {method} {request_path}"))
    })
}

/// Whether the request path is accepted by the route's `path` pattern.
#[must_use]
pub fn path_matches(http: &HttpMatch, request_path: &str) -> bool {
    let route_path = http.path.trim_end_matches('/');
    if route_path.is_empty() {
        return true;
    }
    request_path == route_path
        || (request_path.starts_with(route_path)
            && request_path.as_bytes().get(route_path.len()) == Some(&b'/'))
}

/// Selects the best matching route for a gRPC upstream.
///
/// gRPC matching is catalogued in the schema but has no reachable proxy code
/// path (DESIGN §4.7); this function exists so the match semantics stay
/// defined and unit-tested. The first (root-first) candidate whose
/// `(service, method)` matches wins, and the returned route is the matched
/// candidate's route.
#[must_use]
pub fn match_grpc<'a>(
    candidates: &[RouteCandidate<'a>],
    service: &str,
    method: &str,
) -> Option<&'a crate::domain::model::Route> {
    candidates
        .iter()
        .find(|candidate| {
            candidate.route.enabled
                && candidate
                    .route
                    .match_config
                    .grpc
                    .as_ref()
                    .is_some_and(|grpc| grpc.service == service && grpc.method == method)
        })
        .map(|candidate| candidate.route)
}

/// Returns the gRPC match of a route, if any.
#[must_use]
pub fn grpc_match(route: &crate::domain::model::Route) -> Option<&GrpcMatch> {
    route.match_config.grpc.as_ref()
}

/// Selects the target endpoint for a proxy request.
///
/// Implements the `X-OAGW-Target-Host` behaviour matrix of ADR-0001:
///
/// | endpoints | alias | header | behaviour |
/// |---|---|---|---|
/// | 1 | any | no | the single endpoint |
/// | 1 | any | yes | validated, then the single endpoint |
/// | 2+ | no common suffix | no | round-robin |
/// | 2+ | no common suffix | yes | the header-selected endpoint |
/// | 2+ | common suffix | no | 400 `missing_target_host` |
/// | 2+ | common suffix | yes | the header-selected endpoint |
///
/// `alias_is_common_suffix` is `true` when the upstream alias was derived
/// from a shared registrable suffix rather than a single hostname.
///
/// # Errors
///
/// * header not a hostname/IP → 400 `invalid_target_host`
/// * header value matches no endpoint → 400 `unknown_target_host`
/// * header absent where required → 400 `missing_target_host`
pub fn select_endpoint(
    endpoints: &[Endpoint],
    alias_is_common_suffix: bool,
    target_host: Option<&str>,
    round_robin_index: u64,
) -> Result<Endpoint, DomainError> {
    let Some(first) = endpoints.first() else {
        return Err(DomainError::RouteNotFound(
            "upstream has no endpoints".to_owned(),
        ));
    };
    if endpoints.len() == 1 {
        if let Some(host) = target_host {
            return validate_target_host(host, endpoints);
        }
        return Ok(first.clone());
    }

    let Some(host) = target_host else {
        if alias_is_common_suffix {
            return Err(DomainError::MissingTargetHost(format!(
                "upstream with {} endpoints shares the alias suffix; set X-OAGW-Target-Host",
                endpoints.len()
            )));
        }
        let index = (round_robin_index % endpoints.len() as u64) as usize;
        return Ok(endpoints[index].clone());
    };
    validate_target_host(host, endpoints)
}

fn validate_target_host(host: &str, endpoints: &[Endpoint]) -> Result<Endpoint, DomainError> {
    let normalized = crate::domain::alias::normalize(host);
    if (is_ip_address(normalized.as_str()) || !normalized.contains(':'))
        // Hostname or IP: must match an endpoint host exactly.
        && let Some(endpoint) = endpoints
            .iter()
            .find(|e| crate::domain::alias::normalize(&e.host) == normalized)
    {
        return Ok(endpoint.clone());
    }
    // Not an IP literal, so the value must be a valid RFC 1123 hostname (or an
    // IPv6 literal, which `is_ip_address` already accepted above).
    if !is_ip_address(normalized.as_str())
        && crate::domain::alias::validate_hostname(normalized.as_str()).is_err()
    {
        return Err(DomainError::InvalidTargetHost(format!(
            "'{host}' is not a hostname or IP address"
        )));
    }
    if normalized.contains('/') || normalized.contains('?') || normalized.contains('#') {
        return Err(DomainError::InvalidTargetHost(format!(
            "'{host}' must not contain a path, query or fragment"
        )));
    }
    if endpoints
        .iter()
        .any(|e| crate::domain::alias::normalize(&e.host) == normalized)
    {
        return Ok(endpoints
            .iter()
            .find(|e| crate::domain::alias::normalize(&e.host) == normalized)
            .cloned()
            .unwrap_or_else(|| endpoints[0].clone()));
    }
    Err(DomainError::UnknownTargetHost(format!(
        "'{host}' does not match any configured endpoint"
    )))
}

#[must_use]
fn is_ip_address(host: &str) -> bool {
    crate::domain::alias::is_ip_address(host)
}

/// Validates query parameters against the route's allowlist.
///
/// # Errors
///
/// Returns 400 when a query parameter is not in `query_allowlist`. An empty
/// allowlist permits no query parameters.
pub fn validate_query_params(http: &HttpMatch, query: &str) -> Result<(), DomainError> {
    if query.is_empty() {
        return Ok(());
    }
    for (key, _) in form_urlencoded::parse(query.as_bytes()) {
        let key = key.to_string();
        if !http
            .query_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&key))
        {
            return Err(DomainError::Validation(format!(
                "query parameter '{key}' is not allowed by the matched route"
            )));
        }
    }
    Ok(())
}

/// Applies `path_suffix_mode` to the proxy path suffix.
///
/// # Errors
///
/// Returns 400 when the route is `disabled` and a suffix is present.
pub fn apply_path_suffix(http: &HttpMatch, path_suffix: &str) -> Result<String, DomainError> {
    match http.path_suffix_mode {
        PathSuffixMode::Disabled => {
            if path_suffix.is_empty() {
                Ok(http.path.clone())
            } else {
                Err(DomainError::Validation(format!(
                    "path suffix '{path_suffix}' is not allowed by this route"
                )))
            }
        }
        PathSuffixMode::Append => {
            let base = http.path.trim_end_matches('/');
            if path_suffix.is_empty() {
                Ok(base.to_owned())
            } else if base.is_empty() {
                let suffix = path_suffix.trim_start_matches('/');
                Ok(format!("/{suffix}"))
            } else {
                let suffix = path_suffix.trim_start_matches('/');
                Ok(format!("{base}/{suffix}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;
