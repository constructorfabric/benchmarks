//! The lazy, concurrency-safe creation of the per-chat vector store (DESIGN 3.7
//! `chat_vector_stores`, "Creation protocol"). No transaction or connection is
//! held across a provider call.
//!
//! 1. Fast path: the chat's row has a store id of the same backend.
//! 2. Otherwise the caller inserts a placeholder row (`vector_store_id NULL`);
//!    the unique `(tenant, chat)` index makes exactly one caller the creator.
//! 3. A loser polls the row with exponential backoff (five reads) and never
//!    calls the provider.
//! 4. The creator creates the provider store, then sets the id with a
//!    compare-and-set; a failure deletes the placeholder.
//! 5. A placeholder older than 120 s belongs to a dead creator: it is deleted
//!    and the protocol restarts.

use std::time::Duration;

use time::OffsetDateTime;
use uuid::Uuid;

use super::upload::{Job, unavailable};
use super::{AttachmentService, VectorStoreRow};
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::ports::VectorStoreRepository;

/// A placeholder this much older than its creation is abandoned.
const STALE_PLACEHOLDER: time::Duration = time::Duration::seconds(120);

/// First wait of a loser; it doubles per poll.
const FIRST_POLL: Duration = Duration::from_millis(100);

/// Reads a loser makes before it gives up.
const POLLS: u32 = 5;

/// What one pass of the protocol decided.
enum Step {
    /// The chat's store.
    Store(String),
    /// The caller is not the creator: wait for the store.
    Wait,
    /// The row was abandoned and deleted: run the protocol again.
    Restart,
}

impl AttachmentService {
    /// The provider vector store of the chat, created on first use.
    ///
    /// # Errors
    /// `ProviderMismatch` when the chat's store belongs to another backend,
    /// `ServiceUnavailable` when the provider fails or the creator does not
    /// finish in time, `Internal` for a database failure.
    pub(super) async fn ensure_vector_store(&self, job: &Job<'_>) -> Result<String, DomainError> {
        for _ in 0..2 {
            match self.store_step(job).await? {
                Step::Store(id) => return Ok(id),
                Step::Wait => return self.wait_for_store(job).await,
                Step::Restart => {}
            }
        }
        Err(unavailable())
    }

    async fn store_step(&self, job: &Job<'_>) -> Result<Step, DomainError> {
        let conn = self.shared.db.conn()?;
        let backend = &job.target.backend;
        match conn.find_vector_store(&job.scope, job.chat.id).await? {
            Some(row) if row.provider != *backend => Err(DomainError::ProviderMismatch),
            Some(VectorStoreRow {
                vector_store_id: Some(id),
                ..
            }) => Ok(Step::Store(id)),
            Some(row) => self.placeholder_step(job, &row).await,
            None => self.create_store(job).await,
        }
    }

    /// A placeholder exists: it is somebody else's work, unless it is stale.
    async fn placeholder_step(
        &self,
        job: &Job<'_>,
        row: &VectorStoreRow,
    ) -> Result<Step, DomainError> {
        if !is_stale(row.created_at, now_utc()) {
            return Ok(Step::Wait);
        }
        tracing::warn!(chat_id = %job.chat.id, "reclaiming an abandoned vector store placeholder");
        self.shared
            .db
            .conn()?
            .delete_vector_store_placeholder(&job.scope, job.chat.id, row.id)
            .await?;
        Ok(Step::Restart)
    }

    /// The creator path: placeholder, provider store, compare-and-set.
    async fn create_store(&self, job: &Job<'_>) -> Result<Step, DomainError> {
        let conn = self.shared.db.conn()?;
        let placeholder = VectorStoreRow {
            id: Uuid::new_v4(),
            tenant_id: job.chat.tenant_id,
            chat_id: job.chat.id,
            vector_store_id: None,
            provider: job.target.backend.clone(),
            created_at: now_utc(),
        };
        match conn
            .insert_vector_store_placeholder(&job.scope, &placeholder)
            .await
        {
            Ok(()) => {}
            // Lost the race: somebody else creates the store.
            Err(DomainError::Conflict { .. }) => return Ok(Step::Wait),
            Err(error) => return Err(error),
        }
        let created = match self
            .shared
            .storage
            .create_vector_store(job.ctx, &job.target)
            .await
        {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(%error, chat_id = %job.chat.id, "vector store creation failed");
                if let Err(cause) = conn
                    .delete_vector_store_placeholder(&job.scope, job.chat.id, placeholder.id)
                    .await
                {
                    tracing::warn!(%cause, "the vector store placeholder was not removed");
                }
                return Err(unavailable());
            }
        };
        if conn
            .set_vector_store_id(&job.scope, job.chat.id, placeholder.id, &created)
            .await?
        {
            return Ok(Step::Store(created));
        }
        // The placeholder was reclaimed as stale while the store was created:
        // the store we made is an orphan.
        self.delete_orphan_store(job, &created).await;
        Ok(Step::Wait)
    }

    async fn delete_orphan_store(&self, job: &Job<'_>, store: &str) {
        if let Err(error) = self
            .shared
            .storage
            .delete_vector_store(job.ctx, &job.target, store)
            .await
        {
            tracing::warn!(%error, "the orphan vector store was not deleted");
        }
    }

    /// A loser (or a creator that lost the compare-and-set): five reads with
    /// exponential backoff, then 503. The provider is never called.
    async fn wait_for_store(&self, job: &Job<'_>) -> Result<String, DomainError> {
        let mut delay = FIRST_POLL;
        for _ in 0..POLLS {
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
            let row = self
                .shared
                .db
                .conn()?
                .find_vector_store(&job.scope, job.chat.id)
                .await?;
            match row {
                Some(row) if row.provider != job.target.backend => {
                    return Err(DomainError::ProviderMismatch);
                }
                Some(VectorStoreRow {
                    vector_store_id: Some(id),
                    ..
                }) => return Ok(id),
                _ => {}
            }
        }
        Err(unavailable())
    }
}

/// A placeholder this much older than its creation is abandoned.
#[must_use]
fn is_stale(created_at: OffsetDateTime, now: OffsetDateTime) -> bool {
    now - created_at > STALE_PLACEHOLDER
}
