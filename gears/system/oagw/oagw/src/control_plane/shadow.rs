//! Ancestor alias resolution and shadowing — `cpt-cf-oagw-algo-alias-shadow-resolve`.
//!
//! The walk supplies the ordered candidate set; this module turns it into the
//! three answers the merge needs: the routing target, which is the candidate
//! at the smallest depth, the ordered ancestor bindings, which are the
//! remaining candidates from the most distant to the least, and the effective
//! `enabled` state, which is the conjunction of the target's own flag with the
//! flag of every matched ancestor row regardless of the sharing modes those
//! rows declare.
//!
//! A candidate whose stored alias disagrees with the resolved one on the
//! normalized form is never a candidate: [`shadow_resolve`] re-derives the
//! stored alias through [`Alias::parse`] and compares the value objects, so
//! case, a trailing dot, and a port can never make a candidate set disagree
//! with a stored alias.
//!
//! The target decides where a request goes. The bindings decide what the
//! request is subject to. A shadowing descendant can replace the target and
//! can replace the values the ancestor marked `inherit`, and it can never
//! replace the values the ancestor marked `enforce` or raise the effective
//! `enabled` state.

// @cpt-dod:cpt-cf-oagw-dod-alias-shadowing:p1

use toolkit_macros::domain_model;

use crate::control_plane::chain::{ChainCandidate, modes_of};
use crate::domain::alias::Alias;
use crate::domain::effective::{AncestorBinding, ContributedFamilies, Family, FamilyContribution};
use crate::domain::route::Route;
use crate::domain::upstream::{SharingMode, Upstream};

/// The four families a sharing-mode decision addresses, in merge order.
const FAMILIES: [Family; 4] = [
    Family::Auth,
    Family::RateLimit,
    Family::Plugins,
    Family::Cors,
];

/// The answer of one alias shadow resolve.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowResolution {
    /// The candidate at the smallest depth: the routing target.
    pub target: ChainCandidate,
    /// The remaining candidates, most distant first.
    pub bindings: Vec<AncestorBinding>,
    /// The target's own `enabled` flag conjoined with every matched ancestor
    /// row's flag.
    pub enabled: bool,
}

/// Resolves the ordered candidate set against the normalized alias.
///
/// `None` is the not-found outcome an empty candidate set produces, which the
/// consumer answers 404; the merge runs only on a shadow resolution.
#[must_use]
pub fn shadow_resolve(candidates: &[ChainCandidate], alias: &Alias) -> Option<ShadowResolution> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-compare
    let matched: Vec<&ChainCandidate> = candidates
        .iter()
        .filter(|candidate| stored_alias(&candidate.row.upstream).as_ref() == Some(alias))
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-compare

    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-empty-if
    if matched.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-empty-return
        return None;
        // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-empty-return
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-empty-if

    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-else
    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-target
    // The candidates arrive ordered by increasing depth, so the closest match
    // wins and a descendant's row shadows an ancestor's.
    let target = matched.iter().min_by_key(|candidate| candidate.depth).copied()?;
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-target

    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-bindings
    let mut ancestors: Vec<&ChainCandidate> = matched
        .iter()
        .copied()
        .filter(|candidate| candidate.depth != target.depth)
        .collect();
    ancestors.sort_by_key(|candidate| std::cmp::Reverse(candidate.depth));
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-bindings

    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-loop
    let bindings: Vec<AncestorBinding> = ancestors
        .iter()
        .map(|candidate| AncestorBinding {
            tenant_id: candidate.tenant_id,
            depth: candidate.depth,
            upstream_id: candidate.upstream_id,
            enabled: candidate.enabled,
            contributed: contributed(&candidate.row.upstream),
        })
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-loop

    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-enabled
    // `enabled` is a row-level state and carries no sharing field, so one
    // disabled ancestor disables the resource for every descendant without a
    // write, and no descendant write can raise it.
    let enabled = matched.iter().all(|candidate| candidate.enabled);
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-enabled

    // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-return
    Some(ShadowResolution {
        target: target.clone(),
        bindings,
        enabled,
    })
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-return
    // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-else
}

