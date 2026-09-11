//! Binding-style creation with tenant-local tags —
//! `cpt-cf-oagw-algo-bind-create-tags`.
//!
//! A create whose normalized alias matches an ancestor's upstream is not an
//! ordinary create: it binds the descendant's row to the ancestor's, and the
//! write it produces is the descendant's own row and nothing else. This module
//! turns the validated create body and the sharing-mode decision into that
//! write set, or returns the refusal the decision produced, so the management
//! flow persists one row and touches no ancestor row at all — including no
//! ancestor tag row.

// @cpt-dod:cpt-cf-oagw-dod-binding-style-creation:p1

use uuid::Uuid;

use crate::control_plane::sharing::{Decisions, Refusal};
use crate::domain::effective::Family;
use crate::domain::upstream::Upstream;

/// The write set a bind-style create persists: the descendant's own row.
#[derive(Debug, Clone, PartialEq)]
pub struct BindWriteSet {
    /// The calling tenant, which owns the row the write persists.
    pub tenant_id: Uuid,
    /// The row to persist, carrying the body's own families and tags.
    pub value: Upstream,
    /// Whether the create bound the descendant's row to an ancestor's.
    pub bound: bool,
}

/// Produces the write set one create persists, binding or ordinary.
///
/// `ancestors` is the number of ancestor bindings the chain walk resolved for
/// the normalized alias at a depth greater than the calling tenant's; zero of
/// them means no ancestor holds the alias and the operation is an ordinary
/// create.
///
/// # Errors
///
/// Returns the refusal the sharing-mode decision produced, which the caller
/// answers instead of writing any row.
pub fn write_set(
    calling_tenant: Uuid,
    ancestors: usize,
    value: Upstream,
    decided: &Result<Decisions, Refusal>,
) -> Result<BindWriteSet, Refusal> {
    // @cpt-begin:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-confirm
    // Confirm the binding: an ancestor row at a greater depth with the same
    // normalized alias. Anything else is not a bind, and the ordinary create
    // path applies.
    let bound = ancestors > 0;
    // @cpt-end:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-confirm

    // @cpt-begin:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-identity
    // The row's identity is the calling tenant's own: `tenant_id` is the
    // calling tenant and `id` is the identifier the create path already
    // derived from nothing but the request, so the ancestor's identifiers, its
    // alias row, and its endpoint set are never copied onto the descendant's
    // row.
    let tenant_id = calling_tenant;
    let mut row = value;
    // @cpt-end:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-identity

    // @cpt-begin:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-families
    // A family whose decision is `own` or `inherit-base` is written as the
    // body carries it; a family whose decision is `forced` is never written to
    // the descendant's row, because the ancestor's live value is applied by
    // `cpt-cf-oagw-algo-field-family-merge` at resolution time. A `forced`
    // decision only arises for a family the body omitted, so the clearing
    // below restates that rule on the row rather than discarding a value the
    // body carried — the decision refuses such a body before this routine runs.
    if let Ok(decisions) = decided {
        for family in [
            Family::Auth,
            Family::RateLimit,
            Family::Plugins,
            Family::Cors,
        ] {
            if !decisions.forced(family) {
                continue;
            }
            match family {
                Family::Auth => row.auth = None,
                Family::RateLimit => row.rate_limit = None,
                Family::Plugins => row.plugins = None,
                Family::Cors => row.cors = None,
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-families

    // @cpt-begin:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-tags
    // The request tags travel on the descendant's row only, exactly as the
    // ordinary create's tag write stores them: `row.tags` is the body's own
    // list, the write that persists it reaches no ancestor's tag rows, and the
    // effective tag set is the add-only union the merge computes at resolution
    // time.
    // @cpt-end:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-tags

    // @cpt-begin:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-refusal-if
    if let Err(refusal) = decided {
        // @cpt-begin:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-refusal-return
        // No row is written and no ancestor value is disclosed.
        return Err(*refusal);
        // @cpt-end:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-refusal-return
    }
    // @cpt-end:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-refusal-if

    // @cpt-begin:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-return
    Ok(BindWriteSet {
        tenant_id,
        value: row,
        bound,
    })
    // @cpt-end:cpt-cf-oagw-algo-bind-create-tags:p1:inst-bindtags-return
}
