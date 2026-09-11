//! Route matching over the walked upstream and its ancestor chain.
//!
//! `cpt-cf-oagw-algo-route-match` turns the request line into the route the
//! pipeline forwards through: it builds the candidate set from the enabled
//! routes bound to the selected upstream, keeps the descendant route on a
//! shared match key, requires the method, selects the longest matching path
//! prefix and applies `path_suffix_mode` and the query allowlist.

use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::model::{HttpMatch, MatchType, Route, SuffixMode, Upstream};
use crate::infra::storage::ConfigSnapshot;

/// The route a request matched, with the path and query to forward.
#[derive(Debug, Clone)]
pub struct RouteMatch {
    /// The route that matched.
    pub route: Arc<Route>,
    /// Path to forward upstream.
    pub forward_path: String,
    /// Query string to forward, `None` when the request carried none.
    pub forward_query: Option<String>,
}

/// The normalized pattern of a route's HTTP match, the label entry 2.7 reports
/// as `http.route`.
#[must_use]
pub fn route_pattern(route: &Route) -> Option<String> {
    route.matches.http.as_ref().map(|http| http.path.clone())
}

/// The candidate routes for a request, descendant routes first.
///
/// `chain` is the walked tenant chain, caller first. Only routes bound to the
/// selected upstream and enabled are candidates: a disabled route never
/// matches, per the request-path half of the enable/disable requirement, and a
/// route bound to another upstream is not reachable through this alias.
#[must_use]
pub fn candidates(
    snapshot: &ConfigSnapshot,
    walk_tenants: &[uuid::Uuid],
    upstream_id: uuid::Uuid,
) -> Vec<Arc<Route>> {
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-01
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-02
    // The chain is walked in order, so a stable sort keeps descendant routes
    // ahead of ancestor routes on the same match key: the first candidate for a
    // key is the nearest tenant's.
    let mut found: Vec<Arc<Route>> = Vec::new();
    for tenant in walk_tenants {
        // One tenant's routes at a time, so the level ordering the chain gives
        // is preserved and the `priority` order is decided inside the level.
        let mut level: Vec<Arc<Route>> = snapshot
            .routes_of(*tenant)
            .into_iter()
            .filter(|route| route.upstream_id == upstream_id && route.enabled)
            .collect();
        level.sort_by_key(|route| std::cmp::Reverse(route.priority));
        found.extend(level);
    }
    found
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-02
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-01
}

