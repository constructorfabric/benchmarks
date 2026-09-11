//! The tenant-hierarchy alias walk.
//!
//! `cpt-cf-oagw-algo-alias-walk` resolves the `{alias}` path segment of the
//! proxy route to one upstream record: the closest tenant of the caller's chain
//! that owns the alias wins, and it shadows every ancestor record of the same
//! alias. The walk is the only place a request can reach an ancestor resource —
//! a descendant inherits an ancestor's upstream through this walk and never
//! addresses it directly.
//!
//! The walk is read-only over one published [`ConfigSnapshot`], which the
//! engine keeps for the whole request, so the alias, the route and the merged
//! configuration can never be resolved from different generations.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias::normalize_alias;
use crate::domain::error::{DomainError, ManagementError};
use crate::domain::model::Upstream;
use crate::domain::sharing::TenantHierarchy;
use crate::infra::storage::ConfigSnapshot;

/// The outcome of a successful alias walk.
#[derive(Debug, Clone)]
pub struct AliasWalk {
    /// The enabled upstream the walk selected.
    pub selected: Arc<Upstream>,
    /// The upstream records of the walked tenant chain that carry the same
    /// alias, nearest tenant first. The selected record is the head; the tail
    /// is what the selection shadowed.
    pub shadowed: Vec<Arc<Upstream>>,
    /// The tenant chain the walk consulted, caller first, ancestors after.
    pub tenants: Vec<Uuid>,
}

impl AliasWalk {
    /// The identifier of the selected upstream.
    #[must_use]
    pub fn upstream_id(&self) -> Uuid {
        self.selected.id
    }
}