/// The normalized alias one stored upstream row carries, when it carries one.
fn stored_alias(upstream: &Upstream) -> Option<Alias> {
    upstream
        .alias
        .as_deref()
        .and_then(|stored| Alias::parse(stored).ok())
}

/// The families one ancestor row contributes to a merge.
///
/// `enforce` and `inherit` both carry the row's value into the merge — one as
/// a forced value, the other as the base a descendant may override — and
/// `private` carries nothing at all, so the value is never read into a result,
/// copied onto any row, or echoed in any answer. `tags` carries no sharing
/// field and always contributes.
#[must_use]
pub fn contributed(upstream: &Upstream) -> ContributedFamilies {
    let modes = modes_of(upstream);
    let mut families = ContributedFamilies {
        auth: None,
        rate_limit: None,
        plugins: None,
        cors: None,
        tags: Some(upstream.tags.clone()),
    };
    for family in FAMILIES {
        let mode = modes.mode_of(family);
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-enforce-if
        if mode == SharingMode::Enforce {
            // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-enforce
            carry(&mut families, family, mode, upstream);
            // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-enforce
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-enforce-if
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-inherit-if
        else if mode == SharingMode::Inherit {
            // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-inherit
            carry(&mut families, family, mode, upstream);
            // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-inherit
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-inherit-if
        // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-private-else
        else {
            // @cpt-begin:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-private
            // Carries nothing for that family: no field of `families` is
            // written, so the value cannot reach any result.
            // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-private
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-shadow-resolve:p1:inst-shadow-private-else
    }
    families
}

/// The families one ancestor route row contributes to a merge.
///
/// A route carries no authentication family, so `auth` is always `None` for a
/// route-layer binding, and `tags` always contributes. The mode a route
/// declares is read from the family's own `sharing` member, which the shipped
/// route schema defaults to `private`.
#[must_use]
pub fn route_contributed(route: &Route) -> ContributedFamilies {
    let share = |sharing: Option<SharingMode>| sharing.unwrap_or(SharingMode::Private);
    ContributedFamilies {
        auth: None,
        rate_limit: route.rate_limit.as_ref().and_then(|limit| {
            visible(share(limit.sharing), limit.clone())
        }),
        plugins: route.plugins.as_ref().and_then(|plugins| {
            visible(share(plugins.sharing), plugins.clone())
        }),
        cors: route.cors.as_ref().and_then(|cors| visible(share(cors.sharing), cors.clone())),
        tags: Some(route.tags.clone()),
    }
}

/// The contribution one family makes when its mode admits one.
fn visible<V>(mode: SharingMode, value: V) -> Option<FamilyContribution<V>> {
    match mode {
        SharingMode::Enforce | SharingMode::Inherit => Some(FamilyContribution { mode, value }),
        SharingMode::Private => None,
    }
}

/// Writes one family's contribution when the row carries the family at all.
fn carry(
    families: &mut ContributedFamilies,
    family: Family,
    mode: SharingMode,
    upstream: &Upstream,
) {
    match family {
        Family::Auth => {
            if let Some(auth) = &upstream.auth {
                families.auth = Some(FamilyContribution {
                    mode,
                    value: auth.clone(),
                });
            }
        }
        Family::RateLimit => {
            if let Some(rate_limit) = &upstream.rate_limit {
                families.rate_limit = Some(FamilyContribution {
                    mode,
                    value: rate_limit.clone(),
                });
            }
        }
        Family::Plugins => {
            if let Some(plugins) = &upstream.plugins {
                families.plugins = Some(FamilyContribution {
                    mode,
                    value: plugins.clone(),
                });
            }
        }
        Family::Cors => {
            if let Some(cors) = &upstream.cors {
                families.cors = Some(FamilyContribution {
                    mode,
                    value: cors.clone(),
                });
            }
        }
    }
}
