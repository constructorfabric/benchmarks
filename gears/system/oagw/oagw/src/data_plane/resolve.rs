//! Consuming the effective configuration at proxy time.
//!
//! Realizes `cpt-cf-oagw-algo-resolve-consume`: the routine the proxy flow
//! reaches after authorization. It looks the normalized alias up in the Data
//! Plane L1 cache, resolves the effective configuration through
//! `cpt-cf-oagw-flow-resolve-effective-config` of
//! `cpt-cf-oagw-feature-hierarchical-config` on a miss, consumes the result in
//! the layer order upstream, then route, then tenant, and answers the four
//! outcomes the caller maps — a resolved configuration, the not-found outcome,
//! the disabled outcome, and the failed-closed outcome.
//!
//! The routine owns the consumption order and the cache, and no merge strategy:
//! the per-family merges are that feature's, called through its entry point.

use std::sync::Arc;

use uuid::Uuid;

use crate::control_plane::alias_derive::{self, DeriveError};
use crate::control_plane::cache::ControlPlaneCache;
use crate::control_plane::effective::{resolve_effective, ResolveError};
use crate::control_plane::chain::walk_candidates;
use crate::control_plane::shadow::shadow_resolve;
use crate::data_plane::cache::{DpCache, DP_CACHE_CAPACITY};
use crate::domain::effective::RouteSelector;
use crate::domain::proxy::{AliasDerivation, ResolvedUpstream, RouteCandidate};

/// The outcome of one consumption, which the caller maps to an answer.
///
/// The not-found outcome covers both an empty candidate set and an unmatched
/// route, and is answered 404 with the `RouteNotFound` variant; the disabled
/// outcome is answered 503 with `LinkUnavailable`; the failed-closed outcome is
/// answered with the platform 500 problem shape and never forwards a request.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// The upstream and its route candidates resolved.
    Resolved(Arc<ResolvedUpstream>),
    /// No chain element holds the alias.
    NotFound,
    /// The effective `enabled` state of the resolved upstream is false.
    Disabled,
    /// The chain is unavailable, unordered, or cyclic, or the alias cannot be
    /// normalized: the resolution failed closed.
    Failed,
}

// @cpt-dod:cpt-cf-oagw-dod-effective-config:p1

/// Consumes the effective configuration one proxy request is subject to.
///
/// The cache is consulted first and populated on a miss; the route keys the
/// candidate set was read under are recorded with the entry so the flush the
/// configuration write path notifies drops them with it.
#[must_use]
pub fn consume(
    store: &crate::store::OagwStore,
    cache: &DpCache,
    calling_tenant: Uuid,
    ancestors: &[Uuid],
    alias: &str,
    selector: &RouteSelector,
) -> Resolution {
    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-key
    // The cache key of ADR 0005: the calling tenant and the normalized alias,
    // which is the shape the flush matches on.
    let normalized = match crate::domain::Alias::parse(alias) {
        Ok(normalized) => normalized,
        Err(_) => return Resolution::Failed,
    };
    let key = DpCache::upstream_key(calling_tenant, normalized.to_string().as_str());
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-key

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-hit-if
    if let Some(hit) = cache.get(&key) {
        // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-hit
        // ADR 0006's flow with caching: the hit answers without a resolution
        // call, so the outcomes the resolution would produce are the ones the
        // entry recorded when it was populated.
        return outcome_of(&hit);
        // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-hit
    }
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-hit-if

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-miss-else
    // The ELSE of the lookup: the key is not in the cache, so the resolution
    // runs and its result is stored under the key of step 1.
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-miss-else

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-resolve
    // The hierarchical-config flow: it walks the tenant chain with shadowing,
    // computes the effective `enabled` state, and merges both layers.
    let resolution = match resolve_effective(store, calling_tenant, ancestors, normalized.to_string().as_str(), selector)
    {
        Ok(Some(resolution)) => resolution,
        Ok(None) => return Resolution::NotFound,
        Err(ResolveError::UnavailableChain) => return Resolution::Failed,
    };
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-resolve

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-order
    // The layer order of consumption is upstream, then route, then tenant: the
    // merged upstream families are read from the upstream layer result, the
    // route families from the route layer result, and the ownership is the
    // tenant the routing target belongs to.
    let (target, candidates) = candidate_set(store, calling_tenant, ancestors, &normalized);
    let Some(target) = target else {
        return Resolution::NotFound;
    };
    let row = &target.row.upstream;
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-order

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-disabled-if
    if !resolution.enabled {
        // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-disabled-return
        // A disabled upstream is never dialed, whatever the chain contributed.
        return Resolution::Disabled;
        // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-disabled-return
    }
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-disabled-if

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-grpc-if
    if row.protocol == crate::gts::PROTOCOL_GRPC {
        // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-grpc-return
        // No HTTP match key is evaluated for a gRPC upstream, so the request is
        // answered with the same not-found outcome any unmatched HTTP request
        // gets.
        return Resolution::NotFound;
        // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-grpc-return
    }
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-grpc-if

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-ok-else
    // The ELSE of the protocol check: the upstream is an HTTP one, so the
    // resolved configuration and its route candidate set are produced.
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-ok-else

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-ok
    let route_keys = candidates
        .iter()
        .filter_map(|candidate| {
            let http = candidate.route.match_config.http.as_ref()?;
            Some(DpCache::route_key(
                candidate.route.upstream_id,
                http.methods.first().map(String::as_str).unwrap_or("GET"),
                http.path.as_str(),
            ))
        })
        .collect();
    let resolved = Arc::new(ResolvedUpstream {
        tenant_id: target.tenant_id,
        upstream_id: target.upstream_id,
        alias: normalized.to_string(),
        alias_derivation: derivation_of(&row.server.endpoints),
        endpoints: row.server.endpoints.clone(),
        protocol: row.protocol.clone(),
        enabled: resolution.enabled,
        headers: row.headers.clone().unwrap_or_default(),
        rate_limit: resolution.upstream.rate_limit,
        plugins: resolution.upstream.plugins,
        cors: resolution.upstream.cors,
        route_candidates: candidates,
    });
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-ok

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-store
    cache.insert(key, Arc::clone(&resolved), route_keys);
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-store

    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-return
    Resolution::Resolved(resolved)
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-return
}

