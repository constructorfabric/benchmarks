//! The I/O drivers of the hierarchical configuration resolution: the
//! tenant-chain walk of `cpt-cf-oagw-algo-tenant-chain-walk` and the
//! alias-shadowing walk of `cpt-cf-oagw-algo-alias-shadowing`, both driven by
//! `cpt-cf-oagw-flow-effective-resolution`.
//!
//! [`EffectiveConfigResolver`] is the only place in the gear that reads a tenant
//! chain and the only place that selects a routing target: it asks the
//! `tenant-resolver` client the gear wiring resolved at init for the chain,
//! walks it through the tenant-scoped repository traits, and hands the selected
//! [`Resolution`] back to its caller, which merges it with
//! [`crate::domain::resolution::Resolution::merge`]. Nothing here merges a
//! field, applies a sharing mode or enforces a limit: that is the pure half of
//! the feature in `crate::domain::resolution`.

// @cpt-begin:cpt-cf-oagw-dod-tenant-isolation:p1:inst-full
// `cpt-cf-oagw-dod-tenant-isolation`: every read the resolution performs goes
// through the tenant-scoped repository traits of the domain-model feature, so
// no read can address a resource outside the requesting subject's chain. The
// two reads below — `find_by_alias(tenant_id, alias)` and
// `list_by_upstream(tenant_id, upstream_id)` — are keyed by a tenant id taken
// from the chain the platform resolver answered with, and a miss in one tier is
// indistinguishable from an absent one, so the walk learns nothing about any
// other tenant's hierarchy. Nothing here names a storage technology, so the
// reads stay portable across the in-memory stores and a SQL backend alike.
// @cpt-end:cpt-cf-oagw-dod-tenant-isolation:p1:inst-full

use std::sync::Arc;

use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias::normalize_alias;
use crate::domain::error::{DomainError, OagwError};
use crate::domain::model::Upstream;
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::domain::resolution::{Resolution, RouteTier, TenantChain, TierUpstream};

// @cpt-begin:cpt-cf-oagw-dod-effective-config-resolution:p1:inst-full
/// The resolver of `cpt-cf-oagw-dod-effective-config-resolution`: it walks the
/// chain descendant→root and selects the closest enabled upstream whose alias
/// matches, treats a found-but-disabled upstream as absent, applies the
/// enabled-inheritance rule that reports a same-alias disabled ancestor above
/// the selection, obtains the chain through the `tenant-resolver` client
/// dependency the gear wiring resolved at init, and returns the not-found and
/// `LinkUnavailable` dispositions through the existing rows of the closed
/// mapping table, leaving the disabled disposition a typed outcome for the
/// caller to render.
///
/// The resolver registers no HTTP route, owns no CRUD path and persists
/// nothing: it reads the three dependencies it is constructed with and returns
/// a value.
pub struct EffectiveConfigResolver {
    /// The tenant-scoped upstream reads of the shadowing walk.
    upstreams: Arc<dyn UpstreamRepository>,
    /// The tenant-scoped route reads of the route tier order.
    routes: Arc<dyn RouteRepository>,
    /// The platform client the chain walk is answered by.
    tenant_resolver: Arc<dyn TenantResolverClient>,
}

