//! `cpt-cf-oagw-algo-resolve-tenant-scope`: tenant-scoped lookups against
//! `ConfigStore::upstreams()`.
// @cpt-dod:cpt-cf-oagw-dod-tenant-scoping:p1

use std::sync::Arc;

use uuid::Uuid;

use crate::model::upstream::Upstream;
use crate::store::OagwState;

use super::TenantHierarchyProvider;

/// `DB: SELECT oagw_upstream WHERE id = {id} AND tenant_id = calling tenant`
/// (`inst-get-upstream-query` and siblings): own-tenant-only visibility,
/// never traversing to ancestor tenants (`inst-scope-visibility-restrict`).
pub(super) fn find_by_id_for_tenant(
    state: &OagwState,
    tenant_id: Uuid,
    id: Uuid,
) -> Option<Arc<Upstream>> {
    state.store.upstreams().get(&id).and_then(|entry| {
        let upstream = entry.value().clone();
        (upstream.tenant_id == tenant_id).then_some(upstream)
    })
}

/// `DB: SELECT oagw_upstream WHERE tenant_id = calling tenant AND alias =
/// resolved alias` (`inst-scope-create-own-lookup`).
pub(super) fn find_by_alias_for_tenant(
    state: &OagwState,
    tenant_id: Uuid,
    alias: &str,
) -> Option<Arc<Upstream>> {
    state.store.upstreams().iter().find_map(|entry| {
        let upstream = entry.value();
        let owns_alias =
            upstream.tenant_id == tenant_id && upstream.alias.as_deref() == Some(alias);
        owns_alias.then(|| upstream.clone())
    })
}

/// Every tenant-owned Upstream for `tenant_id` (`inst-list-upstreams-query`).
pub(super) fn list_for_tenant(state: &OagwState, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
    state
        .store
        .upstreams()
        .iter()
        .filter(|entry| entry.value().tenant_id == tenant_id)
        .map(|entry| entry.value().clone())
        .collect()
}

/// Walk `tenant_id`'s ancestor chain (nearest parent to root) searching for
/// an upstream sharing `alias` (`inst-scope-create-ancestor-walk`).
pub(super) fn find_ancestor_upstream(
    state: &OagwState,
    hierarchy: &dyn TenantHierarchyProvider,
    tenant_id: Uuid,
    alias: &str,
) -> Option<Arc<Upstream>> {
    hierarchy
        .ancestors(tenant_id)
        .into_iter()
        .find_map(|ancestor_id| find_by_alias_for_tenant(state, ancestor_id, alias))
}

/// `true` when some ancestor of `tenant_id` has a `disabled` upstream
/// sharing `alias` (`cpt-cf-oagw-state-upstream-enabled-lifecycle`
/// `inst-state-disabled-reenable-blocked`).
pub(super) fn ancestor_disabled(
    state: &OagwState,
    hierarchy: &dyn TenantHierarchyProvider,
    tenant_id: Uuid,
    alias: &str,
) -> bool {
    hierarchy
        .ancestors(tenant_id)
        .into_iter()
        .any(|ancestor_id| {
            find_by_alias_for_tenant(state, ancestor_id, alias).is_some_and(|up| !up.enabled)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::model::upstream::{Endpoint, EndpointScheme, ServerConfig};

    fn sample_upstream(tenant_id: Uuid, alias: &str, enabled: bool) -> Upstream {
        Upstream {
            id: Some(Uuid::new_v4()),
            enabled,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "example.com".to_owned(),
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

    #[derive(Debug)]
    struct FixedHierarchy(Vec<Uuid>);
    impl TenantHierarchyProvider for FixedHierarchy {
        fn ancestors(&self, _tenant_id: Uuid) -> Vec<Uuid> {
            self.0.clone()
        }
    }

    #[test]
    fn find_by_id_only_returns_the_owning_tenants_row() {
        let state = OagwState::new(OagwConfig::default());
        let tenant = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();
        let up = sample_upstream(tenant, "example.com", true);
        let id = up.id.unwrap();
        state.store.upstreams().insert(id, Arc::new(up));

        assert!(find_by_id_for_tenant(&state, tenant, id).is_some());
        assert!(find_by_id_for_tenant(&state, other_tenant, id).is_none());
    }

    #[test]
    fn ancestor_disabled_true_only_when_an_ancestor_shares_the_alias_and_is_disabled() {
        let state = OagwState::new(OagwConfig::default());
        let ancestor = Uuid::new_v4();
        let child = Uuid::new_v4();
        let up = sample_upstream(ancestor, "shared.example.com", false);
        state.store.upstreams().insert(up.id.unwrap(), Arc::new(up));

        let hierarchy = FixedHierarchy(vec![ancestor]);
        assert!(ancestor_disabled(
            &state,
            &hierarchy,
            child,
            "shared.example.com"
        ));
        assert!(!ancestor_disabled(
            &state,
            &hierarchy,
            child,
            "unrelated.example.com"
        ));
    }
}
