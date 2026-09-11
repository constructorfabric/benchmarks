//! Match the Route Within the Resolved Upstream
//! (`cpt-cf-oagw-algo-proxy-match-route`).

use std::sync::Arc;

use axum::http::Method;

use crate::model::route::{HttpMethod, Route};
use crate::proxy::constants::HTTP_PROTOCOL_ID;
use crate::proxy::resolve::AncestorLevel;
use crate::store::OagwState;

/// The matched route, plus the matched `match.http.path` prefix length
/// (consumed by the outbound-path computation in guard-rule application).
#[derive(Debug, Clone)]
pub(crate) struct RouteResolution {
    pub route: Arc<Route>,
    /// The winning candidate's tenant distance, kept for tests and for a
    /// future observability label; the merge algorithm re-derives distance
    /// from the ancestor chain itself rather than reading this field.
    #[allow(dead_code)]
    pub distance: u32,
    pub matched_prefix_len: usize,
}

fn method_matches(candidate: &Route, method: &Method) -> bool {
    let Some(http) = &candidate.route_match.http else {
        return false;
    };
    let candidate_method = match *method {
        Method::GET => HttpMethod::Get,
        Method::POST => HttpMethod::Post,
        Method::PUT => HttpMethod::Put,
        Method::DELETE => HttpMethod::Delete,
        Method::PATCH => HttpMethod::Patch,
        _ => return false,
    };
    http.methods.contains(&candidate_method)
}

