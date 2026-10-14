//! The chat-cleanup handler
//! (`cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler`) with the attachment file
//! cleanup, the secondary file cleanup and the vector store cleanup. [`ChatCleanupWorker::handle`]
//! decides one leased message; `infra::chat_cleanup_handler` maps the outcome to the shared
//! outbox.
//!
//! Timing: the delivery timer starts at entry of `handle`; the timer is read immediately before
//! every provider-bound call (the policy snapshot read, the primary file delete, the secondary
//! file delete, the vector store delete) and no call starts once `DELIVERY_BUDGET` has elapsed;
//! every provider call is raced against `timer.sleep(PROVIDER_CALL_DEADLINE)` with the call
//! polled first, and a hit deadline is a failed call of that resource.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::cleanup::{secondary_target, storage_target_for_backend};
use crate::domain::attachments::provider::{ProviderFailure, StorageProvider};
use crate::domain::attachments::store::SecondaryRef;
use crate::domain::attachments::timing::Timer;
use crate::domain::maintenance::HandlerOutcome;
use crate::domain::maintenance::chat_payload::{ChatCleanupWork, parse_chat_cleanup_payload};
use crate::domain::maintenance::chat_store::{ChatCleanupStore, ChatFacts, PendingAttachment};
use crate::domain::maintenance::metrics::{
    CleanupMetrics, REASON_PROVIDER_ERROR, REASON_VECTOR_STORE_DELETE_FAILED, RESOURCE_FILE,
    RESOURCE_VECTOR_STORE,
};
use crate::domain::maintenance::secondary::resolve_secondary_alias;
use crate::domain::maintenance::settings::{
    CleanupSettings, DELIVERY_BUDGET, PROVIDER_CALL_DEADLINE,
};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::Clock;

/// The collaborators of [`ChatCleanupWorker`].
#[domain_model]
pub struct ChatCleanupDeps {
    pub store: Arc<dyn ChatCleanupStore>,
    /// The Files and Vector Stores client of the storage providers.
    pub storage: Arc<dyn StorageProvider>,
    /// The Anthropic Files client; `None` records `mini_chat_secondary_cleanup_skipped`.
    pub secondary_storage: Option<Arc<dyn StorageProvider>>,
    pub providers: Arc<ProviderRegistry>,
    /// The policy snapshot access for the secondary target (read without a user id).
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub clock: Arc<dyn Clock>,
    /// The monotonic delivery timer.
    pub timer: Arc<dyn Timer>,
    pub metrics: Arc<dyn CleanupMetrics>,
    pub settings: CleanupSettings,
}

/// The outcome of the cleanup of one attachment row.
enum FileOutcome {
    Finished,
    StillPending,
    Skipped,
    /// The delivery budget ended before the next provider-bound call.
    BudgetExhausted,
    Infrastructure,
}

/// The per-delivery memo of the secondary target: the policy snapshot is read at most once per
/// delivery.
enum SecondaryMemo {
    Unresolved,
    /// Resolved: the alias of the Anthropic Files delete, or none.
    Resolved(Option<String>),
}

/// The path that owns the provider cleanup of an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupOwner {
    /// The attachment-delete path (`cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler`).
    AttachmentDelete,
    /// This handler.
    ChatCleanup,
}

