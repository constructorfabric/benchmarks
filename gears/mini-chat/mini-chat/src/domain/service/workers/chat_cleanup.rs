//! The chat cleanup handler logic
//! (`cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`,
//! `-clean-chat-attachment`, `-clean-chat-vector-store`): deletes the provider
//! files, the Anthropic secondary copies and the vector store of a soft-deleted
//! chat, with per-attachment attempt counting, a delivery budget and the
//! vector-store ordering rule.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::clock::{WorkerClock, with_timeout};
use super::metrics::{CleanupResource, WorkerMetrics};
use super::ports::{ChatCleanupStore, PendingAttachment};
use super::{CHAT_CLEANUP_CONCURRENCY, HandlerOutcome, POLICY_CALL_BOUND};
use crate::config::ProviderKind;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::attachments::model::{CleanupState, StorageTarget};
use crate::domain::service::attachments::ports::{
    BackgroundRuntime, StorageClient, StorageDirectory,
};
use crate::domain::service::quota::service::Clock;
use crate::domain::service::stream::ports::ProviderDirectory;

/// What the handler is built from.
pub struct ChatCleanupDeps {
    /// The store of the handler.
    pub store: Arc<dyn ChatCleanupStore>,
    /// Provider storage client (file, vector store and secondary deletes).
    pub storage: Arc<dyn StorageClient>,
    /// Provider entries by storage backend label.
    pub directory: Arc<dyn StorageDirectory>,
    /// Model-policy catalog (the secondary alias resolution).
    pub policy: Arc<dyn PolicyCatalog>,
    /// Provider entries by catalog provider id.
    pub providers: Arc<dyn ProviderDirectory>,
    /// Instruments.
    pub metrics: Arc<dyn WorkerMetrics>,
    /// The S2S context of the gear.
    pub runtime: Arc<dyn BackgroundRuntime>,
    /// Wall clock of the timestamps.
    pub clock: Clock,
    /// Monotonic clock of the deadlines.
    pub timer: Arc<dyn WorkerClock>,
    /// `cleanup_worker.max_attempts`.
    pub max_attempts: u32,
    /// `PROVIDER_DELETE_TIMEOUT`.
    pub delete_timeout: Duration,
    /// `CHAT_CLEANUP_DELIVERY_BUDGET`.
    pub delivery_budget: Duration,
}

/// The chat cleanup handler logic.
pub struct ChatCleanupService {
    deps: ChatCleanupDeps,
}

/// How one attachment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOutcome {
    /// `cleanup_status` is `done`.
    Done,
    /// `cleanup_status` is `failed` (the attempts are exhausted).
    Failed,
    /// A failed attempt below the limit; the row stays `pending`.
    StillPending,
    /// The call was cut by the delivery deadline; no attempt counted.
    Deferred,
    /// Not started: the delivery deadline had passed.
    NotReached,
    /// A database statement failed.
    DatabaseError,
}

/// How a bounded provider call ended.
enum CallEnd<T> {
    Finished(T),
    /// Cut by the delivery deadline before `PROVIDER_DELETE_TIMEOUT` elapsed.
    CutByDeadline,
    /// `PROVIDER_DELETE_TIMEOUT` elapsed.
    TimedOut,
}

impl ChatCleanupService {
    /// Creates the service.
    #[must_use]
    pub fn new(deps: ChatCleanupDeps) -> Self {
        Self { deps }
    }

    /// Runs `future` bounded by `PROVIDER_DELETE_TIMEOUT` lowered to the time
    /// left until `deadline`.
    async fn bounded<T>(
        &self,
        deadline: Duration,
        future: impl std::future::Future<Output = T> + Send,
    ) -> CallEnd<T> {
        let remaining = deadline.saturating_sub(self.deps.timer.now());
        let limit = remaining.min(self.deps.delete_timeout);
        if limit.is_zero() {
            return CallEnd::CutByDeadline;
        }
        match with_timeout(&*self.deps.timer, limit, future).await {
            Some(value) => CallEnd::Finished(value),
            None if limit < self.deps.delete_timeout => CallEnd::CutByDeadline,
            None => CallEnd::TimedOut,
        }
    }

