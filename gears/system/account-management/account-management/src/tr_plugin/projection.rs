//! AM-row to SDK-type projection helpers.
//!
//! Provisioning rows are excluded structurally by the
//! [`crate::domain::tenant::hierarchy_read_port::TenantHierarchyReadPort`]
//! impl at query time; the `None` arms on these projection helpers
//! are defense-in-depth catches for the rare case where a
//! provisioning row leaks through the port.

use tenant_resolver_sdk::{TenantId, TenantInfo, TenantRef, TenantStatus as SdkTenantStatus};

use crate::domain::tenant::model::{TenantModel, TenantStatus as DomainTenantStatus};

/// Map AM's domain `TenantStatus` (4-variant, includes `Provisioning`)
/// onto the SDK-visible 3-variant enum. Returns `None` for
/// `Provisioning` so callers fail closed if a provisioning row ever
/// reaches projection.
#[must_use]
pub(super) fn map_status_to_sdk(status: DomainTenantStatus) -> Option<SdkTenantStatus> {
    match status {
        DomainTenantStatus::Active => Some(SdkTenantStatus::Active),
        DomainTenantStatus::Suspended => Some(SdkTenantStatus::Suspended),
        DomainTenantStatus::Deleted => Some(SdkTenantStatus::Deleted),
        DomainTenantStatus::Provisioning => None,
    }
}

/// Project a [`TenantModel`] onto the SDK [`TenantInfo`].
///
/// `tenant_type` is supplied by the caller. The `None` return arm
/// signals defense-in-depth against a provisioning row reaching
/// projection (the port should have excluded it).
#[must_use]
pub(super) fn row_to_tenant_info(
    row: TenantModel,
    tenant_type: Option<String>,
) -> Option<TenantInfo> {
    let sdk_status = map_status_to_sdk(row.status)?;
    Some(TenantInfo {
        id: TenantId(row.id),
        name: row.name,
        status: sdk_status,
        tenant_type,
        parent_id: row.parent_id.map(TenantId),
        self_managed: row.self_managed,
    })
}

/// Project a [`TenantModel`] onto the SDK [`TenantRef`] (no name).
#[must_use]
pub(super) fn row_to_tenant_ref(
    row: &TenantModel,
    tenant_type: Option<String>,
) -> Option<TenantRef> {
    let sdk_status = map_status_to_sdk(row.status)?;
    Some(TenantRef {
        id: TenantId(row.id),
        status: sdk_status,
        tenant_type,
        parent_id: row.parent_id.map(TenantId),
        self_managed: row.self_managed,
    })
}
