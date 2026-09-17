//! Tenant hierarchy: chain resolution and graded configuration.
//!
//! `DESIGN.md` §"Hierarchical Configuration" gives OAGW a three-mode sharing
//! model per configuration field (`private` / `inherit` / `enforce`) and a
//! hierarchy walk for alias resolution: descendant → root, closest match wins.
//! This module owns both halves:
//!
//! * [`TenantChain`] produces the caller's chain, from the platform's
//!   `tenant-resolver` when one is available and from the caller's own tenant
//!   otherwise. Resolution is *never* allowed to fail a request: a resolver
//!   outage degrades to a single-tenant chain.
//! * [`effective_upstream`] folds the ancestor definitions into the selected
//!   one with the merge table of the design (auth override / enforce, rate
//!   `min`, plugin concatenation, CORS union, tag union).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use tenant_resolver_sdk::{BarrierMode, GetAncestorsOptions, TenantResolverClient};
use uuid::Uuid;

use crate::domain::SecurityContext;
use crate::domain::dto::{
    AuthConfig, CorsConfig, PluginBindings, Sharing, SustainedRate, Upstream, UpstreamConfig,
};

/// Capacity of the ancestor-chain cache, in tenants.
const CHAIN_CACHE_CAPACITY: usize = 4096;

/// One tenant of a caller's chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantNode {
    /// Tenant id.
    pub id: Uuid,
    /// `false` for a suspended or deleted tenant: such a tenant contributes
    /// nothing to a walk, neither configuration nor an ancestor link.
    pub active: bool,
}

impl TenantNode {
    /// The caller's own tenant, which is always part of its own chain.
    #[must_use]
    pub fn own(id: Uuid) -> Self {
        Self { id, active: true }
    }
}

/// Provider of a tenant's ancestor chain.
///
/// The chain is ordered **descendant first**: index 0 is `tenant` itself, the
/// last entry is the root of the tree (or `tenant` when it *is* the root).
#[async_trait]
pub trait TenantChain: Send + Sync + 'static {
    /// The caller's chain. Implementations must not fail: an unavailable
    /// resolver degrades to the single-tenant chain.
    async fn chain(&self, ctx: &SecurityContext, tenant: Uuid) -> Vec<TenantNode>;

    /// Tenant ids of the *active* part of the chain, descendant first.
    ///
    /// This is the form the data plane walks: deleted and suspended tenants
    /// are skipped, the caller's own tenant always comes first.
    async fn active_ids(&self, ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        self.chain(ctx, tenant)
            .await
            .into_iter()
            .filter(|node| node.active)
            .map(|node| node.id)
            .collect()
    }
}

/// The chain of exactly one tenant: the caller's own.
///
/// Used when no `tenant-resolver` client is published on the client hub, and
/// as the degrade path when the resolver cannot be reached.
#[derive(Debug, Default, Clone, Copy)]
pub struct SingleTenantChain;

#[async_trait]
impl TenantChain for SingleTenantChain {
    async fn chain(&self, _ctx: &SecurityContext, tenant: Uuid) -> Vec<TenantNode> {
        vec![TenantNode::own(tenant)]
    }
}

/// A chain built from a static parent map.
///
/// Deployments without a resolver can name the tree directly; the integration
/// tests use it to exercise shadowing and inheritance deterministically.
#[derive(Debug, Default, Clone)]
pub struct StaticChain {
    parents: BTreeMap<Uuid, Uuid>,
    inactive: BTreeSet<Uuid>,
}

impl StaticChain {
    /// Build the chain from a `tenant → parent` map.
    #[must_use]
    pub fn new(parents: BTreeMap<Uuid, Uuid>) -> Self {
        Self {
            parents,
            inactive: BTreeSet::new(),
        }
    }

    /// Mark a tenant as not active (deleted / suspended): it is skipped by
    /// every walk, including as an ancestor link.
    #[must_use]
    pub fn with_inactive(mut self, id: Uuid) -> Self {
        self.inactive.insert(id);
        self
    }
}

