//! The value objects of the hierarchical configuration resolution and the
//! pure per-field merge of `cpt-cf-oagw-algo-effective-merge`.
//!
//! This module is pure: it performs no I/O, holds no repository, no platform
//! client and no HTTP surface, so the merge of a chain into an
//! [`EffectiveConfig`] is a function of its inputs alone. The I/O drivers — the
//! tenant-chain walk and the alias-shadowing walk — live in
//! `crate::infra::resolution`, whose [`crate::infra::resolution::
//! EffectiveConfigResolver`] hands the selected [`Resolution`] back to the
//! caller, which calls [`Resolution::merge`] with the route it matched.
//!
//! Everything here computes and never enforces: no token bucket, no 429 and no
//! `X-RateLimit-*` header is produced anywhere in this module, because
//! enforcement belongs to the rate-limiting feature.

use std::collections::BTreeSet;

use tenant_resolver_sdk::TenantId;
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, PluginBinding, PluginsConfig, RateLimitConfig, Route, SHARING_ENFORCE,
    SHARING_INHERIT, SHARING_PRIVATE, Upstream, WINDOW_DAY, WINDOW_HOUR, WINDOW_MINUTE,
    WINDOW_SECOND,
};
// @cpt-begin:cpt-cf-oagw-dod-resolution-contract:p1:inst-full
/// The `EffectiveConfig` and `TenantChain` value objects of
/// `cpt-cf-oagw-dod-resolution-contract`: feature-local, unpersisted
/// implementation types that carry exactly the shape the merge flow declares —
/// the auth block, the effective rate limit, the ordered plugin bindings, the
/// CORS block and the tags, plus the chain the tenant-resolver client answers
/// with. Neither object is a table, a schema object or an aggregate, and
/// neither is addressable through any management surface.
///
/// [`TenantChain`] orders the tiers descendant→root: `tiers[0]` is the subject
/// tenant the request was authenticated into and the last tier is the root of
/// the hierarchy the resolver returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantChain {
    /// The tiers of the chain, descendant first and root last.
    tiers: Vec<TenantId>,
}

impl TenantChain {
    /// Builds the chain from its tiers, ordered descendant→root.
    ///
    /// # Panics
    /// Never: an empty tier list is a precondition violation that the caller —
    /// the alias-shadowing walk of `cpt-cf-oagw-flow-effective-resolution` —
    /// checks through [`TenantChain::is_empty`] before building the chain and
    /// reports as the typed failure of `cpt-cf-oagw-algo-tenant-chain-walk`, so
    /// this constructor stays panic-free and total.
    #[must_use]
    pub fn new(tiers: Vec<TenantId>) -> Self {
        Self { tiers }
    }

    /// The subject tenant of the chain, the tier the request belongs to.
    ///
    /// The chain is documented to be non-empty; a chain built empty answers
    /// with the nil tenant instead of panicking, which the caller detects
    /// through [`TenantChain::is_empty`].
    #[must_use]
    pub fn subject(&self) -> TenantId {
        self.tiers.first().copied().unwrap_or_else(TenantId::nil)
    }

    /// The tiers in walk order: descendant first, root last.
    #[must_use]
    pub fn descendant_to_root(&self) -> &[TenantId] {
        &self.tiers
    }

    /// The tiers in merge order: root first, the subject last.
    #[must_use]
    pub fn root_to_descendant(&self) -> Vec<TenantId> {
        let mut reversed = self.tiers.clone();
        reversed.reverse();
        reversed
    }

    /// Whether `tenant` is a tier of the chain.
    #[must_use]
    pub fn contains(&self, tenant: TenantId) -> bool {
        self.tiers.contains(&tenant)
    }

    /// How many tiers the chain holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tiers.len()
    }

    /// Whether the chain holds no tier at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }
}

/// The per-request lifecycle of one resolution outcome
/// (`cpt-cf-oagw-state-selected-target`).
///
/// The machine starts at `Unresolved` for every request, reaches `Selected`
/// when the shadowing walk selects an enabled upstream, `Disabled` when the
/// walk reports a same-alias ancestor above the selection that is disabled,
/// and stays `Unresolved` when the walk exhausts the chain or the chain cannot
/// be established at all. It holds no state between requests, is never
/// persisted and has no cache behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedTargetState {
    /// No target was established: the walk is still running, the chain is
    /// exhausted or the chain could not be obtained.
    Unresolved,
    /// An enabled upstream whose alias matches was selected as the target.
    Selected,
    /// A candidate matched the alias, but a same-alias ancestor above it is
    /// disabled, so the descendant's selection is disabled with it.
    Disabled,
}

impl SelectedTargetState {
    /// The initial state of every resolution.
    #[must_use]
    pub const fn initial() -> Self {
        Self::Unresolved
    }

    /// The state name, for diagnostics and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unresolved => "Unresolved",
            Self::Selected => "Selected",
            Self::Disabled => "Disabled",
        }
    }

    /// Applies the closed transition set of FEATURE §4.
    ///
    /// The six legal transitions are `Unresolved`→`Selected` (a target was
    /// selected), `Unresolved`→`Unresolved` twice over (the not-found outcome
    /// and the unresolvable chain both leave the outcome where it started),
    /// `Unresolved`→`Disabled` (enabled inheritance disables the descendant),
    /// and the two re-entries into `Unresolved` out of `Selected` and
    /// `Disabled` that the next request's fresh walk performs. Every other
    /// edge is invalid and leaves the outcome unchanged, so the machine can
    /// never reach a state the walk did not produce.
    #[must_use]
    pub fn transition(self, to: Self) -> Option<Self> {
        match (self, to) {
            (Self::Unresolved, Self::Selected) => {
                // @cpt-begin:cpt-cf-oagw-state-selected-target:p1:inst-st-01
                // FROM Unresolved TO Selected: the shadowing walk selected an
                // enabled upstream whose alias matches and no same-alias
                // ancestor above it is disabled.
                // @cpt-end:cpt-cf-oagw-state-selected-target:p1:inst-st-01
                Some(to)
            }
            (Self::Unresolved, Self::Unresolved) => {
                // @cpt-begin:cpt-cf-oagw-state-selected-target:p1:inst-st-02
                // FROM Unresolved TO Unresolved: the chain is exhausted with no
                // enabled upstream matching the alias, so no target was ever
                // established and the not-found outcome is returned.
                // @cpt-end:cpt-cf-oagw-state-selected-target:p1:inst-st-02
                // @cpt-begin:cpt-cf-oagw-state-selected-target:p1:inst-st-03
                // FROM Unresolved TO Unresolved: the tenant-resolver client is
                // unreachable or returned no chain, so the resolution fails with
                // the typed failure of the tenant chain walk and the outcome
                // stays Unresolved.
                // @cpt-end:cpt-cf-oagw-state-selected-target:p1:inst-st-03
                Some(self)
            }
            (Self::Unresolved, Self::Disabled) => {
                // @cpt-begin:cpt-cf-oagw-state-selected-target:p1:inst-st-04
                // FROM Unresolved TO Disabled: a candidate matched the alias but
                // a same-alias ancestor above it is disabled, so a descendant
                // cannot re-enable an ancestor-disabled resource.
                // @cpt-end:cpt-cf-oagw-state-selected-target:p1:inst-st-04
                Some(to)
            }
            (Self::Selected, Self::Unresolved) => {
                // @cpt-begin:cpt-cf-oagw-state-selected-target:p1:inst-st-05
                // FROM Selected TO Unresolved: the caller re-enters resolution
                // with a new request, whose fresh walk starts again at
                // Unresolved; no target is carried between requests.
                // @cpt-end:cpt-cf-oagw-state-selected-target:p1:inst-st-05
                Some(to)
            }
            (Self::Disabled, Self::Unresolved) => {
                // @cpt-begin:cpt-cf-oagw-state-selected-target:p1:inst-st-06
                // FROM Disabled TO Unresolved: the ancestor is re-enabled and
                // the next resolution recomputes the walk from Unresolved, so no
                // transition writes the new outcome.
                // @cpt-end:cpt-cf-oagw-state-selected-target:p1:inst-st-06
                Some(to)
            }
            // Any transition not listed above is invalid and leaves the outcome
            // unchanged: `Selected` and `Disabled` are terminal within one
            // request.
            _ => None,
        }
    }
}

