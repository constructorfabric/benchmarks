//! Tests for the caller's tenant chain (TR-09, FR-020).

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use tenant_resolver_sdk::models::{GetAncestorsOptions, GetAncestorsResponse, TenantRef, TenantStatus};
use tenant_resolver_sdk::{
    GetDescendantsOptions, GetDescendantsResponse, GetTenantsOptions, IsAncestorOptions,
    TenantId, TenantInfo, TenantResolverClient, TenantResolverError,
};

use crate::proxy::resolve_chain;

/// A security context whose subject is `tenant`, which is all the chain builder reads.
fn ctx(tenant: uuid::Uuid) -> SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|_| toolkit_security::SecurityContext::anonymous())
}

/// A stub resolver answering `get_ancestors` from a canned result.
struct StubResolver {
    answer: Result<Vec<TenantId>, TenantResolverError>,
}

/// A bare reference to a tenant, as the ancestors response carries them.
fn reference(id: TenantId) -> TenantRef {
    TenantRef {
        id,
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

#[async_trait]
impl TenantResolverClient for StubResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::NoPluginAvailable)
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::NoPluginAvailable)
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Err(TenantResolverError::NoPluginAvailable)
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        match &self.answer {
            Ok(ancestors) => Ok(GetAncestorsResponse {
                tenant: reference(TenantId(uuid::Uuid::nil())),
                ancestors: ancestors.iter().map(|id| reference(*id)).collect(),
            }),
            Err(TenantResolverError::TenantNotFound { tenant_id }) => {
                Err(TenantResolverError::TenantNotFound { tenant_id: *tenant_id })
            }
            Err(TenantResolverError::ServiceUnavailable(detail)) => {
                Err(TenantResolverError::ServiceUnavailable(detail.clone()))
            }
            Err(TenantResolverError::Internal(detail)) => {
                Err(TenantResolverError::Internal(detail.clone()))
            }
            Err(err) => Err(match err {
                TenantResolverError::Unauthorized => TenantResolverError::Unauthorized,
                TenantResolverError::NoPluginAvailable => TenantResolverError::NoPluginAvailable,
                other => TenantResolverError::Internal(other.to_string()),
            }),
        }
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Err(TenantResolverError::NoPluginAvailable)
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor_id: TenantId,
        _descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Err(TenantResolverError::NoPluginAvailable)
    }
}

/// A caller whose tenant the resolver has never heard of is still served: the resolver
/// has answered authoritatively that the tenant has no ancestors, so the chain is the
/// caller's own tenant alone and they see nothing of anyone else's (TR-09) — not a
/// `500` that reports a platform fault for an ordinary tenant.
#[tokio::test]
async fn a_tenant_the_resolver_does_not_know_is_its_own_chain() {
    let own = uuid::Uuid::new_v4();
    let resolver = StubResolver {
        answer: Err(TenantResolverError::TenantNotFound { tenant_id: TenantId(own) }),
    };
    let chain = resolve_chain(Some(&resolver), &ctx(own))
        .await
        .expect("an unknown tenant is an answer, not a failure");
    assert_eq!(chain.iter().collect::<Vec<_>>(), vec![own]);
    assert!(chain.own().is_some_and(|tenant| tenant == own));
}

/// A caller with ancestors gets them, closest first.
#[tokio::test]
async fn a_tenant_with_ancestors_resolves_the_whole_chain() {
    let own = uuid::Uuid::new_v4();
    let parent = uuid::Uuid::new_v4();
    let root = uuid::Uuid::new_v4();
    let resolver = StubResolver { answer: Ok(vec![TenantId(parent), TenantId(root)]) };
    let chain = resolve_chain(Some(&resolver), &ctx(own))
        .await
        .expect("the resolver answered");
    assert_eq!(chain.iter().collect::<Vec<_>>(), vec![own, parent, root]);
}

/// A root tenant's chain is itself: the resolver answers with an empty parent chain.
#[tokio::test]
async fn a_root_tenant_has_no_ancestors_to_walk() {
    let own = uuid::Uuid::new_v4();
    let resolver = StubResolver { answer: Ok(Vec::new()) };
    let chain = resolve_chain(Some(&resolver), &ctx(own))
        .await
        .expect("the resolver answered");
    assert_eq!(chain.iter().collect::<Vec<_>>(), vec![own]);
}

