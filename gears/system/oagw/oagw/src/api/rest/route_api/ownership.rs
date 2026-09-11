//! Route Upstream-Ownership Resolution
//! (`cpt-cf-oagw-algo-route-upstream-ownership-resolve`).
//!
//! Runs only at create time (`upstream_id` is immutable thereafter, per
//! `cpt-cf-oagw-dod-route-upstream-ownership`). No tenant-hierarchy walk is
//! performed: a route may only reference an upstream directly owned by the
//! calling tenant, never an ancestor's. Ownership is decided by
//! [`Upstream::tenant_id`] -- the internal, never-on-the-wire bookkeeping
//! field `cpt-cf-oagw-feature-upstream-management` (entry 2.2) stamps on
//! every stored [`Upstream`] from its own create handler's `SecurityContext`.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::model::upstream::Upstream;

/// Resolve `upstream_id` against the calling tenant, per
/// `inst-ownership-query`/`inst-ownership-notfound-if`/`inst-ownership-return`.
/// Returns `None` when the upstream does not exist, or belongs to a
/// different tenant (including an ancestor's -- no hierarchy walk is
/// performed, so an exact `tenant_id` match is the whole check). Never
/// rejects based on the resolved upstream's endpoint scheme
/// (`inst-ownership-no-scheme-restriction`) -- this function does not even
/// inspect `server.endpoints`.
// @cpt-algo:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p2
// @cpt-dod:cpt-cf-oagw-dod-route-upstream-ownership:p1
// @cpt-begin:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-query
// @cpt-begin:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-notfound-if
// @cpt-begin:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-notfound-return
// @cpt-begin:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-else
// @cpt-begin:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-no-scheme-restriction
// @cpt-begin:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-return
#[must_use]
pub fn resolve_owned_upstream(
    upstreams: &DashMap<Uuid, Arc<Upstream>>,
    tenant_id: Uuid,
    upstream_id: Uuid,
) -> Option<Arc<Upstream>> {
    let entry = upstreams.get(&upstream_id)?;
    let upstream = Arc::clone(entry.value());
    if upstream.tenant_id == tenant_id {
        Some(upstream)
    } else {
        None
    }
}
// @cpt-end:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-return
// @cpt-end:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-no-scheme-restriction
// @cpt-end:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-else
// @cpt-end:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-notfound-return
// @cpt-end:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-notfound-if
// @cpt-end:cpt-cf-oagw-algo-route-upstream-ownership-resolve:p1:inst-ownership-query

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::upstream::{Endpoint, EndpointScheme, ServerConfig};

    fn sample_upstream(tenant_id: Uuid) -> Upstream {
        Upstream {
            id: Some(Uuid::new_v4()),
            enabled: true,
            alias: Some("svc".to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "svc.internal".to_owned(),
                    port: 443,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        }
    }

    #[test]
    fn resolves_an_upstream_owned_by_the_calling_tenant() {
        let upstreams = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        upstreams.insert(upstream_id, Arc::new(sample_upstream(tenant_id)));

        let resolved = resolve_owned_upstream(&upstreams, tenant_id, upstream_id);
        assert!(resolved.is_some());
    }

    #[test]
    fn returns_none_for_a_nonexistent_upstream() {
        let upstreams: DashMap<Uuid, Arc<Upstream>> = DashMap::new();
        assert!(resolve_owned_upstream(&upstreams, Uuid::new_v4(), Uuid::new_v4()).is_none());
    }

    #[test]
    fn returns_none_for_an_upstream_owned_by_a_different_tenant() {
        let upstreams = DashMap::new();
        let owner_tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        upstreams.insert(upstream_id, Arc::new(sample_upstream(owner_tenant_id)));

        let resolved = resolve_owned_upstream(&upstreams, Uuid::new_v4(), upstream_id);
        assert!(resolved.is_none());
    }

    #[test]
    fn does_not_reject_a_plaintext_http_upstream_reference() {
        let upstreams = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let mut upstream = sample_upstream(tenant_id);
        upstream.server.endpoints[0].scheme = EndpointScheme::Http;
        upstream.server.endpoints[0].port = 80;
        upstreams.insert(upstream_id, Arc::new(upstream));

        let resolved = resolve_owned_upstream(&upstreams, tenant_id, upstream_id);
        assert!(resolved.is_some());
    }
}
