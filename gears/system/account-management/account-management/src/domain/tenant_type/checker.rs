//! Tenant-type compatibility barrier — Feature 2.3 (`tenant-type-enforcement`).
//!
//! Pre-write barrier invoked by saga step 3 (`inst-algo-saga-type-check`) of
//! `algo-tenant-hierarchy-management-create-tenant-saga` before any
//! `tenants` or `tenant_closure` row is rewritten. Evaluates the parent
//! `tenant_type` against the child type's `allowed_parent_types` trait
//! resolved through the GTS Types Registry (`gts.cf.core.am.tenant_type.v1~`).
//!
//! This gear owns the **trait abstraction**. Two implementations exist:
//!
//! * [`InertTenantTypeChecker`] — admits everything. Used in unit tests
//!   only; not bound in production. AM declares `types-registry` as a
//!   hard `deps` and the gear entry-point fail-closes `init` when
//!   `TypesRegistryClient` cannot be resolved from `ClientHub`, so the
//!   real implementation below is always wired in production.
//! * [`crate::infra::types_registry::GtsTenantTypeChecker`] — the real
//!   implementation that wraps `types_registry_sdk::TypesRegistryClient`.
//!   Resolves both schemas via `get_type_schemas_by_uuid` (one batched
//!   round-trip), reads `effective_traits().allowed_parent_types`, and
//!   admits iff the parent's chained GTS identifier is a member.
//!   The `ClientHub` binding lives in the AM gear entry-point.
//!
//! Failure mapping per FEATURE §6 (status / reason follow the
//! canonical mapping in [`crate::infra::canonical_mapping`]):
//!
//! * Child / parent type pairing not allowed → [`DomainError::TypeNotAllowed`]
//!   (HTTP 400, `failed_precondition`, reason `TYPE_NOT_ALLOWED`).
//! * Same-type nesting not permitted → [`DomainError::TypeNotAllowed`].
//! * Registry unreachable / trait-resolution failure →
//!   [`DomainError::ServiceUnavailable`] (HTTP 503, canonical
//!   `unavailable`).
//! * Child / parent type not registered or malformed trait →
//!   [`DomainError::InvalidTenantType`] (HTTP 400,
//!   `invalid_argument`, reason `INVALID_TENANT_TYPE`).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Pre-write tenant-type compatibility barrier.
///
/// Implementations resolve the GTS `allowed_parent_types` trait for both
/// the child and parent tenant types and answer one yes/no question:
/// is this parent / child type pairing admitted by the registered
/// schema? Same-type nesting is admitted iff the type's
/// `allowed_parent_types` trait contains the type's own chained GTS
/// identifier (per `algo-same-type-nesting-admission`).
///
/// The barrier MUST NOT cache type definitions across calls
/// (`dod-tenant-type-enforcement-gts-availability-surface`); every
/// invocation re-resolves against GTS so trait updates and re-types
/// take effect immediately.
#[async_trait]
pub trait TenantTypeChecker: Send + Sync {
    /// Validate parent-child type compatibility.
    ///
    /// # Errors
    ///
    /// * [`DomainError::TypeNotAllowed`] — the parent type is not a member of
    ///   the child type's effective `allowed_parent_types`, or same-type
    ///   nesting was requested but the trait does not include the child
    ///   type's own identifier.
    /// * [`DomainError::InvalidTenantType`] — the child type is not
    ///   registered or its effective trait is malformed.
    /// * [`DomainError::ServiceUnavailable`] — the GTS Types Registry is
    ///   unreachable, times out, or returns a trait-resolution failure.
    // @cpt-begin:cpt-cf-account-management-algo-tenant-type-enforcement-allowed-parent-types-evaluation:p1:inst-algo-apte-trait-contract
    // @cpt-begin:cpt-cf-account-management-algo-tenant-type-enforcement-same-type-nesting-admission:p1:inst-algo-stn-trait-contract
    async fn check_parent_child(
        &self,
        parent_type: Uuid,
        child_type: Uuid,
    ) -> Result<(), DomainError>;
    // @cpt-end:cpt-cf-account-management-algo-tenant-type-enforcement-same-type-nesting-admission:p1:inst-algo-stn-trait-contract
    // @cpt-end:cpt-cf-account-management-algo-tenant-type-enforcement-allowed-parent-types-evaluation:p1:inst-algo-apte-trait-contract
}

/// No-op checker — admits every parent / child pairing. Reserved for
/// unit tests; not bound in production. AM declares `types-registry`
/// as a hard `deps` and the gear entry-point fail-closes `init`
/// when `TypesRegistryClient` cannot be resolved from `ClientHub`.
#[domain_model]
#[derive(Debug, Default, Clone)]
pub struct InertTenantTypeChecker;

#[async_trait]
impl TenantTypeChecker for InertTenantTypeChecker {
    async fn check_parent_child(
        &self,
        _parent_type: Uuid,
        _child_type: Uuid,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Build an `Arc<dyn TenantTypeChecker>` over [`InertTenantTypeChecker`].
/// Used by unit tests so callers don't have to spell out the
/// `Arc::new(InertTenantTypeChecker)` form at every site.
#[must_use]
pub fn inert_tenant_type_checker() -> Arc<dyn TenantTypeChecker + Send + Sync> {
    Arc::new(InertTenantTypeChecker)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inert_checker_admits_every_pairing() {
        let c = InertTenantTypeChecker;
        let parent = Uuid::from_u128(0x1);
        let child = Uuid::from_u128(0x2);
        c.check_parent_child(parent, child)
            .await
            .expect("inert admits any pairing");
    }

    #[tokio::test]
    async fn inert_checker_admits_same_type_nesting() {
        let c = InertTenantTypeChecker;
        let same = Uuid::from_u128(0x3);
        c.check_parent_child(same, same)
            .await
            .expect("inert admits same-type nesting");
    }
}