/// Every other resolver answer is an outage, and the request fails rather than degrading
/// to a chain that would drop the enforced ancestor constraints (FR-020).
#[tokio::test]
async fn a_resolver_outage_fails_the_request_instead_of_shrinking_the_chain() {
    let own = uuid::Uuid::new_v4();
    for outage in [
        TenantResolverError::ServiceUnavailable("resolver is down".to_owned()),
        TenantResolverError::Internal("boom".to_owned()),
        TenantResolverError::NoPluginAvailable,
        TenantResolverError::Unauthorized,
    ] {
        let resolver = StubResolver { answer: Err(outage) };
        let chain = resolve_chain(Some(&resolver), &ctx(own)).await;
        assert!(
            chain.is_err(),
            "a resolver outage must not be read as `no ancestors`"
        );
    }
}

/// Without a resolver wired the caller is the whole chain, which is the correct behaviour
/// for a deployment that has no hierarchy to ask about.
#[tokio::test]
async fn without_a_resolver_the_caller_is_its_own_chain() {
    let own = uuid::Uuid::new_v4();
    let chain = resolve_chain(None, &ctx(own))
        .await
        .expect("nothing to resolve");
    assert_eq!(chain.iter().collect::<Vec<_>>(), vec![own]);
}

/// Route matching: a method the routes do not take is a different answer from a path they
/// do not name (US2/AC9 vs US2/AC3).
mod matching {
    use super::*;
    use crate::domain::route::{HttpMatch, PathSuffixMode, Route, RouteMatch};
    use crate::domain::upstream::{Endpoint, Server, Upstream};
    use crate::security::SecurityContextHolder;
    use crate::store::{OagwStore, TenantChain};
    use axum::http::HeaderMap;
    use bytes::Bytes;

    /// A proxy request for `method` and `path`, with the fields the matcher ignores blank.
    fn request(method: &str, path: &str) -> crate::proxy::ProxyRequest {
        let own = uuid::Uuid::new_v4();
        crate::proxy::ProxyRequest {
            method: method.parse().expect("a valid method"),
            alias: "stub.local".to_owned(),
            path: path.to_owned(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            client_ip: "127.0.0.1".to_owned(),
            security: SecurityContextHolder::new(ctx(own), vec![own]),
            upgrade: None,
        }
    }

    /// An upstream whose endpoints make its alias explicit rather than a common suffix.
    fn upstream() -> Upstream {
        Upstream {
            alias: "stub.local".to_owned(),
            server: Server {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: "api.example.com".to_owned(),
                    port: 443,
                }],
                ..Server::default()
            },
            ..Upstream::default()
        }
    }

    /// An HTTP route matching `path` for `methods`.
    fn route(path: &str, methods: &[&str], priority: i64) -> Route {
        Route {
            r#match: Some(RouteMatch::Http(HttpMatch {
                methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            })),
            priority,
            enabled: true,
            ..Route::default()
        }
    }

    /// A store holding one upstream owned by `tenant`, with the routes given.
    fn store(routes: &[Route]) -> (Upstream, OagwStore, TenantChain) {
        let store = OagwStore::new();
        let tenant = uuid::Uuid::new_v4();
        let upstream = store
            .insert_upstream(upstream(), tenant)
            .expect("the upstream is created");
        let chain = TenantChain::single(tenant);
        for route in routes {
            let mut bound = route.clone();
            bound.upstream_id.clone_from(&upstream.id);
            store.insert_route(bound, &chain).expect("the route is created");
        }
        (upstream, store, chain)
    }

    #[test]
    fn a_method_the_route_does_not_take_is_a_validation_error() {
        let (upstream, store, chain) = store(&[route("/v1/echo", &["POST"], 1)]);
        let matched = crate::proxy::match_route(&store, &upstream, &chain, &request("GET", "/v1/echo"));
        let err = matched.expect_err("GET is not one of the route's methods");
        assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError, "{err}");
    }

    #[test]
    fn a_method_the_route_takes_is_matched() {
        let (upstream, store, chain) = store(&[route("/v1/echo", &["POST"], 1)]);
        let matched = crate::proxy::match_route(&store, &upstream, &chain, &request("POST", "/v1/echo"));
        let route = matched.expect("POST is one of the route's methods");
        assert_eq!(route.http_match().map(|m| m.path.as_str()), Some("/v1/echo"));
    }

    #[test]
    fn a_path_no_route_names_is_not_found() {
        let (upstream, store, chain) = store(&[route("/v1/echo", &["POST"], 1)]);
        let matched = crate::proxy::match_route(&store, &upstream, &chain, &request("POST", "/v1/other"));
        let err = matched.expect_err("no route names that path");
        assert_eq!(err.kind(), crate::error::ErrorKind::RouteNotFound, "{err}");
    }

    /// Two routes on one path for different methods: the method still decides, so a
    /// caller using a verb the path knows is routed rather than refused.
    #[test]
    fn a_path_served_by_two_methods_still_routes_the_matching_one() {
        let (upstream, store, chain) =
            store(&[route("/v1/echo", &["POST"], 1), route("/v1/echo", &["GET"], 2)]);
        for (method, priority) in [("GET", 2), ("POST", 1)] {
            let matched =
                crate::proxy::match_route(&store, &upstream, &chain, &request(method, "/v1/echo"));
            let route = matched.expect("each declared method is routed");
            assert_eq!(route.priority, priority, "{method} picks its own route");
        }
    }

    /// A path the routes know under a method they all refuse is still a validation error,
    /// whatever other methods the same path serves.
    #[test]
    fn a_path_served_by_two_methods_still_rejects_a_third() {
        let (upstream, store, chain) =
            store(&[route("/v1/echo", &["POST"], 1), route("/v1/echo", &["GET"], 2)]);
        let matched = crate::proxy::match_route(&store, &upstream, &chain, &request("DELETE", "/v1/echo"));
        let err = matched.expect_err("DELETE is not served on that path");
        assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError, "{err}");
    }
}

