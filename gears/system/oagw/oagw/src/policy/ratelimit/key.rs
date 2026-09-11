//! Rate-limit counter-key selection (`cpt-cf-oagw-algo-ratelimit-scope-key`,
//! `cpt-cf-oagw-dod-ratelimit-scope-selection`): map the effective
//! `rate_limit.scope` plus the already-resolved request context to exactly
//! one token-bucket key, in the `resource_type`/`resource_id`/`scope`/
//! `scope_id` shape ADR-0003's Redis key structure documents (reused here
//! for this feature's local, non-Redis bucket lookup).

use uuid::Uuid;

use crate::model::upstream::RateLimitScope;

/// Which resource contributed the effective configuration
/// (`inst-ratelimit-scope-key-01`): the Route, when it was the
/// most-specific contributor per `cpt-cf-oagw-algo-ratelimit-effective-limit`,
/// else the Upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ResourceRef {
    Upstream(Uuid),
    Route(Uuid),
}

/// The scope-specific subject folded into the counter key
/// (`inst-ratelimit-scope-key-04` through `inst-ratelimit-scope-key-09`).
/// `global` and `route` carry no subject of their own: `global` shares one
/// bucket per resource, `route` collapses onto the route id alone (carried
/// by [`CounterKey::resource`] directly).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ScopeSubject {
    Global,
    Tenant(Uuid),
    User(Uuid),
    Ip(String),
    Route,
}

/// One token bucket's identity: exactly one bucket exists per distinct
/// `CounterKey` value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CounterKey {
    resource: ResourceRef,
    subject: ScopeSubject,
}

/// The request context `cpt-cf-oagw-feature-proxy-core` has already
/// resolved, needed to select a counter key for any `scope` value.
pub(crate) struct ScopeContext {
    /// The resource that contributed the effective configuration
    /// (`inst-ratelimit-scope-key-01`).
    pub contributing_resource: ResourceRef,
    /// The matched route's own id, used unconditionally by `scope: route`
    /// (`inst-ratelimit-scope-key-10`/`-11`) regardless of which resource
    /// contributed the effective configuration.
    pub route_id: Uuid,
    pub tenant_id: Uuid,
    pub principal_id: Uuid,
    pub client_ip: Option<String>,
}

/// `cpt-cf-oagw-algo-ratelimit-scope-key`: compose the counter key for
/// `scope` against `ctx`.
// @cpt-algo:cpt-cf-oagw-algo-ratelimit-scope-key:p1
// @cpt-dod:cpt-cf-oagw-dod-ratelimit-scope-selection:p1
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-01
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-02
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-03
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-04
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-05
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-06
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-07
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-08
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-09
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-10
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-11
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-12
pub(crate) fn select_counter_key(scope: RateLimitScope, ctx: &ScopeContext) -> CounterKey {
    match scope {
        RateLimitScope::Global => CounterKey {
            resource: ctx.contributing_resource,
            subject: ScopeSubject::Global,
        },
        RateLimitScope::Tenant => CounterKey {
            resource: ctx.contributing_resource,
            subject: ScopeSubject::Tenant(ctx.tenant_id),
        },
        RateLimitScope::User => CounterKey {
            resource: ctx.contributing_resource,
            subject: ScopeSubject::User(ctx.principal_id),
        },
        RateLimitScope::Ip => CounterKey {
            resource: ctx.contributing_resource,
            subject: ScopeSubject::Ip(ctx.client_ip.clone().unwrap_or_default()),
        },
        RateLimitScope::Route => CounterKey {
            resource: ResourceRef::Route(ctx.route_id),
            subject: ScopeSubject::Route,
        },
    }
}
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-12
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-11
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-10
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-09
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-08
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-07
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-06
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-05
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-04
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-03
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-02
// @cpt-end:cpt-cf-oagw-algo-ratelimit-scope-key:p1:inst-ratelimit-scope-key-01

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(contributing_resource: ResourceRef) -> ScopeContext {
        ScopeContext {
            contributing_resource,
            route_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            principal_id: Uuid::new_v4(),
            client_ip: Some("203.0.113.7".to_owned()),
        }
    }

    #[test]
    fn global_scope_shares_one_bucket_per_resource_regardless_of_subject() {
        let resource = ResourceRef::Upstream(Uuid::new_v4());
        let a = ctx(resource);
        let mut b = ctx(resource);
        b.tenant_id = Uuid::new_v4();
        assert_eq!(
            select_counter_key(RateLimitScope::Global, &a),
            select_counter_key(RateLimitScope::Global, &b)
        );
    }

    #[test]
    fn tenant_scope_produces_independent_keys_per_tenant() {
        let resource = ResourceRef::Route(Uuid::new_v4());
        let a = ctx(resource);
        let b = ctx(resource);
        assert_ne!(
            select_counter_key(RateLimitScope::Tenant, &a),
            select_counter_key(RateLimitScope::Tenant, &b)
        );
    }

    #[test]
    fn user_scope_keys_by_principal_id_not_tenant() {
        let resource = ResourceRef::Route(Uuid::new_v4());
        let mut a = ctx(resource);
        let mut b = ctx(resource);
        a.tenant_id = b.tenant_id; // same tenant
        a.principal_id = Uuid::new_v4();
        b.principal_id = Uuid::new_v4();
        assert_ne!(
            select_counter_key(RateLimitScope::User, &a),
            select_counter_key(RateLimitScope::User, &b)
        );
    }

    #[test]
    fn ip_scope_keys_by_client_ip() {
        let resource = ResourceRef::Route(Uuid::new_v4());
        let mut a = ctx(resource);
        let mut b = ctx(resource);
        a.client_ip = Some("198.51.100.1".to_owned());
        b.client_ip = Some("198.51.100.2".to_owned());
        assert_ne!(
            select_counter_key(RateLimitScope::Ip, &a),
            select_counter_key(RateLimitScope::Ip, &b)
        );
    }

    #[test]
    fn route_scope_collapses_onto_the_route_id_regardless_of_contributor() {
        let route_id = Uuid::new_v4();
        let mut upstream_contributed = ctx(ResourceRef::Upstream(Uuid::new_v4()));
        upstream_contributed.route_id = route_id;
        let mut route_contributed = ctx(ResourceRef::Route(Uuid::new_v4()));
        route_contributed.route_id = route_id;
        // Different tenants/users/ips still collapse onto the same bucket.
        route_contributed.tenant_id = Uuid::new_v4();
        assert_eq!(
            select_counter_key(RateLimitScope::Route, &upstream_contributed),
            select_counter_key(RateLimitScope::Route, &route_contributed)
        );
    }
}