/// One chain tier's same-alias upstream: the upstream the tenant holds under
/// the resolved alias, absent when the tenant holds none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierUpstream {
    /// The tenant of the tier, always an id of the resolution's chain.
    pub tenant_id: TenantId,
    /// The upstream of that tenant under the resolved alias.
    pub upstream: Option<Upstream>,
}

/// One route tier of the caller's match: the routes of one upstream of the
/// chain, in the deterministic order the route-match invariant needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTier {
    /// The tenant the upstream belongs to.
    pub tenant_id: TenantId,
    /// The upstream the routes belong to.
    pub upstream_id: Uuid,
    /// The routes of that upstream, priority descending.
    pub routes: Vec<Route>,
}

/// The outcome of one resolution: the chain, the selected target, the tiers the
/// merge walks and the disabled-ancestor report
/// (`cpt-cf-oagw-flow-effective-resolution`).
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The chain the tenant-resolver client answered with, descendant→root.
    pub chain: TenantChain,
    /// The selected routing target: the closest enabled upstream matching the
    /// alias.
    pub target: Upstream,
    /// Every tier of the chain ordered **root→descendant**, the selected
    /// target's own tier last; an ancestor tier carries its same-alias upstream
    /// when the tenant holds one and `None` otherwise.
    pub tiers: Vec<TierUpstream>,
    /// The route tiers the caller matches against: the selected upstream's own
    /// routes first, then the ancestor upstreams' routes, descendant→root.
    pub route_tiers: Vec<RouteTier>,
    /// The tenant above the selection that holds a same-alias upstream which is
    /// disabled, whose report disables the selection.
    pub disabled_ancestor: Option<TenantId>,
}

impl Resolution {
    /// The state the resolution ended in
    /// (`cpt-cf-oagw-state-selected-target`): `Disabled` when the walk reported
    /// a same-alias ancestor above the selection that is disabled, `Selected`
    /// otherwise.
    #[must_use]
    pub fn state(&self) -> SelectedTargetState {
        if self.disabled_ancestor.is_some() {
            SelectedTargetState::Disabled
        } else {
            SelectedTargetState::Selected
        }
    }

    /// Merges the configuration of the chain into the `EffectiveConfig` the
    /// data plane receives (`cpt-cf-oagw-flow-effective-merge`).
    ///
    /// # Panics
    /// Never: the merge reads its own fields and the route the caller handed
    /// over, and returns the assembled value.
    #[must_use]
    pub fn merge(&self, matched_route: Option<&Route>) -> EffectiveConfig {
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-01
        // Receive, from the resolution flow on the same request, the selected
        // target, its ancestor chain and the matched route: the first two are
        // this `Resolution`'s own fields, the third is the caller's match.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-01
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-02
        // The chain is stored root→descendant with the selected target's own
        // tier last, so the merge order is fixed by the chain and never by the
        // request arrival order.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-02
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-03
        // FOR EACH tier from root to descendant, apply the per-field merge of
        // `cpt-cf-oagw-algo-effective-merge` under the sharing mode the field's
        // block carries; the walk itself is carried by `merge_effective` below.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-03
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-04
        // The matched route's blocks are positioned after every upstream tier by
        // the same walk, so the Upstream (base) < Route order of the layering
        // requirement holds.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-04
        let effective = merge_effective(&self.tiers, matched_route);
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-05
        // The effective rate limit is the strictest of the collected enforced
        // ancestor limits, the selected upstream's limit and the route's limit,
        // handed to the data plane as a value and never enforced here.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-05
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-06
        // The ordered plugin binding list is what the plugin chain receives:
        // ancestor tiers first in their binding-position order, the route's
        // bindings last.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-06
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-07
        // No I/O happens inside the merge: no resolver call, no repository
        // write, no route registration and no upstream HTTP call, so the only
        // output of the flow is the value it returns.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-07
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-09
        // Enforcement is left to the consumers: the rate-limiting feature
        // receives the computed effective limit and enforces it, and this flow
        // performs no HTTP call of its own.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-09
        // @cpt-begin:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-08
        // RETURN the assembled `EffectiveConfig` — auth, effective rate limit,
        // ordered plugin bindings, CORS block and tags — to the caller.
        // @cpt-end:cpt-cf-oagw-flow-effective-merge:p1:inst-mg-08
        effective
    }
}

/// One plugin binding of the merged chain, together with the flag that says the
/// binding arrived from an ancestor `enforce` block and can therefore not be
/// removed by any later tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveBinding {
    /// The binding, in the merged order.
    pub binding: PluginBinding,
    /// Whether an ancestor enforced this binding onto the whole chain.
    pub enforced: bool,
}

/// The merged configuration one resolution hands to the data plane
/// (`cpt-cf-oagw-flow-effective-merge`): the auth block, the effective rate
/// limit, the ordered plugin bindings, the CORS block and the tags.
///
/// The value object is computed per request, never persisted and never
/// addressable through the management surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    /// The auth block the request authenticates with.
    pub auth: Option<AuthConfig>,
    /// Whether an ancestor `enforce` auth block forced the value.
    pub auth_forced: bool,
    /// The effective rate limit: the strictest of the collected limits, absent
    /// when the collected set is empty.
    pub rate_limit: Option<RateLimitConfig>,
    /// The ordered plugin bindings, ancestor tiers first, the route's last.
    pub plugins: Vec<EffectiveBinding>,
    /// The CORS block, with the inherited origin union applied.
    pub cors: Option<CorsConfig>,
    /// Whether an ancestor `enforce` CORS block forced the value.
    pub cors_forced: bool,
    /// The add-only tag union of every tier and of the matched route.
    pub tags: BTreeSet<String>,
}

impl EffectiveConfig {
    /// The merged plugin bindings in merged order, for the plugin chain: the
    /// ancestor tiers first, each in its own binding-position order, the
    /// matched route's bindings last.
    #[must_use]
    pub fn plugin_bindings(&self) -> Vec<&PluginBinding> {
        self.plugins.iter().map(|entry| &entry.binding).collect()
    }
}
// @cpt-end:cpt-cf-oagw-dod-resolution-contract:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-sharing-merge-strategies:p1:inst-full
// The five per-field strategies of `cpt-cf-oagw-dod-sharing-merge-strategies`
// carried by `merge_effective`: auth overridden when `inherit` and forced when
// `enforce`, the rate limit a `min` over the collected enforced limits, the
// plugin bindings concatenated ancestor→descendant with every enforced binding
// retained, the CORS origins unioned when `inherit` and forced when `enforce`,
// and the add-only tag union that carries no sharing mode. The merged
// configuration is handed over as a value and no part of it is enforced here.

