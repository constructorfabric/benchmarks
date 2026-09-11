//! Resolve the Alias Through the Tenant Chain
//! (`cpt-cf-oagw-algo-proxy-resolve-alias`).

use std::sync::Arc;

use uuid::Uuid;

use crate::model::upstream::Upstream;
use crate::proxy::hierarchy::TenantHierarchyProvider;
use crate::store::OagwState;

/// One same-alias declaration found while walking the tenant chain, tagged
/// with its tenant distance from the calling tenant (0 = calling tenant).
#[derive(Debug, Clone)]
pub(crate) struct AncestorLevel {
    /// Redundant with `upstream.tenant_id` by construction; retained
    /// alongside it because it is the natural join key a future
    /// tenant-scoped query (e.g. rate-limit scope `tenant`) would read,
    /// without forcing every caller to dereference `upstream`.
    #[allow(dead_code)]
    pub tenant_id: Uuid,
    pub distance: u32,
    pub upstream: Arc<Upstream>,
}

/// Output of `cpt-cf-oagw-algo-proxy-resolve-alias`: the selected upstream
/// (last entry), the retained same-alias ancestor chain ordered root to
/// child, and the effective enabled state.
#[derive(Debug, Clone)]
pub(crate) struct AliasResolution {
    /// Ordered root (largest distance) to child/selected (distance 0 or the
    /// smallest found), per `inst-proxy-merge-walk`'s root-to-child fold
    /// order.
    pub chain: Vec<AncestorLevel>,
    pub effective_enabled: bool,
}

impl AliasResolution {
    /// The routing target: the closest (smallest-distance) declaration.
    #[must_use]
    pub fn selected(&self) -> &AncestorLevel {
        // `resolve_alias` never returns an `AliasResolution` with an empty
        // chain (see its `is_empty` early return), so this is total.
        self.chain
            .last()
            .expect("AliasResolution::chain is never empty")
    }
}

fn find_upstream_by_tenant_alias(
    state: &OagwState,
    tenant_id: Uuid,
    alias: &str,
) -> Option<Arc<Upstream>> {
    state.store.upstreams().iter().find_map(|entry| {
        let upstream = entry.value();
        let owns = upstream.tenant_id == tenant_id
            && upstream
                .alias
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(alias));
        owns.then(|| Arc::clone(upstream))
    })
}