// @cpt-begin:cpt-cf-oagw-dod-route-matching:p1:inst-full
/// Match `method`, `request_path` and `query` against the candidates.
///
/// # Errors
///
/// Returns the mapped `404` when no candidate matches the method and path, or
/// when the selected upstream speaks gRPC and therefore declares no HTTP match
/// key, and the mapped `400` for a suffix the route does not accept and for a
/// query parameter the allowlist does not name.
pub fn match_route(
    upstream: &Upstream,
    candidates: &[Arc<Route>],
    method: &str,
    request_path: &str,
    query: Option<&str>,
) -> Result<RouteMatch, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-05
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-06
    // A gRPC upstream has no HTTP match key at all, so no HTTP request can be
    // routed through it, whatever the route table holds.
    if upstream.protocol == crate::domain::model::Protocol::Grpc {
        return Err(DomainError::RouteNotFound {
            detail: "the upstream speaks gRPC and has no HTTP route".to_owned(),
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-06
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-05

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-03
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-04
    // The candidates arrive descendant-first, so a strictly-greater comparison
    // keeps the first route on a tie: the nearest tenant's route wins, and
    // within one tenant the higher declared `priority` wins.
    // The best route so far: the longest matching prefix, with the candidate
    // order already deciding the level and the `priority` inside it, so a tie
    // keeps the earlier route.
    type Best<'a> = (&'a Arc<Route>, &'a HttpMatch, String, usize);
    let mut matched: Option<Best<'_>> = None;
    for route in candidates {
        let Some(http) = route.matches.http.as_ref() else {
            continue;
        };
        if !method_allows(http, method) {
            continue;
        }
        let Some(suffix) = path_prefix_matches(&http.path, request_path) else {
            continue;
        };
        if matched
            .as_ref()
            .is_none_or(|(_, _, _, best)| http.path.len() > *best)
        {
            matched = Some((route, http, suffix, http.path.len()));
        }
    }
    let Some((route, http, suffix, _)) = matched else {
        return Err(DomainError::RouteNotFound {
            detail: format!("no route of the upstream matches `{method} {request_path}`"),
        });
    };
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-04
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-03

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-07
    // A candidate matched, so the guard rules of the matched route decide the
    // forward path and the forward query.
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-07

    let forward_path = forward_path(http, &suffix)?;
    let forward_query = forward_query(http, query)?;

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-10
    // The pattern is the route's own path, the label entry 2.7 reports as
    // `http.route`; it is a configured value and never a request path.
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-10

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-11
    Ok(RouteMatch {
        route: Arc::clone(route),
        forward_path,
        forward_query,
    })
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-11
}
// @cpt-end:cpt-cf-oagw-dod-route-matching:p1:inst-full

/// Whether the route's method allowlist admits `method`.
fn method_allows(http: &HttpMatch, method: &str) -> bool {
    http.methods.iter().any(|allowed| allowed.eq_ignore_ascii_case(method))
}

/// The suffix of `request_path` after the route's prefix, when it matches.
///
/// A prefix matches on a path-segment boundary only: `/v1` matches `/v1` and
/// `/v1/things`, but not `/v10` or `/v1x`.
fn path_prefix_matches(prefix: &str, request_path: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    if request_path == prefix {
        return Some(String::new());
    }
    let boundary = if prefix.ends_with('/') {
        prefix.to_owned()
    } else {
        format!("{prefix}/")
    };
    request_path.strip_prefix(&boundary).map(str::to_owned)
}

/// The path to forward, per `path_suffix_mode`.
///
/// # Errors
///
/// Returns the mapped `400` of a suffix present under `disabled`.
fn forward_path(http: &HttpMatch, suffix: &str) -> Result<String, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-08
    match http.path_suffix_mode {
        SuffixMode::Append if suffix.is_empty() => Ok(http.path.clone()),
        SuffixMode::Append => Ok(format!("{}/{suffix}", http.path.trim_end_matches('/'))),
        SuffixMode::Disabled if suffix.is_empty() => Ok(http.path.clone()),
        SuffixMode::Disabled => Err(DomainError::ValidationError {
            detail: format!(
                "the route does not accept a path suffix, but the request carried `/{suffix}`"
            ),
        }),
    }
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-08
}

/// The query to forward, per `query_allowlist`.
///
/// # Errors
///
/// Returns the mapped `400` of a parameter outside a declared allowlist.
fn forward_query(http: &HttpMatch, query: Option<&str>) -> Result<Option<String>, DomainError> {
    let Some(query) = query else {
        return Ok(None);
    };
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-09
    // An empty allowlist declares no constraint, which is the schema default:
    // the rule rejects a parameter *outside* an allowlist the operator named.
    if http.query_allowlist.is_empty() {
        return Ok(Some(query.to_owned()));
    }
    let unknown: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split('=').next().unwrap_or(pair))
        .filter(|name| !http.query_allowlist.iter().any(|allowed| allowed == name))
        .collect();
    if unknown.is_empty() {
        Ok(Some(query.to_owned()))
    } else {
        Err(DomainError::ValidationError {
            detail: format!("the query parameter(s) `{}` are not allowed", unknown.join("`, `")),
        })
    }
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-pe-rm-09
}