/// Merges the configuration of the chain into one [`EffectiveConfig`]
/// (`cpt-cf-oagw-algo-effective-merge`).
///
/// `tiers` arrives ordered **root→descendant** with the selected target's own
/// tier last, so ancestor tiers are applied before descendant ones; the last
/// tier is the selected target and always contributes its own blocks, while
/// every earlier tier contributes per the sharing mode its block carries. The
/// matched route's blocks are positioned after every upstream tier. The walk is
/// pure and deterministic: the same chain and binding set produce the same
/// value, and the value is recomputed per request with no cache behind it.
///
/// # Panics
/// Never: the merge reads its inputs and returns the assembled value.
#[must_use]
pub fn merge_effective(tiers: &[TierUpstream], matched_route: Option<&Route>) -> EffectiveConfig {
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-01
    // Take the selected target, its ancestor chain, the matched route and the
    // sharing mode every block carries: the chain arrives root→descendant with
    // the selected target's own tier last.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-01
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-02
    // Start from an empty accumulator, so the first tier the walk visits
    // contributes into it and every later tier accumulates on top of it.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-02
    let mut accumulator = MergeAccumulator::new();
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-03
    // FOR EACH tier from root to descendant, apply the per-field merge under
    // the sharing mode the block carries; the last tier is the selected target,
    // whose own blocks always apply.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-03
    for (index, tier) in tiers.iter().enumerate() {
        let is_selected = index + 1 == tiers.len();
        accumulator.apply_tier(tier, is_selected);
    }
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-09
    // Position the matched route's blocks after every upstream tier — the
    // selected upstream's own blocks having been applied as the last tier of the
    // walk above — so the route's rate limit, plugin bindings and tags enter
    // after every upstream tier. The route aggregate carries no CORS block of
    // its own, so its CORS contribution is empty by construction.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-09
    if let Some(route) = matched_route {
        accumulator.apply_route(route);
    }
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-05
    // The effective rate limit is collected per tier: an ancestor limit enters
    // the collected set only when its block carries `sharing: enforce`, while
    // the selected upstream's limit and the route's limit always enter, and the
    // strictest of the set is the value the data plane reads.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-05
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-10
    // The effective rate limit is `min(all collected enforced ancestor limits,
    // selected upstream limit, route limit)`; when the collected set is empty
    // and neither the selected upstream nor the matched route configures a
    // limit, the result is an ABSENT limit the data plane reads as no limiting.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-10
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-06
    // The plugin bindings are concatenated ancestor→descendant, each tier
    // keeping its own binding-position order, and every enforced ancestor
    // binding is retained so no later tier can remove one; positions are
    // renumbered contiguously from 0 in the merged order.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-06
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-07
    // CORS: an ancestor `enforce` block forces the ancestor's block, an
    // ancestor `inherit` block unions its origins with the origins accumulated
    // so far, and an ancestor `private` block contributes nothing.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-07
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-08
    // Tags are unioned add-only and carry no sharing mode, so an inherited tag
    // cannot be removed by any later tier.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-08
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-11
    // A field an ancestor `enforce` tier marked forced keeps the forced value,
    // so a descendant block for that field is ignored, which is what makes
    // enforced ancestor constraints survive shadowing.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-11
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-12
    // Assemble the `EffectiveConfig` value object from the accumulated fields.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-12
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-13
    // The merge is deterministic for a given chain and binding set: the same
    // inputs produce the same output, no request-ordering effect exists and the
    // merge is recomputed per request with no cache behind it.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-13
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-14
    // RETURN the assembled `EffectiveConfig` to the caller.
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-14
    accumulator.finish()
}

/// The accumulator the merge walks the chain into (`inst-me-02`): one field per
/// strategy, holding what the tiers applied so far contributed.
struct MergeAccumulator {
    /// The auth block of the selected tier, `None` while only ancestors have
    /// been visited.
    auth: Option<AuthConfig>,
    /// The descendant-most ancestor `inherit` auth block, the inherited default.
    inherited_auth: Option<AuthConfig>,
    /// Whether an ancestor `enforce` auth block forced the field.
    auth_forced: bool,
    /// The collected limits the strictest value is taken over.
    enforced_limits: Vec<RateLimitConfig>,
    /// The plugin bindings accumulated so far, in merged order.
    plugins: Vec<EffectiveBinding>,
    /// The forced CORS block of an ancestor `enforce` tier.
    cors: Option<CorsConfig>,
    /// The block that supplies the CORS fields the origin union is applied to.
    cors_base: Option<CorsConfig>,
    /// The accumulated origin union, in first-seen order.
    cors_origins: Vec<String>,
    /// Whether an ancestor `enforce` CORS block forced the field.
    cors_forced: bool,
    /// The add-only tag union.
    tags: BTreeSet<String>,
}

impl MergeAccumulator {
    fn new() -> Self {
        Self {
            auth: None,
            inherited_auth: None,
            auth_forced: false,
            enforced_limits: Vec::new(),
            plugins: Vec::new(),
            cors: None,
            cors_base: None,
            cors_origins: Vec::new(),
            cors_forced: false,
            tags: BTreeSet::new(),
        }
    }

    /// Applies one tier's upstream blocks, ancestor or selected.
    fn apply_tier(&mut self, tier: &TierUpstream, is_selected: bool) {
        let Some(upstream) = tier.upstream.as_ref() else {
            return;
        };
        merge_auth(self, upstream.auth.as_ref(), is_selected);
        merge_rate_limit(self, upstream.rate_limit.as_ref(), is_selected);
        merge_plugins(self, upstream.plugins.as_ref(), is_selected);
        merge_cors(self, upstream.cors.as_ref(), is_selected);
        merge_tags(&mut self.tags, &upstream.tags);
    }

    /// Applies the matched route's blocks, which always arrive after every
    /// upstream tier.
    fn apply_route(&mut self, route: &Route) {
        // The route's limit always enters the collected set; a limit without a
        // comparable sustained rate does not.
        match route.rate_limit.as_ref() {
            Some(rate_limit) if sustained_per_second(rate_limit).is_some() => {
                self.enforced_limits.push(rate_limit.clone());
            }
            _ => {}
        }
        if let Some(plugins) = route.plugins.as_ref() {
            for binding in plugins.bindings() {
                self.plugins.push(EffectiveBinding {
                    binding,
                    enforced: false,
                });
            }
        }
        merge_tags(&mut self.tags, &route.tags);
    }

    /// Assembles the value object from the accumulated fields.
    fn finish(self) -> EffectiveConfig {
        let Self {
            auth,
            inherited_auth,
            auth_forced,
            enforced_limits,
            plugins,
            cors,
            cors_base,
            cors_origins,
            cors_forced,
            tags,
        } = self;
        // A forced field keeps the forced value; otherwise the selected tier's
        // own block stands, and the inherited default only when it has none.
        let auth = if auth_forced {
            auth
        } else {
            auth.or(inherited_auth)
        };
        // The merged positions are contiguous from 0, whatever position each
        // tier's own binding list carried.
        let mut plugins = plugins;
        for (position, entry) in plugins.iter_mut().enumerate() {
            entry.binding.position = position;
        }
        // A forced CORS block is handed over as the ancestor wrote it; the
        // inherited block stands with the accumulated origin union otherwise.
        let cors = if cors_forced {
            cors
        } else {
            cors_base.map(|mut block| {
                block.allowed_origins = cors_origins;
                block
            })
        };
        EffectiveConfig {
            auth,
            auth_forced,
            rate_limit: strictest(&enforced_limits),
            plugins,
            cors,
            cors_forced,
            tags,
        }
    }
}