#[async_trait]
impl TenantChain for StaticChain {
    async fn chain(&self, _ctx: &SecurityContext, tenant: Uuid) -> Vec<TenantNode> {
        let mut chain = vec![TenantNode {
            id: tenant,
            active: !self.inactive.contains(&tenant),
        }];
        let mut seen = BTreeSet::from([tenant]);
        let mut cursor = tenant;
        while let Some(parent) = self.parents.get(&cursor).copied() {
            if !seen.insert(parent) {
                break; // defensive: a malformed cycle must not loop forever
            }
            // An inactive tenant is reported as it is — inactive — so the walk
            // still reaches the tenants above it but never reads its
            // configuration.
            chain.push(TenantNode {
                id: parent,
                active: !self.inactive.contains(&parent),
            });
            cursor = parent;
        }
        chain
    }
}

/// Chain resolved through the platform `tenant-resolver`, cached per tenant.
///
/// A failed or missing lookup is logged and answered with the caller's own
/// tenant only, so an AM outage costs multi-tenant inheritance but never a
/// request.
pub struct ResolverChain {
    client: Arc<dyn TenantResolverClient>,
    cache: MemoryCache<Uuid, Vec<TenantNode>>,
    ttl: Duration,
}

impl std::fmt::Debug for ResolverChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolverChain")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl ResolverChain {
    /// Build the chain resolver.
    ///
    /// `ttl_secs` is the cache lifetime: short, so a topology change is picked
    /// up quickly, long enough to keep the walk off the hot path.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>, ttl_secs: u64) -> Self {
        Self {
            client,
            cache: MemoryCache::new(CHAIN_CACHE_CAPACITY),
            ttl: Duration::from_secs(ttl_secs),
        }
    }
}

#[async_trait]
impl TenantChain for ResolverChain {
    async fn chain(&self, ctx: &SecurityContext, tenant: Uuid) -> Vec<TenantNode> {
        let (cached, _) = self.cache.get(&tenant);
        if let Some(chain) = cached {
            return chain;
        }

        // Barriers are ignored on purpose: a self-managed sub-tree still
        // inherits its ancestor's shared upstream definitions, exactly as the
        // credstore's `shared` secrets do. What a caller may *do* with them is
        // the PDP's decision, not the chain's.
        let options = GetAncestorsOptions {
            barrier_mode: BarrierMode::Ignore,
        };
        let ancestors = match self
            .client
            .get_ancestors(ctx, tenant_resolver_sdk::TenantId(tenant), &options)
            .await
        {
            Ok(response) => response.ancestors,
            Err(error) => {
                tracing::warn!(
                    err = %error,
                    tenant = %tenant,
                    "tenant-resolver get_ancestors failed; degrading to a single-tenant chain"
                );
                return vec![TenantNode::own(tenant)];
            }
        };
        let chain = std::iter::once(TenantNode {
            id: tenant,
            active: true,
        })
        .chain(ancestors.into_iter().map(|tenant| TenantNode {
            id: tenant.id.0,
            active: matches!(tenant.status, tenant_resolver_sdk::TenantStatus::Active),
        }))
        .collect::<Vec<_>>();

        self.cache.put(&tenant, chain.clone(), Some(self.ttl));
        chain
    }
}

/// `true` when an ancestor's field is visible to its descendants at all.
#[must_use]
pub fn is_shared(sharing: Sharing) -> bool {
    !matches!(sharing, Sharing::Private)
}

/// `true` when an upstream definition carries at least one field a descendant
/// is allowed to inherit.
///
/// Used by the management *listing*: `private` definitions stay invisible to
/// descendants there, exactly as the sharing table prescribes.
#[must_use]
pub fn shares_with_descendants(config: &UpstreamConfig) -> bool {
    config
        .auth
        .as_ref()
        .is_some_and(|auth| is_shared(auth.sharing))
        || config
            .rate_limit
            .as_ref()
            .is_some_and(|rate| is_shared(rate.sharing))
        || config
            .cors
            .as_ref()
            .is_some_and(|cors| is_shared(cors.sharing))
        || is_shared(config.plugins.sharing)
}

