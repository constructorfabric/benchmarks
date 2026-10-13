//! The chat vector store creation protocol (DESIGN table `chat_vector_stores`, "Creation
//! protocol"): one provider store per chat, created lazily by the first document upload.
//!
//! No transaction or connection is held across a provider call. The unique `(tenant_id, chat_id)`
//! row is the only arbiter: the request that inserts the placeholder creates the store, the others
//! poll for it.

use std::time::Duration;

use tracing::{debug, warn};
use uuid::Uuid;

use super::model::VectorStoreRow;
use super::service::{AttachmentService, UploadSession};
use crate::domain::clock;
use crate::domain::error::DomainError;

/// A placeholder still without a store this long after its creation was abandoned by its creator.
const STALE_PLACEHOLDER: Duration = Duration::from_secs(120);
/// Polls of a request that lost the race for the placeholder.
const LOSER_POLLS: u32 = 5;
/// First wait of the loser polls; it doubles.
const LOSER_FIRST_WAIT: Duration = Duration::from_millis(100);
/// Restarts of the protocol after a stale placeholder was reclaimed.
const MAX_RECLAIMS: u32 = 3;

impl AttachmentService {
    /// The provider vector store of the chat, created when the chat has none.
    ///
    /// # Errors
    /// `StorageUnavailable` when the provider fails or the winner does not publish its store in
    /// time, `ProviderMismatch` when the store belongs to another backend, or an internal error.
    pub(super) async fn resolve_vector_store(
        &self,
        session: &UploadSession,
    ) -> Result<String, DomainError> {
        for _ in 0..MAX_RECLAIMS {
            let existing = self
                .repo
                .find_store(&session.scope, session.chat_id)
                .await?;
            let Some(row) = existing else {
                return self.create_or_follow(session).await;
            };
            match row.vector_store_id.clone() {
                Some(vector_store) => return checked(session, &row, vector_store),
                None if is_stale(&row) => {
                    debug!(chat_id = %session.chat_id, "reclaiming a stale vector store placeholder");
                    self.repo.delete_placeholder(&session.scope, row.id).await?;
                }
                None => return self.await_store(session).await,
            }
        }
        Err(DomainError::StorageUnavailable)
    }

    /// Inserts the placeholder: the winner creates the store, the loser waits for it.
    async fn create_or_follow(&self, session: &UploadSession) -> Result<String, DomainError> {
        let inserted = self
            .repo
            .insert_store_placeholder(
                &session.scope,
                session.ctx.subject_tenant_id(),
                session.chat_id,
                &session.storage.primary.backend,
                clock::now(),
            )
            .await?;
        match inserted {
            Some(row_id) => self.create_store(session, row_id).await,
            None => self.await_store(session).await,
        }
    }

    /// The winner path: create the provider store and publish its id with a compare-and-set.
    async fn create_store(
        &self,
        session: &UploadSession,
        row_id: Uuid,
    ) -> Result<String, DomainError> {
        let created = self
            .storage
            .create_vector_store(&session.ctx, &session.storage.primary)
            .await;
        let vector_store = match created {
            Ok(vector_store) => vector_store,
            Err(err) => {
                warn!(chat_id = %session.chat_id, error = %err, "the vector store could not be created");
                self.drop_placeholder(session, row_id).await;
                return Err(DomainError::StorageUnavailable);
            }
        };
        let published = self
            .repo
            .set_store_id(&session.scope, row_id, &vector_store)
            .await;
        match published {
            Ok(true) => Ok(vector_store),
            Ok(false) => {
                // The placeholder was reclaimed as stale while the store was being created.
                self.discard_store(session, &vector_store).await;
                self.await_store(session).await
            }
            Err(err) => {
                self.discard_store(session, &vector_store).await;
                self.drop_placeholder(session, row_id).await;
                Err(err)
            }
        }
    }

    /// Deletes a provider store nobody uses, best effort.
    async fn discard_store(&self, session: &UploadSession, vector_store: &str) {
        let deleted = self
            .storage
            .delete_vector_store(&session.ctx, &session.storage.primary, vector_store)
            .await;
        if let Err(err) = deleted {
            warn!(error = %err, "an unused vector store could not be deleted");
        }
    }

    async fn drop_placeholder(&self, session: &UploadSession, row_id: Uuid) {
        if let Err(err) = self.repo.delete_placeholder(&session.scope, row_id).await {
            warn!(error = %err, "the vector store placeholder could not be deleted");
        }
    }

    /// The loser path: polls the row with exponential backoff until its store id is published.
    async fn await_store(&self, session: &UploadSession) -> Result<String, DomainError> {
        let mut wait = LOSER_FIRST_WAIT;
        for _ in 0..LOSER_POLLS {
            tokio::time::sleep(wait).await;
            wait = wait.saturating_mul(2);
            let Some(row) = self
                .repo
                .find_store(&session.scope, session.chat_id)
                .await?
            else {
                // The winner failed and removed its placeholder.
                return Err(DomainError::StorageUnavailable);
            };
            if let Some(vector_store) = row.vector_store_id.clone() {
                return checked(session, &row, vector_store);
            }
        }
        Err(DomainError::StorageUnavailable)
    }
}

/// The store of a row after the provider check: a store of another backend is a conflict.
fn checked(
    session: &UploadSession,
    row: &VectorStoreRow,
    vector_store: String,
) -> Result<String, DomainError> {
    if row.provider == session.storage.primary.backend {
        Ok(vector_store)
    } else {
        Err(DomainError::ProviderMismatch)
    }
}

/// Whether the placeholder outlived its creator.
fn is_stale(row: &VectorStoreRow) -> bool {
    clock::now() - row.created_at > STALE_PLACEHOLDER
}
