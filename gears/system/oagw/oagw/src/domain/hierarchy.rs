// Created: 2026-09-01 by Constructor Tech
//! Tenant-chain walking and hierarchical configuration merging.
//!
//! `docs/DESIGN.md` §3.2 "Hierarchical Configuration" and §3.3 "Tenant
//! Scoping": alias resolution walks descendant → root with closest match
//! winning, while `enforce`-mode ancestor constraints stay active across
//! shadowing.

use super::is_visible;
use super::model::{Route, SharingMode, Upstream};
use super::store::Store;
use crate::domain::errors::OagwError;

/// One tenant's contribution to a resolved configuration.
#[derive(Debug, Clone)]
pub struct Level {
    /// Tenant that owns the resources at this level.
    pub tenant_id: String,
    /// The upstream this tenant defines for the requested alias, if any.
    pub upstream: Option<Upstream>,
    /// Routes this tenant defines against the selected upstream.
    pub routes: Vec<Route>,
}

/// The result of walking the chain for one alias.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// Levels from descendant (first) to root (last).
    pub levels: Vec<Level>,
    /// Index into `levels` of the tenant whose upstream won.
    pub selected: usize,
}

impl Resolution {
    /// The upstream the walk selected.
    #[must_use]
    pub fn upstream(&self) -> Option<&Upstream> {
        self.levels
            .get(self.selected)
            .and_then(|l| l.upstream.as_ref())
    }

    /// Every ancestor upstream from `selected` towards the root.
    pub fn ancestors(&self) -> impl Iterator<Item = &Level> {
        self.levels.iter().skip(self.selected + 1)
    }

    /// Every ancestor *below* the selected level (between caller and it).
    pub fn ancestors_between(&self) -> impl Iterator<Item = &Level> {
        self.levels.iter().take(self.selected)
    }
}

/// Walk `chain` (descendant first) collecting every level that defines
/// `alias`.
///
/// The walk does not stop at the first hit: an ancestor that defines the
/// same alias still contributes its `enforce` constraints and its routes,
/// which are inherited by descendants even though the management API never
/// shows them. The *descendant-most* definition is the routing target, so
/// `selected` points at it.
#[must_use]
pub fn resolve(store: &Store, chain: &[String], alias: &str) -> Resolution {
    let mut levels = Vec::new();
    let mut selected = 0;
    for (index, tenant_id) in chain.iter().enumerate() {
        let upstream = store.get_upstream_by_alias(tenant_id, alias);
        let routes = upstream
            .as_ref()
            .map(|u| {
                store
                    .list_routes(tenant_id)
                    .into_iter()
                    .filter(|r| r.upstream_id == u.id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if upstream.is_some() && levels.iter().all(|l: &Level| l.upstream.is_none()) {
            selected = index;
        }
        levels.push(Level {
            tenant_id: tenant_id.clone(),
            upstream,
            routes,
        });
    }
    Resolution { levels, selected }
}

/// Collect the effective upstream configuration for a proxy request.
///
/// Ancestor `enforce` constraints and tags are unioned in even when a
/// descendant shadows the alias.
pub fn effective_upstream(resolution: &Resolution) -> Result<Upstream, OagwError> {
    let selected = resolution
        .upstream()
        .ok_or_else(|| OagwError::route_not_found("no upstream resolves this alias"))?;
    let mut effective = selected.clone();
    let mut tags = selected.tags.clone();

    for level in resolution.ancestors() {
        if let Some(ancestor) = &level.upstream {
            let auth_sharing = ancestor
                .auth
                .as_ref()
                .map_or(SharingMode::Private, |a| a.sharing);
            if is_visible(auth_sharing) && effective.auth.is_none() {
                effective.auth = ancestor.auth.clone();
            }
            if let Some(anc) = &ancestor.rate_limit
                && anc.sharing == SharingMode::Enforce
            {
                effective.rate_limit = Some(match &effective.rate_limit {
                    Some(desc) => super::merge_rate_limits(anc, desc),
                    None => anc.clone(),
                });
            }
        }
        for tag in &ancestor_tags(level) {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }
    effective.tags = tags;
    Ok(effective)
}

fn ancestor_tags(level: &Level) -> Vec<String> {
    level
        .upstream
        .as_ref()
        .map(|u| u.tags.clone())
        .unwrap_or_default()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{
        Burst, Endpoint, RateLimit, RateWindow, ServerConfig, SustainedRate,
    };

    fn upstream(alias: &str, tenant: &str, rate: Option<u64>) -> Upstream {
        Upstream {
            id: format!("u-{tenant}"),
            tenant_id: tenant.to_owned(),
            alias: alias.to_owned(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: "api.openai.com".to_owned(),
                    port: 443,
                }],
            },
            rate_limit: rate.map(|r| RateLimit {
                sharing: SharingMode::Enforce,
                sustained: SustainedRate {
                    rate: r,
                    window: RateWindow::Second,
                },
                burst: Some(Burst { capacity: r }),
                ..RateLimit::default()
            }),
            ..Upstream::default()
        }
    }

    #[test]
    fn the_closest_match_wins() {
        let store = Store::new();
        store
            .insert_upstream(upstream("api.openai.com", "root", None))
            .expect("root");
        store
            .insert_upstream(upstream("api.openai.com", "leaf", None))
            .expect("leaf");
        let chain = ["leaf".to_owned(), "mid".to_owned(), "root".to_owned()];
        let r = resolve(&store, &chain, "api.openai.com");
        assert_eq!(r.levels[r.selected].tenant_id, "leaf");
    }

    #[test]
    fn a_missing_leaf_falls_through_to_the_root() {
        let store = Store::new();
        store
            .insert_upstream(upstream("api.openai.com", "root", None))
            .expect("root");
        let chain = ["leaf".to_owned(), "root".to_owned()];
        let r = resolve(&store, &chain, "api.openai.com");
        assert_eq!(r.levels[r.selected].tenant_id, "root");
    }

    #[test]
    fn an_unknown_alias_yields_no_selection() {
        let store = Store::new();
        let chain = ["leaf".to_owned(), "root".to_owned()];
        let r = resolve(&store, &chain, "nope.example.com");
        assert!(r.upstream().is_none());
    }

    #[test]
    fn enforced_ancestor_rate_limits_bind_across_shadowing() {
        let store = Store::new();
        store
            .insert_upstream(upstream("api.openai.com", "root", Some(10_000)))
            .expect("root");
        let mut leaf = upstream("api.openai.com", "leaf", None);
        leaf.rate_limit = Some(RateLimit {
            sharing: SharingMode::Private,
            sustained: SustainedRate {
                rate: 500,
                window: RateWindow::Second,
            },
            ..RateLimit::default()
        });
        store.insert_upstream(leaf).expect("leaf");
        let chain = ["leaf".to_owned(), "root".to_owned()];
        let r = resolve(&store, &chain, "api.openai.com");
        let effective = effective_upstream(&r).expect("effective");
        assert_eq!(effective.tenant_id, "leaf");
        let rl = effective.rate_limit.expect("rate limit");
        assert_eq!(rl.sustained.rate, 500);
        assert_eq!(rl.capacity(), 500);
    }
}
