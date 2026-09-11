//! Route matching and endpoint selection for the data plane.
//!
//! Realizes `cpt-cf-oagw-algo-ph-route-matching`,
//! `cpt-cf-oagw-algo-ph-endpoint-selection` and the `X-OAGW-Target-Host`
//! decision matrix from ADR-0001 Appendix A.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, PathSuffixMode, Route, Upstream};

/// The outcome of matching a request against an upstream's routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    /// The route that matched.
    pub route: Route,
    /// The path to send upstream.
    pub target_path: String,
}

/// Split `/{alias}` or `/{alias}/{suffix}` into its parts.
#[must_use]
pub fn split_alias_and_suffix(rest: &str) -> (String, String) {
    let trimmed = rest.trim_start_matches('/');
    match trimmed.split_once('/') {
        Some((alias, suffix)) => (alias.to_owned(), format!("/{suffix}")),
        None => (trimmed.to_owned(), String::new()),
    }
}

/// Whether a request path is covered by a route's path prefix.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    if !path.starts_with(prefix) {
        return false;
    }
    // A prefix must end on a segment boundary so `/v1` does not match `/v10`.
    matches!(path.as_bytes().get(prefix.len()), None | Some(b'/'))
}

/// Select the route for a request.
///
/// Ordering is resolved by longest matching path prefix; the frozen route
/// schema carries no `priority` field, which is a documented deviation from
/// DESIGN's domain model. Disabled routes never match.
///
/// # Errors
/// Returns [`DomainError::NotFound`] when no enabled route accepts the request.
// @cpt-begin:cpt-cf-oagw-dod-ph-route-matching:p1:inst-full
pub fn match_route(routes: &[Route], method: &str, suffix: &str) -> Result<Matched, DomainError> {
    let path = if suffix.is_empty() { "/" } else { suffix };
    let method_up = method.to_ascii_uppercase();

    let mut best: Option<(&Route, &str)> = None;
    for r in routes.iter().filter(|r| r.enabled) {
        let Some(h) = r.match_.http.as_ref() else {
            continue;
        };
        if !h
            .methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&method_up))
        {
            continue;
        }
        if !prefix_matches(&h.path, path) {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|(_, p)| h.path.len() > p.len());
        if better {
            best = Some((r, h.path.as_str()));
        }
    }

    let (route, prefix) = best.ok_or_else(|| {
        DomainError::not_found("route", format!("{method_up} {path}"))
    })?;
    let http = route
        .match_
        .http
        .as_ref()
        .ok_or_else(|| DomainError::Internal {
            message: "matched route lost its http match".to_owned(),
        })?;

    let target_path = match http.path_suffix_mode {
        PathSuffixMode::Disabled => prefix.to_owned(),
        PathSuffixMode::Append => path.to_owned(),
    };

    Ok(Matched {
        route: route.clone(),
        target_path,
    })
}
// @cpt-end:cpt-cf-oagw-dod-ph-route-matching:p1:inst-full

/// Filter a query string down to a route's allowlist.
///
/// An empty allowlist forwards the query unchanged.
#[must_use]
pub fn filter_query(allowlist: &[String], query: Option<&str>) -> Option<String> {
    let q = query?;
    if allowlist.is_empty() {
        return Some(q.to_owned());
    }
    let kept: Vec<&str> = q
        .split('&')
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or(pair);
            allowlist.iter().any(|a| a == name)
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("&"))
    }
}