/// Fold an ancestor chain into the upstream the walk selected.
///
/// `selected` is the closest chain match — the caller's own upstream when it
/// has one, otherwise the nearest ancestor's. `ancestors` are the *other*
/// members of the chain that define the same alias, closest first; an entry
/// the descendant cannot see (`private` fields only) must have been filtered
/// out before it gets here.
#[must_use]
pub fn effective_upstream(selected: &Upstream, ancestors: &[&Upstream]) -> Upstream {
    let mut effective = selected.clone();
    for ancestor in ancestors {
        // `enabled` is ANDed: an ancestor-disabled upstream stays disabled for
        // its descendants, and shadowing is not a way to re-enable it.
        effective.config.enabled &= ancestor.config.enabled;
        // Tags have no sharing mode: add-only union, and a descendant can
        // never remove an inherited tag.
        for tag in &ancestor.config.tags {
            if !effective.config.tags.contains(tag) {
                effective.config.tags.push(tag.clone());
            }
        }
        fold_plugins(&mut effective.config.plugins, &ancestor.config.plugins);
        if let Some(auth) = ancestor.config.auth.as_ref() {
            fold_auth(&mut effective.config.auth, auth);
        }
        if let Some(rate) = ancestor.config.rate_limit.as_ref() {
            fold_rate_limit(&mut effective.config.rate_limit, rate);
        }
        if let Some(cors) = ancestor.config.cors.as_ref() {
            fold_cors(&mut effective.config.cors, cors);
        }
    }
    effective
}