    /// Handles one chat-cleanup message of `(tenant_id, chat_id)` delivered
    /// `attempts` times before (0 on the first delivery).
    #[allow(clippy::cognitive_complexity)]
    pub async fn handle(&self, tenant_id: Uuid, chat_id: Uuid, attempts: i16) -> HandlerOutcome {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-delivery-budget
        let delivery_number = i64::from(attempts) + 1;
        let deadline = deps.timer.now() + deps.delivery_budget;
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-delivery-budget
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-load-deleted-chat
        let chat = match deps.store.chat(tenant_id, chat_id).await {
            Ok(chat) => chat,
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the chat of a cleanup message could not be read");
                return HandlerOutcome::Retry;
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-load-deleted-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-chat-missing
        let Some(chat) = chat else {
            tracing::error!(%chat_id, %tenant_id, "a chat cleanup message names an unknown chat");
            return HandlerOutcome::Reject("chat not found".to_owned());
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-chat-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-chat-active
        if !chat.deleted {
            tracing::error!(%chat_id, %tenant_id, "a chat cleanup message names an active chat");
            return HandlerOutcome::Reject("chat is not soft-deleted".to_owned());
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-chat-active
        // The S2S context is needed only from here on, so a message that names
        // a missing or active chat is rejected even by an unbound runtime. The
        // gear binds the runtime before the outbox delivers; an unbound runtime
        // here is a plain `Retry` that is neither measured nor dead-lettered
        // (this handler dead-letters only through the vector store rule below).
        let Some(ctx) = deps.runtime.s2s_context() else {
            tracing::warn!(%chat_id, "the gear runtime is not bound yet, the cleanup is retried");
            return HandlerOutcome::Retry;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-claim-stragglers
        if let Err(error) = deps
            .store
            .claim_stragglers(tenant_id, chat_id, (deps.clock)())
            .await
        {
            tracing::warn!(%chat_id, %error, "the attachments of a deleted chat could not be claimed");
            return HandlerOutcome::Retry;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-claim-stragglers
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-select-pending-attachments
        let rows = match deps.store.pending_attachments(tenant_id, chat_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the pending attachments could not be read");
                return HandlerOutcome::Retry;
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-select-pending-attachments
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-resolve-secondary-target
        let needs_secondary = rows
            .iter()
            .any(|row| row.secondary_uploaded && row.secondary_file_id.is_some());
        let secondary_alias = if needs_secondary {
            self.resolve_secondary_alias(&ctx, tenant_id, chat_id, &chat.model, deadline)
                .await
        } else {
            None
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-resolve-secondary-target
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-for-each-pending
        let outcomes: Vec<RowOutcome> = stream::iter(rows)
            .map(|row| {
                let (ctx, alias) = (&ctx, secondary_alias.as_deref());
                async move {
                    if self.deps.timer.now() >= deadline {
                        return RowOutcome::NotReached;
                    }
                    self.clean_row(ctx, tenant_id, chat_id, &row, alias, deadline)
                        .await
                }
            })
            .buffer_unordered(CHAT_CLEANUP_CONCURRENCY)
            .collect()
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-for-each-pending
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-attachment-db-error
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-files-retry
        if outcomes.contains(&RowOutcome::DatabaseError) {
            return HandlerOutcome::Retry;
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-files-retry
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-attachment-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-recheck-terminal
        let summary = match deps.store.summary(tenant_id, chat_id).await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the cleanup states of a deleted chat could not be read");
                return HandlerOutcome::Retry;
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-recheck-terminal
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-still-pending
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-files-retry
        if summary.any_pending {
            return HandlerOutcome::Retry;
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-files-retry
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-if-still-pending
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-run-vector-store-cleanup
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-to-store-outstanding
        self.clean_vector_store(
            &ctx,
            tenant_id,
            chat_id,
            delivery_number,
            summary.any_failed,
            deadline,
        )
        .await
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-to-store-outstanding
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-run-vector-store-cleanup
    }

    /// Step 9: the alias of the Anthropic secondary target of the chat's model;
    /// `None` (with a warning) when anything fails or the kind differs.
    async fn resolve_secondary_alias(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        chat_id: Uuid,
        model: &str,
        deadline: Duration,
    ) -> Option<String> {
        let deps = &self.deps;
        let limit = deadline
            .saturating_sub(deps.timer.now())
            .min(POLICY_CALL_BOUND);
        let user = ctx.subject_id();
        let snapshot = with_timeout(&*deps.timer, limit, async {
            let version = deps.policy.current_policy_version(user).await?;
            deps.policy
                .policy_snapshot(user, version.policy_version)
                .await
        })
        .await;
        let alias = match snapshot {
            Some(Ok(snapshot)) => snapshot
                .model_catalog
                .iter()
                .find(|entry| entry.id == model)
                .and_then(|entry| deps.providers.resolve(&entry.provider_id, tenant_id).ok())
                .filter(|target| target.kind == ProviderKind::AnthropicMessages)
                .map(|target| target.alias),
            Some(Err(_)) | None => None,
        };
        if alias.is_none() {
            tracing::warn!(%chat_id, "no secondary target could be resolved for the chat");
        }
        alias
    }

    /// The cleanup of one attachment
    /// (`cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment`).
    async fn clean_row(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        chat_id: Uuid,
        row: &PendingAttachment,
        secondary_alias: Option<&str>,
        deadline: Duration,
    ) -> RowOutcome {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-no-provider-file
        let Some(file_id) = row.provider_file_id.as_deref() else {
            return self.mark_done(tenant_id, chat_id, row, false).await;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-no-provider-file
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-delete-chat-file
        let cause = match deps
            .directory
            .storage_for_backend(&row.storage_backend, tenant_id)
        {
            None => Some("storage backend not configured".to_owned()),
            Some(target) => {
                match self
                    .bounded(deadline, deps.storage.delete_file(ctx, &target, file_id))
                    .await
                {
                    CallEnd::Finished(Ok(())) => None,
                    CallEnd::Finished(Err(error)) => Some(error.cause()),
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-cut-by-deadline
                    CallEnd::CutByDeadline => return RowOutcome::Deferred,
                    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-cut-by-deadline
                    CallEnd::TimedOut => Some("timeout".to_owned()),
                }
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-delete-chat-file
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-chat-file-failed
        if let Some(cause) = cause {
            return self.record_failure(tenant_id, chat_id, row, &cause).await;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-chat-file-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-delete-chat-secondary
        if row.secondary_uploaded
            && let Some(secondary_id) = row.secondary_file_id.as_deref()
        {
            self.delete_secondary(ctx, chat_id, row, secondary_alias, secondary_id, deadline)
                .await;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-delete-chat-secondary
        self.mark_done(tenant_id, chat_id, row, true).await
    }

    /// Step 5: the best-effort delete of the secondary copy.
    async fn delete_secondary(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        row: &PendingAttachment,
        alias: Option<&str>,
        secondary_id: &str,
        deadline: Duration,
    ) {
        let deps = &self.deps;
        let failure = match alias {
            None => Some("no secondary target".to_owned()),
            Some(alias) => match self
                .bounded(
                    deadline,
                    deps.storage.delete_secondary(ctx, alias, secondary_id),
                )
                .await
            {
                CallEnd::Finished(Ok(())) => None,
                CallEnd::Finished(Err(error)) => Some(error.cause()),
                CallEnd::CutByDeadline => Some("delivery deadline".to_owned()),
                CallEnd::TimedOut => Some("timeout".to_owned()),
            },
        };
        if let Some(cause) = failure {
            tracing::warn!(%chat_id, attachment_id = %row.id, cause, "the secondary copy was not deleted");
            deps.metrics.chat_secondary_cleanup_skipped();
        }
    }

    /// Step 6 and 7: `done`, and the completed metric when a provider file was
    /// deleted.
    async fn mark_done(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row: &PendingAttachment,
        deleted: bool,
    ) -> RowOutcome {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-mark-chat-file-done
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-done
        if let Err(error) = deps
            .store
            .finish(
                tenant_id,
                chat_id,
                row.id,
                CleanupState::Done,
                (deps.clock)(),
            )
            .await
        {
            tracing::warn!(%chat_id, attachment_id = %row.id, %error, "the cleanup state could not be recorded");
            return RowOutcome::DatabaseError;
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-done
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-mark-chat-file-done
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-return-chat-file-done
        if deleted {
            deps.metrics.chat_cleanup_completed(CleanupResource::File);
        }
        RowOutcome::Done
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-return-chat-file-done
    }

    /// Step 4 of the attachment algorithm: a failed attempt.
    #[allow(clippy::cognitive_complexity)]
    async fn record_failure(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row: &PendingAttachment,
        cause: &str,
    ) -> RowOutcome {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-record-chat-file-attempt
        let attempts = match deps
            .store
            .record_attempt(tenant_id, chat_id, row.id, cause, (deps.clock)())
            .await
        {
            Ok(Some(attempts)) => attempts,
            Ok(None) => return RowOutcome::StillPending,
            Err(error) => {
                tracing::warn!(%chat_id, attachment_id = %row.id, %error, "the cleanup attempt could not be recorded");
                return RowOutcome::DatabaseError;
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-record-chat-file-attempt
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-chat-file-exhausted
        if i64::from(attempts) >= i64::from(deps.max_attempts) {
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-failed
            if let Err(error) = deps
                .store
                .finish(
                    tenant_id,
                    chat_id,
                    row.id,
                    CleanupState::Failed,
                    (deps.clock)(),
                )
                .await
            {
                tracing::warn!(%chat_id, attachment_id = %row.id, %error, "the failed cleanup could not be recorded");
                return RowOutcome::DatabaseError;
            }
            // @cpt-end:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-failed
            tracing::error!(%chat_id, attachment_id = %row.id, cause, "provider file cleanup of a deleted chat failed for good");
            deps.metrics.chat_cleanup_failed(CleanupResource::File);
            return RowOutcome::Failed;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-if-chat-file-exhausted
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-chat-file-retry
        deps.metrics.chat_cleanup_retry(CleanupResource::File);
        RowOutcome::StillPending
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment:p1:inst-chat-file-retry
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-background-workers-vector-store-cleanup:p1
    /// The vector store cleanup
    /// (`cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store`).
    #[allow(clippy::cognitive_complexity)]
    async fn clean_vector_store(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        chat_id: Uuid,
        delivery_number: i64,
        any_failed: bool,
        deadline: Duration,
    ) -> HandlerOutcome {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-select-chat-store
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-replay-dead-letter
        let row = match deps.store.vector_store(tenant_id, chat_id).await {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the vector store row could not be read");
                return HandlerOutcome::Retry;
            }
        };
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-replay-dead-letter
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-select-chat-store
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-no-store
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-files-to-released
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-released-redelivery
        let Some(row) = row else {
            return HandlerOutcome::Ok;
        };
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-released-redelivery
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-files-to-released
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-no-store
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-delete-placeholder-row
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-store-to-released
        let Some(store_id) = row.vector_store_id.as_deref() else {
            return match deps
                .store
                .delete_store_row(tenant_id, chat_id, row.id, true)
                .await
            {
                Ok(_) => HandlerOutcome::Ok,
                Err(error) => {
                    tracing::warn!(%chat_id, %error, "the placeholder vector store row could not be deleted");
                    HandlerOutcome::Retry
                }
            };
        };
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-store-to-released
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-delete-placeholder-row
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-no-time-for-store
        if deadline.saturating_sub(deps.timer.now()) < deps.delete_timeout {
            return HandlerOutcome::Retry;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-no-time-for-store
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-record-failed-debt
        if any_failed {
            deps.metrics.chat_cleanup_store_with_failed_attachments();
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-record-failed-debt
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-delete-provider-store
        let target: Option<StorageTarget> =
            deps.directory.storage_for_backend(&row.provider, tenant_id);
        let failure = match target {
            None => Some("storage backend not configured".to_owned()),
            Some(target) => match self
                .bounded(
                    deadline,
                    deps.storage.delete_vector_store(ctx, &target, store_id),
                )
                .await
            {
                CallEnd::Finished(Ok(())) => None,
                CallEnd::Finished(Err(error)) => Some(error.cause()),
                CallEnd::CutByDeadline | CallEnd::TimedOut => Some("timeout".to_owned()),
            },
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-delete-provider-store
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-store-delete-failed
        if let Some(cause) = failure {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-record-store-retry
            tracing::warn!(%chat_id, cause, "the chat vector store could not be deleted");
            deps.metrics
                .chat_cleanup_retry(CleanupResource::VectorStore);
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-record-store-retry
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-store-exhausted
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-to-dead-lettered
            if delivery_number >= i64::from(deps.max_attempts) {
                deps.metrics
                    .chat_cleanup_failed(CleanupResource::VectorStore);
                tracing::error!(%chat_id, "the chat vector store delete is dead-lettered");
                return HandlerOutcome::Reject(format!(
                    "vector store delete: max attempts ({}) reached",
                    deps.max_attempts
                ));
            }
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-to-dead-lettered
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-store-exhausted
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-return-store-retry
            return HandlerOutcome::Retry;
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-return-store-retry
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-if-store-delete-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-delete-store-row
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-bound-to-absent
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-store-to-released
        if let Err(error) = deps
            .store
            .delete_store_row(tenant_id, chat_id, row.id, false)
            .await
        {
            tracing::warn!(%chat_id, %error, "the vector store row could not be deleted");
            return HandlerOutcome::Retry;
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-chat-cleanup:p1:inst-store-to-released
        // @cpt-end:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-bound-to-absent
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-delete-store-row
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-return-store-ok
        deps.metrics
            .chat_cleanup_completed(CleanupResource::VectorStore);
        HandlerOutcome::Ok
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store:p1:inst-return-store-ok
    }
}
