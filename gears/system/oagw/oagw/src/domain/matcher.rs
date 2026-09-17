//! Route matching and endpoint selection for the data plane (ADR-0001).

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::domain::error::{DomainError, ErrorExtensions, ErrorKind};
use crate::domain::model::{Endpoint, HttpMatch, Route, RouteMatch};

/// Round-robin cursor shared by every multi-endpoint upstream.
#[derive(Debug, Default)]
pub struct RoundRobin {
    cursor: AtomicUsize,
}

impl RoundRobin {
    /// Create a cursor starting at the first endpoint.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cursor: AtomicUsize::new(0),
        }
    }

    /// Advance the cursor and return the next position.
    fn next(&self, modulus: usize) -> usize {
        if modulus == 0 {
            return 0;
        }
        self.cursor.fetch_add(1, Ordering::Relaxed) % modulus
    }
}

/// Route selected for a request, with the path the upstream should receive.
#[derive(Debug)]
pub struct RouteSelection<'a> {
    /// Matched route.
    pub route: &'a Route,
    /// Path forwarded upstream (route `path` plus the suffix, when allowed).
    pub path: String,
}

/// Candidate routes to match against, already ordered descendant-first.
#[derive(Debug, Clone, Copy)]
pub struct MatchInput<'a> {
    /// Request method.
    pub method: &'a str,
    /// Request path, without the `/proxy/{alias}` prefix.
    pub path: &'a str,
    /// Query parameter names present on the request.
    pub query_keys: &'a [String],
}

/// Select the route that serves `input` out of `candidates`.
///
/// Candidates are visited in the order given (descendant tenant first); the
/// first candidate that matches wins only after the list is sorted by path
/// specificity and priority, so shadowing resolves to the most specific rule.
///
/// # Errors
///
/// Returns `cf.oagw.route.not_found.v1` when nothing matches and a
/// validation error when a matching route rejects the request's query
/// parameters.
pub fn select_route<'a>(
    candidates: &[&'a Route],
    input: &MatchInput<'_>,
) -> Result<RouteSelection<'a>, DomainError> {
    let mut matched: Vec<(&'a Route, String)> = Vec::new();
    for route in candidates {
        let RouteMatch::Http(rules) = &route.spec.r#match else {
            continue;
        };
        let Some(suffix) = http_path_suffix(rules, input.path) else {
            continue;
        };
        // A `disabled` route rejects path-suffix usage outright: only the exact
        // configured path is served (route.v1 schema).
        if !suffix.is_empty()
            && rules.path_suffix_mode == crate::domain::model::PathSuffixMode::Disabled
        {
            continue;
        }
        if !rules
            .methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(input.method))
        {
            continue;
        }
        if let Some(name) = input
            .query_allowlist_violations(&rules.query_allowlist)
            .into_iter()
            .next()
        {
            return Err(DomainError::validation(format!(
                "query parameter `{name}` is not in the route's query allowlist"
            )));
        }
        matched.push((*route, suffix.to_owned()));
    }
    matched.sort_by(|(left, left_suffix), (right, right_suffix)| {
        path_pattern_len(right)
            .cmp(&path_pattern_len(left))
            .then(right.spec.priority.cmp(&left.spec.priority))
            .then_with(|| right_suffix.len().cmp(&left_suffix.len()))
    });
    let Some((route, suffix)) = matched.into_iter().next() else {
        return Err(DomainError::route_not_found(format!(
            "no route matches {} {}",
            input.method, input.path
        )));
    };
    let RouteMatch::Http(rules) = &route.spec.r#match else {
        return Err(DomainError::route_not_found("route is not an HTTP route"));
    };
    let mut path = rules.path.clone();
    if rules.path_suffix_mode == crate::domain::model::PathSuffixMode::Append {
        path.push_str(&suffix);
    }
    Ok(RouteSelection { route, path })
}

impl MatchInput<'_> {
    /// Query parameter names the route does not allow.
    fn query_allowlist_violations(&self, allowlist: &[String]) -> Vec<String> {
        self.query_keys
            .iter()
            .filter(|key| !allowlist.iter().any(|allowed| allowed == *key))
            .cloned()
            .collect()
    }
}

/// Split `path` into the part consumed by `rules.path` and the suffix.
///
/// Returns `None` when the route's path is not a prefix of the request path.
#[must_use]
pub fn http_path_suffix<'p>(rules: &HttpMatch, path: &'p str) -> Option<&'p str> {
    let base = rules.path.trim_end_matches('/');
    let request = path.trim_end_matches('/');
    if base.is_empty() {
        return Some(path);
    }
    if request == base {
        return Some("");
    }
    let request = if path.len() > request.len() {
        path
    } else {
        request
    };
    request
        .strip_prefix(base)
        .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
}

