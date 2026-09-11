//! Route match uniqueness — `cpt-cf-oagw-algo-match-uniqueness`.
//!
//! One key per declared method, compared against the enabled routes of the
//! same upstream excluding the row being replaced. The comparison set is the
//! derived enabled-match index the store maintains on every write, which is
//! what makes the check a lookup rather than a scan. Two routes that differ in
//! any one of the three components never collide, and two disabled routes with
//! identical keys are stored without a conflict.
//!
//! A hit answers the catalogue's `MatchConflict` row — 409 — naming the
//! colliding route's identifier, which is system-generated and so carries no
//! request value.

use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::route::Route;
use crate::store::{MatchKey, OagwStore};

/// Expands one route into one key per declared method.
///
/// A route that declares three methods contributes three keys, each carrying
/// the path and the priority.
#[must_use]
pub fn match_keys(route: &Route) -> Vec<MatchKey> {
    // @cpt-begin:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-expand
    let priority = route.priority.unwrap_or_default();
    let http = &route.match_config.http;
    let grpc = &route.match_config.grpc;
    let mut keys = Vec::new();
    if let Some(http) = http {
        keys.extend(http.methods.iter().map(|method| MatchKey {
            upstream_id: route.upstream_id,
            path: http.path.clone(),
            priority,
            method: method.clone(),
        }));
    }
    if let Some(grpc) = grpc {
        keys.push(MatchKey {
            upstream_id: route.upstream_id,
            path: grpc.service.clone(),
            priority,
            method: grpc.method.clone(),
        });
    }
    keys
    // @cpt-end:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-expand
}

/// Confirms no other enabled route of the same upstream holds an incoming key.
///
/// # Errors
///
/// Returns the `MatchConflict` catalogue row — which answers 409 — naming the
/// colliding route's identifier on the first hit.
#[allow(clippy::result_large_err)]
pub fn confirm(
    keys: &[MatchKey],
    comparison: &std::collections::BTreeMap<MatchKey, Uuid>,
    replaced: Option<Uuid>,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-loop
    for key in keys {
        // @cpt-begin:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-collide-if
        if let Some(holder) = comparison.get(key)
            && Some(*holder) != replaced
        {
            // @cpt-begin:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-collide
            return Err(conflict(*holder));
            // @cpt-end:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-collide
        }
        // @cpt-end:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-collide-if
    }
    // @cpt-end:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-loop
    // @cpt-begin:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-return
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-return
}

/// Confirms the match rule of one route against the store's derived index.
///
/// The comparison set is restricted to the enabled routes of the same upstream
/// and excludes the row being replaced; a disabled incoming route contributes
/// no key and is never in conflict.
///
/// # Errors
///
/// Returns the `MatchConflict` catalogue row on the first hit.
#[allow(clippy::result_large_err)]
pub fn confirm_route(
    store: &OagwStore,
    tenant_id: Uuid,
    route: &Route,
    replaced: Option<Uuid>,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-set
    if !route.enabled.unwrap_or(true) {
        return Ok(());
    }
    let comparison = store.enabled_match_index(tenant_id);
    // @cpt-end:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-set
    confirm(&match_keys(route), &comparison, replaced)
}

/// The 409 the first colliding key answers with.
fn conflict(holder: Uuid) -> DomainError {
    DomainError::gateway(
        ErrorKind::MatchConflict,
        format!("route {holder} already holds this match rule"),
    )
}