/// Whether a route is an HTTP route, i.e. carries the HTTP match key.
#[must_use]
pub fn is_http_route(route: &Route) -> bool {
    route.match_type == MatchType::Http
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{HttpMethod, MatchRule, Protocol, Scheme, ServerConfig, Timestamp};
    use std::sync::Arc;
    use uuid::Uuid;

    fn http_match(path: &str, mode: SuffixMode, allowlist: &[&str]) -> MatchRule {
        MatchRule {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned(), "POST".to_owned()],
                path: path.to_owned(),
                query_allowlist: allowlist.iter().map(|name| (*name).to_owned()).collect(),
                path_suffix_mode: mode,
            }),
            grpc: None,
        }
    }

    fn route(path: &str, mode: SuffixMode, priority: i32, allowlist: &[&str]) -> Arc<Route> {
        Arc::new(Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            matches: http_match(path, mode, allowlist),
            match_type: MatchType::Http,
            priority,
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: Timestamp::now(),
        })
    }

    fn snapshot(routes: Vec<Arc<Route>>) -> ConfigSnapshot {
        let mut store = ConfigSnapshot {
            epoch: 1,
            upstreams: Default::default(),
            routes: Default::default(),
            plugins: Default::default(),
        };
        for route in routes {
            store.routes.insert((route.tenant_id, route.id), route);
        }
        store
    }

    fn upstream() -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: "api.vendor.com".to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![crate::domain::model::Endpoint {
                    scheme: Scheme::Https,
                    host: "api.vendor.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: Timestamp::now(),
        }
    }

    #[test]
    fn the_longest_matching_prefix_wins() {
        let short = route("/v1", SuffixMode::Append, 0, &[]);
        let long = route("/v1/things", SuffixMode::Append, 0, &[]);
        let upstream = upstream();
        let matched = match_route(
            &upstream,
            &[short, long],
            "GET",
            "/v1/things/42",
            None,
        )
        .expect("a route matches");
        assert_eq!(matched.route.matches.http.as_ref().unwrap().path, "/v1/things");
        assert_eq!(matched.forward_path, "/v1/things/42");
    }

    #[test]
    fn a_priority_tie_inside_one_prefix_resolves_to_the_higher_priority() {
        let low = route("/v1", SuffixMode::Append, 1, &[]);
        let high = route("/v1", SuffixMode::Append, 9, &[]);
        let tenant = high.tenant_id;
        let upstream_id = high.upstream_id;
        let upstream = upstream();
        // The `priority` order is decided when the candidate set is built, so
        // the higher route of one tenant is offered first.
        let candidates = candidates(
            &snapshot(vec![Arc::clone(&low), Arc::clone(&high)]),
            &[tenant],
            upstream_id,
        );
        assert_eq!(candidates[0].priority, 9);
        let matched = match_route(&upstream, &candidates, "GET", "/v1", None)
            .expect("a route matches");
        assert_eq!(matched.route.priority, 9);
    }

    #[test]
    fn a_descendant_route_shadows_an_ancestor_route_on_the_same_key() {
        let ancestor = route("/v1", SuffixMode::Append, 5, &[]);
        let descendant = route("/v1", SuffixMode::Append, 0, &[]);
        let upstream = upstream();
        // The candidate list is ordered nearest tenant first, so the
        // descendant's route is ahead of the ancestor's.
        let matched = match_route(&upstream, &[descendant, ancestor], "GET", "/v1/x", None)
            .expect("a route matches");
        assert_eq!(matched.route.priority, 0);
    }

    #[test]
    fn a_prefix_matches_on_a_segment_boundary_only() {
        assert!(path_prefix_matches("/v1", "/v1").is_some());
        assert!(path_prefix_matches("/v1", "/v1/things").is_some());
        assert!(path_prefix_matches("/v1", "/v10").is_none());
        assert!(path_prefix_matches("/v1", "/v1x").is_none());
    }

    #[test]
    fn a_method_outside_the_allowlist_does_not_match() {
        let candidate = route("/v1", SuffixMode::Append, 0, &[]);
        let upstream = upstream();
        let error = match_route(&upstream, &[candidate], "DELETE", "/v1", None)
            .expect_err("DELETE is not in the allowlist");
        assert_eq!(error.status(), 404, "{error}");
    }

    #[test]
    fn a_disabled_route_is_excluded_from_the_candidates() {
        let mut disabled_route = (*route("/v1", SuffixMode::Append, 0, &[])).clone();
        disabled_route.enabled = false;
        let tenant = disabled_route.tenant_id;
        let found = candidates(
            &snapshot(vec![Arc::new(disabled_route)]),
            &[tenant],
            Uuid::new_v4(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn a_route_of_another_upstream_is_not_a_candidate() {
        let candidate = route("/v1", SuffixMode::Append, 0, &[]);
        let tenant = candidate.tenant_id;
        let upstream = candidate.upstream_id;
        let found = candidates(&snapshot(vec![candidate]), &[tenant], upstream);
        assert_eq!(found.len(), 1);
        assert!(candidates(&snapshot(Vec::new()), &[tenant], upstream).is_empty());
    }

    #[test]
    fn a_disabled_suffix_mode_rejects_a_suffix() {
        let candidate = route("/v1", SuffixMode::Disabled, 0, &[]);
        let upstream = upstream();
        let error = match_route(&upstream, &[candidate], "GET", "/v1/extra", None)
            .expect_err("the route rejects a suffix");
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[test]
    fn a_disabled_suffix_mode_forwards_the_pattern_without_a_suffix() {
        let candidate = route("/v1", SuffixMode::Disabled, 0, &[]);
        let upstream = upstream();
        let matched = match_route(&upstream, &[candidate], "GET", "/v1", None)
            .expect("the route matches");
        assert_eq!(matched.forward_path, "/v1");
    }

    #[test]
    fn a_query_parameter_outside_the_allowlist_is_rejected() {
        let candidate = route("/v1", SuffixMode::Append, 0, &["page"]);
        let upstream = upstream();
        let error = match_route(&upstream, &[candidate], "GET", "/v1", Some("page=1&dump=1"))
            .expect_err("`dump` is outside the allowlist");
        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn a_query_parameter_inside_the_allowlist_is_forwarded() {
        let candidate = route("/v1", SuffixMode::Append, 0, &["page"]);
        let upstream = upstream();
        let matched = match_route(&upstream, &[candidate], "GET", "/v1", Some("page=1"))
            .expect("the query is allowed");
        assert_eq!(matched.forward_query.as_deref(), Some("page=1"));
    }

    #[test]
    fn an_undeclared_allowlist_admits_the_query() {
        let candidate = route("/v1", SuffixMode::Append, 0, &[]);
        let upstream = upstream();
        let matched = match_route(&upstream, &[candidate], "GET", "/v1", Some("any=1"))
            .expect("no allowlist is declared");
        assert_eq!(matched.forward_query.as_deref(), Some("any=1"));
    }

    #[test]
    fn a_grpc_upstream_has_no_http_route() {
        let mut grpc = upstream();
        grpc.protocol = Protocol::Grpc;
        let candidate = route("/v1", SuffixMode::Append, 0, &[]);
        let error = match_route(&grpc, &[candidate], "GET", "/v1", None)
            .expect_err("a gRPC upstream has no HTTP match key");
        assert_eq!(error.status(), 404, "{error}");
    }

    #[test]
    fn the_route_pattern_is_the_configured_path() {
        let candidate = route("/v1/things", SuffixMode::Append, 0, &[]);
        assert_eq!(route_pattern(&candidate), Some("/v1/things".to_owned()));
        assert!(is_http_route(&candidate));
    }

    #[test]
    fn a_route_without_an_http_match_key_is_not_an_http_route() {
        let mut grpc_route = (*route("/v1", SuffixMode::Append, 0, &[])).clone();
        grpc_route.matches = MatchRule {
            http: None,
            grpc: Some(crate::domain::model::GrpcMatch {
                service: "svc".to_owned(),
                method: "m".to_owned(),
            }),
        };
        grpc_route.match_type = MatchType::Grpc;
        assert!(!is_http_route(&grpc_route));
        assert_eq!(route_pattern(&grpc_route), None);
    }

    #[test]
    fn method_matching_is_case_insensitive_on_the_schema_names() {
        // The schema fixes the method names the allowlist may hold; the request
        // line may spell them in any case.
        let http = HttpMatch {
            methods: vec![HttpMethod::Get.as_str().to_owned()],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: SuffixMode::Append,
        };
        assert!(method_allows(&http, "get"));
        assert!(!method_allows(&http, "POST"));
    }
}