/// Pick the endpoint a request should be sent to (ADR-0001 matrix).
///
/// # Errors
///
/// Returns the `MissingTargetHost` / `InvalidTargetHost` / `UnknownTargetHost`
/// routing errors of the `X-OAGW-Target-Host` behaviour matrix.
pub fn select_endpoint<'a>(
    endpoints: &'a [Endpoint],
    alias: &str,
    alias_explicit: bool,
    target_host: Option<&str>,
    round_robin: &RoundRobin,
) -> Result<&'a Endpoint, DomainError> {
    let routing_error = |kind: ErrorKind, detail: String, invalid: Option<String>| {
        DomainError::new(kind, detail).with_extensions(ErrorExtensions {
            valid_hosts: endpoints.iter().map(|e| e.host.clone()).collect(),
            invalid_value: invalid,
            alias: Some(alias.to_owned()),
            ..ErrorExtensions::default()
        })
    };
    if endpoints.is_empty() {
        return Err(routing_error(
            ErrorKind::MissingTargetHost,
            format!("upstream `{alias}` has no configured endpoint"),
            None,
        ));
    }
    if endpoints.len() == 1 {
        let endpoint = &endpoints[0];
        if let Some(host) = target_host
            && !host_matches(endpoint, host)
        {
            return Err(routing_error(
                ErrorKind::UnknownTargetHost,
                format!("`{host}` is not a configured endpoint"),
                Some(host.to_owned()),
            ));
        }
        return Ok(endpoint);
    }
    let shared_suffix = endpoints_share_suffix(endpoints);
    let Some(host) = target_host else {
        if shared_suffix {
            let suffix = crate::domain::alias::registrable_suffix(&endpoints[0].host)
                .unwrap_or(endpoints[0].host.as_str());
            return Err(routing_error(
                ErrorKind::MissingTargetHost,
                format!(
                    "upstream `{alias}` spans several endpoints sharing the suffix \
                     `{suffix}`; set the X-OAGW-Target-Host header"
                ),
                None,
            ));
        }
        let index = round_robin.next(endpoints.len());
        return Ok(&endpoints[index]);
    };
    if !is_target_host_shape(host) {
        return Err(routing_error(
            ErrorKind::InvalidTargetHost,
            format!("X-OAGW-Target-Host `{host}` is not a bare hostname or IP"),
            Some(host.to_owned()),
        ));
    }
    let Some(endpoint) = endpoints.iter().find(|e| host_matches(e, host)) else {
        return Err(routing_error(
            ErrorKind::UnknownTargetHost,
            format!("`{host}` is not a configured endpoint of `{alias}`"),
            Some(host.to_owned()),
        ));
    };
    let _ = alias_explicit;
    Ok(endpoint)
}

/// Whether the endpoint pool needs `X-OAGW-Target-Host` to disambiguate.
fn endpoints_share_suffix(endpoints: &[Endpoint]) -> bool {
    if endpoints.len() <= 1 {
        return false;
    }
    let mut suffixes: Vec<&str> = Vec::new();
    for endpoint in endpoints {
        match crate::domain::alias::registrable_suffix(&endpoint.host) {
            Some(suffix) => suffixes.push(suffix),
            None => return false,
        }
    }
    let Some(first) = suffixes.first() else {
        return false;
    };
    suffixes.iter().all(|suffix| suffix == first)
}

/// Length of a route's literal path pattern, used for specificity ordering.
fn path_pattern_len(route: &Route) -> usize {
    match &route.spec.r#match {
        RouteMatch::Http(rules) => rules.path.len(),
        RouteMatch::Grpc(_) => 0,
    }
}

/// Whether `host` names `endpoint` (host equality, ignoring the port).
fn host_matches(endpoint: &Endpoint, host: &str) -> bool {
    let normalized = host.trim().trim_end_matches('.').to_ascii_lowercase();
    normalized == endpoint.host.to_ascii_lowercase()
}

/// Whether `host` is a bare hostname or IP literal (no port, path or scheme).
///
/// A bracketed IPv6 literal is accepted as a single token: its colons belong
/// to the address, not to a `host:port` pair.
#[must_use]
pub fn is_target_host_shape(host: &str) -> bool {
    let value = host.trim();
    if value.is_empty() || value.contains(['/', '@', '?', '#']) {
        return false;
    }
    if let Some(ipv6) = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return !ipv6.is_empty() && crate::domain::model::parse_ip(ipv6).is_some();
    }
    !value.contains(':')
        && (crate::domain::model::validate_host(value).is_ok()
            || crate::domain::model::parse_ip(value).is_some())
}

#[cfg(test)]
#[path = "matcher_tests.rs"]
mod matcher_tests;
