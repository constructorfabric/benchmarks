//! Tenant-metadata domain gear.
//!
//! Implements FEATURE `tenant-metadata` (see
//! `gears/system/account-management/docs/features/feature-tenant-metadata.md`).
//! Owns [`MetadataService`](service::MetadataService), the
//! [`MetadataRepo`](repo::MetadataRepo) storage seam, the GTS schema
//! [`registry`] port, and the [`MetadataRow`] / [`UpsertOutcome`] value
//! types projected by the repo.

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use toolkit_macros::domain_model;

pub mod registry;
pub mod repo;
pub mod service;
pub mod type_id;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_tests.rs"]
mod service_tests;

/// One direct-on-tenant metadata entry.
///
/// Mirrors [`crate::infra::storage::entity::tenant_metadata::Model`]
/// column-for-column. The `value` column carries an opaque
/// GTS-validated payload; the storage entity types it as `Json` while
/// this domain model uses [`serde_json::Value`] so the service layer
/// can pass payloads from `account-management-sdk::metadata` without
/// dragging the `SeaORM` `Json` newtype into the public surface.
#[domain_model]
#[derive(Debug, Clone)]
pub struct MetadataRow {
    pub tenant_id: Uuid,
    pub schema_uuid: Uuid,
    pub value: Value,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// Monotonic version for the optimistic-lock contract: starts at
    /// 1, +1 per UPDATE. Pinned in
    /// `UpsertMetadataRequest::expected_version`.
    pub version: i64,
}

/// Discriminated upsert result returned by
/// [`repo::MetadataRepo::upsert_for_tenant`].
///
/// The service layer maps the discriminator onto HTTP 201 vs 200 per
/// FEATURE §3.3 / §6 AC line 393. Both arms carry the post-upsert row
/// snapshot so the handler can build the response body without a
/// follow-up `SELECT`.
#[domain_model]
#[derive(Debug, Clone)]
pub enum UpsertOutcome {
    /// The row did not exist before this call — maps to HTTP 201.
    Inserted(MetadataRow),
    /// The row already existed and was updated — maps to HTTP 200.
    Updated(MetadataRow),
}

impl UpsertOutcome {
    /// Borrow the post-upsert row snapshot regardless of arm.
    #[must_use]
    pub fn row(&self) -> &MetadataRow {
        match self {
            Self::Inserted(row) | Self::Updated(row) => row,
        }
    }

    /// Convert into the post-upsert row, dropping the insert/update
    /// discriminator. Useful for unit tests that only need to assert on
    /// the column shape.
    #[must_use]
    pub fn into_row(self) -> MetadataRow {
        match self {
            Self::Inserted(row) | Self::Updated(row) => row,
        }
    }

    /// Returns `true` iff the upsert created a new row.
    #[must_use]
    pub const fn was_inserted(&self) -> bool {
        matches!(self, Self::Inserted(_))
    }
}
