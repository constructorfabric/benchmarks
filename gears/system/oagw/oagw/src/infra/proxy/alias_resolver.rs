//! Tenant-hierarchy alias resolution
//! (`cpt-cf-oagw-flow-request-proxy-alias-resolution`,
//! `cpt-cf-oagw-algo-request-proxy-alias-resolve`).
//!
//! The alias is resolved *through the caller's tenant chain*, not in a single
//! tenant: the walk starts at the calling tenant and follows the ancestors to
//! the root, and the first level holding an upstream with the requested alias
//! is the selected one. Shadowing therefore selects only the *routing* target
//! — the walk continues to the root to collect the `enabled` state and the
//! enforced ancestor configuration of every upstream it shadows, so an
//! ancestor can still disable the alias and can still constrain it.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::DomainError;
use crate::domain::repo::{UpstreamRecord, UpstreamRepository};
use crate::domain::services::management::{Actor, AncestorResolver};

/// One level of the alias walk, in walk order.
#[derive(Debug, Clone)]
pub struct AliasLevel {
    /// The tenant the level looked in.
    pub tenant_id: Uuid,
    /// The distance from the calling tenant: `0` for the caller itself.
    pub distance: usize,
    /// The upstream the level holds under the alias, when it holds one.
    pub record: Option<UpstreamRecord>,
}

/// The walk the alias resolution performed.
#[derive(Debug, Clone)]
pub struct AliasWalk {
    /// Every level, from the calling tenant to the root.
    pub levels: Vec<AliasLevel>,
    /// The index of the selected level in [`Self::levels`].
    pub selected: usize,
}

impl AliasWalk {
    /// The selected upstream.
    #[must_use]
    pub fn upstream(&self) -> &UpstreamRecord {
        self.levels[self.selected]
            .record
            .as_ref()
            .expect("the selected level holds the resolved upstream")
    }

    /// The levels that shadow *behind* the selected one: the ancestors the
    /// walk continued past, in walk order.
    #[must_use]
    pub fn shadowed(&self) -> impl Iterator<Item = &AliasLevel> {
        self.levels[self.selected + 1..].iter()
    }

    /// Whether any upstream the walk found is disabled, which disables the
    /// alias for every descendant (`inst-rp-alias-6`).
    #[must_use]
    pub fn any_disabled(&self) -> bool {
        self.levels
            .iter()
            .any(|level| level.record.as_ref().is_some_and(|record| !record.upstream.enabled))
    }
}

/// Resolve an alias through a tenant chain
/// (`inst-rp-alias-1` .. `-10`).
///
/// # Errors
///
/// Returns the not-found outcome when no level of the chain holds the alias,
/// and whatever the ancestor resolver or the repository produce otherwise.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-1
// `inst-rp-alias-1` .. `-10`, `inst-rp-al-alias-1` .. `-9`: the tenant-chain
// walk — normalized alias, leaf first, the disabled-upstream verdict and the
// shadowing the selected level implies.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-9
pub async fn resolve(
    upstreams: &Arc<dyn UpstreamRepository>,
    ancestors: &Arc<dyn AncestorResolver>,
    actor: &Actor,
    requested: &str,
) -> Result<AliasWalk, DomainError> {
    // Step 1: normalize — ASCII lowercase with the trailing dot stripped, so
    // resolution is case-insensitive.
    let normalized = alias::normalize_alias(requested);
    if normalized.is_empty() {
        return Err(DomainError::NotFound { resource_type: "upstream" });
    }

    let mut chain = vec![actor.tenant_id];
    chain.extend(ancestors.ancestors(actor, actor.tenant_id).await?);

    let mut levels: Vec<AliasLevel> = Vec::with_capacity(chain.len());
    for (distance, tenant_id) in chain.into_iter().enumerate() {
        // A level that holds no upstream with the alias contributes nothing;
        // every other repository failure is a real failure and propagates.
        let record = match upstreams.get_by_alias(tenant_id, &normalized) {
            Ok(record) => Some(record),
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(error),
        };
        levels.push(AliasLevel { tenant_id, distance, record });
    }

    let Some(selected) = levels.iter().position(|level| level.record.is_some()) else {
        return Err(DomainError::NotFound { resource_type: "upstream" });
    };
    Ok(AliasWalk { levels, selected })
}
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-10
//

/// The tenant layers the walk contributes to the effective-configuration
/// merge, ordered root to leaf
/// (`inst-rp-alias-9`, `inst-rp-al-config-2`).
///
/// The merge order is base → most specific, and the tenant chain is the most
/// specific group, so the ancestors are handed over *reversed*: the root
/// contributes first and the closest ancestor last.
#[must_use]
// @cpt-end:cpt-cf-oagw-flow-request-proxy-alias-resolution:p1:inst-rp-alias-1
pub fn ancestor_layers(walk: &AliasWalk) -> Vec<&UpstreamRecord> {
    let mut ancestors: Vec<&UpstreamRecord> = walk
        .shadowed()
        .filter_map(|level| level.record.as_ref())
        .collect();
    ancestors.reverse();
    ancestors
}
