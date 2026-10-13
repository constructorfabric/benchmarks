//! The chat cleanup handler (DESIGN "Cleanup on Chat Deletion", "Vector store cleanup ordering",
//! B.9.2): deletes the provider files of every `pending` attachment of a soft-deleted chat, then
//! the chat's vector store once no attachment cleanup is outstanding.

use std::sync::Arc;

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::cleanup::{CleanupResult, RESOURCE_FILE, system_context};
use super::model::PROVIDER_KIND_ANTHROPIC;
use super::ports::{CleanupRepository, PendingCleanup, SecondaryRoute, StorageGateway};
use crate::domain::cleanup::{CHAT_SOFT_DELETE_REASON, ChatCleanupPayload};
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::metrics::Metrics;
use crate::domain::ports::ModelPolicyGateway;
use crate::domain::summary::SYSTEM_SUBJECT_ID;

/// The `resource_type` label of vector store cleanups.
const RESOURCE_VECTOR_STORE: &str = "vector_store";
/// The `reason` label of a failed vector store delete.
const VECTOR_STORE_DELETE_FAILED: &str = "vector_store_delete_failed";

/// The chat cleanup handler.
pub struct ChatCleanup {
    repo: Arc<dyn CleanupRepository>,
    storage: Arc<dyn StorageGateway>,
    policy: Arc<dyn ModelPolicyGateway>,
    metrics: Arc<dyn Metrics>,
    max_attempts: u32,
}

impl ChatCleanup {
    /// Creates the handler; `max_attempts` is `cleanup_worker.max_attempts`.
    #[must_use]
    pub fn new(
        repo: Arc<dyn CleanupRepository>,
        storage: Arc<dyn StorageGateway>,
        policy: Arc<dyn ModelPolicyGateway>,
        metrics: Arc<dyn Metrics>,
        max_attempts: u32,
    ) -> Self {
        Self {
            repo,
            storage,
            policy,
            metrics,
            max_attempts,
        }
    }

    /// Handles one delivery. `delivery` counts the deliveries of the message including this one;
    /// an infrastructure failure (database) asks for a retry without a limit.
    pub async fn handle(&self, payload: &ChatCleanupPayload, delivery: u32) -> CleanupResult {
        if payload.reason != CHAT_SOFT_DELETE_REASON {
            return CleanupResult::Reject(format!("unknown cleanup reason `{}`", payload.reason));
        }
        match self.run(payload, delivery).await {
            Ok(result) => result,
            Err(err) => {
                error!(chat_id = %payload.chat_id, error = %err, "the chat cleanup hit an infrastructure failure");
                CleanupResult::Retry
            }
        }
    }

    async fn run(
        &self,
        payload: &ChatCleanupPayload,
        delivery: u32,
    ) -> Result<CleanupResult, DomainError> {
        let (tenant_id, chat_id) = (payload.tenant_id, payload.chat_id);
        if !self.repo.chat_is_deleted(tenant_id, chat_id).await? {
            return Ok(CleanupResult::Reject("chat is not soft-deleted".to_owned()));
        }
        if self.clean_attachments(tenant_id, chat_id).await? {
            info!(chat_id = %chat_id, "attachment cleanups are outstanding; retrying later");
            return Ok(CleanupResult::Retry);
        }
        self.clean_vector_store(tenant_id, chat_id, delivery).await
    }

    /// Deletes the provider file of every pending attachment; `true` when any attachment is still
    /// `pending` afterwards (a failed attempt below the limit). Rows of the chat that still have
    /// no cleanup owner are taken first, so none is forgotten.
    async fn clean_attachments(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        let claimed = self
            .repo
            .claim_unowned_cleanups(tenant_id, chat_id, clock::now())
            .await?;
        if claimed > 0 {
            warn!(chat_id = %chat_id, claimed, "taking attachments of a deleted chat that had no cleanup owner");
        }
        let pending = self.repo.pending_cleanups(tenant_id, chat_id).await?;
        let mut secondary_route: Option<Option<SecondaryRoute>> = None;
        let mut outstanding = false;
        for attachment in pending {
            let secondary = match &attachment.secondary_file_id {
                Some(_) => {
                    if secondary_route.is_none() {
                        secondary_route = Some(self.secondary_route(tenant_id, chat_id).await);
                    }
                    secondary_route.clone().flatten()
                }
                None => None,
            };
            if !self
                .clean_attachment(tenant_id, &attachment, secondary.as_ref())
                .await?
            {
                outstanding = true;
            }
        }
        Ok(outstanding)
    }

    /// Cleans one attachment; `false` when it stays `pending`.
    async fn clean_attachment(
        &self,
        tenant_id: Uuid,
        attachment: &PendingCleanup,
        secondary: Option<&SecondaryRoute>,
    ) -> Result<bool, DomainError> {
        let Some(file_id) = attachment.provider_file_id.as_deref() else {
            self.repo
                .mark_cleanup_done(tenant_id, attachment.id, clock::now())
                .await?;
            return Ok(true);
        };
        match self.delete_primary(tenant_id, attachment, file_id).await {
            Ok(()) => {
                if let (Some(file), Some(route)) = (&attachment.secondary_file_id, secondary) {
                    self.delete_secondary(tenant_id, &route.alias, file).await;
                } else if attachment.secondary_file_id.is_some() {
                    self.metrics
                        .secondary_cleanup_skipped(PROVIDER_KIND_ANTHROPIC);
                }
                self.repo
                    .mark_cleanup_done(tenant_id, attachment.id, clock::now())
                    .await?;
                self.metrics.cleanup_completed(RESOURCE_FILE);
                Ok(true)
            }
            Err(cause) => self.record_failure(tenant_id, attachment.id, &cause).await,
        }
    }