/// Ancestor plugins run *before* the descendant's own (`ancestor.plugins +
/// descendant.plugins`), and an enforced plugin cannot be dropped by a
/// descendant that simply stops listing it.
fn fold_plugins(descendant: &mut PluginBindings, ancestor: &PluginBindings) {
    if !is_shared(ancestor.sharing) || ancestor.items.is_empty() {
        return;
    }
    let mut merged = ancestor.items.clone();
    for item in &descendant.items {
        if !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    descendant.items = merged;
}

/// Auth: a descendant may use its own credentials under `inherit`; `enforce`
/// cannot be overridden at all.
fn fold_auth(descendant: &mut Option<AuthConfig>, ancestor: &AuthConfig) {
    match ancestor.sharing {
        Sharing::Private => {}
        Sharing::Enforce => *descendant = Some(ancestor.clone()),
        Sharing::Inherit => {
            if descendant.is_none() {
                *descendant = Some(ancestor.clone());
            }
        }
    }
}

/// Rate limits: the stricter side always wins, so an ancestor's limit caps a
/// descendant even under `inherit` (DESIGN merge table).
fn fold_rate_limit(
    descendant: &mut Option<crate::domain::dto::RateLimitConfig>,
    ancestor: &crate::domain::dto::RateLimitConfig,
) {
    if !is_shared(ancestor.sharing) {
        return;
    }
    match descendant {
        Some(own) => {
            if requests_per_second(&ancestor.sustained) < requests_per_second(&own.sustained) {
                own.sustained = ancestor.sustained.clone();
            }
            own.burst.capacity = own.burst.capacity.min(ancestor.burst.capacity);
        }
        None => *descendant = Some(ancestor.clone()),
    }
}

/// Sustained rate normalised to requests per second, so a `10/second` and a
/// `600/minute` ceiling are comparable.
fn requests_per_second(rate: &SustainedRate) -> f64 {
    let window = f64::from(u32::try_from(rate.window.secs().max(1)).unwrap_or(u32::MAX));
    f64::from(rate.rate) / window
}

/// CORS: `enforce` replaces the descendant's policy, `inherit` unions origins
/// into it.
fn fold_cors(descendant: &mut Option<CorsConfig>, ancestor: &CorsConfig) {
    if !is_shared(ancestor.sharing) {
        return;
    }
    if matches!(ancestor.sharing, Sharing::Enforce) {
        *descendant = Some(ancestor.clone());
        return;
    }
    match descendant {
        Some(own) => {
            for origin in &ancestor.allowed_origins {
                if !own.allowed_origins.contains(origin) {
                    own.allowed_origins.push(origin.clone());
                }
            }
        }
        None => *descendant = Some(ancestor.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Burst, RateLimitConfig, SustainedRate, UpstreamConfig};
    use std::collections::BTreeMap;

    const ROOT: Uuid = Uuid::from_u128(1);
    const MIDDLE: Uuid = Uuid::from_u128(2);
    const LEAF: Uuid = Uuid::from_u128(3);

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(LEAF)
            .subject_type("user")
            .subject_tenant_id(LEAF)
            .build()
            .expect("security context")
    }

    /// `LEAF → MIDDLE → ROOT`.
    fn static_chain() -> StaticChain {
        StaticChain::new(BTreeMap::from([(LEAF, MIDDLE), (MIDDLE, ROOT)]))
    }

    #[tokio::test]
    async fn a_static_chain_walks_from_the_caller_to_the_root() {
        let chain = static_chain();
        let nodes = chain.chain(&ctx(), LEAF).await;
        let ids: Vec<Uuid> = nodes.iter().map(|node| node.id).collect();
        assert_eq!(ids, vec![LEAF, MIDDLE, ROOT]);
        assert!(nodes.iter().all(|node| node.active));
    }

    #[tokio::test]
    async fn an_inactive_tenant_is_skipped_by_the_walk() {
        let chain = static_chain().with_inactive(MIDDLE);
        let nodes = chain.chain(&ctx(), LEAF).await;
        // The topology is still traversed — ROOT is reachable through MIDDLE —
        // but the inactive tenant itself contributes nothing.
        assert_eq!(nodes.len(), 3);
        assert!(!nodes[1].active);
        assert!(nodes[2].active);
        // The inactive tenant is dropped from the ids the data plane walks.
        assert_eq!(chain.active_ids(&ctx(), LEAF).await, vec![LEAF, ROOT]);
    }

    fn upstream(tenant: Uuid, config: UpstreamConfig) -> Upstream {
        Upstream {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            created_at: 0,
            config,
        }
    }

    fn config() -> UpstreamConfig {
        UpstreamConfig {
            alias: Some(String::from("api.example.com")),
            ..UpstreamConfig::default()
        }
    }

    #[test]
    fn an_ancestor_disabled_upstream_stays_disabled() {
        let mut ancestor = config();
        ancestor.enabled = false;
        ancestor.tags = vec![String::from("inherited")];
        let selected = upstream(LEAF, config());
        let effective = effective_upstream(&selected, &[&upstream(ROOT, ancestor)]);
        assert!(!effective.config.enabled);
        assert_eq!(effective.config.tags, vec![String::from("inherited")]);
    }

    #[test]
    fn tags_are_an_add_only_union() {
        let mut ancestor = config();
        ancestor.tags = vec![String::from("partner"), String::from("shared")];
        let mut own = config();
        own.tags = vec![String::from("shared"), String::from("mine")];
        let effective = effective_upstream(&upstream(LEAF, own), &[&upstream(ROOT, ancestor)]);
        assert_eq!(
            effective.config.tags,
            vec![
                String::from("shared"),
                String::from("mine"),
                String::from("partner")
            ]
        );
    }

    #[test]
    fn inherit_mode_auth_is_overridden_by_the_descendant() {
        let mut ancestor = config();
        ancestor.auth = Some(crate::domain::dto::AuthConfig {
            plugin_type: Some(String::from("apikey")),
            sharing: Sharing::Inherit,
            config: serde_json::json!({ "key": "ancestor" }),
            ..AuthConfig::default()
        });
        let mut own = config();
        own.auth = Some(crate::domain::dto::AuthConfig {
            plugin_type: Some(String::from("apikey")),
            sharing: Sharing::Private,
            config: serde_json::json!({ "key": "mine" }),
            ..AuthConfig::default()
        });
        let effective =
            effective_upstream(&upstream(LEAF, own), &[&upstream(ROOT, ancestor.clone())]);
        assert_eq!(
            effective.config.auth.expect("own auth").config["key"],
            "mine"
        );

        // Without its own credentials the descendant inherits the ancestor's.
        let effective = effective_upstream(
            &upstream(LEAF, config()),
            &[&upstream(ROOT, ancestor.clone())],
        );
        assert_eq!(
            effective.config.auth.expect("inherited").config["key"],
            "ancestor"
        );

        // `private` ancestor credentials never flow down.
        ancestor.auth.as_mut().expect("auth").sharing = Sharing::Private;
        let effective = effective_upstream(&upstream(LEAF, config()), &[&upstream(ROOT, ancestor)]);
        assert!(effective.config.auth.is_none());
    }

    #[test]
    fn enforce_mode_auth_cannot_be_overridden() {
        let mut ancestor = config();
        ancestor.auth = Some(crate::domain::dto::AuthConfig {
            plugin_type: Some(String::from("apikey")),
            sharing: Sharing::Enforce,
            config: serde_json::json!({ "key": "enforced" }),
            ..AuthConfig::default()
        });
        let mut own = config();
        own.auth = Some(crate::domain::dto::AuthConfig {
            plugin_type: Some(String::from("apikey")),
            sharing: Sharing::Private,
            config: serde_json::json!({ "key": "mine" }),
            ..AuthConfig::default()
        });
        let effective = effective_upstream(&upstream(LEAF, own), &[&upstream(ROOT, ancestor)]);
        assert_eq!(
            effective.config.auth.expect("enforced").config["key"],
            "enforced"
        );
    }

    #[test]
    fn rate_limits_take_the_stricter_side() {
        let mut ancestor = config();
        ancestor.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Enforce,
            sustained: SustainedRate {
                rate: 10_000,
                window: crate::domain::dto::RateWindow::Minute,
            },
            burst: Burst { capacity: 100 },
            ..RateLimitConfig::default()
        });
        let mut own = config();
        own.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Private,
            sustained: SustainedRate {
                rate: 100,
                window: crate::domain::dto::RateWindow::Second,
            },
            burst: Burst { capacity: 5 },
            ..RateLimitConfig::default()
        });
        let effective = effective_upstream(
            &upstream(LEAF, own.clone()),
            &[&upstream(ROOT, ancestor.clone())],
        );
        let limit = effective.config.rate_limit.as_ref().expect("merged");
        // 100/s beats 10_000/min (≈166.7/s).
        assert_eq!(limit.sustained.rate, 100);
        assert_eq!(limit.burst.capacity, 5);
        // The ancestor caps a looser descendant: 10_000/min < 1000/s.
        own.rate_limit.as_mut().expect("own").sustained.rate = 1_000;
        let effective = effective_upstream(&upstream(LEAF, own), &[&upstream(ROOT, ancestor)]);
        let capped = effective.config.rate_limit.as_ref().expect("capped");
        assert_eq!(capped.sustained.rate, 10_000);
        assert_eq!(
            capped.sustained.window,
            crate::domain::dto::RateWindow::Minute
        );
    }

    #[test]
    fn cors_unions_origins_under_inherit_and_replaces_under_enforce() {
        let mut ancestor = config();
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec![String::from("https://partner.example")],
            ..CorsConfig::default()
        });
        let mut own = config();
        own.cors = Some(CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: vec![String::from("https://leaf.example")],
            ..CorsConfig::default()
        });
        let effective = effective_upstream(
            &upstream(LEAF, own.clone()),
            &[&upstream(ROOT, ancestor.clone())],
        );
        assert_eq!(
            effective.config.cors.expect("merged").allowed_origins,
            vec![
                String::from("https://leaf.example"),
                String::from("https://partner.example")
            ]
        );

        ancestor.cors.as_mut().expect("cors").sharing = Sharing::Enforce;
        let effective = effective_upstream(&upstream(LEAF, own), &[&upstream(ROOT, ancestor)]);
        assert_eq!(
            effective.config.cors.expect("enforced").allowed_origins,
            vec![String::from("https://partner.example")]
        );
    }

    #[test]
    fn ancestor_plugins_run_before_the_descendant_ones() {
        let mut ancestor = config();
        ancestor.plugins = crate::domain::dto::PluginBindings {
            sharing: Sharing::Enforce,
            items: vec![String::from(
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            )],
        };
        let mut own = config();
        own.plugins = crate::domain::dto::PluginBindings {
            sharing: Sharing::Private,
            items: vec![String::from(
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
            )],
        };
        let effective =
            effective_upstream(&upstream(LEAF, own), &[&upstream(ROOT, ancestor.clone())]);
        let items = effective.config.plugins.items;
        assert_eq!(items.len(), 2);
        assert!(items[0].ends_with("required_headers.v1"));
        assert!(items[1].ends_with("request_id.v1"));

        // A private ancestor plugin chain is invisible to the descendant.
        ancestor.plugins.sharing = Sharing::Private;
        let effective = effective_upstream(&upstream(LEAF, config()), &[&upstream(ROOT, ancestor)]);
        assert!(effective.config.plugins.items.is_empty());
    }

    #[test]
    fn only_shared_fields_count_as_visible() {
        assert!(!shares_with_descendants(&config()));
        let mut shared = config();
        shared.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Enforce,
            ..RateLimitConfig::default()
        });
        assert!(shares_with_descendants(&shared));
    }
}
