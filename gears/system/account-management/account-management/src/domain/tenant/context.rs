//! AM-internal resolved-tenant snapshot.
//!
//! Distinct type from the public-SPI
//! [`account_management_sdk::IdpTenantContext`] envelope so AM-internal
//! evolution (additional resolver fields, internal correlation handles,
//! audit-only metadata) does not leak into the `IdpPluginClient`
//! contract and vice versa. The field shape is identical today; the
//! types are kept separate by design so they can diverge.
//!
//! The conversion at the SPI boundary goes through the
//! [`From<&TenantContext> for IdpTenantContext`] impl declared below
//! — every `IdpPluginClient` request builder constructs its
//! `IdpTenantContext` from the AM-internal value via `(&ctx).into()`.

use account_management_sdk::IdpTenantContext;
use gts::GtsTypeId;
use serde_json::Value;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Resolved tenant snapshot built by the AM-internal saga / service
/// layer (`TenantService::resolve_active_tenant`,
/// `UserService::resolve_active_tenant`, the retention + reaper
/// pipelines, the bootstrap saga). Carries everything an
/// `IdpPluginClient` call needs plus any future AM-internal fields
/// (correlation handles, audit-only metadata) that MUST NOT leak
/// into the public plugin contract.
///
/// Convert to the public-SPI envelope via the
/// [`From<&TenantContext> for IdpTenantContext`] impl at every
/// `IdpPluginClient` request build-site.
///
/// `#[non_exhaustive]` preserves the same forward-compat guarantee as
/// the public SDK type — adding internal fields stays a minor-version
/// change for impl-crate consumers.
#[domain_model]
#[derive(Debug, Clone)]
#[non_exhaustive]
#[allow(
    clippy::struct_field_names,
    reason = "every field IS tenant-scoped (id / name / type / metadata); stripping the prefix loses the contract that the value comes from AM-resolved tenant state — same rationale as the SDK-side IdpTenantContext"
)]
pub struct TenantContext {
    /// Stable tenant identifier (the same UUID stored in AM's
    /// `tenants.id` column).
    pub tenant_id: Uuid,
    /// Human-readable tenant name at resolution time. AM treats it as
    /// mutable (tenant rename is allowed) so the value reflects AM's
    /// source of truth at call time, not a snapshot from the
    /// provisioning call.
    pub tenant_name: String,
    /// Resolved tenant type as a chained `GtsTypeId`
    /// (e.g. `gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~`).
    /// Mandatory at the resolve-helper boundary — a Types Registry
    /// outage surfaces as `DomainError::ServiceUnavailable` rather
    /// than leaking an `Option` through this struct.
    pub tenant_type: GtsTypeId,
    /// Opaque plugin-private metadata loaded from
    /// `tenant_idp_metadata`. AM does NOT interpret the shape; the
    /// plugin owns it end-to-end. `None` when no row exists for the
    /// tenant (plugin never produced a per-tenant blob) or the row's
    /// `metadata` column is SQL NULL.
    pub metadata: Option<Value>,
}

impl TenantContext {
    /// Build a [`TenantContext`] from the resolved tenant snapshot.
    /// Used by the AM-internal resolve helpers and saga builders.
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        tenant_name: impl Into<String>,
        tenant_type: GtsTypeId,
        metadata: Option<Value>,
    ) -> Self {
        Self {
            tenant_id,
            tenant_name: tenant_name.into(),
            tenant_type,
            metadata,
        }
    }
}

/// Field-by-field clone into the public-SPI envelope. The two types
/// are intentionally separate so AM-internal additions do not appear
/// on the plugin SPI; today the conversion is a verbatim clone of the
/// four shared fields.
///
/// `&TenantContext` (not `TenantContext`) so the same internal value
/// can be reused on subsequent builder calls (e.g. retention pipeline
/// invoking `deprovision_tenant` after building a separate
/// audit-side context from the same resolved snapshot).
impl From<&TenantContext> for IdpTenantContext {
    fn from(ctx: &TenantContext) -> Self {
        Self::new(
            ctx.tenant_id,
            ctx.tenant_name.clone(),
            ctx.tenant_type.clone(),
            ctx.metadata.clone(),
        )
    }
}