    /// Records a failed attempt; `true` when the attachment became terminally `failed`.
    async fn record_failure(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
    ) -> Result<bool, DomainError> {
        let failure = self
            .repo
            .record_cleanup_failure(
                tenant_id,
                attachment_id,
                cause,
                self.max_attempts,
                clock::now(),
            )
            .await?;
        if failure.terminal {
            error!(attachment_id = %attachment_id, attempts = failure.attempts, "the provider file could not be deleted; giving up on it");
            self.metrics.cleanup_failed(RESOURCE_FILE);
        } else {
            self.metrics.cleanup_retry(RESOURCE_FILE, "provider_error");
        }
        Ok(failure.terminal)
    }

    async fn delete_primary(
        &self,
        tenant_id: Uuid,
        attachment: &PendingCleanup,
        file_id: &str,
    ) -> Result<(), String> {
        let ctx = system_context(tenant_id).map_err(|err| err.to_string())?;
        let route = self
            .storage
            .route_for_backend(tenant_id, &attachment.storage_backend)
            .map_err(|err| err.to_string())?;
        self.storage
            .delete_file(&ctx, &route, file_id)
            .await
            .map_err(|err| err.to_string())
    }

    async fn delete_secondary(&self, tenant_id: Uuid, alias: &str, file_id: &str) {
        let outcome = match system_context(tenant_id) {
            Ok(ctx) => self
                .storage
                .delete_secondary(&ctx, alias, file_id)
                .await
                .map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        if let Err(cause) = outcome {
            warn!(error = %cause, "the secondary file could not be deleted");
            self.metrics
                .secondary_cleanup_skipped(PROVIDER_KIND_ANTHROPIC);
        }
    }

    /// The secondary copy route of the chat, through the provider of its model.
    async fn secondary_route(&self, tenant_id: Uuid, chat_id: Uuid) -> Option<SecondaryRoute> {
        let model = match self.repo.chat_model(tenant_id, chat_id).await {
            Ok(model) => model?,
            Err(err) => {
                warn!(error = %err, "the model of the chat could not be read");
                return None;
            }
        };
        let snapshot = match self.policy.current_snapshot(SYSTEM_SUBJECT_ID).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                warn!(error = %err, "the policy could not be read to locate the secondary copies");
                return None;
            }
        };
        let entry: &ModelCatalogEntry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == model)?;
        self.storage
            .chat_storage(tenant_id, &entry.provider_id)
            .ok()?
            .secondary
    }

    /// Deletes the vector store when it is delete-eligible (nothing is `pending`), then its row.
    async fn clean_vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        delivery: u32,
    ) -> Result<CleanupResult, DomainError> {
        let Some(row) = self.repo.chat_store(tenant_id, chat_id).await? else {
            return Ok(CleanupResult::Done);
        };
        let Some(vector_store) = row.vector_store_id.as_deref() else {
            // A placeholder whose creator never published a store: nothing exists at the provider.
            debug!(chat_id = %chat_id, "removing a vector store placeholder");
            self.repo.delete_store_row(tenant_id, row.id).await?;
            return Ok(CleanupResult::Done);
        };
        if self.repo.failed_cleanups(tenant_id, chat_id).await? > 0 {
            warn!(chat_id = %chat_id, "deleting the vector store although attachment cleanups failed");
            self.metrics.cleanup_vector_store_with_failed_attachments();
        }
        let deleted = self
            .delete_store(tenant_id, &row.provider, vector_store)
            .await;
        match deleted {
            Ok(()) => {
                self.repo.delete_store_row(tenant_id, row.id).await?;
                self.metrics.cleanup_completed(RESOURCE_VECTOR_STORE);
                Ok(CleanupResult::Done)
            }
            Err(cause) => Ok(self.store_delete_failed(chat_id, delivery, &cause)),
        }
    }

    /// A failed vector store delete: retried until the delivery that reaches the attempt limit,
    /// which dead-letters the message (the store row stays for a replay).
    fn store_delete_failed(&self, chat_id: Uuid, delivery: u32, cause: &str) -> CleanupResult {
        self.metrics
            .cleanup_retry(RESOURCE_VECTOR_STORE, VECTOR_STORE_DELETE_FAILED);
        if delivery >= self.max_attempts {
            error!(chat_id = %chat_id, error = %cause, "the vector store could not be deleted; dead-lettering the message");
            self.metrics.cleanup_failed(RESOURCE_VECTOR_STORE);
            CleanupResult::Reject(format!(
                "vector store delete: max attempts ({}) reached",
                self.max_attempts
            ))
        } else {
            warn!(chat_id = %chat_id, error = %cause, "the vector store delete failed; retrying later");
            CleanupResult::Retry
        }
    }

    async fn delete_store(
        &self,
        tenant_id: Uuid,
        backend: &str,
        vector_store: &str,
    ) -> Result<(), String> {
        let ctx: SecurityContext = system_context(tenant_id).map_err(|err| err.to_string())?;
        let route = self
            .storage
            .route_for_backend(tenant_id, backend)
            .map_err(|err| err.to_string())?;
        self.storage
            .delete_vector_store(&ctx, &route, vector_store)
            .await
            .map_err(|err| err.to_string())
    }
}
