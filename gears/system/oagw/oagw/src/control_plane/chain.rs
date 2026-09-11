//! The tenant chain walk of the hierarchical configuration feature.
//!
//! [`walk_candidates`] is `cpt-cf-oagw-algo-tenant-chain-walk`: it walks the
//! ordered ancestor chain from the calling tenant to the platform root and
//! issues one tenant-scoped read per element for the normalized alias. A chain
//! the platform tenant-resolver cannot answer in order is an unavailable chain
//! and the walk fails closed, rather than ordering candidates against a chain
//! it cannot order.
//!
//! [`chain_of`] is the adapter the API layer calls: it asks the
//! `tenant-resolver` gear for the ancestor chain and drops the tenants it
//! retired — `status: deleted` — before ordering it, so a retired tenant is
//! never an active participant of a resolution. It answers `None` when the
//! gear is absent, when the call fails, or when the answer cannot be ordered;
//! every caller fails closed on `None`.

// @cpt-dod:cpt-cf-oagw-dod-tenant-chain-walk:p1

use std::sync::Arc;

use toolkit_security::SecurityContext;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantResolverClient, TenantStatus};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::alias::Alias;
use crate::domain::effective::{FamilyModes, TenantChain};
use crate::domain::upstream::Upstream;
use crate::store::{OagwStore, UpstreamRow};

/// Why the walk could not be run.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UnavailableChain {
    /// The platform tenant-resolver supplied no usable chain.
    #[error("the platform tenant-resolver supplied no ordered ancestor chain")]
    Unordered,
}

/// One upstream row the walk matched, with everything the shadow resolve and
/// the merge need from it.
///
/// The candidate carries the whole row, not only the visible families: which
/// families are visible is decided later, by the sharing-mode decision, from
/// the modes recorded here. An `enforce` mode is recorded as a mode and never
/// pre-applied, so the walk stays a read and the decision stays in one place.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ChainCandidate {
    /// The depth of the owning tenant in the chain: `0` for the calling
    /// tenant and growing towards the root.
    pub depth: usize,
    /// The tenant that owns the row.
    pub tenant_id: Uuid,
    /// The identifier of the matched upstream row.
    pub upstream_id: Uuid,
    /// The row's `enabled` flag, which participates in the effective enabled
    /// state regardless of every sharing mode.
    pub enabled: bool,
    /// The per-family sharing modes the row declares.
    pub modes: FamilyModes,
    /// The matched row itself.
    pub row: UpstreamRow,
}

impl ChainCandidate {
    /// Builds the candidate the walk appends for one matched row.
    #[must_use]
    pub fn of(depth: usize, row: &UpstreamRow) -> Self {
        Self {
            depth,
            tenant_id: row.tenant_id,
            upstream_id: row.upstream.id,
            enabled: row.upstream.enabled,
            modes: modes_of(&row.upstream),
            row: row.clone(),
        }
    }
}

/// The per-family sharing modes one upstream row declares.
///
/// A family the row omits, and a family whose `sharing` member the row omits,
/// both take the schema default `private`; [`FamilyModes::new`] takes the
/// default for whatever it is not told about.
#[must_use]
pub fn modes_of(upstream: &Upstream) -> FamilyModes {
    FamilyModes::new(
        upstream.auth.as_ref().and_then(|auth| auth.sharing),
        upstream.rate_limit.as_ref().and_then(|limit| limit.sharing),
        upstream.plugins.as_ref().and_then(|plugins| plugins.sharing),
        upstream.cors.as_ref().and_then(|cors| cors.sharing),
    )
}

/// The `upstream:{tenant_id}:{alias}` Control Plane L1 cache key of ADR 0005
/// one per-element read is addressed by.
#[must_use]
pub fn cache_key(tenant_id: Uuid, alias: &Alias) -> String {
    format!("upstream:{tenant_id}:{alias}")
}

/// Walks the ancestor chain from the calling tenant to the platform root,
/// looking for the normalized alias.
///
/// # Errors
///
/// Returns [`UnavailableChain::Unordered`] when the resolver's answer cannot
/// be ordered — a cycle, a repeated element, or a calling tenant that is not
/// its first element — and the caller fails closed rather than resolving
/// against it.
pub fn walk_candidates(
    store: &OagwStore,
    calling_tenant: Uuid,
    ancestors: &[Uuid],
    alias: &Alias,
) -> Result<Vec<ChainCandidate>, UnavailableChain> {
    // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-read
    let chain = TenantChain::from_resolver(calling_tenant, ancestors);
    // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-read

    // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-unavailable-if
    let Some(chain) = chain else {
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-unavailable-return
        return Err(UnavailableChain::Unordered);
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-unavailable-return
    };
    // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-unavailable-if

    // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-else
    let mut candidates = Vec::new();
    // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-else

    // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-loop
    for (depth, tenant) in chain.tenants().iter().enumerate() {
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-hit-if
        if let Some(row) = store.upstream_by_alias(*tenant, alias) {
            // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-hit
            candidates.push(ChainCandidate::of(depth, &row));
            // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-hit
        }
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-hit-if
    }
    // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-loop

    // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-return
    Ok(candidates)
    // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-return
}

/// Asks the platform tenant-resolver for the ancestor chain of one tenant and
/// orders it, calling tenant first.
///
/// A tenant the resolver retired — `status: deleted` — is dropped before the
/// chain is ordered, because a retired tenant is not an active participant of
/// any resolution. `None` answers a failed call, a missing client, and an
/// answer that cannot be ordered; the caller fails closed on all three.
#[must_use]
pub async fn chain_of(
    client: Option<&Arc<dyn TenantResolverClient>>,
    context: &SecurityContext,
    tenant: Uuid,
) -> Option<TenantChain> {
    let Some(client) = client else {
        tracing::debug!("no tenant-resolver client registered; the chain is unavailable");
        return None;
    };
    let answer = client
        .get_ancestors(
            context,
            tenant_resolver_sdk::TenantId(tenant),
            &GetAncestorsOptions::default(),
        )
        .await
        .inspect_err(|error| {
            tracing::debug!(%error, "the tenant-resolver refused the ancestor chain");
        })
        .ok()?;
    let ancestors: Vec<Uuid> = answer
        .ancestors
        .iter()
        .filter(|reference| reference.status != TenantStatus::Deleted)
        .map(|reference| reference.id.0)
        .collect();
    TenantChain::from_resolver(tenant, &ancestors)
}
