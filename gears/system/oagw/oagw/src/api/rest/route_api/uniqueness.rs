//! Route Match-Rule Uniqueness Check
//! (`cpt-cf-oagw-algo-route-uniqueness-check`).
//!
//! Restricts only pairs of **enabled** HTTP routes under the same upstream:
//! a disabled candidate, or either side's `grpc` match, is exempt (no
//! documented uniqueness invariant covers gRPC routes).

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::model::route::{HttpMatch, Route};

/// `true` when persisting `candidate` (under `upstream_id`, owned by
/// `tenant_id`, excluding `exclude_route_id` on replace) would collide with
/// another enabled route's `(path, priority)` for an overlapping method.
// @cpt-algo:cpt-cf-oagw-algo-route-uniqueness-check:p2
// @cpt-dod:cpt-cf-oagw-dod-route-match-uniqueness:p1
#[must_use]
pub fn collides(
    routes: &DashMap<Uuid, Arc<Route>>,
    tenant_id: Uuid,
    upstream_id: Uuid,
    exclude_route_id: Option<Uuid>,
    candidate_enabled: bool,
    candidate_http: Option<&HttpMatch>,
    candidate_priority: Option<i64>,
) -> bool {
    // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-disabled-if
    // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-disabled-return
    if !candidate_enabled {
        return false;
    }
    // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-disabled-return
    // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-disabled-if

    // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-grpc-if
    // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-grpc-return
    let Some(candidate_http) = candidate_http else {
        return false;
    };
    // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-grpc-return
    // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-grpc-if

    // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-http-else
    // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-query
    for entry in routes.iter() {
        let other = entry.value();
        if other.tenant_id != tenant_id || other.upstream_id != upstream_id {
            continue;
        }
        if Some(other.id.unwrap_or_default()) == exclude_route_id {
            continue;
        }
        // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-query
        // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-foreach-route
        if !other.enabled {
            continue;
        }
        let Some(other_http) = other.route_match.http.as_ref() else {
            continue;
        };
        // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-collision-if
        if other_http.path == candidate_http.path && other.priority == candidate_priority {
            // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-foreach-method
            let overlap = other_http
                .methods
                .iter()
                .any(|m| candidate_http.methods.contains(m));
            // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-foreach-method
            if overlap {
                // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-collision-return
                return true;
                // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-collision-return
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-collision-if
        // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-foreach-route
    }
    // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-http-else

    // @cpt-begin:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-pass-return
    false
    // @cpt-end:cpt-cf-oagw-algo-route-uniqueness-check:p1:inst-uniqueness-pass-return
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::route::{HttpMethod, PathSuffixMode, RouteMatch};

    fn seed_route(
        routes: &DashMap<Uuid, Arc<Route>>,
        tenant_id: Uuid,
        upstream_id: Uuid,
        path: &str,
        priority: i64,
        methods: Vec<HttpMethod>,
        enabled: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let route = Route {
            id: Some(id),
            tenant_id,
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
            enabled,
            priority: Some(priority),
        };
        routes.insert(id, Arc::new(route));
        id
    }

    fn http(path: &str, methods: Vec<HttpMethod>) -> HttpMatch {
        HttpMatch {
            methods,
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }

    #[test]
    fn detects_a_collision_on_same_path_priority_and_overlapping_method() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            true,
        );

        let candidate = http("/v1/models", vec![HttpMethod::Get, HttpMethod::Post]);
        assert!(collides(
            &routes,
            tenant_id,
            upstream_id,
            None,
            true,
            Some(&candidate),
            Some(10),
        ));
    }

    #[test]
    fn disjoint_methods_do_not_collide() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            true,
        );

        let candidate = http("/v1/models", vec![HttpMethod::Post]);
        assert!(!collides(
            &routes,
            tenant_id,
            upstream_id,
            None,
            true,
            Some(&candidate),
            Some(10),
        ));
    }

    #[test]
    fn different_priority_does_not_collide() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            true,
        );

        let candidate = http("/v1/models", vec![HttpMethod::Get]);
        assert!(!collides(
            &routes,
            tenant_id,
            upstream_id,
            None,
            true,
            Some(&candidate),
            Some(20),
        ));
    }

    #[test]
    fn a_disabled_candidate_never_collides() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            true,
        );

        let candidate = http("/v1/models", vec![HttpMethod::Get]);
        assert!(!collides(
            &routes,
            tenant_id,
            upstream_id,
            None,
            false,
            Some(&candidate),
            Some(10),
        ));
    }

    #[test]
    fn a_disabled_existing_route_is_not_a_collision_partner() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            false,
        );

        let candidate = http("/v1/models", vec![HttpMethod::Get]);
        assert!(!collides(
            &routes,
            tenant_id,
            upstream_id,
            None,
            true,
            Some(&candidate),
            Some(10),
        ));
    }

    #[test]
    fn a_grpc_candidate_never_collides() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            true,
        );

        assert!(!collides(
            &routes,
            tenant_id,
            upstream_id,
            None,
            true,
            None,
            None,
        ));
    }

    #[test]
    fn excludes_the_route_being_replaced_from_the_collision_scan() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let existing_id = seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            true,
        );

        let candidate = http("/v1/models", vec![HttpMethod::Get]);
        assert!(!collides(
            &routes,
            tenant_id,
            upstream_id,
            Some(existing_id),
            true,
            Some(&candidate),
            Some(10),
        ));
    }

    #[test]
    fn a_different_tenant_or_upstream_is_never_a_collision_partner() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        seed_route(
            &routes,
            tenant_id,
            upstream_id,
            "/v1/models",
            10,
            vec![HttpMethod::Get],
            true,
        );

        let candidate = http("/v1/models", vec![HttpMethod::Get]);
        assert!(!collides(
            &routes,
            Uuid::new_v4(),
            upstream_id,
            None,
            true,
            Some(&candidate),
            Some(10),
        ));
        assert!(!collides(
            &routes,
            tenant_id,
            Uuid::new_v4(),
            None,
            true,
            Some(&candidate),
            Some(10),
        ));
    }
}