/// The owner of the provider cleanup of the attachments of a chat.
fn cleanup_owner(chat: &ChatFacts) -> CleanupOwner {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-active
    if !chat.deleted {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-attachment-path
        // the attachment of an active chat is left to the attachment-delete path; a failed
        // attachment delete of an active chat is never picked up by chat cleanup
        return CleanupOwner::AttachmentDelete;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-attachment-path
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-active
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-transfer
    // the ownership moved to chat cleanup at the commit of the chat soft-delete: from then on this
    // handler exclusively owns the provider files of every `pending` row and the chat's vector
    // store, and the attachment cleanup handler acknowledges such messages without action
    CleanupOwner::ChatCleanup
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-transfer
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-deleted
}

/// The chat-cleanup handler logic.
#[domain_model]
pub struct ChatCleanupWorker {
    deps: ChatCleanupDeps,
}

/// One delivery: the start of its timer and the memo of the secondary target.
struct Delivery {
    started: Duration,
    tenant_id: Uuid,
    chat_id: Uuid,
    model: String,
    secondary_target: SecondaryMemo,
}

impl ChatCleanupWorker {
    /// Create the worker over its collaborators.
    #[must_use]
    pub fn new(deps: ChatCleanupDeps) -> Self {
        Self { deps }
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-timer
    /// Whether the delivery time budget has elapsed; read immediately before every provider-bound
    /// call, so no call starts after 20 s and the latest call, started at 20 s with a 5 s
    /// deadline, ends by 25 s, inside the 30 s lease.
    fn budget_ended(&self, delivery: &Delivery) -> bool {
        self.deps.timer.monotonic().saturating_sub(delivery.started) >= DELIVERY_BUDGET
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-timer

    /// Race a provider-bound call against the fixed call deadline; `None` when the deadline hit.
    async fn within<T>(&self, call: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            result = call => Some(result),
            () = self.deps.timer.sleep(PROVIDER_CALL_DEADLINE) => None,
        }
    }

    /// Decide one leased message with its 0-based `attempts` (the vector store reject rule
    /// counts every delivery of the message).
    pub async fn handle(&self, payload: &[u8], attempts: i16) -> HandlerOutcome {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-timer
        // the delivery timer starts at handler entry on the injected monotonic timer
        let started = self.deps.timer.monotonic();
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-timer
        let work = match Self::parse_work(payload) {
            Ok(work) => work,
            Err(outcome) => return outcome,
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-running-turn
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-no-audit
        // a turn running at the deletion is left to finish and be billed (its own error handling
        // applies when its `file_search` hits a deleted store); the deletion and its cleanup emit
        // no audit event
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-no-audit
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-running-turn
        let mut delivery = match self.load_delivery(&work, started).await {
            Ok(delivery) => delivery,
            Err(outcome) => return outcome,
        };
        let pending = match self.load_pending(&work).await {
            Ok(pending) => pending,
            Err(outcome) => return outcome,
        };
        if let Some(outcome) = self.clean_rows(&mut delivery, &pending).await {
            return outcome;
        }
        if let Some(outcome) = self.still_pending(&delivery).await {
            return outcome;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-budget-accounting
        // a `Retry` caused by the time budget counts as one delivery in `attempts`, which the
        // store reject rule reads, because the shared outbox counts every redelivery; it is never
        // a row attempt, and it never starts the store call, so the store is deleted only when no
        // attachment row is `pending`
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-budget-accounting
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-vector-store
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-idempotent
        // every step re-reads the persisted state, skips `done` and `failed` rows and a missing
        // store row, and treats a provider 404 as success: a redelivery, a replay or a concurrent
        // delivery after a lease expiry has no duplicate effect
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-idempotent
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-target
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-return
        // best effort with the operational target of one hour under normal conditions: the shared
        // outbox retries with backoff on provider errors; this is not a guaranteed SLA
        self.clean_vector_store(&delivery, attempts).await
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-return
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-target
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-vector-store
    }

    /// Parse the payload; `Err` carries the `Reject` of a malformed message.
    fn parse_work(payload: &[u8]) -> Result<ChatCleanupWork, HandlerOutcome> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-parse
        let parsed = parse_chat_cleanup_payload(payload);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-parse
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-malformed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-malformed
        match parsed {
            Ok(work) => Ok(work),
            Err(reason) => {
                tracing::error!(%reason, "the chat cleanup message is malformed");
                Err(HandlerOutcome::Reject(format!(
                    "malformed chat cleanup payload: {reason}"
                )))
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-malformed
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-malformed
    }

    /// Load the `pending` rows; `Err` carries the `Retry` of a failed read.
    async fn load_pending(
        &self,
        work: &ChatCleanupWork,
    ) -> Result<Vec<PendingAttachment>, HandlerOutcome> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-no-terminal
        // only the `pending` rows are loaded: a `done` or `failed` row is never processed again,
        // and a `failed` row of a deleted chat is not re-attempted by any automatic path or replay
        // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-debt-terminal
        // the debt of a `failed` row stays: a later delivery or replay leaves it as it is
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-load-pending
        let loaded = self
            .deps
            .store
            .pending_attachments(work.tenant_id, work.chat_id)
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-load-pending
        // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-debt-terminal
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-no-terminal
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-pending-db
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-pending-db
        match loaded {
            Ok(rows) => Ok(rows),
            Err(error) => {
                tracing::warn!(%error, "the pending rows read failed; the message is retried");
                Err(HandlerOutcome::Retry)
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-pending-db
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-pending-db
    }

    /// Load the chat; `Err` carries the outcome of a failed read, a missing chat or an active
    /// chat.
    async fn load_delivery(
        &self,
        work: &ChatCleanupWork,
        started: Duration,
    ) -> Result<Delivery, HandlerOutcome> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-load-chat
        // the chat is read without the deleted filter, with the model chosen at creation
        let loaded = self
            .deps
            .store
            .load_chat(work.tenant_id, work.chat_id)
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-load-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-chat-db
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-chat-db
        let chat = match loaded {
            Ok(chat) => chat,
            Err(error) => {
                // an infrastructure failure: retried without a limit
                tracing::warn!(%error, "the chat read failed; the message is retried");
                return Err(HandlerOutcome::Retry);
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-chat-db
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-chat-db
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-active
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-active
        let Some(chat) = chat else {
            return Err(HandlerOutcome::Reject("chat not found".to_owned()));
        };
        // an active chat is never processed by this handler
        if cleanup_owner(&chat) != CleanupOwner::ChatCleanup {
            return Err(HandlerOutcome::Reject(
                "chat is not soft-deleted".to_owned(),
            ));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-active
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-active
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-outbox-only
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-return
        // the provider cleanup of a deleted chat runs on this outbox-driven path only: no request
        // path, upload reaper or background indexing task deletes its provider resources
        Ok(Delivery {
            started,
            tenant_id: work.tenant_id,
            chat_id: work.chat_id,
            model: chat.model,
            secondary_target: SecondaryMemo::Unresolved,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-return
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-outbox-only
    }

    /// Clean every pending row once, within the delivery time budget; `Some` carries the outcome
    /// that ends the delivery.
    async fn clean_rows(
        &self,
        delivery: &mut Delivery,
        pending: &[PendingAttachment],
    ) -> Option<HandlerOutcome> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-each
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-bounded
        // every row is attempted at most once per delivery, and the first row of a delivery is
        // always started (the timer shows no elapsed time yet), so each budget-driven `Retry`
        // makes progress and the per-row `cleanup_worker.max_attempts` budget ends the loop
        for (index, row) in pending.iter().enumerate() {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-budget-stop
            if self.budget_ended(delivery) {
                tracing::info!(
                    chat_id = %delivery.chat_id,
                    rows_left = pending.len() - index,
                    "the delivery time budget ended; the remaining rows wait for the next delivery"
                );
                break;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-budget-stop
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-file
            let outcome = self.clean_file(delivery, row).await;
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-file
            match outcome {
                FileOutcome::Finished | FileOutcome::StillPending | FileOutcome::Skipped => {}
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-file-budget
                FileOutcome::BudgetExhausted => break,
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-file-budget
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-file-db
                FileOutcome::Infrastructure => return Some(HandlerOutcome::Retry),
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-file-db
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-bounded
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-each
        None
    }

    /// The vector store is not touched while any row is `pending`; `Some` carries the `Retry`.
    async fn still_pending(&self, delivery: &Delivery) -> Option<HandlerOutcome> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-count-pending
        let counted = self
            .deps
            .store
            .count_pending(delivery.tenant_id, delivery.chat_id)
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-count-pending
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-still-pending
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-still-pending
        match counted {
            Ok(0) => None,
            Ok(_) => {
                // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-retry
                // the rows still `pending` (a failed delete below the budget, a row not attempted
                // because the time budget ended) keep the chat in the files state
                Some(HandlerOutcome::Retry)
                // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-retry
            }
            Err(error) => {
                tracing::warn!(%error, "the pending count failed; the message is retried");
                Some(HandlerOutcome::Retry)
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-return-still-pending
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler:p1:inst-cc-still-pending
    }

    async fn clean_file(&self, delivery: &mut Delivery, row: &PendingAttachment) -> FileOutcome {
        let d = &self.deps;
        let tenant_id = delivery.tenant_id;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-no-file
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-no-file
        let Some(file_id) = row.provider_file_id.as_deref() else {
            // the upload never reached the provider: nothing exists there to delete
            return self.mark_done(tenant_id, row.id).await;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-no-file
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-no-file
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-target
        let target = storage_target_for_backend(&d.providers, tenant_id, &row.storage_backend);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-target
        let Some(target) = target else {
            return self
                .attempt_failed(tenant_id, row.id, "the storage backend maps to no provider")
                .await;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-budget
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-budget
        if self.budget_ended(delivery) {
            // no provider call and no write: the row stays `pending`, no attempt is counted
            return FileOutcome::BudgetExhausted;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-budget
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-budget
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-delete
        // bounded by the fixed provider call deadline; a hit deadline is a failed call
        let deleted = self.within(d.storage.delete_file(&target, file_id)).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-success
        match deleted {
            Some(Ok(()) | Err(ProviderFailure::NotFound)) => {}
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-success
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-failed
            Some(Err(ProviderFailure::Transient)) => {
                return self
                    .attempt_failed(tenant_id, row.id, "the provider delete failed transiently")
                    .await;
            }
            Some(Err(ProviderFailure::Other)) => {
                return self
                    .attempt_failed(tenant_id, row.id, "the provider rejected the delete")
                    .await;
            }
            None => {
                return self
                    .attempt_failed(tenant_id, row.id, "the provider call exceeded its deadline")
                    .await;
            } // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-failed
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-secondary
        // the outcome of the secondary delete never blocks the next step, except that an ended
        // time budget leaves the row `pending` without any write
        if self.delete_secondary(delivery, row).await {
            return FileOutcome::BudgetExhausted;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-secondary
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return
        self.mark_done(tenant_id, row.id).await
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return
    }

    async fn mark_done(&self, tenant_id: Uuid, id: Uuid) -> FileOutcome {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-mark-done
        // the write is guarded by `cleanup_status` `pending`
        let written = d.store.mark_done(tenant_id, id, d.clock.now()).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-mark-done
        match written {
            Ok(true) => {
                d.metrics.completed(RESOURCE_FILE);
                FileOutcome::Finished
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-no-lock
            // the two paths are not locked against each other: when the attachment handler checked the
            // chat before the soft-delete, both may delete the same file; 404 counts as success on both
            // and the terminal write is guarded by `cleanup_status` `pending`, so exactly one path writes
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-guard-lost
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-skipped
            // the attachment cleanup handler finished the row first: harmless, because both
            // paths treat 404 as success and the guard lets exactly one of them write the end
            Ok(false) => FileOutcome::Skipped,
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-skipped
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-guard-lost
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries:p1:inst-cb-no-lock
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-db
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-db
            Err(error) => {
                // `cleanup_attempts` is not incremented: the next delivery repeats the
                // idempotent delete
                tracing::warn!(%error, "the done write failed; the message is retried");
                FileOutcome::Infrastructure
            } // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-db
              // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-db
        }
    }

    async fn attempt_failed(&self, tenant_id: Uuid, id: Uuid, cause: &str) -> FileOutcome {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-record-attempt
        // the cause is a fixed text without provider body or identifiers
        let recorded = d
            .store
            .record_failed_attempt(tenant_id, id, cause, d.settings.max_attempts, d.clock.now())
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-record-attempt
        match recorded {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-exhausted
            Ok(Some(record)) if record.exhausted => {
                // the row is terminal provider-file debt; the handler goes on with the others
                d.metrics.failed(RESOURCE_FILE);
                FileOutcome::Finished
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-exhausted
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-pending
            Ok(Some(_)) => {
                d.metrics.retry(RESOURCE_FILE, REASON_PROVIDER_ERROR);
                FileOutcome::StillPending
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file:p1:inst-af-return-pending
            Ok(None) => FileOutcome::Skipped,
            Err(error) => {
                tracing::warn!(%error, "the attempt write failed; the message is retried");
                FileOutcome::Infrastructure
            }
        }
    }

    /// The best-effort secondary delete; `true` when the delivery budget ended before it.
    async fn delete_secondary(&self, delivery: &mut Delivery, row: &PendingAttachment) -> bool {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-none
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-none
        let Some(file_id) = row
            .secondary_file_id
            .as_deref()
            .filter(|id| !id.is_empty() && row.secondary_status == "uploaded")
        else {
            return false;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-none
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-none
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-budget
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-budget
        if self.budget_ended(delivery) {
            // no provider call starts after the budget
            return true;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-budget
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-budget
        let kind = row
            .secondary_provider_kind
            .as_deref()
            .unwrap_or("anthropic");
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-skip
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-skip
        let Some(client) = &d.secondary_storage else {
            self.skip_secondary(delivery, row, kind);
            return false;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-skip
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-skip
        self.resolve_secondary_target(delivery).await;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-skip
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-skip
        let SecondaryMemo::Resolved(Some(alias)) = &delivery.secondary_target else {
            // the primary row is never marked `failed` because of the secondary
            self.skip_secondary(delivery, row, kind);
            return false;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-skip
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-skip
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-budget-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-budget-delete
        if self.budget_ended(delivery) {
            // the memo keeps the resolved target; the row stays `pending`, no attempt is counted
            return true;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return-budget-delete
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-budget-delete
        let target = secondary_target(&SecondaryRef {
            file_id: file_id.to_owned(),
            provider_kind: kind.to_owned(),
            upstream_alias: alias.clone(),
        });
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-delete
        // a 404 is success; any other failure, a hit deadline included, is logged without the
        // file id and not retried
        let deleted = self
            .within(client.delete_secondary_file(&target, file_id))
            .await;
        match deleted {
            Some(Ok(()) | Err(ProviderFailure::NotFound)) => {}
            Some(Err(failure)) => {
                tracing::warn!(
                    chat_id = %delivery.chat_id,
                    attachment_id = %row.id,
                    ?failure,
                    "the secondary file delete failed; it is not retried"
                );
            }
            None => {
                tracing::warn!(
                    chat_id = %delivery.chat_id,
                    attachment_id = %row.id,
                    "the secondary file delete exceeded its deadline; it is not retried"
                );
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-no-block
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return
        // the primary row, its `cleanup_attempts` and its `cleanup_status` do not change because
        // of the secondary outcome
        false
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-return
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-no-block
    }

    /// Resolve the secondary target once per delivery and keep it in the memo; the snapshot read
    /// is bounded by the 5 s deadline.
    async fn resolve_secondary_target(&self, delivery: &mut Delivery) {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-resolve
        if !matches!(delivery.secondary_target, SecondaryMemo::Unresolved) {
            return;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-snapshot
        // read without a user id: the handler runs outside a user scope
        let snapshot = self.within(d.policy.current_snapshot(Uuid::nil())).await;
        let snapshot = snapshot.and_then(Result::ok);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-snapshot
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-model
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-fallback
        // the chat's model is looked up without the enabled filter, its provider is accepted for
        // the Anthropic kind only, and a failed lookup falls back to the tenant's single
        // Anthropic provider
        let alias = resolve_secondary_alias(
            snapshot.as_ref(),
            &d.providers,
            delivery.tenant_id,
            &delivery.model,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-fallback
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-provider
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-model
        delivery.secondary_target = SecondaryMemo::Resolved(alias);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file:p1:inst-sf-resolve
    }

    fn skip_secondary(&self, delivery: &Delivery, row: &PendingAttachment, kind: &str) {
        self.deps.metrics.secondary_skipped(kind);
        tracing::warn!(
            chat_id = %delivery.chat_id,
            attachment_id = %row.id,
            "the secondary file is not deleted: no Anthropic Files target"
        );
    }

    async fn clean_vector_store(&self, delivery: &Delivery, attempts: i16) -> HandlerOutcome {
        let d = &self.deps;
        let (tenant_id, chat_id) = (delivery.tenant_id, delivery.chat_id);
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-eligible
        // runs only when no attachment row of the chat is `pending`: every row is `done` or
        // `failed` (checked by the handler before this call)
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-eligible
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-load
        let loaded = d.store.vector_store(tenant_id, chat_id).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-load
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-load-db
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-load-db
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(%error, "the vector store row read failed; the message is retried");
                return HandlerOutcome::Retry;
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-load-db
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-load-db
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-none
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-none
        let Some(row) = loaded else {
            // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-purged
            // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-debt
            // no store row: the chat never had a document or a previous delivery finished; it is
            // provider-purged, with file debt when a row is `failed`
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-purged
            return HandlerOutcome::Ok;
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-purged
            // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-debt
            // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-purged
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-none
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-none
        // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-done
        // every attachment row is `done` or `failed` and the store row still exists
        // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-files-done
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-placeholder
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-placeholder
        let Some(store_id) = row.vector_store_id.as_deref() else {
            return self.drop_placeholder(delivery, row.id).await;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-placeholder
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-placeholder
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-budget
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-budget
        if self.budget_ended(delivery) {
            // no provider call, no debt metric and no store attempt: every attachment row is
            // already terminal, so the next delivery goes straight to the store delete; this
            // delivery counts like any `Retry` but never produces a `Reject`
            return HandlerOutcome::Retry;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-budget
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-budget
        if let Some(outcome) = self.record_debt(delivery).await {
            return outcome;
        }
        if self.delete_store(delivery, &row.provider, store_id).await {
            return self.finish_store(delivery, row.id).await;
        }
        self.store_delete_failed(attempts)
    }

    /// A creation placeholder (a NULL `vector_store_id`): its row is deleted without a provider
    /// call; an upload that creates the store afterwards loses its compare-and-set and deletes
    /// the new store itself.
    async fn drop_placeholder(&self, delivery: &Delivery, row_id: Uuid) -> HandlerOutcome {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-placeholder
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-placeholder
        let removed = self
            .deps
            .store
            .delete_vector_store_row(delivery.tenant_id, delivery.chat_id, row_id, true)
            .await;
        match removed {
            Ok(_) => HandlerOutcome::Ok,
            Err(error) => {
                tracing::warn!(%error, "the placeholder delete failed; the message is retried");
                HandlerOutcome::Retry
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-placeholder
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-placeholder
    }

    /// Record the debt metric before the store delete when a row is `failed`; `Some` carries the
    /// `Retry` of a failed read.
    async fn record_debt(&self, delivery: &Delivery) -> Option<HandlerOutcome> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-failed-rows
        let failed_rows = d
            .store
            .has_failed(delivery.tenant_id, delivery.chat_id)
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-failed-rows
        match failed_rows {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-record-debt
            // recorded before the delete: the store is released although file debt remains
            Ok(true) => d.metrics.vector_store_with_failed_attachments(),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-record-debt
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, "the failed rows read failed; the message is retried");
                return Some(HandlerOutcome::Retry);
            }
        }
        None
    }

    /// Delete the provider store; whether it is gone (2xx or 404).
    async fn delete_store(&self, delivery: &Delivery, provider: &str, store_id: &str) -> bool {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-target
        let target = storage_target_for_backend(&d.providers, delivery.tenant_id, provider);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-target
        // an unresolvable label counts as a failed delete
        let Some(target) = target else {
            return false;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-delete
        // one call for the whole store, bounded by the fixed provider call deadline; a hit
        // deadline is a failed delete
        let deleted = self
            .within(d.storage.delete_vector_store(&target, store_id))
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-files-separate
        // the store delete removes only the index references: the provider files are deleted
        // by their own rows
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-files-separate
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-success
        matches!(deleted, Some(Ok(()) | Err(ProviderFailure::NotFound)))
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-success
    }

    /// The store is gone: its row is the durable completion marker.
    async fn finish_store(&self, delivery: &Delivery, row_id: Uuid) -> HandlerOutcome {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-delete-row
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-chat-cleanup
        // the transition of the chat vector store from bound to absent: the provider store was
        // deleted (or was already gone) after every attachment row became terminal
        let removed = d
            .store
            .delete_vector_store_row(delivery.tenant_id, delivery.chat_id, row_id, false)
            .await;
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store:p1:inst-vsm-chat-cleanup
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-delete-row
        match removed {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-ok
            Ok(_) => {
                // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-store-purged
                // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-store-debt
                // purged when every row is `done`, purged with file debt when a row is `failed`
                d.metrics.completed(RESOURCE_VECTOR_STORE);
                HandlerOutcome::Ok
                // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-store-debt
                // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-store-purged
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-ok
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-delete-row-db
            Err(error) => {
                // the next delivery deletes the store again and gets 404
                tracing::warn!(%error, "the vector store row delete failed; the message is retried");
                HandlerOutcome::Retry
            } // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-delete-row-db
        }
    }

    /// The store delete failed: the row is kept; `Reject` on the delivery that reaches the
    /// limit, `Retry` before.
    fn store_delete_failed(&self, attempts: i16) -> HandlerOutcome {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-store-retry
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-keep-row
        // the `chat_vector_stores` row stays as it is
        d.metrics
            .retry(RESOURCE_VECTOR_STORE, REASON_VECTOR_STORE_DELETE_FAILED);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-keep-row
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-failed
        let limit = d.settings.max_attempts;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-reject
        // every delivery of the message counts, those that returned `Retry` while rows were
        // `pending` or because the time budget ended included
        if i64::from(attempts) >= i64::from(limit) - 1 {
            d.metrics.failed(RESOURCE_VECTOR_STORE);
            // a replay of the dead letter retries the delete
            HandlerOutcome::Reject(format!(
                "vector store delete: max attempts ({limit}) reached"
            ))
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-reject
        } else {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-retry
            HandlerOutcome::Retry
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup:p1:inst-vs-return-retry
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge:p1:inst-pp-store-retry
    }
}

#[cfg(test)]
#[path = "chat_cleanup_tests.rs"]
mod chat_cleanup_tests;