/// Whether a target-host header value is a bare hostname or IP literal:
/// no scheme, no port, no path, no userinfo, no whitespace.
#[must_use]
pub fn is_bare_host(v: &str) -> bool {
    if v.is_empty() || v.len() > 253 {
        return false;
    }
    if v.contains(char::is_whitespace) {
        return false;
    }
    // A bracketed IPv6 literal is the one shape allowed to carry colons.
    if v.starts_with('[') {
        return v.ends_with(']')
            && v[1..v.len() - 1].parse::<std::net::Ipv6Addr>().is_ok();
    }
    if v.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    // Otherwise: a hostname, so no delimiter characters at all.
    !v.chars().any(|c| matches!(c, '/' | '?' | '#' | '@' | ':' | '\\'))
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Round-robin cursor over an upstream's endpoint pool.
#[derive(Debug, Default)]
pub struct RoundRobin {
    next: AtomicUsize,
}

impl RoundRobin {
    /// The next index in a pool of `len` endpoints.
    pub fn next(&self, len: usize) -> usize {
        if len <= 1 {
            return 0;
        }
        self.next.fetch_add(1, Ordering::Relaxed) % len
    }
}

/// Select the endpoint to connect to.
///
/// Implements ADR-0001's `X-OAGW-Target-Host` matrix. A single-endpoint pool
/// needs no header; a multi-endpoint pool is selected round-robin unless the
/// header names one. A header that is present but malformed, or well-formed
/// but not in the pool, is a distinct 400 each.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an unusable target-host header.
// @cpt-begin:cpt-cf-oagw-dod-ph-endpoint-selection:p1:inst-full
pub fn select_endpoint<'a>(
    up: &'a Upstream,
    target_host: Option<&str>,
    rr: &RoundRobin,
) -> Result<&'a Endpoint, DomainError> {
    let pool = &up.server.endpoints;
    let valid_hosts = || {
        pool.iter()
            .map(|e| e.host.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };

    match target_host {
        None => {
            // No header: single-endpoint pools are unambiguous, multi-endpoint
            // pools load-balance round-robin.
            pool.get(rr.next(pool.len())).ok_or_else(|| {
                DomainError::Internal {
                    message: "upstream has an empty endpoint pool".to_owned(),
                }
            })
        }
        Some(raw) => {
            let want = raw.trim();
            if !is_bare_host(want) {
                // Present but malformed.
                return Err(DomainError::validation(
                    crate::domain::error::TARGET_HOST_HEADER,
                    format!(
                        "invalid target host `{raw}`; valid hosts are: {}",
                        valid_hosts()
                    ),
                ));
            }
            pool.iter()
                .find(|e| e.host.eq_ignore_ascii_case(want))
                .ok_or_else(|| {
                    // Well-formed but not in the pool.
                    DomainError::validation(
                        crate::domain::error::TARGET_HOST_HEADER,
                        format!(
                            "unknown target host `{want}`; valid hosts are: {}",
                            valid_hosts()
                        ),
                    )
                })
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-ph-endpoint-selection:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{HttpMatch, RouteMatch, Scheme, Server};
    use uuid::Uuid;

    fn route(path: &str, methods: &[&str], mode: PathSuffixMode, enabled: bool) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            enabled,
            tags: vec![],
            upstream_id: Uuid::nil(),
            match_: RouteMatch {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: mode,
                }),
                grpc: None,
            },
            plugins: Default::default(),
            rate_limit: None,
        }
    }

    #[test]
    fn alias_and_suffix_split() {
        assert_eq!(
            split_alias_and_suffix("/example.com/a/b"),
            ("example.com".to_owned(), "/a/b".to_owned())
        );
        assert_eq!(
            split_alias_and_suffix("/example.com"),
            ("example.com".to_owned(), String::new())
        );
    }

    #[test]
    fn longest_prefix_wins() {
        let routes = vec![
            route("/", &["GET"], PathSuffixMode::Append, true),
            route("/v1", &["GET"], PathSuffixMode::Append, true),
            route("/v1/users", &["GET"], PathSuffixMode::Append, true),
        ];
        let m = match_route(&routes, "GET", "/v1/users/7").unwrap();
        assert_eq!(m.route.match_.http.unwrap().path, "/v1/users");
        assert_eq!(m.target_path, "/v1/users/7");
    }

    #[test]
    fn a_prefix_must_end_on_a_segment_boundary() {
        let routes = vec![route("/v1", &["GET"], PathSuffixMode::Append, true)];
        assert!(match_route(&routes, "GET", "/v10/x").is_err());
        assert!(match_route(&routes, "GET", "/v1/x").is_ok());
        assert!(match_route(&routes, "GET", "/v1").is_ok());
    }

    #[test]
    fn method_must_match() {
        let routes = vec![route("/v1", &["GET"], PathSuffixMode::Append, true)];
        assert!(match_route(&routes, "POST", "/v1").is_err());
        assert!(match_route(&routes, "get", "/v1").is_ok());
    }

    #[test]
    fn disabled_routes_never_match() {
        let routes = vec![route("/v1", &["GET"], PathSuffixMode::Append, false)];
        assert!(match_route(&routes, "GET", "/v1").is_err());
    }

    #[test]
    fn suffix_mode_disabled_drops_the_remainder() {
        let routes = vec![route("/v1", &["GET"], PathSuffixMode::Disabled, true)];
        let m = match_route(&routes, "GET", "/v1/users/7").unwrap();
        assert_eq!(m.target_path, "/v1");
    }

    #[test]
    fn an_empty_suffix_matches_the_root_route() {
        let routes = vec![route("/", &["GET"], PathSuffixMode::Append, true)];
        let m = match_route(&routes, "GET", "").unwrap();
        assert_eq!(m.target_path, "/");
    }

    #[test]
    fn query_allowlist_filters_when_non_empty() {
        let allow = vec!["a".to_owned(), "c".to_owned()];
        assert_eq!(
            filter_query(&allow, Some("a=1&b=2&c=3")).as_deref(),
            Some("a=1&c=3")
        );
        assert_eq!(filter_query(&allow, Some("b=2")), None);
        // An empty allowlist forwards everything.
        assert_eq!(
            filter_query(&[], Some("a=1&b=2")).as_deref(),
            Some("a=1&b=2")
        );
        assert_eq!(filter_query(&allow, None), None);
    }

    fn upstream(hosts: &[&str]) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            enabled: true,
            alias: "a".to_owned(),
            tags: vec![],
            server: Server {
                endpoints: hosts
                    .iter()
                    .map(|h| Endpoint {
                        scheme: Scheme::Http,
                        host: (*h).to_owned(),
                        port: Some(80),
                    })
                    .collect(),
            },
            protocol: crate::domain::validate::PROTOCOL_HTTP.to_owned(),
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn a_single_endpoint_pool_needs_no_header() {
        let up = upstream(&["only.example"]);
        let rr = RoundRobin::default();
        assert_eq!(select_endpoint(&up, None, &rr).unwrap().host, "only.example");
    }

    #[test]
    fn a_matching_header_selects_that_endpoint() {
        let up = upstream(&["a.example", "b.example"]);
        let rr = RoundRobin::default();
        assert_eq!(
            select_endpoint(&up, Some("b.example"), &rr).unwrap().host,
            "b.example"
        );
        // Case-insensitively.
        assert_eq!(
            select_endpoint(&up, Some("B.Example"), &rr).unwrap().host,
            "b.example"
        );
    }

    #[test]
    fn a_malformed_header_is_a_validation_error_naming_valid_hosts() {
        let up = upstream(&["a.example", "b.example"]);
        let rr = RoundRobin::default();
        let err = select_endpoint(&up, Some("bad host"), &rr).unwrap_err();
        match err {
            DomainError::Validation { field, message } => {
                assert_eq!(field, crate::domain::error::TARGET_HOST_HEADER);
                assert!(message.contains("invalid target host"));
                assert!(message.contains("a.example"));
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_header_value_is_distinct_from_a_malformed_one() {
        let up = upstream(&["a.example", "b.example"]);
        let rr = RoundRobin::default();
        let err = select_endpoint(&up, Some("c.example"), &rr).unwrap_err();
        match err {
            DomainError::Validation { message, .. } => {
                assert!(message.contains("unknown target host"));
                assert!(message.contains("b.example"));
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn a_multi_endpoint_pool_rotates_without_a_header() {
        let up = upstream(&["a.example", "b.example"]);
        let rr = RoundRobin::default();
        let first = select_endpoint(&up, None, &rr).unwrap().host.clone();
        let second = select_endpoint(&up, None, &rr).unwrap().host.clone();
        assert_ne!(first, second);
    }
}