impl EffectiveConfigResolver {
    /// Builds the resolver over the three dependencies the proxy pipeline
    /// already holds: the two repository traits of the domain-model feature and
    /// the `tenant-resolver` client the gear wiring resolved at init. No
    /// platform dependency is resolved here.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            tenant_resolver,
        }
    }

    /// Resolves the effective configuration inputs of one proxied request.
    ///
    /// The outcome carries the chain, the selected target, the tiers the merge
    /// walks, the route tiers the caller matches against and the
    /// disabled-ancestor report.
    ///
    /// # Errors
    /// Returns the `LinkUnavailable` row (503) when the tenant chain cannot be
    /// established, and the `RouteNotFound` row (404) when the walk exhausts
    /// the chain with no enabled upstream matching the alias. A repository
    /// failure that is not a not-found — a store that cannot answer — is
    /// returned as the `ValidationError` row the existing mapping of the domain
    /// error hands over, and never as an absent tier. A same-alias ancestor
    /// above the selection that is disabled is **not** an error: it is returned
    /// as the typed `Disabled` outcome for the caller to render.
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        alias: &str,
    ) -> Result<Resolution, OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-01
        // Receive the resolution request of one proxied request from the proxy
        // pipeline — the alias, the method and path the caller matches
        // afterwards, and the request's security context — and apply the
        // alias-normalization rule of the alias-resolution feature to it, which
        // is used here without being re-declared and without any case handling
        // of its own: `Api.OpenAI.COM.` resolves the stored `api.openai.com`.
        let alias = normalize_alias(alias);
        // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-01
        let chain = self.tenant_chain(ctx).await?;
        // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-05
        // ELSE walk the chain from the descendant end to the root end through
        // the alias-shadowing walk, which selects the closest enabled upstream
        // whose alias matches and reports any same-alias ancestor above it that
        // is disabled.
        // The walk answers a chain the store may fail on its own: only the
        // not-found reading of a tier stays the "absent" answer of the walk,
        // any other repository failure is a resolution failure, not an absent
        // tier (see `same_alias_upstream`).
        let mut walk = self.shadowing(&chain, &alias)?;
        // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-05
        let target = walk
            .selected()
            .cloned()
            .ok_or_else(|| {
                // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-06
                // IF the walk exhausts the chain with no enabled upstream
                // matching the alias.
                // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-07
                // The walk found no enabled upstream under the normalized alias
                // in any tier of the chain.
                // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-07
                let error = OagwError::route_not_found(format!(
                    "oagw.resolution: no enabled upstream matches the alias '{alias}' in the subject's tenant chain"
                ));
                // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-06
                // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-07
                // RETURN the not-found disposition to the caller, which renders
                // it through the existing `RouteNotFound` row of the mapping
                // table; this flow performs no HTTP response of its own.
                // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-07
                // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-08
                // No default upstream, no fallback alias and no cross-tenant
                // lookup is attempted on the way out: the not-found outcome is
                // the answer.
                // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-08
                error
            })?;
        // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-08
        // ELSE fix the route tier order for the caller's match — the selected
        // upstream's own routes first, then the routes of the ancestor upstreams
        // reached through the chain, so a descendant route takes priority over
        // an inherited ancestor route. The match execution itself stays with the
        // proxy pipeline.
        let route_tiers = self.route_tiers(&walk)?;
        // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-08
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-10
        // The selection fixes the routing target only: enforced ancestor
        // constraints are never bypassed by shadowing, because the merge still
        // walks the whole ancestor chain after this walk has returned.
        // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-10
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-11
        // The walk re-derives no alias and re-checks no uniqueness: it reads the
        // alias the management surface stored, so alias derivation, alias update
        // behaviour and the per-tenant `(tenant_id, alias)` invariant stay with
        // the alias-resolution and management-api features.
        // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-11
        let resolution = Resolution {
            // The chain is handed over by move — it is the last read of it — so
            // the resolution owns it without a second allocation.
            chain,
            target,
            // The walk is consumed tier by tier here: the merge order is the
            // walk order reversed in place.
            tiers: walk.merge_tiers(),
            route_tiers,
            disabled_ancestor: walk.disabled_above,
        };
        if walk.disabled_above.is_some() {
            // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-09
            // IF the walk reported a same-alias ancestor above the selection
            // that is disabled — the enabled-inheritance rule disables the
            // descendant's selection.
            // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-09
            // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-10
            // RETURN the disabled disposition to the caller — the proxy
            // pipeline, which the management-api feature names as the owner of
            // the proxy-time 503 rejection of a disabled upstream — which
            // renders the 503 gateway rejection; this flow adds no row to the
            // closed mapping table and leaves the disposition's rendering to
            // the caller, and the state machine records the outcome as
            // `Disabled`.
            // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-10
            return Ok(resolution);
        }
        // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-11
        // ELSE RETURN to the caller the selected target, its ancestor chain and
        // the route tier the caller matches against — the caller's merge flow
        // continues on the same request.
        // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-11
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-12
        // RETURN the selected target with its ancestor chain and the
        // disabled-ancestor report, or the not-found outcome above.
        // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-12
        Ok(resolution)
    }

    /// The tenant chain of the request's subject
    /// (`cpt-cf-oagw-algo-tenant-chain-walk`).
    ///
    /// # Errors
    /// Returns the `LinkUnavailable` row when the client call fails or answers
    /// with no chain at all; there is no fallback to a single-tenant assumption.
    async fn tenant_chain(&self, ctx: &SecurityContext) -> Result<TenantChain, OagwError> {
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-01
        // Take the subject tenant id from the request's security context as the
        // single input of the walk: it is the only tenant identity this feature
        // reads, and every later lookup is keyed by a tenant id taken from the
        // chain it produces.
        let subject = ctx.subject_tenant_id();
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-01
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-02
        // The `tenant-resolver` client was resolved through the toolkit client
        // hub inside `init()` by the dependency-wiring DoD of the gear-wiring
        // feature; this algorithm resolves no platform dependency of its own and
        // declares none as a gear-level dependency.
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-02
        // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-02
        // Obtain the subject's tenant chain through the tenant-chain walk, the
        // only tenant-hierarchy input of this flow.
        // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-02
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-03
        // Request the chain for that tenant id and receive it descendant first
        // and root last, the only tenant-hierarchy source of this feature.
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-03
        // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-03
        // IF the tenant chain cannot be established: the `tenant-resolver`
        // client is unreachable, the call fails, or no chain is returned for the
        // subject tenant.
        let response = match self
            .tenant_resolver
            .get_ancestors(ctx, TenantId(subject), &GetAncestorsOptions::default())
            .await
        {
            Err(failure) => {
                // @cpt-begin:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-04
                // CATCH the failure and RETURN the `LinkUnavailable` failure
                // (503) mapped by the error-mapping algorithm for the caller to
                // render; no new row is added to the table, and there is no
                // fallback to a single-tenant assumption.
                // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-04
                // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-04
                // The resolver call failed, the client is unreachable, or no
                // chain is returned for the subject tenant.
                // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-04
                // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-05
                // CATCH the failure and RETURN the `LinkUnavailable` failure
                // (503) through the closed mapping table, adding no row: a chain
                // that cannot be established fails the resolution, and there is
                // no fallback to a single-tenant assumption.
                // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-05
                return Err(unresolvable_chain(&failure.to_string()));
            }
            Ok(response) => response,
        };
        // The descendant→root chain is the subject tier followed by the ordered
        // ancestors, direct parent first and root last.
        let mut tiers = Vec::with_capacity(response.ancestors.len() + 1);
        tiers.push(response.tenant.id);
        tiers.extend(response.ancestors.iter().map(|ancestor| ancestor.id));
        let chain = TenantChain::new(tiers);
        // The chain answers the subject only when its head is the tenant the
        // walk asked about, and a nil head is the empty chain the subject
        // accessor reports for a resolver that answered with no tier at all. A
        // chain that begins with another tenant would key every later lookup
        // outside the requesting subject's hierarchy, so it is a chain that
        // cannot be established: the same typed failure as the unreachable
        // resolver above, and never a single-tenant fallback.
        let head = chain.subject().0;
        if chain.is_empty() || head.is_nil() || head != subject {
            return Err(unresolvable_chain(
                "the tenant-resolver returned no chain for the subject tenant",
            ));
        }
        // @cpt-end:cpt-cf-oagw-flow-effective-resolution:p1:inst-ef-03
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-06
        // Read nothing outside the returned chain: the alias walk and the merge
        // key every lookup on a tenant id of this chain, so no resource
        // belonging to another tenant's hierarchy is reachable from this
        // algorithm.
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-06
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-07
        // Hold the chain in memory for the request only: it is never persisted,
        // never written to a store and never logged.
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-07
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-08
        // Recompute the chain on every request: no cached chain, no TTL and no
        // invalidation path exists in this feature, because any configuration
        // cache is out of scope this release.
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-08
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-09
        // RETURN the chain to the caller, which hands it to the alias-shadowing
        // walk for target selection and to the effective-merge algorithm for the
        // merge.
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-tc-09
        Ok(chain)
    }

    /// The alias-shadowing walk over the chain
    /// (`cpt-cf-oagw-algo-alias-shadowing`): descendant first, root last, the
    /// closest enabled match winning.
    ///
    /// # Errors
    /// Returns the repository failure of a tier read as-is: only the
    /// not-found reading of a tier is the "absent" answer the walk continues
    /// on, every other failure is a resolution failure.
    fn shadowing(&self, chain: &TenantChain, alias: &str) -> Result<ShadowingWalk, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-01
        // Take the already-normalized alias the resolution flow received —
        // normalized to ASCII lowercase with trailing dots stripped by the rule
        // of the alias-normalization algorithm, applied at the entry of the flow
        // without being re-declared here and without any case handling of its
        // own.
        // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-01
        let mut walk = ShadowingWalk {
            tiers: Vec::with_capacity(chain.len()),
            selected_index: None,
            disabled_above: None,
        };
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-02
        // FOR EACH tenant in the chain, from the descendant end to the root end,
        // look up an upstream by `(tenant_id, alias)` through the tenant-scoped
        // repository traits of the domain-model feature.
        // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-02
        for tier in chain.descendant_to_root().iter().map(|tier| tier.0) {
            let found = self.same_alias_upstream(tier, alias)?;
            let present = found.is_some();
            let enabled = found.as_ref().is_some_and(|upstream| upstream.enabled);
            // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-05
            // ELSE IF an upstream is found AND its `enabled` flag is false.
            // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-05
            // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-06
            // Treat it as absent and continue the walk: a disabled upstream
            // never becomes the target, so a disabled configuration cannot serve
            // traffic and cannot be re-enabled from below. The tier it belongs
            // to therefore records no upstream, and neither the merge nor the
            // route tiers see a disabled configuration.
            // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-06
            let recorded = found.filter(|_| enabled);
            match recorded {
                Some(upstream) => {
                    // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-03
                    // IF an upstream is found AND its `enabled` flag is true.
                    // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-03
                    // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-04
                    // Select it when nothing is selected yet — the closest match
                    // wins, and a descendant upstream with the same alias
                    // shadows an ancestor's — and keep scanning the tiers above
                    // it, so the walk can still report a disabled ancestor
                    // above. An enabled match above an already-selected one
                    // stays an ancestor tier of the merge, whose enforced
                    // constraints the target is still under.
                    // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-04
                    if walk.selected_index.is_none() {
                        walk.selected_index = Some(walk.tiers.len());
                    }
                    walk.tiers.push(TierUpstream {
                        tenant_id: TenantId(tier),
                        upstream: Some(upstream),
                    });
                }
                None => {
                    // @cpt-begin:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-09
                    // Inspect the chain above the selected target for a
                    // same-alias upstream that is disabled, and report it when
                    // one is found: enabled inheritance disables the descendant's
                    // selection, and the state machine records the outcome as
                    // `Disabled`.
                    // @cpt-end:cpt-cf-oagw-algo-alias-shadowing:p1:inst-as-09
                    if present && walk.selected_index.is_some() && walk.disabled_above.is_none() {
                        walk.disabled_above = Some(TenantId(tier));
                    }
                    walk.tiers.push(TierUpstream {
                        tenant_id: TenantId(tier),
                        upstream: None,
                    });
                }
            }
        }
        Ok(walk)
    }

    /// The tenant-scoped read of one chain tier: the upstream the tenant holds
    /// under the alias, absent when it holds none.
    ///
    /// The `NotFound` reading is the only "absent" reading: it is the ordinary
    /// answer of a tier that holds no upstream under the alias, and the tenant
    /// scope of the trait already makes a resource of another tenant
    /// indistinguishable from an absent one. Every other repository failure is
    /// not a statement about the tier at all — a store that cannot answer is
    /// not a store that holds nothing — so it is handed back to the caller
    /// through the existing mapping of the domain error, instead of silently
    /// walking on as if the tier were empty.
    ///
    /// # Errors
    /// Returns every repository failure but `NotFound`.
    fn same_alias_upstream(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        match self.upstreams.find_by_alias(tenant_id, alias) {
            Ok(upstream) => Ok(Some(upstream)),
            Err(DomainError::NotFound { .. }) => Ok(None),
            Err(other) => {
                tracing::debug!("oagw.resolution: one chain tier answered {other}");
                Err(other)
            }
        }
    }

    /// The route tiers the caller matches against: the selected upstream's own
    /// routes first, then the routes of the ancestor upstreams reached through
    /// the chain, descendant→root.
    ///
    /// # Errors
    /// Returns the repository failure of a route read as-is, the same reserved
    /// "absent" reading `same_alias_upstream` applies.
    fn route_tiers(&self, walk: &ShadowingWalk) -> Result<Vec<RouteTier>, OagwError> {
        let tiers = walk.selected_to_root();
        let mut route_tiers = Vec::with_capacity(tiers.len());
        for tier in tiers.iter() {
            let Some(upstream) = tier.upstream.as_ref().filter(|upstream| upstream.enabled) else {
                continue;
            };
            let Some(upstream_id) = upstream.id() else {
                continue;
            };
            route_tiers.push(self.route_tier(tier.tenant_id.0, upstream_id)?);
        }
        Ok(route_tiers)
    }

    /// One route tier: the routes of one upstream of one chain tier, in the
    /// deterministic order the route-match invariant needs.
    ///
    /// # Errors
    /// Returns every repository failure but `NotFound`, which stays the
    /// routeless tier.
    fn route_tier(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<RouteTier, OagwError> {
        let routes = match self.routes.list_by_upstream(tenant_id, upstream_id) {
            Ok(routes) => routes,
            // A miss is the ordinary answer of an upstream that holds no route
            // at all.
            Err(DomainError::NotFound { .. }) => Vec::new(),
            Err(other) => {
                tracing::debug!("oagw.resolution: one route tier answered {other}");
                return Err(other.into());
            }
        };
        Ok(RouteTier {
            tenant_id: TenantId(tenant_id),
            upstream_id,
            routes,
        })
    }
}
// @cpt-end:cpt-cf-oagw-dod-effective-config-resolution:p1:inst-full