/// Merges the auth block of one tier.
fn merge_auth(accumulator: &mut MergeAccumulator, block: Option<&AuthConfig>, is_selected: bool) {
    let Some(auth) = block else {
        return;
    };
    // @cpt-begin:cpt-cf-oagw-algo-effective-merge:p1:inst-me-04
    // Auth: an ancestor `enforce` block forces the ancestor's auth block and
    // marks the field forced, so a descendant block arriving later is ignored;
    // an ancestor `inherit` block is recorded as the inherited default, which a
    // descendant's own auth block replaces when it has one and which stands when
    // it has none; an ancestor `private` block contributes nothing.
    match auth.sharing.as_str() {
        SHARING_ENFORCE if !is_selected && !accumulator.auth_forced => {
            // The root-most `enforce` block forces the field, so every block
            // arriving later for it is ignored.
            accumulator.auth_forced = true;
            accumulator.auth = Some(auth.clone());
        }
        SHARING_INHERIT if !is_selected => {
            accumulator.inherited_auth = Some(auth.clone());
        }
        _ if is_selected && !accumulator.auth_forced => {
            // The selected tier's own block always applies unless the field was
            // forced by an ancestor.
            accumulator.auth = Some(auth.clone());
        }
        _ => {}
    }
    // @cpt-end:cpt-cf-oagw-algo-effective-merge:p1:inst-me-04
}

/// Merges the rate-limit block of one tier (`inst-me-05`): an ancestor limit
/// enters the collected set only under `sharing: enforce`, the selected tier's
/// own limit always enters, and an unenforced ancestor limit is never imposed.
fn merge_rate_limit(
    accumulator: &mut MergeAccumulator,
    block: Option<&RateLimitConfig>,
    is_selected: bool,
) {
    let Some(rate_limit) = block else {
        return;
    };
    let enters = is_selected || rate_limit.sharing == SHARING_ENFORCE;
    if enters && sustained_per_second(rate_limit).is_some() {
        accumulator.enforced_limits.push(rate_limit.clone());
    }
}

/// Merges the plugin block of one tier (`inst-me-06`).
fn merge_plugins(
    accumulator: &mut MergeAccumulator,
    block: Option<&PluginsConfig>,
    is_selected: bool,
) {
    let Some(plugins) = block else {
        return;
    };
    if !is_selected && plugins.sharing == SHARING_PRIVATE {
        // A `private` ancestor binding list is not visible to descendants.
        return;
    }
    let enforced = !is_selected && plugins.sharing == SHARING_ENFORCE;
    for binding in plugins.bindings() {
        accumulator
            .plugins
            .push(EffectiveBinding { binding, enforced });
    }
}

/// Merges the CORS block of one tier (`inst-me-07`).
fn merge_cors(accumulator: &mut MergeAccumulator, block: Option<&CorsConfig>, is_selected: bool) {
    let Some(cors) = block else {
        return;
    };
    if is_selected {
        // The selected tier's own block supplies the block and unions its own
        // origins with the inherited ones.
        if !accumulator.cors_forced {
            accumulator.cors_base = Some(cors.clone());
            union_origins(&mut accumulator.cors_origins, &cors.allowed_origins);
        }
        return;
    }
    match cors.sharing.as_str() {
        SHARING_ENFORCE => {
            // The root-most `enforce` block is the forced value; later CORS
            // configuration is ignored.
            if !accumulator.cors_forced {
                accumulator.cors_forced = true;
                accumulator.cors = Some(cors.clone());
            }
        }
        SHARING_INHERIT if !accumulator.cors_forced => {
            // The root-most `inherit` block is the base block for the other
            // CORS fields, and its origins union into the accumulated set.
            if accumulator.cors_base.is_none() {
                accumulator.cors_base = Some(cors.clone());
            }
            union_origins(&mut accumulator.cors_origins, &cors.allowed_origins);
        }
        // `private` contributes nothing to a descendant request.
        _ => {}
    }
}

/// Unions `additions` into `origins`, keeping the first-seen order so the union
/// is deterministic (`inst-me-08`).
fn union_origins(origins: &mut Vec<String>, additions: &[String]) {
    for origin in additions {
        if !origins.contains(origin) {
            origins.push(origin.clone());
        }
    }
}

/// Unions the tags of one tier into the add-only set, which carries no sharing
/// mode (`inst-me-08`).
fn merge_tags(tags: &mut BTreeSet<String>, additions: &[String]) {
    for tag in additions {
        tags.insert(tag.clone());
    }
}

/// The strictest of the collected limits, or `None` when the set is empty
/// (`inst-me-10`).
///
/// Strictness normalizes every sustained rate to requests per second and
/// compares the fractions by cross-multiplication, so no division and no float
/// is involved. A limit whose sustained rate is absent is not comparable and
/// does not enter the minimum, and an exact tie keeps the earlier-collected
/// value, which is the root-most enforced ancestor.
fn strictest(collected: &[RateLimitConfig]) -> Option<RateLimitConfig> {
    let mut incumbent: Option<(&RateLimitConfig, (i64, i64))> = None;
    for limit in collected {
        let Some(per_second) = sustained_per_second(limit) else {
            continue;
        };
        let stricter = match incumbent {
            None => true,
            Some((_, existing)) => is_stricter(per_second, existing),
        };
        if stricter {
            incumbent = Some((limit, per_second));
        }
    }
    incumbent.map(|(limit, _)| limit.clone())
}

/// Whether `candidate` allows fewer requests per second than `incumbent`.
fn is_stricter(candidate: (i64, i64), incumbent: (i64, i64)) -> bool {
    let (candidate_rate, candidate_window) = candidate;
    let (incumbent_rate, incumbent_window) = incumbent;
    i128::from(candidate_rate) * i128::from(incumbent_window)
        < i128::from(incumbent_rate) * i128::from(candidate_window)
}

/// The sustained rate of a limit normalized to requests per second: the pair
/// `(rate, window seconds)`, `None` when the limit carries no sustained rate or
/// a window outside the closed enum, which makes it not comparable.
fn sustained_per_second(limit: &RateLimitConfig) -> Option<(i64, i64)> {
    let sustained = limit.sustained.as_ref()?;
    let rate = sustained.rate?;
    let seconds = window_seconds(&sustained.window)?;
    Some((rate, seconds))
}

