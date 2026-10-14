//! The persistence port of the upload reaper
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper`): the stale-row selection
//! and the per-row guarded failure transaction. `infra::upload_reaper` runs the scans over it and
//! `infra::storage::attachment_repo` implements it.

use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::model::{AttachmentKind, AttachmentStatus};
use crate::domain::attachments::store::AttachmentCleanupQueue;
use crate::domain::error::DomainError;

/// The rows one scan selects at most (fixed, not configurable).
pub const REAPER_BATCH: u32 = 100;

/// A stale `pending` or `uploaded` row the reaper found.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleUpload {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// `pending` or `uploaded`: the `from_status` of the abandoned metric.
    pub status: AttachmentStatus,
    pub provider_file_id: Option<String>,
    pub storage_backend: String,
    pub kind: AttachmentKind,
    pub secondary_file_id: Option<String>,
    pub updated_at: OffsetDateTime,
}

/// The persistence of the reaper.
#[async_trait]
pub trait UploadReaperStore: Send + Sync {
    /// At most `limit` rows with `status` `pending` or `uploaded`, `deleted_at IS NULL`,
    /// `cleanup_status IS NULL` and `updated_at` before `cutoff`, oldest `updated_at` first (ties
    /// by id); the comparison and the order use the chronological timestamp key on `SQLite`,
    /// never raw text comparison; every tenant is scanned.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn select_stale(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<StaleUpload>, DomainError>;

    /// One transaction: set `status` `failed`, `error_code` `upload_abandoned` and `updated_at`
    /// `now`, guarded by the same status, `deleted_at IS NULL`, `cleanup_status IS NULL` and
    /// `updated_at` before `cutoff`; no row affected rolls back and returns `false` (skipped).
    /// With a `provider_file_id` the same transaction sets `cleanup_status` `pending` and
    /// `cleanup_updated_at` and enqueues one `attachment_upload_abandoned` message through
    /// `cleanup` (`secondary_ref` null); the wake fires after the commit. `true` when the row
    /// was reaped.
    ///
    /// # Errors
    ///
    /// The enqueue error or `Internal`; the transaction is rolled back and nothing changed.
    async fn reap(
        &self,
        row: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError>;
}