/// Segment-boundary prefix check: `prefix` must equal `path_expr` exactly,
/// or be followed immediately by `/` in `path_expr` (`inst-proxy-route-prefix-key`).
fn is_segment_prefix(prefix: &str, path_expr: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    if path_expr == prefix {
        return true;
    }
    path_expr
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Normalize the inbound path suffix to a leading-slash path expression
/// (`inst-proxy-route-normalize-path`).
pub(crate) fn normalize_path_expr(path_suffix: &str) -> String {
    if path_suffix.is_empty() {
        "/".to_owned()
    } else {
        format!("/{path_suffix}")
    }
}

/// `cpt-cf-oagw-algo-proxy-match-route`: collect candidate routes owned by
/// the selected upstream and its same-alias ancestor upstreams, exclude
/// disabled ones, filter by method/prefix match keys, and order by longest
/// prefix, then smallest tenant distance, then highest `priority`
/// (`inst-proxy-route-priority-direction`'s fixed descending-priority
/// reading).
// @cpt-algo:cpt-cf-oagw-algo-proxy-match-route:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-route-matching:p1
// @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-if-not-http
// @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-return-not-http
pub(crate) fn match_route(
    state: &OagwState,
    chain: &[AncestorLevel],
    method: &Method,
    path_suffix: &str,
) -> Option<RouteResolution> {
    let selected = chain.last()?;
    if selected.upstream.protocol != HTTP_PROTOCOL_ID {
        return None;
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-return-not-http
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-if-not-http

    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-collect
    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-exclude-disabled
    let mut candidates: Vec<(Arc<Route>, u32)> = Vec::new();
    for level in chain {
        for entry in state.store.routes().iter() {
            let route = entry.value();
            if route.upstream_id == level.upstream.id.unwrap_or_default() && route.enabled {
                candidates.push((Arc::clone(route), level.distance));
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-exclude-disabled
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-collect

    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-normalize-path
    let path_expr = normalize_path_expr(path_suffix);
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-normalize-path

    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-foreach
    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-method-key
    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-prefix-key
    let mut survivors: Vec<(Arc<Route>, u32, usize)> = candidates
        .into_iter()
        .filter_map(|(route, distance)| {
            let http = route.route_match.http.as_ref()?;
            if !method_matches(&route, method) {
                return None;
            }
            if !is_segment_prefix(&http.path, &path_expr) {
                return None;
            }
            let prefix_len = http.path.len();
            Some((route, distance, prefix_len))
        })
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-prefix-key
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-method-key
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-foreach

    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-if-none
    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-return-none
    if survivors.is_empty() {
        return None;
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-return-none
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-if-none

    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-order
    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-priority-direction
    survivors.sort_by(|a, b| {
        b.2.cmp(&a.2) // longest prefix first
            .then(a.1.cmp(&b.1)) // smallest tenant distance first
            .then(
                b.0.priority.unwrap_or(0).cmp(&a.0.priority.unwrap_or(0)), // highest priority first
            )
    });
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-priority-direction
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-order

    // @cpt-begin:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-return
    let (route, distance, matched_prefix_len) = survivors.into_iter().next()?;
    Some(RouteResolution {
        route,
        distance,
        matched_prefix_len,
    })
    // @cpt-end:cpt-cf-oagw-algo-proxy-match-route:p2:inst-proxy-route-return
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::model::route::{HttpMatch, PathSuffixMode, RouteMatch};
    use crate::model::upstream::{Endpoint, EndpointScheme, ServerConfig, Upstream};

    fn ancestor(tenant_id: uuid::Uuid, distance: u32) -> AncestorLevel {
        AncestorLevel {
            tenant_id,
            distance,
            upstream: Arc::new(Upstream {
                id: Some(uuid::Uuid::new_v4()),
                enabled: true,
                alias: Some("svc".to_owned()),
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme: EndpointScheme::Https,
                        host: "example.com".to_owned(),
                        port: 443,
                    }],
                },
                protocol: HTTP_PROTOCOL_ID.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
                tenant_id,
            }),
        }
    }

    fn route(
        upstream_id: uuid::Uuid,
        path: &str,
        priority: i64,
        methods: Vec<HttpMethod>,
    ) -> Route {
        Route {
            id: Some(uuid::Uuid::new_v4()),
            tenant_id: uuid::Uuid::new_v4(),
            tags: Vec::new(),
            upstream_id,
            route_match: RouteMatch {
                http: Some(HttpMatch {
                    methods,
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled: true,
            priority: Some(priority),
        }
    }

    #[test]
    fn longest_prefix_wins_over_shorter_match() {
        let state = OagwState::new(OagwConfig::default());
        let level = ancestor(uuid::Uuid::new_v4(), 0);
        let up_id = level.upstream.id.unwrap();
        let short = route(up_id, "/v1", 1, vec![HttpMethod::Get]);
        let long = route(up_id, "/v1/models", 1, vec![HttpMethod::Get]);
        state
            .store
            .routes()
            .insert(short.id.unwrap(), Arc::new(short));
        state
            .store
            .routes()
            .insert(long.id.unwrap(), Arc::new(long));

        let resolved = match_route(&state, &[level], &Method::GET, "v1/models/x").unwrap();
        assert_eq!(
            resolved.route.route_match.http.as_ref().unwrap().path,
            "/v1/models"
        );
    }

    #[test]
    fn method_not_allowlisted_is_a_non_match() {
        let state = OagwState::new(OagwConfig::default());
        let level = ancestor(uuid::Uuid::new_v4(), 0);
        let up_id = level.upstream.id.unwrap();
        let r = route(up_id, "/v1", 1, vec![HttpMethod::Post]);
        state.store.routes().insert(r.id.unwrap(), Arc::new(r));

        assert!(match_route(&state, &[level], &Method::GET, "v1").is_none());
    }

    #[test]
    fn disabled_route_is_invisible_to_matching() {
        let state = OagwState::new(OagwConfig::default());
        let level = ancestor(uuid::Uuid::new_v4(), 0);
        let up_id = level.upstream.id.unwrap();
        let mut r = route(up_id, "/v1", 1, vec![HttpMethod::Get]);
        r.enabled = false;
        state.store.routes().insert(r.id.unwrap(), Arc::new(r));

        assert!(match_route(&state, &[level], &Method::GET, "v1").is_none());
    }

    #[test]
    fn grpc_protocol_upstream_never_matches() {
        let state = OagwState::new(OagwConfig::default());
        let mut level = ancestor(uuid::Uuid::new_v4(), 0);
        let mut up = (*level.upstream).clone();
        up.protocol = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1".to_owned();
        level.upstream = Arc::new(up);
        let up_id = level.upstream.id.unwrap();
        let r = route(up_id, "/v1", 1, vec![HttpMethod::Get]);
        state.store.routes().insert(r.id.unwrap(), Arc::new(r));

        assert!(match_route(&state, &[level], &Method::GET, "v1").is_none());
    }

    #[test]
    fn descendant_route_outranks_inherited_ancestor_route_at_equal_prefix() {
        let state = OagwState::new(OagwConfig::default());
        let ancestor_level = ancestor(uuid::Uuid::new_v4(), 1);
        let mut child_level = ancestor(uuid::Uuid::new_v4(), 0);
        child_level.upstream = Arc::new(Upstream {
            id: Some(uuid::Uuid::new_v4()),
            ..(*child_level.upstream).clone()
        });

        let ancestor_route = route(
            ancestor_level.upstream.id.unwrap(),
            "/v1",
            1,
            vec![HttpMethod::Get],
        );
        let child_route = route(
            child_level.upstream.id.unwrap(),
            "/v1",
            1,
            vec![HttpMethod::Get],
        );
        state
            .store
            .routes()
            .insert(ancestor_route.id.unwrap(), Arc::new(ancestor_route));
        state
            .store
            .routes()
            .insert(child_route.id.unwrap(), Arc::new(child_route.clone()));

        let resolved =
            match_route(&state, &[ancestor_level, child_level], &Method::GET, "v1").unwrap();
        assert_eq!(resolved.route.upstream_id, child_route.upstream_id);
    }

    #[test]
    fn higher_priority_wins_at_equal_prefix_and_distance() {
        let state = OagwState::new(OagwConfig::default());
        let level = ancestor(uuid::Uuid::new_v4(), 0);
        let up_id = level.upstream.id.unwrap();
        let low = route(up_id, "/v1", 1, vec![HttpMethod::Get]);
        let high = route(up_id, "/v1", 5, vec![HttpMethod::Get]);
        let high_id = high.id.unwrap();
        state.store.routes().insert(low.id.unwrap(), Arc::new(low));
        state.store.routes().insert(high_id, Arc::new(high));

        let resolved = match_route(&state, &[level], &Method::GET, "v1").unwrap();
        assert_eq!(resolved.route.id, Some(high_id));
    }
}