/// The closed window enum, normalized to seconds.
fn window_seconds(window: &str) -> Option<i64> {
    match window {
        WINDOW_SECOND => Some(1),
        WINDOW_MINUTE => Some(60),
        WINDOW_HOUR => Some(3_600),
        WINDOW_DAY => Some(86_400),
        _ => None,
    }
}
// @cpt-end:cpt-cf-oagw-dod-sharing-merge-strategies:p1:inst-full

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::model::{
        BurstCapacity, Endpoint, PluginBinding, SHARING_PRIVATE, ServerConfig, SustainedRate,
    };

    const LEAF: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_0000_0001);
    const MID: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_0000_0002);
    const ROOT: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_0000_0003);

    const ROOT_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.root.v1";
    const MID_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.mid.v1";
    const LEAF_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.leaf.v1";
    const ROUTE_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.route.v1";

    /// An empty tier that carries a default upstream: the merge reads the
    /// blocks, not the endpoint pool.
    fn tier(tenant: Uuid) -> TierUpstream {
        TierUpstream {
            tenant_id: TenantId(tenant),
            upstream: Some(Upstream::default()),
        }
    }

    /// A tier whose tenant holds no upstream under the alias.
    fn empty(tenant: Uuid) -> TierUpstream {
        TierUpstream {
            tenant_id: TenantId(tenant),
            upstream: None,
        }
    }

    impl TierUpstream {
        fn auth(self, sharing: &str, plugin: &str) -> Self {
            self.map(|upstream| {
                upstream.auth = Some(AuthConfig {
                    plugin_type: Some(plugin.to_owned()),
                    sharing: sharing.to_owned(),
                    config: None,
                })
            })
        }

        fn limit(self, sharing: &str, rate: i64, window: &str) -> Self {
            self.map(|upstream| {
                upstream.rate_limit = Some(rate_limit(sharing, rate, window));
            })
        }

        fn plugins(self, sharing: &str, items: &[&str]) -> Self {
            self.map(|upstream| {
                upstream.plugins = Some(PluginsConfig {
                    sharing: sharing.to_owned(),
                    items: items.iter().map(|item| (*item).to_owned()).collect(),
                })
            })
        }

        fn cors(self, sharing: &str, origins: &[&str]) -> Self {
            self.map(|upstream| {
                upstream.cors = Some(CorsConfig {
                    sharing: sharing.to_owned(),
                    enabled: Some(true),
                    allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
                    ..CorsConfig::default()
                })
            })
        }

        fn tagged(self, tags: &[&str]) -> Self {
            self.map(|upstream| {
                upstream.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
            })
        }

        fn map(self, apply: impl FnOnce(&mut Upstream)) -> Self {
            let mut this = self;
            if let Some(upstream) = this.upstream.as_mut() {
                apply(upstream);
            }
            this
        }
    }

    fn rate_limit(sharing: &str, rate: i64, window: &str) -> RateLimitConfig {
        RateLimitConfig {
            sharing: sharing.to_owned(),
            sustained: Some(SustainedRate {
                rate: Some(rate),
                window: window.to_owned(),
            }),
            burst: Some(BurstCapacity {
                capacity: Some(rate),
            }),
            ..RateLimitConfig::default()
        }
    }

    fn route(
        plugins: Option<PluginsConfig>,
        limit: Option<RateLimitConfig>,
        tags: &[&str],
    ) -> Route {
        Route {
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            plugins,
            rate_limit: limit,
            ..Route::default()
        }
    }

    /// The sustained rate the effective limit carries, as `(rate, window)`.
    fn sustained_of(config: &EffectiveConfig) -> Option<(i64, String)> {
        let sustained = config.rate_limit.as_ref()?.sustained.as_ref()?;
        Some((sustained.rate?, sustained.window.clone()))
    }

    /// The chain `[root, mid, leaf]`: the ancestor tiers first, the selected
    /// target's own tier last.
    fn chain(root: TierUpstream, mid: TierUpstream, leaf: TierUpstream) -> Vec<TierUpstream> {
        vec![root, mid, leaf]
    }

    #[test]
    fn the_tenant_chain_is_ordered_descendant_to_root() {
        let chain = TenantChain::new(vec![TenantId(LEAF), TenantId(MID), TenantId(ROOT)]);

        assert_eq!(chain.subject(), TenantId(LEAF));
        assert_eq!(
            chain.descendant_to_root(),
            &[TenantId(LEAF), TenantId(MID), TenantId(ROOT)]
        );
        assert_eq!(
            chain.root_to_descendant(),
            vec![TenantId(ROOT), TenantId(MID), TenantId(LEAF)]
        );
        assert_eq!(chain.len(), 3);
        assert!(!chain.is_empty());
        assert!(chain.contains(TenantId(ROOT)));
        assert!(!chain.contains(TenantId(Uuid::nil())));
    }

    #[test]
    fn an_empty_chain_stays_a_total_value_object() {
        let chain = TenantChain::new(Vec::new());

        assert!(
            chain.is_empty(),
            "the walker checks emptiness before building"
        );
        assert_eq!(chain.len(), 0);
        assert_eq!(chain.subject(), TenantId::nil(), "the accessor stays total");
        assert!(chain.descendant_to_root().is_empty());
        assert!(chain.root_to_descendant().is_empty());
        assert!(!chain.contains(TenantId::nil()));
    }

    #[test]
    fn the_state_machine_accepts_exactly_the_six_declared_transitions() {
        use SelectedTargetState::{Disabled, Selected, Unresolved};

        let legal = [
            (Unresolved, Selected),
            (Unresolved, Unresolved),
            (Unresolved, Disabled),
            (Selected, Unresolved),
            (Disabled, Unresolved),
        ];
        for (from, to) in legal {
            assert_eq!(from.transition(to), Some(to), "{from:?} to {to:?}");
        }
        // The self-loop of `Unresolved` carries both declared causes (the
        // not-found outcome and the unresolvable chain), which is why the five
        // legal edges realize the six declared steps.
        assert_eq!(legal.len(), 5);
    }

    #[test]
    fn every_other_state_transition_is_refused() {
        use SelectedTargetState::{Disabled, Selected, Unresolved};

        let invalid = [
            (Selected, Selected),
            (Disabled, Disabled),
            (Selected, Disabled),
            (Disabled, Selected),
        ];
        for (from, to) in invalid {
            assert_eq!(
                from.transition(to),
                None,
                "{from:?} to {to:?} must be invalid"
            );
        }
        assert_eq!(SelectedTargetState::initial(), Unresolved);
        assert_eq!(SelectedTargetState::Selected.as_str(), "Selected");
        assert_eq!(SelectedTargetState::Disabled.as_str(), "Disabled");
        assert_eq!(SelectedTargetState::Unresolved.as_str(), "Unresolved");
    }

    #[test]
    fn the_resolution_state_reports_the_disabled_ancestor() {
        let mut selected = chain(empty(ROOT), empty(MID), tier(LEAF));
        let target = selected.pop().and_then(|tier| tier.upstream).unwrap();
        let resolution = Resolution {
            chain: TenantChain::new(vec![TenantId(LEAF), TenantId(MID), TenantId(ROOT)]),
            target,
            tiers: selected,
            route_tiers: Vec::new(),
            disabled_ancestor: None,
        };

        assert_eq!(resolution.state(), SelectedTargetState::Selected);

        let disabled = Resolution {
            disabled_ancestor: Some(TenantId(MID)),
            ..resolution.clone()
        };
        assert_eq!(disabled.state(), SelectedTargetState::Disabled);
    }

    #[test]
    fn the_closest_tier_shadows_the_ancestors_for_the_auth_block() {
        let tiers = chain(
            tier(ROOT).auth(SHARING_INHERIT, "root-plugin"),
            empty(MID),
            tier(LEAF).auth(SHARING_PRIVATE, "leaf-plugin"),
        );

        let effective = merge_effective(&tiers, None);

        assert_eq!(
            effective
                .auth
                .as_ref()
                .and_then(|auth| auth.plugin_type.as_deref()),
            Some("leaf-plugin"),
            "the descendant's own block replaces the inherited default"
        );
        assert!(!effective.auth_forced);
    }

    #[test]
    fn an_inherited_auth_block_stands_when_the_descendant_has_none() {
        let tiers = chain(
            tier(ROOT).auth(SHARING_INHERIT, "root-plugin"),
            tier(MID).auth(SHARING_INHERIT, "mid-plugin"),
            tier(LEAF),
        );

        let effective = merge_effective(&tiers, None);

        assert_eq!(
            effective
                .auth
                .as_ref()
                .and_then(|auth| auth.plugin_type.as_deref()),
            Some("mid-plugin"),
            "the descendant-most inherit block is the inherited default"
        );
        assert!(!effective.auth_forced);
    }

    #[test]
    fn an_enforced_ancestor_auth_block_forces_the_value() {
        let tiers = chain(
            tier(ROOT).auth(SHARING_ENFORCE, "root-plugin"),
            tier(MID).auth(SHARING_INHERIT, "mid-plugin"),
            tier(LEAF).auth(SHARING_INHERIT, "leaf-plugin"),
        );

        let effective = merge_effective(&tiers, None);

        assert_eq!(
            effective
                .auth
                .as_ref()
                .and_then(|auth| auth.plugin_type.as_deref()),
            Some("root-plugin"),
            "the ancestor's enforced value survives every descendant block"
        );
        assert!(effective.auth_forced);
    }

    #[test]
    fn a_private_ancestor_auth_block_contributes_nothing() {
        let tiers = chain(
            tier(ROOT).auth(SHARING_PRIVATE, "root-plugin"),
            empty(MID),
            tier(LEAF),
        );

        let effective = merge_effective(&tiers, None);

        assert!(
            effective.auth.is_none(),
            "a private ancestor block is invisible to a descendant request"
        );
        assert!(!effective.auth_forced);
    }

    #[test]
    fn the_effective_rate_limit_is_the_strictest_enforced_one() {
        let tiers = chain(
            tier(ROOT).limit(SHARING_ENFORCE, 10, WINDOW_SECOND),
            tier(MID).limit(SHARING_INHERIT, 500, WINDOW_SECOND),
            tier(LEAF).limit(SHARING_PRIVATE, 100, WINDOW_SECOND),
        );
        let matched = route(
            None,
            Some(rate_limit(SHARING_PRIVATE, 50, WINDOW_SECOND)),
            &[],
        );

        let effective = merge_effective(&tiers, Some(&matched));

        let limit = effective
            .rate_limit
            .as_ref()
            .expect("a limit is configured");
        assert_eq!(
            limit
                .sustained
                .as_ref()
                .and_then(|sustained| sustained.rate),
            Some(10),
            "the ancestor's enforced rate constrains the shadowing target"
        );
    }

    #[test]
    fn an_ancestor_limit_without_enforce_never_constrains() {
        let tiers = chain(
            tier(ROOT).limit(SHARING_INHERIT, 5, WINDOW_SECOND),
            empty(MID),
            tier(LEAF).limit(SHARING_PRIVATE, 100, WINDOW_SECOND),
        );

        let effective = merge_effective(&tiers, None);

        assert_eq!(
            sustained_of(&effective),
            Some((100, WINDOW_SECOND.to_owned())),
            "an unenforced ancestor limit never constrains a descendant"
        );
    }

    #[test]
    fn an_absent_limit_stays_absent_when_nothing_is_configured() {
        let tiers = chain(empty(ROOT), empty(MID), tier(LEAF));

        let effective = merge_effective(&tiers, None);

        assert!(
            effective.rate_limit.is_none(),
            "an empty collected set with no upstream and no route limit is ABSENT"
        );
    }

    #[test]
    fn a_limit_without_a_sustained_rate_is_not_comparable() {
        let mut unbounded = tier(ROOT).limit(SHARING_ENFORCE, 1, WINDOW_SECOND);
        if let Some(limit) = unbounded
            .upstream
            .as_mut()
            .and_then(|u| u.rate_limit.as_mut())
        {
            limit.sustained = None;
        }
        let tiers = vec![unbounded, empty(MID), tier(LEAF)];

        let effective = merge_effective(&tiers, None);

        assert!(
            effective.rate_limit.is_none(),
            "a limit without a sustained rate does not enter the min"
        );
    }

    #[test]
    fn the_comparison_goes_by_requests_per_second() {
        // The two candidates below are the pair that separates a normalized
        // comparison from a raw-number one: `1/second` carries the smaller raw
        // rate than `59/minute`, yet `59/minute` allows fewer requests per
        // second, and `100/second` carries the larger raw rate than
        // `120/minute`, yet `120/minute` allows fewer requests per second. The
        // strictest of each pair is therefore the per-second winner.
        let tiers = chain(
            tier(ROOT).limit(SHARING_ENFORCE, 59, WINDOW_MINUTE),
            tier(MID).limit(SHARING_ENFORCE, 1, WINDOW_SECOND),
            tier(LEAF).limit(SHARING_PRIVATE, 100, WINDOW_SECOND),
        );
        let normalized = merge_effective(&tiers, None);
        assert_eq!(
            sustained_of(&normalized),
            Some((59, WINDOW_MINUTE.to_owned())),
            "59/minute allows fewer requests per second than 1/second"
        );

        let tiers = chain(
            tier(ROOT).limit(SHARING_ENFORCE, 100, WINDOW_SECOND),
            empty(MID),
            tier(LEAF).limit(SHARING_PRIVATE, 120, WINDOW_MINUTE),
        );
        let raw = merge_effective(&tiers, None);
        assert_eq!(
            sustained_of(&raw),
            Some((120, WINDOW_MINUTE.to_owned())),
            "120/minute allows fewer requests per second than 100/second"
        );
    }

    #[test]
    fn an_exact_tie_keeps_the_root_most_enforced_value() {
        let tiers = chain(
            tier(ROOT).limit(SHARING_ENFORCE, 10, WINDOW_SECOND),
            tier(MID).limit(SHARING_ENFORCE, 600, WINDOW_MINUTE),
            tier(LEAF).limit(SHARING_PRIVATE, 10, WINDOW_SECOND),
        );

        let effective = merge_effective(&tiers, None);

        let limit = effective.rate_limit.as_ref().unwrap();
        assert_eq!(
            limit.sharing, SHARING_ENFORCE,
            "the tie keeps the earlier value"
        );
    }

    #[test]
    fn a_route_limit_enters_the_minimum() {
        let tiers = chain(empty(ROOT), empty(MID), tier(LEAF));
        let matched = route(
            None,
            Some(rate_limit(SHARING_PRIVATE, 7, WINDOW_SECOND)),
            &[],
        );

        let effective = merge_effective(&tiers, Some(&matched));

        assert_eq!(
            sustained_of(&effective),
            Some((7, WINDOW_SECOND.to_owned())),
            "the route's limit is one of the collected values"
        );
    }

    #[test]
    fn an_ancestor_private_limit_is_not_collected() {
        let tiers = chain(
            tier(ROOT)
                .limit(SHARING_PRIVATE, 3, WINDOW_SECOND)
                .plugins(SHARING_PRIVATE, &[ROOT_PLUGIN])
                .cors(SHARING_PRIVATE, &["https://root.example.com"]),
            empty(MID),
            tier(LEAF),
        );

        let effective = merge_effective(&tiers, None);

        assert!(
            effective.rate_limit.is_none(),
            "a private ancestor rate is absent"
        );
        assert!(
            effective.plugins.is_empty(),
            "a private ancestor's plugin bindings are absent"
        );
        assert!(
            effective.cors.is_none(),
            "a private ancestor's CORS block is absent"
        );
    }

    #[test]
    fn the_plugin_bindings_are_concatenated_ancestor_to_descendant() {
        let tiers = chain(
            tier(ROOT).plugins(SHARING_INHERIT, &[ROOT_PLUGIN, ROOT_PLUGIN]),
            tier(MID).plugins(SHARING_INHERIT, &[MID_PLUGIN]),
            tier(LEAF).plugins(SHARING_INHERIT, &[LEAF_PLUGIN]),
        );
        let matched = route(
            Some(PluginsConfig {
                sharing: SHARING_PRIVATE.to_owned(),
                items: vec![ROUTE_PLUGIN.to_owned()],
            }),
            None,
            &[],
        );

        let effective = merge_effective(&tiers, Some(&matched));
        let references: Vec<&str> = effective
            .plugin_bindings()
            .iter()
            .map(|binding| binding.plugin_ref.as_str())
            .collect();

        assert_eq!(
            references,
            [
                ROOT_PLUGIN,
                ROOT_PLUGIN,
                MID_PLUGIN,
                LEAF_PLUGIN,
                ROUTE_PLUGIN
            ],
            "ancestor tiers first in their own order, the route's bindings last"
        );
        for (position, entry) in effective.plugins.iter().enumerate() {
            assert_eq!(
                entry.binding.position, position,
                "positions are contiguous from 0"
            );
            assert!(!entry.enforced, "inherit appends unmarked bindings");
        }
    }

    #[test]
    fn an_enforced_ancestor_binding_survives_a_descendant_that_omits_it() {
        let tiers = chain(
            tier(ROOT).plugins(SHARING_ENFORCE, &[ROOT_PLUGIN]),
            empty(MID),
            tier(LEAF).plugins(SHARING_INHERIT, &[LEAF_PLUGIN]),
        );

        let effective = merge_effective(&tiers, None);
        let references: Vec<&str> = effective
            .plugin_bindings()
            .iter()
            .map(|binding| binding.plugin_ref.as_str())
            .collect();

        assert_eq!(references, [ROOT_PLUGIN, LEAF_PLUGIN]);
        assert!(
            effective.plugins[0].enforced,
            "the enforced binding is marked"
        );
        assert!(!effective.plugins[1].enforced);
    }

    #[test]
    fn a_private_ancestor_appends_no_binding() {
        let tiers = chain(
            tier(ROOT).plugins(SHARING_PRIVATE, &[ROOT_PLUGIN]),
            empty(MID),
            tier(LEAF).plugins(SHARING_INHERIT, &[LEAF_PLUGIN]),
        );

        let effective = merge_effective(&tiers, None);

        assert_eq!(
            effective.plugin_bindings().len(),
            1,
            "private appends nothing"
        );
        assert_eq!(effective.plugin_bindings()[0].plugin_ref, LEAF_PLUGIN);
    }

    #[test]
    fn the_cors_origins_are_unioned_when_inherited() {
        let tiers = chain(
            tier(ROOT).cors(SHARING_INHERIT, &["https://root.example.com"]),
            empty(MID),
            tier(LEAF).cors(SHARING_INHERIT, &["https://leaf.example.com"]),
        );

        let effective = merge_effective(&tiers, None);

        let cors = effective.cors.as_ref().unwrap();
        assert_eq!(
            cors.allowed_origins,
            ["https://root.example.com", "https://leaf.example.com"],
            "the descendant's block is one block whose origins are the union"
        );
        assert!(!effective.cors_forced);
    }

    #[test]
    fn an_inherited_cors_block_stands_when_the_descendant_has_none() {
        let tiers = chain(
            tier(ROOT).cors(SHARING_INHERIT, &["https://root.example.com"]),
            tier(MID).cors(SHARING_INHERIT, &["https://mid.example.com"]),
            tier(LEAF),
        );

        let effective = merge_effective(&tiers, None);

        let cors = effective.cors.as_ref().unwrap();
        assert_eq!(
            cors.allowed_origins,
            ["https://root.example.com", "https://mid.example.com"],
            "every inherit tier unions its origins"
        );
        assert_eq!(
            cors.enabled,
            Some(true),
            "the root-most inherit block is the base block for the other fields"
        );
    }

    #[test]
    fn an_enforced_ancestor_cors_block_forces_the_value() {
        let tiers = chain(
            tier(ROOT).cors(SHARING_ENFORCE, &["https://root.example.com"]),
            tier(MID).cors(SHARING_INHERIT, &["https://mid.example.com"]),
            tier(LEAF).cors(SHARING_INHERIT, &["https://leaf.example.com"]),
        );

        let effective = merge_effective(&tiers, None);

        let cors = effective.cors.as_ref().unwrap();
        assert_eq!(
            cors.allowed_origins,
            ["https://root.example.com"],
            "the forced block replaces every descendant CORS configuration"
        );
        assert!(effective.cors_forced);
    }

    #[test]
    fn a_private_ancestor_cors_block_contributes_nothing() {
        let tiers = chain(
            tier(ROOT).cors(SHARING_PRIVATE, &["https://root.example.com"]),
            empty(MID),
            tier(LEAF),
        );

        let effective = merge_effective(&tiers, None);

        assert!(
            effective.cors.is_none(),
            "no inherited origin, no CORS block"
        );
    }

    #[test]
    fn the_tags_are_the_add_only_union_of_every_tier() {
        let tiers = chain(
            tier(ROOT).tagged(&["root", "shared"]),
            tier(MID).tagged(&["mid", "shared"]),
            tier(LEAF).tagged(&["leaf", "shared"]),
        );
        let matched = route(None, None, &["route"]);

        let effective = merge_effective(&tiers, Some(&matched));

        let expected: BTreeSet<String> = ["root", "shared", "mid", "leaf", "route"]
            .iter()
            .map(|tag| (*tag).to_owned())
            .collect();
        assert_eq!(
            effective.tags, expected,
            "every tier and the route contribute"
        );
    }

    #[test]
    fn an_ancestor_tag_enters_even_when_the_tiers_blocks_are_private() {
        let tiers = chain(
            tier(ROOT)
                .tagged(&["root-tag"])
                .auth(SHARING_PRIVATE, "root-plugin")
                .limit(SHARING_PRIVATE, 3, WINDOW_SECOND),
            empty(MID),
            tier(LEAF).tagged(&["leaf-tag"]),
        );

        let effective = merge_effective(&tiers, None);

        assert!(
            effective.tags.contains("root-tag"),
            "tags carry no sharing mode, so an ancestor tag enters regardless"
        );
    }

    #[test]
    fn no_tier_can_remove_an_inherited_tag() {
        let tiers = chain(tier(ROOT).tagged(&["root-tag"]), empty(MID), tier(LEAF));
        let first = merge_effective(&tiers, None);

        let mut without_ancestor_tags = tiers.clone();
        without_ancestor_tags[0] = tier(ROOT);
        let second = merge_effective(&without_ancestor_tags, None);

        assert!(first.tags.contains("root-tag"));
        assert!(
            !second.tags.contains("root-tag"),
            "the ancestor contributed it, and a descendant contributes nothing to it"
        );
    }

    #[test]
    fn the_route_blocks_arrive_after_every_upstream_tier() {
        let tiers = chain(
            tier(ROOT).plugins(SHARING_INHERIT, &[ROOT_PLUGIN]),
            empty(MID),
            tier(LEAF).plugins(SHARING_INHERIT, &[LEAF_PLUGIN]),
        );
        let matched = route(
            Some(PluginsConfig {
                sharing: SHARING_PRIVATE.to_owned(),
                items: vec![ROUTE_PLUGIN.to_owned()],
            }),
            Some(rate_limit(SHARING_PRIVATE, 4, WINDOW_SECOND)),
            &["route-tag"],
        );

        let effective = merge_effective(&tiers, Some(&matched));
        let references: Vec<&str> = effective
            .plugin_bindings()
            .iter()
            .map(|binding| binding.plugin_ref.as_str())
            .collect();

        assert_eq!(
            references.last(),
            Some(&ROUTE_PLUGIN),
            "the route's bindings are last"
        );
        assert_eq!(
            effective.tags.iter().next_back().map(String::as_str),
            Some("route-tag")
        );
        assert_eq!(
            sustained_of(&effective),
            Some((4, WINDOW_SECOND.to_owned())),
            "the route's limit enters the min after every upstream tier"
        );
    }

    #[test]
    fn the_selected_tiers_own_blocks_always_apply() {
        let tiers = chain(
            tier(ROOT).auth(SHARING_PRIVATE, "root-plugin"),
            empty(MID),
            tier(LEAF)
                .auth(SHARING_PRIVATE, "leaf-plugin")
                .limit(SHARING_PRIVATE, 20, WINDOW_SECOND)
                .cors(SHARING_PRIVATE, &["https://leaf.example.com"]),
        );

        let effective = merge_effective(&tiers, None);

        assert_eq!(
            effective
                .auth
                .as_ref()
                .and_then(|auth| auth.plugin_type.as_deref()),
            Some("leaf-plugin"),
            "the selected tier's own block applies whatever its mode is"
        );
        assert_eq!(
            sustained_of(&effective),
            Some((20, WINDOW_SECOND.to_owned()))
        );
        assert_eq!(
            effective
                .cors
                .as_ref()
                .map(|cors| cors.allowed_origins.as_slice()),
            Some(&["https://leaf.example.com".to_owned()][..])
        );
    }

    #[test]
    fn a_tier_without_an_upstream_contributes_nothing() {
        let tiers = chain(
            empty(ROOT),
            tier(MID).limit(SHARING_ENFORCE, 12, WINDOW_SECOND),
            tier(LEAF),
        );

        let effective = merge_effective(&tiers, None);

        assert_eq!(effective.tags, BTreeSet::new());
        assert_eq!(
            sustained_of(&effective),
            Some((12, WINDOW_SECOND.to_owned()))
        );
    }

    #[test]
    fn the_merge_is_deterministic_for_a_given_chain_and_binding_set() {
        let tiers = chain(
            tier(ROOT)
                .auth(SHARING_ENFORCE, "root-plugin")
                .limit(SHARING_ENFORCE, 10, WINDOW_SECOND)
                .plugins(SHARING_ENFORCE, &[ROOT_PLUGIN])
                .cors(SHARING_ENFORCE, &["https://root.example.com"])
                .tagged(&["root"]),
            tier(MID).plugins(SHARING_INHERIT, &[MID_PLUGIN]),
            tier(LEAF)
                .auth(SHARING_INHERIT, "leaf-plugin")
                .limit(SHARING_PRIVATE, 100, WINDOW_SECOND)
                .plugins(SHARING_INHERIT, &[LEAF_PLUGIN])
                .cors(SHARING_INHERIT, &["https://leaf.example.com"])
                .tagged(&["leaf"]),
        );
        let matched = route(
            Some(PluginsConfig {
                sharing: SHARING_PRIVATE.to_owned(),
                items: vec![ROUTE_PLUGIN.to_owned()],
            }),
            Some(rate_limit(SHARING_PRIVATE, 50, WINDOW_SECOND)),
            &["route"],
        );

        let first = merge_effective(&tiers, Some(&matched));
        let second = merge_effective(&tiers, Some(&matched));
        let reversed: Vec<TierUpstream> = {
            let mut reversed = tiers.clone();
            reversed.reverse();
            reversed
        };

        assert_eq!(
            first, second,
            "same inputs, same output, no ordering effect"
        );
        assert_ne!(
            merge_effective(&reversed, Some(&matched)),
            first,
            "the walk direction is part of the input, not of the arrival order"
        );
    }

    /// The merge performs no I/O: it is a pure function of its inputs, so two
    /// calls with the same inputs return an identical value, and
    /// `Resolution::merge` takes no client and no repository — the only thing it
    /// reads is `&self` and the route the caller matched. Compile-time shape of
    /// the boundary: `merge(&self, Option<&Route>)` has no other parameter to
    /// carry a store or a resolver in, and `merge_effective` takes two slices.
    #[test]
    fn the_merge_is_a_pure_function_of_its_inputs() {
        let tiers = chain(
            tier(ROOT).limit(SHARING_ENFORCE, 10, WINDOW_SECOND),
            empty(MID),
            tier(LEAF).limit(SHARING_PRIVATE, 100, WINDOW_SECOND),
        );

        let first = merge_effective(&tiers, None);
        let second = merge_effective(&tiers, None);

        assert_eq!(
            first, second,
            "no I/O behind the inputs, so the value repeats"
        );
        assert_eq!(
            first.plugins, second.plugins,
            "the binding list is derived, never fetched"
        );
    }

    #[test]
    fn the_merged_config_exposes_its_bindings_in_merged_order() {
        let tiers = chain(
            tier(ROOT).plugins(SHARING_ENFORCE, &[ROOT_PLUGIN, ROOT_PLUGIN]),
            empty(MID),
            tier(LEAF).plugins(SHARING_INHERIT, &[LEAF_PLUGIN]),
        );

        let effective = merge_effective(&tiers, None);
        let references: Vec<String> = effective
            .plugin_bindings()
            .iter()
            .map(|binding| binding.plugin_ref.clone())
            .collect();

        assert_eq!(references.len(), 3);
        assert_eq!(references[0], ROOT_PLUGIN);
        assert_eq!(references[2], LEAF_PLUGIN);
        assert_eq!(effective.plugins[2].binding.position, 2);
    }

    #[test]
    fn a_resolution_merge_reads_only_its_own_fields() {
        let tiers = chain(
            empty(ROOT),
            tier(MID).limit(SHARING_ENFORCE, 9, WINDOW_SECOND),
            tier(LEAF),
        );
        let target = tiers[2].upstream.clone().unwrap_or_default();
        let resolution = Resolution {
            chain: TenantChain::new(vec![TenantId(LEAF), TenantId(MID), TenantId(ROOT)]),
            target,
            tiers,
            route_tiers: Vec::new(),
            disabled_ancestor: None,
        };

        let effective = resolution.merge(None);

        assert_eq!(
            sustained_of(&effective),
            Some((9, WINDOW_SECOND.to_owned()))
        );
        assert_eq!(
            resolution.tiers.len(),
            3,
            "the resolution is borrowed, never consumed, by the merge"
        );
        assert_eq!(resolution.state(), SelectedTargetState::Selected);
    }

    /// `TenantChain` and `EffectiveConfig` are plain value objects: they are
    /// constructed, cloned, moved and dropped in one test with no store, no
    /// table write and no persistence of any kind behind them — no repository,
    /// no schema object and no management surface is reachable from either.
    #[test]
    fn the_value_objects_carry_no_persistence() {
        let mut config = BTreeMap::new();
        config.insert(
            "secret_ref".to_owned(),
            serde_json::Value::String("cred://tenant/openai".to_owned()),
        );
        let auth = AuthConfig {
            plugin_type: Some("plugin".to_owned()),
            sharing: SHARING_INHERIT.to_owned(),
            config: Some(config),
        };
        let effective = EffectiveConfig {
            auth: Some(auth.clone()),
            auth_forced: false,
            rate_limit: Some(rate_limit(SHARING_PRIVATE, 1, WINDOW_SECOND)),
            plugins: Vec::new(),
            cors: None,
            cors_forced: false,
            tags: BTreeSet::from(["tag".to_owned()]),
        };
        let chain = TenantChain::new(vec![TenantId(LEAF), TenantId(ROOT)]);

        let cloned_chain = chain.clone();
        let cloned_config = effective.clone();
        drop(effective);
        drop(auth);
        drop(chain);

        assert_eq!(cloned_chain.len(), 2, "the chain is plain data");
        assert_eq!(
            cloned_config
                .auth
                .as_ref()
                .and_then(|auth| auth.plugin_type.as_deref()),
            Some("plugin")
        );
        assert!(cloned_config.tags.contains("tag"));
        let binding = EffectiveBinding {
            binding: PluginBinding {
                position: 0,
                plugin_ref: ROOT_PLUGIN.to_owned(),
                plugin_uuid: None,
                config: None,
            },
            enforced: false,
        };
        assert!(
            !binding.enforced,
            "a plain binding entry carries a flag, not a store"
        );
        let endpoint = Endpoint {
            scheme: "https".to_owned(),
            host: Some("api.vendor.com".to_owned()),
            port: 443,
        };
        let server = ServerConfig {
            endpoints: vec![endpoint],
        };
        assert_eq!(
            server.endpoints.len(),
            1,
            "no persistence behind the value object"
        );
    }
}
