//! Error-mapping helpers between domain `Result`s and the SDK error
//! taxonomy.
//!
//! The plugin only ever surfaces two SDK variants: `TenantNotFound`
//! (constructed at call sites that distinguish "no row" from
//! "transient failure") and `Internal` (every transient backend
//! failure -- DESIGN §3.8 + FEATURE §5).
//!
//! Backend error details (SQL fragments, scope diagnostics, etc.)
//! are emitted via `tracing::warn!` for operator visibility but are
//! NEVER returned to the plugin caller -- the SDK boundary receives
//! only an opaque marker so a misconfigured pool cannot leak DSN-
//! shaped strings to gateway clients.

use tenant_resolver_sdk::TenantResolverError;

use crate::domain::error::DomainError;

const STORAGE_INTERNAL_MSG: &str = "tenant resolver storage failure";

/// Convert a `DomainError` returned by the `TenantHierarchyReadPort`
/// adapter into a transient SDK error. The detailed cause is logged
/// server-side; only an opaque marker is returned to the plugin
/// caller.
#[must_use]
pub(super) fn domain_err_to_tr_err(err: &DomainError) -> TenantResolverError {
    tracing::warn!(
        target: "tr_plugin",
        error = %err,
        "tr-plugin domain port read failed"
    );
    TenantResolverError::Internal(STORAGE_INTERNAL_MSG.to_owned())
}