/// The outcome a cached entry answers with.
fn outcome_of(entry: &ResolvedUpstream) -> Resolution {
    if !entry.enabled {
        return Resolution::Disabled;
    }
    if entry.is_grpc() {
        return Resolution::NotFound;
    }
    if entry.route_candidates.is_empty() {
        return Resolution::NotFound;
    }
    Resolution::Resolved(Arc::new(entry.clone()))
}

/// The ordered route candidate set of the chain, most distant first.
///
/// Every chain element that holds an alias-matched upstream row contributes the
/// enabled HTTP routes of its own row, which is the same scoping
/// `cpt-cf-oagw-algo-field-family-merge`'s route layer reads: a route of a
/// tenant outside the chain is never a candidate.
#[must_use]
fn candidate_set(
    store: &crate::store::OagwStore,
    calling_tenant: Uuid,
    ancestors: &[Uuid],
    alias: &crate::domain::Alias,
) -> (
    Option<crate::control_plane::chain::ChainCandidate>,
    Vec<RouteCandidate>,
) {
    // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-fail-if
    // The chain must be readable and orderable for a candidate set to exist at
    // all: an unavailable, unordered, or cyclic chain, or a shadow resolution
    // that finds no target, is the failed-closed outcome.
    let walked = walk_candidates(store, calling_tenant, ancestors, alias)
        .ok()
        .and_then(|candidates| shadow_resolve(&candidates, alias).map(|shadow| (shadow, candidates)));
    let Some((shadow, _candidates)) = walked else {
        // @cpt-begin:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-fail-return
        // Failure with no partial configuration: the caller answers the
        // platform 500 problem shape and never forwards a request resolved
        // against an incomplete chain.
        return (None, Vec::new());
        // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-fail-return
    };
    // @cpt-end:cpt-cf-oagw-algo-resolve-consume:p1:inst-resc-fail-if

    let mut rows: Vec<RouteCandidate> = Vec::new();
    for binding in &shadow.bindings {
        for row in store.routes_of_upstream(binding.tenant_id, binding.upstream_id) {
            if row.route.match_config.http.is_some() {
                rows.push(RouteCandidate {
                    tenant_id: binding.tenant_id,
                    depth: binding.depth,
                    route: row.route,
                });
            }
        }
    }
    for row in store.routes_of_upstream(shadow.target.tenant_id, shadow.target.upstream_id) {
        if row.route.match_config.http.is_some() {
            rows.push(RouteCandidate {
                tenant_id: shadow.target.tenant_id,
                depth: shadow.target.depth,
                route: row.route,
            });
        }
    }
    (Some(shadow.target), rows)
}

/// The alias derivation kind the endpoint-selection matrix keys on.
///
/// A single-endpoint pool never reaches the derivation branch of the matrix, so
/// the kind is recorded as the explicit one; a multi-endpoint pool is derived
/// when the endpoint set yields a common suffix and explicit otherwise.
#[must_use]
fn derivation_of(endpoints: &[crate::domain::Endpoint]) -> AliasDerivation {
    if endpoints.len() < 2 {
        return AliasDerivation::Explicit;
    }
    match alias_derive::derive(endpoints) {
        Ok(_) => AliasDerivation::Derived,
        Err(DeriveError::NonDerivable) => AliasDerivation::Explicit,
    }
}

impl Resolution {
    /// Maps the outcome to the catalogue failure the caller answers with.
    ///
    /// The resolved outcome is not a failure; the not-found outcome is the 404
    /// an unmatched request is answered with, and the disabled outcome is the
    /// 503 an undialable upstream is answered with. The failed-closed outcome
    /// carries no catalogue row: the caller answers it with the platform RFC
    /// 9457 500 problem shape and never forwards a request, so `None` names no
    /// success here — the caller tests the outcome directly.
    #[must_use]
    pub fn failure_of(&self) -> Option<crate::domain::error::DomainError> {
        use crate::domain::error::{DomainError, ErrorKind};
        match self {
            Self::Resolved(_) | Self::Failed => None,
            Self::NotFound => Some(DomainError::gateway(
                ErrorKind::RouteNotFound,
                "no chain element holds the alias the request addressed",
            )),
            Self::Disabled => Some(DomainError::gateway(
                ErrorKind::LinkUnavailable,
                "the resolved upstream is disabled, so the request is never dialed",
            )),
        }
    }
}

/// The control-plane cache generation the resolution observed, for the
/// diagnostics of the observability feature.
///
/// The Data Plane cache carries no generation of its own: its invalidation is
/// the explicit flush, and the generation the write path advances is the
/// control plane's.
#[must_use]
pub fn control_plane_generation(cache: &ControlPlaneCache) -> u64 {
    cache.generation()
}

/// The entry count the Data Plane cache holds, for the diagnostics of the
/// observability feature.
#[must_use]
pub fn cache_entries(cache: &DpCache) -> usize {
    let _ = DP_CACHE_CAPACITY;
    cache.len()
}
