//! Full-replacement diff — `cpt-cf-oagw-algo-put-replace-diff`.
//!
//! Builds the write set one `PUT` applies in one transaction: the immutable
//! fields come from the addressed row, every configuration family is
//! overwritten with the body's value, the optional families the body omits are
//! cleared, the tags are replaced in full, and `enabled` is the one family
//! carried forward when the body omits it.
//!
//! The diff is where the two cross-row checks of a replacement run: a route
//! re-runs match uniqueness against the other enabled routes of its upstream,
//! and an upstream recomputes its derived alias and compares it with the
//! stored one.

use uuid::Uuid;

use crate::control_plane::alias_derive;
use crate::domain::alias::Alias;
use crate::control_plane::match_uniqueness;
use crate::control_plane::validation::ResourceKind;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::route::Route;
use crate::domain::upstream::Upstream;
use crate::store::{OagwStore, RouteRow, UpstreamRow};

/// The write set one replacement produces, applied in one transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteSet<T> {
    /// Owning tenant, taken from the addressed row and never from the body.
    pub tenant_id: Uuid,
    /// Addressed identifier, taken from the addressed row and never from the
    /// body.
    pub id: Uuid,
    /// The row content to write.
    pub value: T,
    /// Whether anything differs; a write set that is empty still reaches the
    /// cache flush.
    pub changed: bool,
}

/// Builds the write set an upstream replacement applies.
///
/// # Errors
///
/// Returns a validation error when the body states an identifier other than
/// the addressed one, and the `AliasConflict` catalogue row — which answers
/// 409 — when the replacement endpoints derive a different alias.
#[allow(clippy::result_large_err)]
pub fn upstream_diff(
    stored: &UpstreamRow,
    stated_id: Option<Uuid>,
    mut replacement: Upstream,
) -> Result<WriteSet<Upstream>, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-immutable
    if let Some(stated) = stated_id
        && stated != stored.upstream.id
    {
        return Err(immutable(ResourceKind::Upstream, "id"));
    }
    let tenant_id = stored.tenant_id;
    let id = stored.upstream.id;
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-immutable

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-upstream-if
    let stored_alias = stored
        .upstream
        .alias
        .as_deref()
        .and_then(|alias| Alias::parse(alias).ok());
    let alias = alias_derive::resolve(
        &replacement.server.endpoints,
        replacement.alias.as_deref(),
        stored_alias.as_ref(),
    );
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-upstream-alias
    // The replacement flow answers a recomputed alias that differs from the
    // stored one with 409 `AliasConflict`: the alias is immutable across
    // updates and the stored alias is left unchanged.
    // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-alias-return
    let alias = alias?;
    // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-alias-return
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-upstream-alias
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-upstream-if
    replacement.alias = Some(alias.to_string());

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-clear
    replacement.id = id;
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-clear

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-tags
    let tags = replacement.tags.clone();
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-tags

    let changed = {
        let mut candidate = replacement.clone();
        let mut held = stored.upstream.clone();
        candidate.tags = Vec::new();
        held.tags = Vec::new();
        candidate != held || tags != stored.tags
    };

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-return
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty-if
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty
    Ok(WriteSet {
        tenant_id,
        id,
        value: replacement,
        changed,
    })
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty-if
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-return
}

/// Builds the write set a route replacement applies.
///
/// # Errors
///
/// Returns a validation error when the body states an identifier or an
/// upstream reference other than the addressed row's, and the `MatchConflict`
/// catalogue row — which answers 409 — when another enabled route of the same
/// upstream holds the match rule.
#[allow(clippy::result_large_err)]
pub fn route_diff(
    store: &OagwStore,
    stored: &RouteRow,
    stated_id: Option<Uuid>,
    mut replacement: Route,
) -> Result<WriteSet<Route>, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-immutable
    if let Some(stated) = stated_id
        && stated != stored.route.id
    {
        return Err(immutable(ResourceKind::Route, "id"));
    }
    let tenant_id = stored.tenant_id;
    let id = stored.route.id;
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-immutable

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-route-if
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-route-upstream
    if replacement.upstream_id != Uuid::nil() && replacement.upstream_id != stored.route.upstream_id
    {
        return Err(immutable(ResourceKind::Route, "upstream_id"));
    }
    replacement.upstream_id = stored.route.upstream_id;
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-route-upstream
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-route-if

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-route-unique
    match_uniqueness::confirm_route(store, tenant_id, &replacement, Some(id))?;
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-route-unique

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-clear
    // Every configuration family is overwritten with the body's value, so an
    // optional family the body omits is cleared; `enabled` is the one family
    // carried forward when the body omits it.
    replacement.id = id;
    replacement.enabled = replacement.enabled.or(stored.route.enabled);
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-clear

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-tags
    let tags = replacement.tags.clone();
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-tags

    let changed = {
        let mut candidate = replacement.clone();
        let mut held = stored.route.clone();
        candidate.tags = Vec::new();
        held.tags = Vec::new();
        candidate != held || tags != stored.tags
    };

    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-return
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty-if
    // @cpt-begin:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty
    Ok(WriteSet {
        tenant_id,
        id,
        value: replacement,
        changed,
    })
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-empty-if
    // @cpt-end:cpt-cf-oagw-algo-put-replace-diff:p1:inst-diff-return
}

/// The validation failure for a body that states an immutable field with a
/// value other than the addressed row's.
fn immutable(kind: ResourceKind, field: &str) -> DomainError {
    DomainError::gateway(
        ErrorKind::ValidationError,
        format!("{field} of a {} is immutable", kind.as_str()),
    )
}