/// FR-020: the effective limit is the strictest of the upstream's and the route's.
mod rate_limits {
    use crate::domain::route::Route;
    use crate::domain::upstream::{RateLimit, RateWindow, SustainedRate, Upstream};
    use crate::proxy::effective_rate_limit;

    /// A rate limit of `rate` requests per `window`.
    fn limit(rate: u64, window: RateWindow) -> RateLimit {
        RateLimit {
            sustained: SustainedRate { rate, window },
            ..RateLimit::default()
        }
    }

    /// An upstream carrying `limit`.
    fn upstream(limit: Option<RateLimit>) -> Upstream {
        Upstream { rate_limit: limit, ..Upstream::default() }
    }

    /// A route carrying `limit`.
    fn route(limit: Option<RateLimit>) -> Route {
        Route { rate_limit: limit, ..Route::default() }
    }

    #[test]
    fn no_limit_on_either_side_means_no_limit() {
        assert_eq!(effective_rate_limit(&upstream(None), &route(None)), None);
    }

    #[test]
    fn a_limit_on_one_side_applies() {
        let only = limit(5, RateWindow::Second);
        assert_eq!(effective_rate_limit(&upstream(Some(only.clone())), &route(None)), Some(only.clone()));
        assert_eq!(effective_rate_limit(&upstream(None), &route(Some(only.clone()))), Some(only));
    }

    #[test]
    fn the_stricter_of_the_two_wins() {
        let relaxed = limit(100, RateWindow::Second);
        let strict = limit(1, RateWindow::Minute);
        let effective = effective_rate_limit(&upstream(Some(relaxed)), &route(Some(strict)))
            .expect("a limit is configured");
        assert_eq!(effective.sustained.rate, 1, "one per minute is stricter than a hundred a second");
        assert_eq!(effective.sustained.window, RateWindow::Minute);
    }

    #[test]
    fn equal_limits_leave_the_upstreams_in_place() {
        let both = limit(3, RateWindow::Minute);
        let effective = effective_rate_limit(&upstream(Some(both.clone())), &route(Some(both)))
            .expect("a limit is configured");
        assert_eq!(effective.sustained.rate, 3);
    }
}
