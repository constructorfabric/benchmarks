//! The per-chat vector store provisioning
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning`,
//! `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`): placeholder insert,
//! single-winner create, compare-and-set, the 5-poll loser path, the 120 s stale reclaim and the
//! provider mismatch rejection. Every access is by `(tenant_id, chat_id)`.

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::provider::{StorageProvider, StorageTarget};
use crate::domain::attachments::timing::Timer;
use crate::domain::error::DomainError;
use crate::domain::quota::Clock;

/// A `chat_vector_stores` row.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorStoreRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// `None` while the provider create is in progress (a placeholder).
    pub vector_store_id: Option<String>,
    pub provider: String,
    pub file_count: i32,
    pub created_at: OffsetDateTime,
}

/// The result of the placeholder insert.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderInsert {
    /// This request owns the placeholder with this row id and must create the store.
    Inserted(Uuid),
    /// `UNIQUE(tenant_id, chat_id)` was violated: another request won.
    AlreadyExists,
}

/// The persistence of `chat_vector_stores`; every method filters on `tenant_id` and `chat_id`,
/// never on the row id or the vector store id alone, and each call commits on its own.
#[async_trait]
pub trait ChatVectorStoreRepo: Send + Sync {
    /// The row of the chat.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn find(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError>;

    /// Insert the placeholder (`vector_store_id` NULL, `file_count` 0, `created_at = now`).
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure other than the unique violation.
    async fn insert_placeholder(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &str,
        now: OffsetDateTime,
    ) -> Result<PlaceholderInsert, DomainError>;

    /// Delete the placeholder guarded by its `id`, `tenant_id`, `chat_id` and
    /// `vector_store_id IS NULL`; whether a row was deleted.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn delete_placeholder(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<bool, DomainError>;

    /// The compare-and-set: store `vector_store_id` where the row `id` matches and
    /// `vector_store_id IS NULL`; whether a row was updated.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn set_vector_store_id(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError>;
}

/// Why no vector store could be provided.
#[domain_model]
#[derive(Debug)]
pub enum VectorStoreFailure {
    /// The provider create failed, or no store appeared within the loser polls.
    VectorStoreFailed,
    /// The chat's store belongs to another `storage_backend`.
    ProviderMismatch,
    /// A database failure.
    Internal(DomainError),
}

/// The collaborators of the provisioning.
#[domain_model]
pub struct VectorStoreDeps<'a> {
    pub repo: &'a dyn ChatVectorStoreRepo,
    pub provider: &'a dyn StorageProvider,
    pub clock: &'a dyn Clock,
    pub timer: &'a dyn Timer,
    pub target: &'a StorageTarget,
}

/// The stored store of a row: the provider must equal the resolved `storage_backend`.
fn check_row(row: &VectorStoreRow, backend: &str) -> Result<String, VectorStoreFailure> {
    match &row.vector_store_id {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-mismatch
        Some(id) if row.provider == backend => Ok(id.clone()),
        Some(_) => Err(VectorStoreFailure::ProviderMismatch),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-mismatch
        None => Err(VectorStoreFailure::VectorStoreFailed),
    }
}

/// Whether a placeholder is older than the stale threshold.
fn is_stale(clock: &dyn Clock, row: &VectorStoreRow, stale_after: time::Duration) -> bool {
    clock.now() - row.created_at > stale_after
}

/// The loser path: at most 5 polls, waiting 200 ms before the first and doubling the wait before
/// each next one; the loser never calls provider create.
async fn loser_path(
    deps: &VectorStoreDeps<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<String, VectorStoreFailure> {
    use crate::domain::attachments::timing::{LOSER_INITIAL_WAIT, LOSER_POLLS};

    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-loser-poll
    let mut wait = LOSER_INITIAL_WAIT;
    for _ in 0..LOSER_POLLS {
        deps.timer.sleep(wait).await;
        wait *= 2;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-loser-read
        let row = deps
            .repo
            .find(tenant_id, chat_id)
            .await
            .map_err(VectorStoreFailure::Internal)?;
        if let Some(row) = row
            && row.vector_store_id.is_some()
        {
            return check_row(&row, &deps.target.storage_backend);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-loser-read
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-loser-poll
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-return-loser-failed
    // no store appeared within the polls
    Err(VectorStoreFailure::VectorStoreFailed)
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-return-loser-failed
}

/// Log a best-effort placeholder delete that failed.
fn log_failed_delete(result: Result<bool, DomainError>) {
    if let Err(error) = result {
        tracing::warn!(%error, "the vector store placeholder was not deleted");
    }
}

/// The winner path: create the store at the provider and bind it to the placeholder with a
/// compare-and-set.
async fn winner_path(
    deps: &VectorStoreDeps<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
    row_id: Uuid,
) -> Result<String, VectorStoreFailure> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-winner
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-create
    let created = deps.provider.create_vector_store(deps.target).await;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-create
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-create-failed
    let Ok(created) = created else {
        // the placeholder is deleted best effort
        log_failed_delete(
            deps.repo
                .delete_placeholder(tenant_id, chat_id, row_id)
                .await,
        );
        return Err(VectorStoreFailure::VectorStoreFailed);
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-create-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-cas
    let bound = deps
        .repo
        .set_vector_store_id(tenant_id, chat_id, row_id, &created)
        .await
        .map_err(VectorStoreFailure::Internal)?;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-cas
    if bound {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-return-created
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-bound
        return Ok(created);
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-bound
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-return-created
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-cas-lost
    // the placeholder was reclaimed meanwhile: the new store is deleted best effort
    if deps
        .provider
        .delete_vector_store(deps.target, &created)
        .await
        .is_err()
    {
        tracing::warn!("the surplus vector store was not deleted");
    }
    loser_path(deps, tenant_id, chat_id).await
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-cas-lost
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-winner
}

/// Get or create the provider vector store of the chat; its `provider` must equal
/// `target.storage_backend`.
///
/// No database transaction or connection is held across a provider call: every repository call
/// commits on its own.
///
/// # Errors
///
/// [`VectorStoreFailure`].
pub async fn provision_vector_store(
    deps: &VectorStoreDeps<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<String, VectorStoreFailure> {
    use crate::domain::attachments::timing::STALE_PLACEHOLDER;

    let backend = deps.target.storage_backend.as_str();
    let stale_after = time::Duration::try_from(STALE_PLACEHOLDER).unwrap_or(time::Duration::MAX);
    let mut restarted = false;
    loop {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-select
        let found = deps
            .repo
            .find(tenant_id, chat_id)
            .await
            .map_err(VectorStoreFailure::Internal)?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-select
        match found {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-fast-path
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-return-reuse
            Some(row) if row.vector_store_id.is_some() => return check_row(&row, backend),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-return-reuse
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-fast-path
            Some(row) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-stale
                if !restarted && is_stale(deps.clock, &row, stale_after) {
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-stale
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-reclaim
                    // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-reclaimed
                    deps.repo
                        .delete_placeholder(tenant_id, chat_id, row.id)
                        .await
                        .map_err(VectorStoreFailure::Internal)?;
                    restarted = true;
                    continue;
                    // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-reclaimed
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-reclaim
                }
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-in-progress
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-goto-loser
                // a placeholder that is not stale belongs to a winner that is still creating
                return loser_path(deps, tenant_id, chat_id).await;
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-goto-loser
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-in-progress
            }
            None => break,
        }
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-insert
    // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-placeholder
    let inserted = deps
        .repo
        .insert_placeholder(tenant_id, chat_id, backend, deps.clock.now())
        .await
        .map_err(VectorStoreFailure::Internal)?;
    // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-placeholder
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-insert
    match inserted {
        PlaceholderInsert::Inserted(row_id) => winner_path(deps, tenant_id, chat_id, row_id).await,
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-lost
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-goto-loser-race
        // `UNIQUE(tenant_id, chat_id)` was violated: the loser never calls provider create
        PlaceholderInsert::AlreadyExists => loser_path(deps, tenant_id, chat_id).await,
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-goto-loser-race
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning:p1:inst-vs-lost
    }
}

#[cfg(test)]
#[path = "vector_store_tests.rs"]
mod vector_store_tests;