/// The typed `LinkUnavailable` failure of an unresolvable chain.
fn unresolvable_chain(detail: &str) -> OagwError {
    OagwError::link_unavailable(format!(
        "oagw.resolution: the tenant chain cannot be established: {detail}"
    ))
}

/// The outcome of the alias-shadowing walk.
struct ShadowingWalk {
    /// Every tier of the chain in walk order, descendant first and root last,
    /// each carrying the same-alias upstream its tenant holds.
    tiers: Vec<TierUpstream>,
    /// The index of the selected tier in the walk order, when one was selected.
    selected_index: Option<usize>,
    /// The closest same-alias disabled upstream above the selection.
    disabled_above: Option<TenantId>,
}

impl ShadowingWalk {
    /// The selected upstream, which is the routing target the caller proxies to.
    fn selected(&self) -> Option<&Upstream> {
        self.tiers
            .get(self.selected_index?)
            .and_then(|tier| tier.upstream.as_ref())
    }

    /// The tiers from the selected one to the root end, in walk order:
    /// descendant→root, the selected target's own tier first.
    fn selected_to_root(&self) -> &[TierUpstream] {
        let start = self.selected_index.unwrap_or(0);
        &self.tiers[start.min(self.tiers.len())..]
    }

    /// The tiers the merge walks: root→descendant, the selected target's own
    /// tier last, so the ancestor tiers are applied before the descendant ones.
    ///
    /// The walk is consumed: the tiers below the selection are dropped, the
    /// rest is reversed in place and taken out, so the merge order costs no
    /// second allocation of the tier list.
    fn merge_tiers(&mut self) -> Vec<TierUpstream> {
        let start = self.selected_index.unwrap_or(0).min(self.tiers.len());
        drop(self.tiers.drain(..start));
        self.tiers.reverse();
        std::mem::take(&mut self.tiers)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tenant_resolver_sdk::{
        GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse, GetTenantsOptions,
        IsAncestorOptions, TenantInfo, TenantRef, TenantResolverError, TenantStatus,
    };
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::*;
    use crate::domain::model::{
        ALGORITHM_TOKEN_BUCKET, AuthConfig, BurstCapacity, CorsConfig, DEFAULT_RATE_COST, Endpoint,
        HttpMatch, MatchConfig, PROTOCOL_HTTP, PluginsConfig, RateLimitConfig, Route, SCOPE_TENANT,
        STRATEGY_REJECT, ServerConfig, SustainedRate,
    };
    use crate::domain::repo::ResourceLifecycle;
    use crate::domain::resolution::{EffectiveConfig, SelectedTargetState};
    use crate::domain::validation::validate_upstream_payload;
    use crate::infra::storage::InMemoryStores;
    use crate::infra::storage::upstream::InMemoryUpstreamRepository;

    const LEAF: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_0000_0001);
    const MID: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_0000_0002);
    const ROOT: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_0000_0003);
    const SUBJECT: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_beef_0001);
    /// A tenant that is no tier of the subject's chain.
    const UNRELATED: Uuid = Uuid::from_u128(0x0a11_ce00_0000_0000_0000_0000_0000_0999);

    const ALIAS: &str = "api.openai.com";
    const ROOT_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.root.v1";
    const LEAF_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.leaf.v1";
    const AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

    /// What the chain double answers with.
    enum ChainAnswer {
        /// The chain, descendant first and root last.
        Chain(Vec<TenantId>),
        /// The client is unreachable.
        Unreachable,
    }

    /// A test double of the platform `tenant-resolver` client that answers the
    /// chain walk with a configured chain, or fails it, and records the tenants
    /// it was asked about. The other five methods of the trait answer the
    /// `TenantNotFound` failure or an empty list, as this feature never calls
    /// them.
    struct ChainTenantResolver {
        answer: Mutex<ChainAnswer>,
        requested: Mutex<Vec<TenantId>>,
    }

    impl ChainTenantResolver {
        fn chain(tiers: &[Uuid]) -> Self {
            Self::answering(ChainAnswer::Chain(
                tiers.iter().copied().map(TenantId).collect(),
            ))
        }

        fn unreachable() -> Self {
            Self::answering(ChainAnswer::Unreachable)
        }

        fn answering(answer: ChainAnswer) -> Self {
            Self {
                answer: Mutex::new(answer),
                requested: Mutex::new(Vec::new()),
            }
        }

        fn requested(&self) -> Vec<TenantId> {
            self.requested.lock().expect("the record lock").clone()
        }
    }

    fn tenant_ref(id: TenantId) -> TenantRef {
        TenantRef {
            id,
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        }
    }

    #[async_trait::async_trait]
    impl TenantResolverClient for ChainTenantResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::TenantNotFound { tenant_id: id })
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::TenantNotFound {
                tenant_id: TenantId::nil(),
            })
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Ok(Vec::new())
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            self.requested.lock().expect("the record lock").push(id);
            let answer = self.answer.lock().expect("the answer lock");
            match &*answer {
                ChainAnswer::Chain(tiers) => Ok(GetAncestorsResponse {
                    tenant: tenant_ref(tiers[0]),
                    ancestors: tiers[1..].iter().copied().map(tenant_ref).collect(),
                }),
                ChainAnswer::Unreachable => Err(TenantResolverError::ServiceUnavailable(
                    "the tenant-resolver gear is unreachable".to_owned(),
                )),
            }
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Ok(GetDescendantsResponse {
                tenant: tenant_ref(id),
                descendants: Vec::new(),
            })
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor_id: TenantId,
            _descendant_id: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Ok(false)
        }
    }

    /// An upstream store that records every `(tenant_id, alias)` lookup key, so
    /// a test can assert that every read is keyed by a tenant of the chain, and
    /// that answers one tenant's read with a repository failure that is not a
    /// not-found, the way a store that cannot answer at all would.
    struct RecordingUpstreams {
        inner: InMemoryUpstreamRepository,
        lookups: Mutex<Vec<(Uuid, String)>>,
        fail_on: Mutex<Option<Uuid>>,
    }

    impl RecordingUpstreams {
        fn new(inner: InMemoryUpstreamRepository) -> Self {
            Self {
                inner,
                lookups: Mutex::new(Vec::new()),
                fail_on: Mutex::new(None),
            }
        }

        fn keys(&self) -> Vec<(Uuid, String)> {
            self.lookups.lock().expect("the record lock").clone()
        }

        /// Makes the `(tenant_id, alias)` read of this tenant answer a
        /// repository failure that is not a not-found.
        fn failing_for(&self, tenant_id: Uuid) {
            *self.fail_on.lock().expect("the fail lock") = Some(tenant_id);
        }
    }

    impl UpstreamRepository for RecordingUpstreams {
        fn insert(&self, upstream: &Upstream) -> Result<Upstream, DomainError> {
            self.inner.insert(upstream)
        }

        fn replace(&self, upstream: &Upstream) -> Result<Upstream, DomainError> {
            self.inner.replace(upstream)
        }

        fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
            self.inner.find(tenant_id, id)
        }

        fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
            self.lookups
                .lock()
                .expect("the record lock")
                .push((tenant_id, alias.to_owned()));
            if *self.fail_on.lock().expect("the fail lock") == Some(tenant_id) {
                return Err(DomainError::already_exists(
                    "alias",
                    "the upstream store cannot answer this read",
                ));
            }
            self.inner.find_by_alias(tenant_id, alias)
        }

        fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
            self.inner.list(tenant_id)
        }

        fn set_enabled(
            &self,
            tenant_id: Uuid,
            id: Uuid,
            enabled: bool,
        ) -> Result<Upstream, DomainError> {
            self.inner.set_enabled(tenant_id, id, enabled)
        }

        fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError> {
            self.inner.lifecycle(tenant_id, id)
        }

        fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
            self.inner.delete(tenant_id, id)
        }
    }

    /// A route store that answers one `(tenant_id, upstream_id)` read with a
    /// repository failure that is not a not-found, and the rest through the
    /// in-memory store it wraps.
    struct FailingRoutes {
        inner: crate::infra::storage::route::InMemoryRouteRepository,
        fail_on: Mutex<Option<(Uuid, Uuid)>>,
    }

    impl FailingRoutes {
        fn new(inner: crate::infra::storage::route::InMemoryRouteRepository) -> Self {
            Self {
                inner,
                fail_on: Mutex::new(None),
            }
        }

        /// Makes the `(tenant_id, upstream_id)` read answer a repository
        /// failure that is not a not-found.
        fn failing_for(&self, tenant_id: Uuid, upstream_id: Uuid) {
            *self.fail_on.lock().expect("the fail lock") = Some((tenant_id, upstream_id));
        }
    }

    impl RouteRepository for FailingRoutes {
        fn insert(&self, route: &Route) -> Result<Route, DomainError> {
            self.inner.insert(route)
        }

        fn replace(&self, route: &Route) -> Result<Route, DomainError> {
            self.inner.replace(route)
        }

        fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
            self.inner.find(tenant_id, id)
        }

        fn list_by_upstream(
            &self,
            tenant_id: Uuid,
            upstream_id: Uuid,
        ) -> Result<Vec<Route>, DomainError> {
            if *self.fail_on.lock().expect("the fail lock") == Some((tenant_id, upstream_id)) {
                return Err(DomainError::already_exists(
                    "upstream_id",
                    "the route store cannot answer this read",
                ));
            }
            self.inner.list_by_upstream(tenant_id, upstream_id)
        }

        fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
            self.inner.list(tenant_id)
        }

        fn set_enabled(
            &self,
            tenant_id: Uuid,
            id: Uuid,
            enabled: bool,
        ) -> Result<Route, DomainError> {
            self.inner.set_enabled(tenant_id, id, enabled)
        }

        fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError> {
            self.inner.lifecycle(tenant_id, id)
        }

        fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
            self.inner.delete(tenant_id, id)
        }
    }

    /// A well-formed upstream of `tenant_id` carrying the alias, with the blocks
    /// the caller mutates in, validated through the resource-validation flow and
    /// stored.
    fn stored_upstream(
        stores: &InMemoryStores,
        tenant_id: Uuid,
        alias: &str,
        enabled: bool,
        apply: impl FnOnce(&mut Upstream),
    ) -> Uuid {
        let mut candidate = Upstream {
            server: Some(ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: Some("api.vendor.com".to_owned()),
                    port: 443,
                }],
            }),
            protocol: Some(PROTOCOL_HTTP.to_owned()),
            ..Upstream::default()
        };
        apply(&mut candidate);
        candidate.enabled = enabled;
        candidate.tenant_id = Some(tenant_id);
        let payload = serde_json::to_value(&candidate).expect("the aggregate serializes");
        let mut upstream =
            validate_upstream_payload(&payload, tenant_id).expect("the upstream payload is valid");
        upstream.alias = Some(alias.to_owned());
        upstream.id = Some(Uuid::new_v4());
        stores
            .upstreams()
            .insert(&upstream)
            .expect("the upstream is stored")
            .id
            .expect("the stored upstream carries its id")
    }

    /// A well-formed enabled HTTP route of one upstream, whose distinct path
    /// keeps the route-match determinism invariant of the store satisfied.
    fn stored_route(
        stores: &InMemoryStores,
        tenant_id: Uuid,
        upstream_id: Uuid,
        tag: &str,
        path: &str,
    ) -> Uuid {
        let route = Route {
            id: Some(Uuid::new_v4()),
            tenant_id: Some(tenant_id),
            upstream_id: Some(upstream_id),
            enabled: true,
            priority: 10,
            tags: vec![tag.to_owned()],
            match_config: Some(MatchConfig {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: Some(path.to_owned()),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: "append".to_owned(),
                }),
                grpc: None,
            }),
            ..Route::default()
        };
        stores
            .routes()
            .insert(&route)
            .expect("the route is stored")
            .id
            .expect("the stored route carries its id")
    }

    fn security_context(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(SUBJECT)
            .subject_tenant_id(tenant)
            .build()
            .expect("a test context carries a subject and a tenant")
    }

    /// The resolver over the recording store triple, plus the chain double a
    /// test asserts on.
    struct Harness {
        stores: InMemoryStores,
        upstreams: Arc<RecordingUpstreams>,
        routes: Arc<dyn RouteRepository>,
        double: Arc<ChainTenantResolver>,
    }

    impl Harness {
        fn with_chain(tiers: &[Uuid]) -> Self {
            let (harness, _) = Self::with(ChainTenantResolver::chain(tiers));
            harness
        }

        fn with_unreachable() -> Self {
            let (harness, _) = Self::with(ChainTenantResolver::unreachable());
            harness
        }

        /// The harness whose route read answers a repository failure that is
        /// not a not-found, together with the failing double a test arms.
        fn with_failing_routes(tiers: &[Uuid]) -> (Self, Arc<FailingRoutes>) {
            let (mut harness, _) = Self::with(ChainTenantResolver::chain(tiers));
            let routes = Arc::new(FailingRoutes::new(harness.stores.routes()));
            harness.routes = Arc::clone(&routes) as Arc<dyn RouteRepository>;
            (harness, routes)
        }

        fn with(resolver: ChainTenantResolver) -> (Self, Arc<ChainTenantResolver>) {
            let double = Arc::new(resolver);
            let stores = InMemoryStores::new();
            let upstreams = Arc::new(RecordingUpstreams::new(stores.upstreams()));
            let routes: Arc<dyn RouteRepository> = Arc::new(stores.routes());
            (
                Self {
                    stores,
                    upstreams,
                    routes,
                    double: Arc::clone(&double),
                },
                double,
            )
        }

        fn resolver(&self) -> EffectiveConfigResolver {
            let resolver: Arc<dyn TenantResolverClient> =
                Arc::clone(&self.double) as Arc<dyn TenantResolverClient>;
            EffectiveConfigResolver::new(
                Arc::clone(&self.upstreams) as Arc<dyn UpstreamRepository>,
                Arc::clone(&self.routes),
                resolver,
            )
        }
    }

    fn rate_limit(sharing: &str, rate: i64, window: &str) -> RateLimitConfig {
        RateLimitConfig {
            sharing: sharing.to_owned(),
            algorithm: ALGORITHM_TOKEN_BUCKET.to_owned(),
            sustained: Some(SustainedRate {
                rate: Some(rate),
                window: window.to_owned(),
            }),
            burst: Some(BurstCapacity {
                capacity: Some(rate),
            }),
            scope: SCOPE_TENANT.to_owned(),
            strategy: STRATEGY_REJECT.to_owned(),
            cost: Some(DEFAULT_RATE_COST),
        }
    }

    fn plugins(sharing: &str, items: &[&str]) -> PluginsConfig {
        PluginsConfig {
            sharing: sharing.to_owned(),
            items: items.iter().map(|item| (*item).to_owned()).collect(),
        }
    }

    fn auth(sharing: &str) -> AuthConfig {
        AuthConfig {
            plugin_type: Some(AUTH_PLUGIN.to_owned()),
            sharing: sharing.to_owned(),
            config: None,
        }
    }

    fn cors(sharing: &str, origins: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing: sharing.to_owned(),
            enabled: Some(true),
            allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            ..CorsConfig::default()
        }
    }

    fn sustained_of(config: &EffectiveConfig) -> Option<i64> {
        config
            .rate_limit
            .as_ref()
            .and_then(|limit| limit.sustained.as_ref())
            .and_then(|sustained| sustained.rate)
    }

    /// The chain `[LEAF, MID, ROOT]` with the leaf and the root both holding the
    /// alias: the leaf is the target and the root enforces a 10/second rate.
    fn shadowing_harness() -> Harness {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, ROOT, ALIAS, true, |upstream| {
            upstream.rate_limit = Some(rate_limit("enforce", 10, "second"));
            upstream.plugins = Some(plugins("enforce", &[ROOT_PLUGIN]));
        });
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |upstream| {
            upstream.rate_limit = Some(rate_limit("private", 100, "second"));
            upstream.plugins = Some(plugins("inherit", &[LEAF_PLUGIN]));
        });
        harness
    }

    #[tokio::test]
    async fn the_closest_enabled_upstream_shadows_the_ancestors() {
        let harness = shadowing_harness();
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();

        assert_eq!(resolution.state(), SelectedTargetState::Selected);
        assert_eq!(resolution.chain.len(), 3);
        assert_eq!(resolution.chain.subject(), TenantId(LEAF));
        assert_eq!(resolution.target.alias.as_deref(), Some(ALIAS));
        assert_eq!(resolution.target.tenant_id, Some(LEAF));
        assert!(resolution.disabled_ancestor.is_none());
        // The merge walks the chain root→descendant, with the selected tier last.
        assert_eq!(
            resolution.tiers.first().map(|tier| tier.tenant_id),
            Some(TenantId(ROOT))
        );
        assert_eq!(
            resolution.tiers.last().map(|tier| tier.tenant_id),
            Some(TenantId(LEAF))
        );
        assert_eq!(
            resolution.tiers.len(),
            3,
            "every tier of the chain from the root to the selection is merged"
        );
        // The root tier carries its same-alias upstream, the empty one none.
        assert!(resolution.tiers[0].upstream.is_some());
        assert!(resolution.tiers[1].upstream.is_none());
    }

    #[tokio::test]
    async fn the_shadowed_target_still_carries_the_enforced_ancestor_constraints() {
        let harness = shadowing_harness();
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();
        let effective = resolution.merge(None);

        assert_eq!(
            sustained_of(&effective),
            Some(10),
            "the enforced ancestor rate constrains the shadowed target"
        );
        let references: Vec<&str> = effective
            .plugins
            .iter()
            .map(|binding| binding.binding.plugin_ref.as_str())
            .collect();
        assert_eq!(references, [ROOT_PLUGIN, LEAF_PLUGIN]);
    }

    #[tokio::test]
    async fn a_disabled_same_alias_ancestor_disables_the_selection() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, MID, ALIAS, false, |_| {});
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();

        // A typed outcome, not an `OagwError`: the caller renders the 503.
        assert_eq!(resolution.state(), SelectedTargetState::Disabled);
        assert_eq!(resolution.disabled_ancestor, Some(TenantId(MID)));
        assert_eq!(resolution.target.alias.as_deref(), Some(ALIAS));
    }

    #[tokio::test]
    async fn a_chain_whose_only_same_alias_upstream_is_disabled_is_not_found() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, MID, ALIAS, false, |_| {});
        let resolver = harness.resolver();

        let error = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "RouteNotFound");
        assert_eq!(error.status(), 404);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert!(!error.is_retriable());
    }

    #[tokio::test]
    async fn an_unknown_alias_is_not_found_without_a_fallback() {
        let harness = shadowing_harness();
        let resolver = harness.resolver();

        let error = resolver
            .resolve(&security_context(LEAF), "absent.vendor.com")
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "RouteNotFound");
        assert_eq!(error.status(), 404);
        // One lookup per chain tier, and no default or fallback alias.
        assert_eq!(harness.upstreams.keys().len(), 3);
    }

    #[tokio::test]
    async fn an_unreachable_tenant_resolver_fails_with_link_unavailable() {
        let harness = Harness::with_unreachable();
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let error = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
        assert!(error.is_retriable());
        // No single-tenant fallback: no upstream lookup happened at all.
        assert!(harness.upstreams.keys().is_empty());
        assert_eq!(harness.double.requested(), vec![TenantId(LEAF)]);
    }

    #[tokio::test]
    async fn a_chain_of_one_tier_still_walks_and_answers_not_found() {
        let harness = Harness::with_chain(&[UNRELATED]);
        // The subject tenant resolves to a chain of one tier, which holds no
        // upstream under the alias: the chain is established, so the outcome is
        // the not-found disposition and never a `LinkUnavailable`.
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let error = resolver
            .resolve(&security_context(UNRELATED), ALIAS)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "RouteNotFound");
        assert_eq!(error.status(), 404);
        assert_eq!(
            harness.upstreams.keys(),
            vec![(UNRELATED, ALIAS.to_owned())],
            "the only tier walked is the subject's own"
        );
        assert_eq!(harness.double.requested(), vec![TenantId(UNRELATED)]);
    }

    #[tokio::test]
    async fn the_sharing_modes_reach_the_effective_config_through_the_walk() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, ROOT, ALIAS, true, |upstream| {
            upstream.auth = Some(auth("private"));
            upstream.cors = Some(cors("inherit", &["https://root.example.com"]));
            upstream.plugins = Some(plugins("enforce", &[ROOT_PLUGIN]));
        });
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |upstream| {
            upstream.cors = Some(cors("inherit", &["https://leaf.example.com"]));
            upstream.plugins = Some(plugins("inherit", &[LEAF_PLUGIN]));
        });
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();
        let effective = resolution.merge(None);

        assert!(
            effective.auth.is_none(),
            "a private ancestor auth block contributes nothing"
        );
        let cors = effective.cors.as_ref().unwrap();
        assert_eq!(
            cors.allowed_origins,
            ["https://root.example.com", "https://leaf.example.com"],
            "the inherited origins are unioned"
        );
        assert_eq!(effective.plugins.len(), 2);
        assert!(
            effective.plugins[0].enforced,
            "the enforced ancestor binding is marked"
        );
    }

    #[tokio::test]
    async fn an_ancestor_inherit_block_stands_when_the_leaf_has_none() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, ROOT, ALIAS, true, |upstream| {
            upstream.auth = Some(auth("inherit"));
        });
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();
        let effective = resolution.merge(None);

        assert_eq!(
            effective
                .auth
                .as_ref()
                .and_then(|auth| auth.plugin_type.as_deref()),
            Some(AUTH_PLUGIN),
            "the inherited default stands when the descendant has no own block"
        );
    }

    #[tokio::test]
    async fn the_alias_is_resolved_case_insensitively() {
        let harness = shadowing_harness();
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), "Api.OpenAI.COM.")
            .await
            .unwrap();

        assert_eq!(resolution.target.alias.as_deref(), Some(ALIAS));
        assert!(
            harness
                .upstreams
                .keys()
                .iter()
                .all(|(_, alias)| alias == ALIAS),
            "every lookup key is the normalized alias"
        );
    }

    #[tokio::test]
    async fn the_route_tiers_are_the_target_first_then_the_ancestors() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        let root_id = stored_upstream(&harness.stores, ROOT, ALIAS, true, |_| {});
        let leaf_id = stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        stored_route(&harness.stores, ROOT, root_id, "root-route", "/v1/root");
        stored_route(&harness.stores, LEAF, leaf_id, "leaf-route", "/v1/chat");
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();

        assert_eq!(resolution.route_tiers.len(), 2);
        assert_eq!(resolution.route_tiers[0].tenant_id, TenantId(LEAF));
        assert_eq!(resolution.route_tiers[0].upstream_id, leaf_id);
        assert_eq!(resolution.route_tiers[1].tenant_id, TenantId(ROOT));
        assert_eq!(resolution.route_tiers[1].upstream_id, root_id);
        assert_eq!(resolution.route_tiers[0].routes[0].tags, ["leaf-route"]);
        assert_eq!(resolution.route_tiers[1].routes[0].tags, ["root-route"]);
    }

    #[tokio::test]
    async fn a_route_of_the_target_takes_priority_over_an_ancestor_route() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        let root_id = stored_upstream(&harness.stores, ROOT, ALIAS, true, |_| {});
        let leaf_id = stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        stored_route(&harness.stores, ROOT, root_id, "root-route", "/v1/root");
        stored_route(&harness.stores, LEAF, leaf_id, "leaf-route-a", "/v1/chat/a");
        stored_route(&harness.stores, LEAF, leaf_id, "leaf-route-b", "/v1/chat/b");
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();

        assert_eq!(
            resolution.route_tiers[0].routes.len(),
            2,
            "both routes of the target's upstream are handed over"
        );
        assert_eq!(resolution.route_tiers[1].routes.len(), 1);
        assert_eq!(resolution.route_tiers[1].routes[0].tags, ["root-route"]);
        // The routes of one tier come back in the deterministic store order.
        let mut tags: Vec<&str> = resolution.route_tiers[0]
            .routes
            .iter()
            .flat_map(|route| route.tags.iter().map(String::as_str))
            .collect();
        tags.sort_unstable();
        assert_eq!(tags, ["leaf-route-a", "leaf-route-b"]);
    }

    #[tokio::test]
    async fn the_tags_and_the_route_limit_reach_the_effective_config() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        let root_id = stored_upstream(&harness.stores, ROOT, ALIAS, true, |upstream| {
            upstream.tags = vec!["root-tag".to_owned()];
            upstream.rate_limit = Some(rate_limit("enforce", 10, "second"));
        });
        let leaf_id = stored_upstream(&harness.stores, LEAF, ALIAS, true, |upstream| {
            upstream.tags = vec!["leaf-tag".to_owned()];
        });
        stored_route(&harness.stores, LEAF, leaf_id, "leaf-route", "/v1/chat");
        let _ = root_id;
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();
        let matched = &resolution.route_tiers[0].routes[0];
        let effective = resolution.merge(Some(matched));

        let tags: Vec<&str> = effective.tags.iter().map(String::as_str).collect();
        assert_eq!(tags, ["leaf-route", "leaf-tag", "root-tag"]);
        assert_eq!(sustained_of(&effective), Some(10));
    }

    #[tokio::test]
    async fn the_walk_never_reads_outside_the_subject_chain() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        // The unrelated tenant holds the same alias, but it is no tier of the
        // subject's chain, so the walk cannot reach it.
        stored_upstream(&harness.stores, UNRELATED, ALIAS, true, |_| {});
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();

        assert_eq!(resolution.target.tenant_id, Some(LEAF));
        let chain = [LEAF, MID, ROOT];
        assert_eq!(harness.upstreams.keys().len(), 3, "one lookup per tier");
        for (tenant, _) in harness.upstreams.keys() {
            assert!(
                chain.contains(&tenant),
                "the lookup key {tenant} is not a tenant of the subject's chain"
            );
        }
        for requested in harness.double.requested() {
            assert_eq!(
                requested.0, LEAF,
                "the walk asks for the subject tenant only"
            );
        }
    }

    #[tokio::test]
    async fn the_chain_is_recomputed_per_request() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let first = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();
        assert!(first.disabled_ancestor.is_none());

        // A store mutation between two resolutions is visible to the second one:
        // no cache, no TTL and no invalidation path exists behind the resolver.
        stored_upstream(&harness.stores, MID, ALIAS, false, |_| {});
        let second = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();

        assert_eq!(
            harness.double.requested().len(),
            2,
            "the chain is walked once per request"
        );
        assert_eq!(second.disabled_ancestor, Some(TenantId(MID)));
        assert_eq!(second.state(), SelectedTargetState::Disabled);
    }

    #[tokio::test]
    async fn a_new_ancestor_configuration_is_visible_to_the_next_request() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let empty = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap()
            .merge(None);
        assert!(empty.rate_limit.is_none(), "no limit is configured yet");

        stored_upstream(&harness.stores, MID, ALIAS, true, |upstream| {
            upstream.rate_limit = Some(rate_limit("enforce", 5, "second"));
        });
        let effective = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap()
            .merge(None);

        assert_eq!(
            sustained_of(&effective),
            Some(5),
            "the ancestor's new enforced limit reaches the next request"
        );
    }

    /// The feature registers no HTTP route of its own: the gear's route shell
    /// stays the closed 27-route set, and the resolution is a per-request
    /// computation that persists nothing.
    #[test]
    fn no_http_route_is_registered_and_nothing_is_persisted() {
        assert_eq!(
            crate::api::rest::route_shell::shell_routes().len(),
            27,
            "the route shell stays the closed 27-route set"
        );
        let resolution = Resolution {
            chain: TenantChain::new(vec![TenantId(LEAF), TenantId(ROOT)]),
            target: Upstream::default(),
            tiers: Vec::new(),
            route_tiers: Vec::new(),
            disabled_ancestor: None,
        };
        let effective = resolution.merge(None);
        assert_eq!(effective.plugins.len(), 0);
        assert_eq!(effective.tags.len(), 0);
    }

    #[tokio::test]
    async fn the_routes_of_the_target_are_handed_over_as_the_store_answers() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        let leaf_id = stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        stored_route(&harness.stores, LEAF, leaf_id, "leaf-route", "/v1/chat");
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();

        // The match decision is the proxy pipeline's: the resolver hands the
        // routes over as the store answers them, no route of its own added.
        assert_eq!(resolution.route_tiers.len(), 1);
        assert!(resolution.route_tiers[0].routes[0].enabled);
    }

    /// The merge is deterministic through the whole resolution: the same chain
    /// and the same binding set answer the same `EffectiveConfig` on repeated
    /// resolutions, and a resolution of another tenant interleaved between them
    /// leaves the outcome untouched (§6).
    #[tokio::test]
    async fn repeated_resolutions_of_one_chain_answer_the_same_effective_config() {
        let harness = shadowing_harness();
        let resolver = harness.resolver();

        let first = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap()
            .merge(None);
        // Another subject's resolution runs in between, the way concurrent
        // requests of different tenants interleave in the proxy pipeline.
        let _ = resolver
            .resolve(&security_context(UNRELATED), "absent.vendor.com")
            .await
            .unwrap_err();
        let second = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap()
            .merge(None);

        assert_eq!(
            first, second,
            "same chain, same binding set, no request-ordering effect"
        );
        assert_eq!(
            sustained_of(&first),
            Some(10),
            "the enforced ancestor rate is part of the repeated answer"
        );
        let references: Vec<&str> = first
            .plugins
            .iter()
            .map(|binding| binding.binding.plugin_ref.as_str())
            .collect();
        assert_eq!(references, [ROOT_PLUGIN, LEAF_PLUGIN]);
        assert_eq!(second.plugins.len(), 2);
    }

    /// §6: an ancestor field carried with `sharing: private` enters neither the
    /// collected enforced-limit set, nor the plugin binding list, nor the CORS
    /// origin union — the ancestor's rate limit, plugin bindings and CORS
    /// origins are absent from the `EffectiveConfig` a descendant request
    /// receives, and only the descendant's own blocks are.
    #[tokio::test]
    async fn a_private_ancestor_field_reaches_no_output_of_the_effective_config() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        stored_upstream(&harness.stores, ROOT, ALIAS, true, |upstream| {
            upstream.rate_limit = Some(rate_limit("private", 10, "second"));
            upstream.plugins = Some(plugins("private", &[ROOT_PLUGIN]));
            upstream.cors = Some(cors("private", &["https://root.example.com"]));
        });
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |upstream| {
            upstream.rate_limit = Some(rate_limit("enforce", 25, "second"));
            upstream.plugins = Some(plugins("inherit", &[LEAF_PLUGIN]));
            upstream.cors = Some(cors("inherit", &["https://leaf.example.com"]));
        });
        let resolver = harness.resolver();

        let resolution = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap();
        let effective = resolution.merge(None);

        assert_eq!(
            sustained_of(&effective),
            Some(25),
            "the private ancestor limit is not collected into the minimum"
        );
        let references: Vec<&str> = effective
            .plugins
            .iter()
            .map(|binding| binding.binding.plugin_ref.as_str())
            .collect();
        assert_eq!(
            references,
            [LEAF_PLUGIN],
            "the private ancestor binding appends nothing"
        );
        assert_eq!(
            effective
                .cors
                .map(|cors| cors.allowed_origins)
                .unwrap_or_default(),
            ["https://leaf.example.com"],
            "the private ancestor origins never enter the union"
        );
    }

    /// A repository failure that is not a not-found is not an absent tier: the
    /// resolution fails with the failure the store answered, and never walks on
    /// as if the tier held nothing.
    #[tokio::test]
    async fn a_repository_failure_of_one_tier_is_not_an_absent_tier() {
        let harness = Harness::with_chain(&[LEAF, MID, ROOT]);
        // The ancestor holds the alias too, so a walk that treated the failing
        // tier as absent would silently select it instead of failing.
        let _root_id = stored_upstream(&harness.stores, ROOT, ALIAS, true, |_| {});
        stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        harness.upstreams.failing_for(LEAF);
        let resolver = harness.resolver();

        let error = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "ValidationError");
        assert_eq!(error.status(), 400);
        assert!(
            !error.detail().contains("no enabled upstream matches"),
            "the failure is not the not-found disposition of an exhausted walk: {error}"
        );
    }

    /// The same reserved reading on the route side: a route read that fails
    /// with anything but a not-found fails the resolution instead of handing
    /// the caller a routeless tier.
    #[tokio::test]
    async fn a_route_store_that_cannot_answer_fails_the_resolution() {
        let (harness, routes) = Harness::with_failing_routes(&[LEAF, MID, ROOT]);
        let leaf_id = stored_upstream(&harness.stores, LEAF, ALIAS, true, |_| {});
        stored_route(&harness.stores, LEAF, leaf_id, "leaf-route", "/v1/chat");
        routes.failing_for(LEAF, leaf_id);
        let resolver = harness.resolver();

        let error = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "ValidationError");
        assert_eq!(error.status(), 400);
        assert!(
            !error.detail().contains("no enabled upstream matches"),
            "the failure is not the not-found disposition of an exhausted walk: {error}"
        );
    }

    /// A chain whose head is not the subject tenant is a chain that cannot be
    /// established: it is refused before any lookup is keyed by it, so no read
    /// can address a hierarchy the requesting subject does not belong to.
    #[tokio::test]
    async fn a_chain_whose_head_is_not_the_subject_tenant_is_unresolvable() {
        // The double answers with the parent's chain while the request's
        // subject is the leaf below it.
        let harness = Harness::with_chain(&[MID, ROOT]);
        stored_upstream(&harness.stores, MID, ALIAS, true, |_| {});
        let resolver = harness.resolver();

        let error = resolver
            .resolve(&security_context(LEAF), ALIAS)
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
        assert!(harness.upstreams.keys().is_empty(), "no lookup happened");
        // The chain is resolved for the subject that asked for it; the refusal
        // is the head-versus-subject comparison, not a lookup keyed by a tier.
        assert_eq!(harness.double.requested(), vec![TenantId(LEAF)]);
    }
}