/// `cpt-cf-oagw-algo-proxy-resolve-alias`: walk the calling tenant's chain
/// descendant to root, collecting every same-alias declaration; the closest
/// wins as the routing target (shadowing), and a disabled selection is
/// terminal rather than a reason to fall through to an ancestor -- the
/// FEATURE's fixed reading of `inst-proxy-alias-disabled-terminal`.
// @cpt-algo:cpt-cf-oagw-algo-proxy-resolve-alias:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-alias-resolution:p1
// @cpt-dod:cpt-cf-oagw-dod-proxy-enable-disable:p1
// @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-order-chain
// @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-foreach-tenant
// @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-lookup
// @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-collect
pub(crate) fn resolve_alias(
    state: &OagwState,
    hierarchy: &dyn TenantHierarchyProvider,
    tenant_id: Uuid,
    alias: &str,
) -> Option<AliasResolution> {
    let mut chain_ids: Vec<(Uuid, u32)> = vec![(tenant_id, 0)];
    for (idx, ancestor_id) in hierarchy.ancestors(tenant_id).into_iter().enumerate() {
        let distance = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        chain_ids.push((ancestor_id, distance));
    }

    let mut matches: Vec<AncestorLevel> = Vec::new();
    for (tid, distance) in chain_ids {
        if let Some(upstream) = find_upstream_by_tenant_alias(state, tid, alias) {
            matches.push(AncestorLevel {
                tenant_id: tid,
                distance,
                upstream,
            });
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-collect
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-lookup
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-foreach-tenant
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-order-chain

    // @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-if-empty
    // @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-return-empty
    if matches.is_empty() {
        return None;
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-return-empty
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-if-empty

    // @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-select-closest
    // @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-effective-enabled
    let effective_enabled = matches.iter().all(|m| m.upstream.enabled);
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-effective-enabled
    // (closest-match-wins is realized by sorting root->child below and
    // `AliasResolution::selected` reading the last, i.e. smallest-distance,
    // entry)
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-select-closest

    // @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-disabled-terminal
    // The caller (`crate::proxy::engine::resolve_and_match`) treats
    // `effective_enabled == false` as terminal `503` rather than retrying
    // resolution against an ancestor; `effective_enabled` is carried
    // through unchanged into the returned `AliasResolution` below for it
    // to read.
    // @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-retain-ancestors
    let mut chain = matches;
    chain.sort_by_key(|level| std::cmp::Reverse(level.distance));
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-retain-ancestors
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-disabled-terminal

    // @cpt-begin:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-return
    Some(AliasResolution {
        chain,
        effective_enabled,
    })
    // @cpt-end:cpt-cf-oagw-algo-proxy-resolve-alias:p2:inst-proxy-alias-return
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::model::upstream::{Endpoint, EndpointScheme, ServerConfig};
    use crate::proxy::hierarchy::NoTenantHierarchy;

    fn upstream(tenant_id: Uuid, alias: &str, enabled: bool) -> Upstream {
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
    fn no_upstream_anywhere_in_chain_returns_none() {
        let state = OagwState::new(OagwConfig::default());
        assert!(resolve_alias(&state, &NoTenantHierarchy, Uuid::new_v4(), "svc").is_none());
    }

    #[test]
    fn own_tenant_declaration_is_selected_with_no_ancestors() {
        let state = OagwState::new(OagwConfig::default());
        let tenant = Uuid::new_v4();
        let up = upstream(tenant, "svc", true);
        state.store.upstreams().insert(up.id.unwrap(), Arc::new(up));

        let resolved = resolve_alias(&state, &NoTenantHierarchy, tenant, "svc").unwrap();
        assert_eq!(resolved.selected().tenant_id, tenant);
        assert!(resolved.effective_enabled);
        assert_eq!(resolved.chain.len(), 1);
    }

    #[test]
    fn descendant_shadows_ancestor_and_ancestor_stays_in_chain() {
        let state = OagwState::new(OagwConfig::default());
        let ancestor = Uuid::new_v4();
        let child = Uuid::new_v4();
        let ancestor_up = upstream(ancestor, "svc", true);
        let child_up = upstream(child, "svc", true);
        state
            .store
            .upstreams()
            .insert(ancestor_up.id.unwrap(), Arc::new(ancestor_up));
        state
            .store
            .upstreams()
            .insert(child_up.id.unwrap(), Arc::new(child_up));

        let hierarchy = FixedHierarchy(vec![ancestor]);
        let resolved = resolve_alias(&state, &hierarchy, child, "svc").unwrap();
        assert_eq!(resolved.selected().tenant_id, child);
        assert_eq!(resolved.chain.len(), 2);
        assert_eq!(resolved.chain[0].tenant_id, ancestor); // root-most first
        assert_eq!(resolved.chain[1].tenant_id, child); // selected last
    }

    #[test]
    fn ancestor_disabled_cascades_even_though_descendant_enabled() {
        let state = OagwState::new(OagwConfig::default());
        let ancestor = Uuid::new_v4();
        let child = Uuid::new_v4();
        let ancestor_up = upstream(ancestor, "svc", false);
        let child_up = upstream(child, "svc", true);
        state
            .store
            .upstreams()
            .insert(ancestor_up.id.unwrap(), Arc::new(ancestor_up));
        state
            .store
            .upstreams()
            .insert(child_up.id.unwrap(), Arc::new(child_up));

        let hierarchy = FixedHierarchy(vec![ancestor]);
        let resolved = resolve_alias(&state, &hierarchy, child, "svc").unwrap();
        assert!(!resolved.effective_enabled);
        // Shadowing still selects the descendant's own upstream as target.
        assert_eq!(resolved.selected().tenant_id, child);
    }
}