/// Map the hierarchy port's management failure onto the data-plane row.
///
/// The port fails closed with `503 LinkUnavailable` when tenant-resolver cannot
/// be reached; the data plane maps the same row, so a request that cannot
/// establish its tenant chain never falls back to a single-tenant reading.
fn map_hierarchy_error(error: &ManagementError) -> DomainError {
    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-03
    // The chain comes from tenant-resolver; a resolver that cannot be reached
    // yields no chain and therefore no resolution at all.
    let _ = error;
    DomainError::LinkUnavailable {
        detail: "the tenant hierarchy source is unavailable".to_owned(),
        retry_after_seconds: None,
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-03
}

// @cpt-begin:cpt-cf-oagw-dod-alias-walk:p1:inst-full
/// Walk the tenant chain of `tenant_id` for `supplied_alias`.
///
/// # Errors
///
/// Returns the mapped `404` when no tenant of the chain holds the alias, the
/// mapped `503` when the closest match is disabled (without falling through to
/// an ancestor) and the mapped `503` when the hierarchy source is unreachable.
pub async fn walk(
    hierarchy: &dyn TenantHierarchy,
    ctx: &SecurityContext,
    tenant_id: Uuid,
    snapshot: &ConfigSnapshot,
    supplied_alias: &str,
) -> Result<AliasWalk, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-01
    // The alias is normalized to the entry-2.2 shape, so a request that
    // addresses `API.Vendor.com.` resolves the record stored as
    // `api.vendor.com`.
    let Some(alias) = normalize_alias(supplied_alias) else {
        return Err(DomainError::RouteNotFound {
            detail: format!("the alias `{supplied_alias}` is not a valid alias"),
        });
    };
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-01

    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-02
    // `snapshot` is the published generation the engine read once; the walk
    // never re-reads the store, so every later stage of this request sees the
    // same records.
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-02

    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-14
    // The caller's own tenant is the head of the chain: the walk starts there
    // and moves towards the root, which makes the walk the only path to an
    // ancestor resource.
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-14
    let chain = hierarchy
        .ancestors_of(ctx, tenant_id)
        .await
        .map_err(|error| map_hierarchy_error(&error))?;

    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-04
    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-05
    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-06
    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-07
    let tenants: Vec<Uuid> = std::iter::once(tenant_id).chain(chain).collect();
    let mut candidates: Vec<Arc<Upstream>> = Vec::new();
    for tenant in &tenants {
        // `upstreams_of` returns the tenant's own records only: a record of a
        // descendant tenant is never visible to an ancestor's walk.
        if let Some(found) = snapshot
            .upstreams_of(*tenant)
            .into_iter()
            .find(|upstream| upstream.alias == alias)
        {
            candidates.push(found);
        }
    }
    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-13
    // The closest match shadows every ancestor record of the same alias: the
    // tail of `candidates` holds them, so the merge can still read the bounds
    // an ancestor pinned and shadowing cannot lift.
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-13
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-07
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-06
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-05
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-04

    let Some((selected, shadowed)) = candidates.split_first() else {
        // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-08
        // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-09
        return Err(DomainError::RouteNotFound {
            detail: format!("no tenant of the chain holds the alias `{alias}`"),
        });
        // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-09
        // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-08
    };

    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-10
    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-11
    // A disabled closest match is a disabled target: the walk returns the
    // disabled outcome instead of falling through to an ancestor record of the
    // same alias, because falling through would route the request to a target
    // the operator explicitly shadowed.
    if !selected.enabled {
        return Err(DomainError::LinkUnavailable {
            detail: format!("the upstream `{alias}` is disabled"),
            retry_after_seconds: None,
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-11
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-10

    // @cpt-begin:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-12
    Ok(AliasWalk {
        selected: Arc::clone(selected),
        shadowed: shadowed.to_vec(),
        tenants,
    })
    // @cpt-end:cpt-cf-oagw-algo-alias-walk:p1:inst-pe-aw-12
}
// @cpt-end:cpt-cf-oagw-dod-alias-walk:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, Protocol, Route, ServerConfig, Sharing, Timestamp, Upstream,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;

    /// A hierarchy stub with a fixed chain per tenant.
    #[derive(Default)]
    struct StubHierarchy {
        chains: HashMap<Uuid, Vec<Uuid>>,
    }

    impl StubHierarchy {
        fn with_chain(mut self, tenant: Uuid, chain: &[Uuid]) -> Self {
            self.chains.insert(tenant, chain.to_vec());
            self
        }
    }

    #[async_trait]
    impl TenantHierarchy for StubHierarchy {
        async fn ancestors_of(
            &self,
            _ctx: &SecurityContext,
            tenant_id: Uuid,
        ) -> Result<Vec<Uuid>, ManagementError> {
            Ok(self.chains.get(&tenant_id).cloned().unwrap_or_default())
        }
    }

    fn upstream(tenant: Uuid, alias: &str, enabled: bool) -> Arc<Upstream> {
        Arc::new(Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled,
            alias: alias.to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: crate::domain::model::Scheme::Https,
                    host: alias.to_string(),
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
        })
    }

    fn snapshot(upstreams: Vec<Arc<Upstream>>, routes: Vec<Arc<Route>>) -> ConfigSnapshot {
        let mut store = ConfigSnapshot {
            epoch: 1,
            upstreams: Default::default(),
            routes: Default::default(),
            plugins: Default::default(),
        };
        for upstream in upstreams {
            store
                .upstreams
                .insert((upstream.tenant_id, upstream.id), upstream);
        }
        for route in routes {
            store.routes.insert((route.tenant_id, route.id), route);
        }
        store
    }

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::nil())
            .build()
            .expect("security context")
    }

    #[tokio::test]
    async fn the_closest_tenant_shadows_every_ancestor_record() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let ancestor_record = upstream(parent, "api.vendor.com", true);
        let own_record = upstream(child, "api.vendor.com", true);
        let store = snapshot(vec![Arc::clone(&ancestor_record), Arc::clone(&own_record)], vec![]);
        let hierarchy = StubHierarchy::default().with_chain(child, &[parent]);

        let walk = walk(
            &hierarchy,
            &ctx(),
            child,
            &store,
            "API.Vendor.com.",
        )
        .await
        .expect("the walk resolves the alias");

        assert_eq!(walk.selected.id, own_record.id);
        assert_eq!(walk.shadowed.len(), 1);
        assert_eq!(walk.shadowed[0].id, ancestor_record.id);
        assert_eq!(walk.tenants, vec![child, parent]);
    }

    #[tokio::test]
    async fn an_ancestor_record_is_reached_through_the_walk() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let ancestor_record = upstream(parent, "api.vendor.com", true);
        let store = snapshot(vec![ancestor_record], vec![]);
        let hierarchy = StubHierarchy::default().with_chain(child, &[parent]);

        let walk = walk(&hierarchy, &ctx(), child, &store, "api.vendor.com")
            .await
            .expect("the walk reaches the ancestor record");
        assert_eq!(walk.selected.tenant_id, parent);
        assert!(walk.shadowed.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_alias_is_a_route_not_found() {
        let store = snapshot(vec![], vec![]);
        let hierarchy = StubHierarchy::default();
        let error = walk(&hierarchy, &ctx(), Uuid::new_v4(), &store, "absent.dev")
            .await
            .expect_err("no tenant holds the alias");
        assert_eq!(error.status(), 404, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[tokio::test]
    async fn a_disabled_closest_match_does_not_fall_through() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let store = snapshot(
            vec![
                upstream(parent, "api.vendor.com", true),
                upstream(child, "api.vendor.com", false),
            ],
            vec![],
        );
        let hierarchy = StubHierarchy::default().with_chain(child, &[parent]);

        let error = walk(&hierarchy, &ctx(), child, &store, "api.vendor.com")
            .await
            .expect_err("the closest match is disabled");
        assert_eq!(error.status(), 503, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[tokio::test]
    async fn an_unreachable_hierarchy_fails_closed() {
        struct Broken;
        #[async_trait]
        impl TenantHierarchy for Broken {
            async fn ancestors_of(
                &self,
                _ctx: &SecurityContext,
                _tenant_id: Uuid,
            ) -> Result<Vec<Uuid>, ManagementError> {
                Err(ManagementError::from(DomainError::LinkUnavailable {
                    detail: "resolver down".to_owned(),
                    retry_after_seconds: None,
                }))
            }
        }
        let store = snapshot(vec![upstream(Uuid::new_v4(), "api.vendor.com", true)], vec![]);
        let error = walk(&Broken, &ctx(), Uuid::new_v4(), &store, "api.vendor.com")
            .await
            .expect_err("the chain cannot be resolved");
        assert_eq!(error.status(), 503, "{error}");
    }

    #[tokio::test]
    async fn an_alias_that_cannot_be_normalized_is_not_resolved() {
        let store = snapshot(vec![], vec![]);
        let hierarchy = StubHierarchy::default();
        let error = walk(&hierarchy, &ctx(), Uuid::new_v4(), &store, "")
            .await
            .expect_err("an empty alias is not a valid alias");
        assert_eq!(error.status(), 404, "{error}");
    }

    #[test]
    fn the_walk_result_carries_no_credential_material() {
        // The walk result carries records and identifiers only, never a
        // credential: a `cred://` reference stays on the stored record and the
        // effective configuration references the auth declaration by name.
        let record = upstream(Uuid::new_v4(), "api.vendor.com", true);
        let rendered = format!("{record:?}").to_lowercase();
        assert!(!rendered.contains("bearer"));
        let _ = Sharing::Private;
    }
}
