//! Leased handler of the chat cleanup queue (`outbox.chat_cleanup_queue_name`,
//! default `mini-chat.chat_cleanup`; DESIGN section 3.6 "Cleanup on Chat
//! Deletion", section 4 "Cleanup on Chat Deletion", B.9.2).
//!
//! One [`ChatCleanupPayload`] per soft-deleted chat. Every query is confined
//! to the payload's tenant and chat. Per delivery:
//!
//! | Situation | Result |
//! |-----------|--------|
//! | payload not a [`ChatCleanupPayload`] | `Reject` |
//! | chat unknown or not soft-deleted | `Reject` (`chat is not soft-deleted`) |
//! | database failure (any step) | `Retry`, no attempt recorded |
//!
//! then for every attachment of the chat with `cleanup_status = pending`
//! (deleted or not), oldest first:
//!
//! | Situation | Outcome |
//! |-----------|---------|
//! | no provider file | `done` |
//! | provider delete 2xx or `404` | secondary copy deleted best effort, `done` |
//! | provider delete failed or timed out | `cleanup_attempts + 1`, `last_cleanup_error`, `cleanup_updated_at`; `failed` at `cleanup_worker.max_attempts`; next attachment |
//!
//! and finally:
//!
//! | Situation | Result |
//! |-----------|--------|
//! | an attachment is still `pending` | `Retry` |
//! | the delivery budget cannot fit the next provider call | `Retry` |
//! | no `chat_vector_stores` row | `Ok` |
//! | placeholder row (creation in flight) | row removed (the creator deletes its store), `Ok` |
//! | store delete 2xx or `404` | row deleted, `Ok` |
//! | store delete failed or timed out | `Retry`; `Reject` on the delivery that reaches `cleanup_worker.max_attempts` (all deliveries count), row kept |
//!
//! `{prefix}_cleanup_vector_store_with_failed_attachments` is counted
//! immediately before a delete of an existing vector store is executed while
//! any attachment of the chat is `failed` (once per executed delete; never
//! without a store, for a placeholder or when the budget stops the delivery
//! first). Other counters: `{prefix}_cleanup_completed`,
//! `{prefix}_cleanup_failed`, `{prefix}_cleanup_retry` (`resource_type =
//! file | vector_store`; retry `reason = provider_error |
//! vector_store_delete_failed`) and `{prefix}_secondary_cleanup_skipped
//! {provider_kind}`.
//!
//! The secondary (Anthropic) copy of an attachment is deleted only when a
//! [`SecondaryFileStorage`] and a [`SecondaryAliasResolver`] are supplied
//! and the alias resolves at delete time from the row's stored provider
//! entry (`secondary_provider_id`; by kind only on a row without one);
//! otherwise it is skipped (logged, counted).
//!
//! **Delivery budget.** The queue lease is the outbox default (30 s, of
//! which the handler gets `duration - headroom`); a handler still running at
//! that deadline is dropped and the delivery is retried without any
//! recorded outcome. The lease covers the whole batch, so the handler
//! implements [`LeasedHandler`] and takes the cancel point of the batch
//! lease once: the provider calls of each message must end by `min(message
//! start + work budget, lease cancel point - DB_RESERVE)`, so a later
//! message of the batch never starts a call it cannot finish before the
//! drop. Every provider call (file, secondary copy, vector store) is bounded
//! by a call timeout ([`PROVIDER_CALL_TIMEOUT`], at most half of the
//! budget): an elapsed primary file delete is a failed attempt
//! (`cleanup_attempts + 1`), an elapsed store delete is a failed store
//! delete, an elapsed secondary delete is logged like a failed one. A
//! provider call is started only while one call timeout still fits before
//! that deadline; otherwise the delivery stops and returns `Retry` (an
//! attachment whose secondary copy did not fit is left `pending`, its
//! primary delete repeats as a `404` success).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter};
use tokio::time::{Instant, timeout};
use toolkit_db::outbox::{
    Batch, HandlerResult, LeaseConfig, LeasedHandler, MessageResult, OutboxMessage,
};
use toolkit_security::AccessScope;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::{
    FileStorage, SecondaryFileStorage, StorageTarget, VectorStoreRecord, VectorStores, storage_now,
};
use crate::infra::db::repo::DbRepository;
use crate::infra::db::repo::attachments::{self as attachment_repo, PendingCleanup};
use crate::infra::db::repo::vector_stores as vector_store_repo;
use crate::infra::outbox::ChatCleanupPayload;

const TARGET: &str = "mini_chat.outbox.chat_cleanup";
const FILE: &str = "file";
const VECTOR_STORE: &str = "vector_store";

/// Upper bound of one provider call (below the 30 s gateway timeout).
pub const PROVIDER_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Part of the handler budget kept free for the database writes that record
/// the outcome of the last provider call.
pub const DB_RESERVE: Duration = Duration::from_secs(2);

/// The provider-call budget of one delivery.
#[derive(Debug, Clone, Copy)]
struct Deadline {
    /// Last instant at which a provider call may still end.
    at: Instant,
    call_timeout: Duration,
}

impl Deadline {
    /// Whether one more provider call (bounded by the call timeout) fits.
    fn allows_call(self) -> bool {
        Instant::now() + self.call_timeout <= self.at
    }
}

/// How the cleanup of one attachment ended within the delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The outcome was recorded (or nothing was left to do).
    Recorded,
    /// The delivery budget cannot fit the next provider call.
    OutOfBudget,
}

/// The diagnostic of an elapsed provider call.
fn timed_out(call: &str, limit: Duration) -> String {
    format!("{call} timed out after {} ms", limit.as_millis())
}

/// Resolves the upstream alias of a secondary provider copy at delete time.
pub use crate::domain::ports::SecondaryAliasResolver;

/// Secondary copy deletion: the storage and the alias resolver.
struct Secondary {
    storage: Arc<dyn SecondaryFileStorage>,
    aliases: Arc<dyn SecondaryAliasResolver>,
}

struct CleanupMetrics {
    completed: Counter<u64>,
    failed: Counter<u64>,
    retry: Counter<u64>,
    with_failed_attachments: Counter<u64>,
    secondary_skipped: Counter<u64>,
}

impl CleanupMetrics {
    /// Counters on the global `mini-chat` meter.
    fn global(prefix: &str) -> Self {
        let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
        Self::new(&opentelemetry::global::meter_with_scope(scope), prefix)
    }

    fn new(meter: &Meter, prefix: &str) -> Self {
        Self {
            completed: meter
                .u64_counter(format!("{prefix}_cleanup_completed"))
                .with_description("Successful provider cleanup operations")
                .build(),
            failed: meter
                .u64_counter(format!("{prefix}_cleanup_failed"))
                .with_description("Provider cleanups that became terminal failures")
                .build(),
            retry: meter
                .u64_counter(format!("{prefix}_cleanup_retry"))
                .with_description("Cleanup retries delegated to the shared outbox")
                .build(),
            with_failed_attachments: meter
                .u64_counter(format!(
                    "{prefix}_cleanup_vector_store_with_failed_attachments"
                ))
                .with_description("Vector store deletions with failed attachment cleanups")
                .build(),
            secondary_skipped: meter
                .u64_counter(format!("{prefix}_secondary_cleanup_skipped"))
                .with_description("Secondary provider file deletions skipped")
                .build(),
        }
    }

    fn completed(&self, resource_type: &'static str) {
        self.completed
            .add(1, &[KeyValue::new("resource_type", resource_type)]);
    }

    fn failed(&self, resource_type: &'static str) {
        self.failed
            .add(1, &[KeyValue::new("resource_type", resource_type)]);
    }

    fn retry(&self, resource_type: &'static str, reason: &'static str) {
        self.retry.add(
            1,
            &[
                KeyValue::new("resource_type", resource_type),
                KeyValue::new("reason", reason),
            ],
        );
    }
}

/// Attempt number (1-based) of `msg`; the outbox counts retries from 0.
fn attempt_number(msg: &OutboxMessage) -> u32 {
    u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1)
}

/// A database failure: `Retry` without recording an attempt.
fn db_retry(payload: &ChatCleanupPayload, err: &DomainError) -> MessageResult {
    warn!(
        target: TARGET,
        chat_id = %payload.chat_id,
        error = %err,
        "chat cleanup: database failure; retrying"
    );
    MessageResult::Retry
}

/// Handler of the chat cleanup queue (module docs).
pub struct ChatCleanupHandler {
    repo: Arc<DbRepository>,
    files: Arc<dyn FileStorage>,
    stores: Arc<dyn VectorStores>,
    secondary: Option<Secondary>,
    max_attempts: u32,
    /// Provider-call budget of one delivery (handler budget minus
    /// [`DB_RESERVE`]).
    work_budget: Duration,
    call_timeout: Duration,
    metrics: CleanupMetrics,
}

impl std::fmt::Debug for ChatCleanupHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatCleanupHandler")
            .field("max_attempts", &self.max_attempts)
            .field("work_budget", &self.work_budget)
            .field("call_timeout", &self.call_timeout)
            .field("secondary", &self.secondary.is_some())
            .finish_non_exhaustive()
    }
}

impl ChatCleanupHandler {
    /// Handler over the repositories, the provider Files and Vector Stores
    /// APIs and `cleanup_worker.max_attempts`; metrics use `metrics_prefix`.
    /// The delivery budget follows the queue's lease (the outbox default,
    /// see [`ChatCleanupHandler::with_lease`]).
    #[must_use]
    pub fn new(
        repo: Arc<DbRepository>,
        files: Arc<dyn FileStorage>,
        stores: Arc<dyn VectorStores>,
        max_attempts: u32,
        metrics_prefix: &str,
    ) -> Self {
        Self {
            repo,
            files,
            stores,
            secondary: None,
            max_attempts: max_attempts.max(1),
            work_budget: Duration::ZERO,
            call_timeout: Duration::ZERO,
            metrics: CleanupMetrics::global(metrics_prefix),
        }
        .with_lease(LeaseConfig::default())
    }

    /// Records the counters on `meter` instead of the global `mini-chat`
    /// meter, named with `metrics_prefix`.
    #[must_use]
    pub fn with_meter(mut self, meter: &Meter, metrics_prefix: &str) -> Self {
        self.metrics = CleanupMetrics::new(meter, metrics_prefix);
        self
    }

    /// Derives the delivery budget from the queue `lease`: `duration -
    /// headroom - DB_RESERVE` for provider calls, each bounded by
    /// [`PROVIDER_CALL_TIMEOUT`] or half of that budget, whichever is
    /// smaller.
    #[must_use]
    pub fn with_lease(self, lease: LeaseConfig) -> Self {
        let work_budget = lease
            .duration
            .saturating_sub(lease.headroom)
            .saturating_sub(DB_RESERVE);
        self.with_timing(work_budget, PROVIDER_CALL_TIMEOUT.min(work_budget / 2))
    }

    /// Sets the provider-call budget of one delivery and the timeout of one
    /// provider call.
    #[must_use]
    pub const fn with_timing(mut self, work_budget: Duration, call_timeout: Duration) -> Self {
        self.work_budget = work_budget;
        self.call_timeout = call_timeout;
        self
    }

    /// Deletes secondary (Anthropic) copies through `storage`, with the alias
    /// resolved by `aliases` at delete time.
    #[must_use]
    pub fn with_secondary(
        mut self,
        storage: Arc<dyn SecondaryFileStorage>,
        aliases: Arc<dyn SecondaryAliasResolver>,
    ) -> Self {
        self.secondary = Some(Secondary { storage, aliases });
        self
    }

    /// Processes one decoded payload at delivery `attempt` (1-based) with
    /// the handler's own budget only (outside a batch lease).
    pub async fn process(&self, payload: &ChatCleanupPayload, attempt: u32) -> MessageResult {
        self.process_within(payload, attempt, None).await
    }

    /// Processes one decoded payload at delivery `attempt` (1-based) under
    /// a batch lease whose cancel point is `lease_end` (`None` outside a
    /// lease): provider calls must end by the earlier of `now + work budget`
    /// and `lease_end - DB_RESERVE`.
    pub async fn process_within(
        &self,
        payload: &ChatCleanupPayload,
        attempt: u32,
        lease_end: Option<Instant>,
    ) -> MessageResult {
        let deadline = self.deadline(lease_end);
        let scope = AccessScope::for_tenant(payload.tenant_id);
        if let Err(result) = self.check_chat(payload, &scope).await {
            return result;
        }
        let failed_attachments = match self.clean_attachments(payload, &scope, deadline).await {
            Ok(failed) => failed,
            Err(result) => return result,
        };
        self.clean_vector_store(payload, &scope, attempt, deadline, failed_attachments)
            .await
    }

    /// Decodes `msg` and processes it under the batch lease ending at
    /// `lease_end` (see [`Self::process_within`]); a malformed payload is
    /// `Reject`.
    pub async fn handle_message(
        &self,
        msg: &OutboxMessage,
        lease_end: Option<Instant>,
    ) -> MessageResult {
        match serde_json::from_slice::<ChatCleanupPayload>(&msg.payload) {
            Ok(payload) => {
                self.process_within(&payload, attempt_number(msg), lease_end)
                    .await
            }
            Err(err) => {
                error!(
                    target: TARGET,
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %err,
                    "malformed chat cleanup payload rejected"
                );
                MessageResult::Reject(format!("malformed chat cleanup payload: {err}"))
            }
        }
    }

    /// The provider-call deadline of a delivery starting now: the handler
    /// budget, capped by the batch lease minus [`DB_RESERVE`].
    fn deadline(&self, lease_end: Option<Instant>) -> Deadline {
        let now = Instant::now();
        let own = now + self.work_budget;
        let at = lease_end.map_or(own, |end| {
            own.min(end.checked_sub(DB_RESERVE).unwrap_or(now))
        });
        Deadline {
            at,
            call_timeout: self.call_timeout,
        }
    }

    /// `Retry` when the delivery budget cannot fit the next provider call.
    fn out_of_budget(payload: &ChatCleanupPayload) -> MessageResult {
        debug!(
            target: TARGET,
            chat_id = %payload.chat_id,
            "chat cleanup delivery budget spent; retrying"
        );
        MessageResult::Retry
    }

    /// `Err` with the final result unless the chat is soft-deleted.
    async fn check_chat(
        &self,
        payload: &ChatCleanupPayload,
        scope: &AccessScope,
    ) -> Result<(), MessageResult> {
        let conn = self
            .repo
            .db()
            .conn()
            .map_err(|err| db_retry(payload, &DomainError::from(err)))?;
        match attachment_repo::chat_deleted(&conn, scope, payload.chat_id).await {
            Ok(Some(true)) => Ok(()),
            Ok(_) => {
                error!(
                    target: TARGET,
                    chat_id = %payload.chat_id,
                    "chat cleanup for a chat that is not soft-deleted rejected"
                );
                Err(MessageResult::Reject("chat is not soft-deleted".to_owned()))
            }
            Err(err) => Err(db_retry(payload, &err)),
        }
    }

    /// Cleans every pending attachment and returns the number of `failed`
    /// attachments of the chat; `Err(Retry)` on a database failure, when the
    /// delivery budget is spent or while any attachment stays `pending`.
    async fn clean_attachments(
        &self,
        payload: &ChatCleanupPayload,
        scope: &AccessScope,
        deadline: Deadline,
    ) -> Result<u64, MessageResult> {
        let pending = {
            let conn = self
                .repo
                .db()
                .conn()
                .map_err(|err| db_retry(payload, &DomainError::from(err)))?;
            attachment_repo::pending_cleanup_for_chat(&conn, scope, payload.chat_id)
                .await
                .map_err(|err| db_retry(payload, &err))?
        };
        for attachment in &pending {
            let step = self
                .clean_attachment(payload, scope, attachment, deadline)
                .await
                .map_err(|err| db_retry(payload, &err))?;
            if step == Step::OutOfBudget {
                return Err(Self::out_of_budget(payload));
            }
        }
        let conn = self
            .repo
            .db()
            .conn()
            .map_err(|err| db_retry(payload, &DomainError::from(err)))?;
        let state = attachment_repo::chat_cleanup_state(&conn, scope, payload.chat_id)
            .await
            .map_err(|err| db_retry(payload, &err))?;
        if state.pending > 0 {
            debug!(
                target: TARGET,
                chat_id = %payload.chat_id,
                pending = state.pending,
                "attachment cleanups outstanding; retrying"
            );
            return Err(MessageResult::Retry);
        }
        Ok(state.failed)
    }

    /// Deletes the provider file (and the secondary copy) of one attachment
    /// within the delivery budget and records the outcome.
    async fn clean_attachment(
        &self,
        payload: &ChatCleanupPayload,
        scope: &AccessScope,
        attachment: &PendingCleanup,
        deadline: Deadline,
    ) -> Result<Step, DomainError> {
        let ids = (payload.chat_id, attachment.id);
        let Some(file_id) = &attachment.provider_file_id else {
            self.mark_done(scope, ids).await?;
            return Ok(Step::Recorded);
        };
        if !deadline.allows_call() {
            return Ok(Step::OutOfBudget);
        }
        let target = StorageTarget {
            backend: attachment.storage_backend.clone(),
            tenant_id: payload.tenant_id,
        };
        let error = match timeout(self.call_timeout, self.files.delete_file(&target, file_id)).await
        {
            Ok(Ok(())) => {
                if self
                    .delete_secondary(payload.tenant_id, attachment, deadline)
                    .await
                    == Step::OutOfBudget
                {
                    return Ok(Step::OutOfBudget);
                }
                self.mark_done(scope, ids).await?;
                return Ok(Step::Recorded);
            }
            Ok(Err(err)) => err.to_string(),
            Err(_) => timed_out("provider file delete", self.call_timeout),
        };
        self.record_failure(scope, ids, &error).await?;
        Ok(Step::Recorded)
    }

    async fn mark_done(&self, scope: &AccessScope, ids: (Uuid, Uuid)) -> Result<(), DomainError> {
        let conn = self.repo.db().conn()?;
        let marked = attachment_repo::mark_cleanup_done(
            &conn,
            self.repo.timestamps(),
            scope,
            ids,
            storage_now(),
        )
        .await?;
        if marked {
            self.metrics.completed(FILE);
            debug!(target: TARGET, attachment_id = %ids.1, "attachment provider cleanup done");
        }
        Ok(())
    }

    async fn record_failure(
        &self,
        scope: &AccessScope,
        ids: (Uuid, Uuid),
        error: &str,
    ) -> Result<(), DomainError> {
        let ts = self.repo.timestamps();
        let max_attempts = self.max_attempts;
        let error_text = error.to_owned();
        let tx_scope = scope.clone();
        let recorded = self
            .repo
            .db()
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    attachment_repo::record_cleanup_failure_in_tx(
                        tx,
                        ts,
                        &tx_scope,
                        ids,
                        &error_text,
                        max_attempts,
                        storage_now(),
                    )
                    .await
                })
            })
            .await?;
        match recorded {
            Some(failure) if failure.terminal => {
                self.metrics.failed(FILE);
                error!(
                    target: TARGET,
                    attachment_id = %ids.1,
                    attempts = failure.attempts,
                    error = %error,
                    "attachment provider cleanup failed permanently"
                );
            }
            Some(failure) => {
                self.metrics.retry(FILE, "provider_error");
                warn!(
                    target: TARGET,
                    attachment_id = %ids.1,
                    attempts = failure.attempts,
                    error = %error,
                    "provider file delete failed; retried by the next delivery"
                );
            }
            None => {}
        }
        Ok(())
    }

    /// Best-effort delete of the secondary copy; never blocks the primary
    /// cleanup outcome. [`Step::OutOfBudget`] (nothing called) when the
    /// delivery budget cannot fit the call.
    async fn delete_secondary(
        &self,
        tenant_id: Uuid,
        attachment: &PendingCleanup,
        deadline: Deadline,
    ) -> Step {
        let Some(file_id) = &attachment.secondary_file_id else {
            return Step::Recorded;
        };
        let kind = attachment
            .secondary_provider_kind
            .clone()
            .unwrap_or_default();
        let resolved = self.secondary.as_ref().and_then(|secondary| {
            secondary
                .aliases
                .upstream_alias(
                    tenant_id,
                    &kind,
                    attachment.secondary_provider_id.as_deref(),
                )
                .map(|alias| (secondary, alias))
        });
        let Some((secondary, alias)) = resolved else {
            self.metrics
                .secondary_skipped
                .add(1, &[KeyValue::new("provider_kind", kind)]);
            warn!(
                target: TARGET,
                attachment_id = %attachment.id,
                "secondary file cleanup skipped: no secondary storage or alias"
            );
            return Step::Recorded;
        };
        if !deadline.allows_call() {
            return Step::OutOfBudget;
        }
        let deleted = timeout(
            self.call_timeout,
            secondary
                .storage
                .delete_secondary_file(tenant_id, file_id, &kind, &alias),
        )
        .await;
        let error = match deleted {
            Ok(Ok(())) => return Step::Recorded,
            Ok(Err(err)) => err.to_string(),
            Err(_) => timed_out("secondary file delete", self.call_timeout),
        };
        warn!(target: TARGET, attachment_id = %attachment.id, error = %error, "secondary file delete failed");
        Step::Recorded
    }

    /// Deletes the chat's vector store once every attachment is terminal;
    /// `failed_attachments` is the number of `failed` attachment cleanups.
    async fn clean_vector_store(
        &self,
        payload: &ChatCleanupPayload,
        scope: &AccessScope,
        attempt: u32,
        deadline: Deadline,
        failed_attachments: u64,
    ) -> MessageResult {
        let row = match self.find_store(scope, payload.chat_id).await {
            Ok(row) => row,
            Err(err) => return db_retry(payload, &err),
        };
        let Some(row) = row else {
            info!(target: TARGET, chat_id = %payload.chat_id, "chat provider cleanup done");
            return MessageResult::Ok;
        };
        let Some(vector_store_id) = &row.vector_store_id else {
            return self.remove_placeholder(payload, scope, &row).await;
        };
        if !deadline.allows_call() {
            return Self::out_of_budget(payload);
        }
        self.count_failed_attachments(payload, failed_attachments);
        let target = StorageTarget {
            backend: row.provider.clone(),
            tenant_id: payload.tenant_id,
        };
        let deleted = timeout(
            self.call_timeout,
            self.stores.delete_vector_store(&target, vector_store_id),
        )
        .await;
        match deleted {
            Ok(Ok(())) => self.remove_row(payload, scope, &row).await,
            Ok(Err(err)) => self.store_delete_failed(payload, attempt, &err),
            Err(_) => self.store_delete_failed(
                payload,
                attempt,
                &timed_out("vector store delete", self.call_timeout),
            ),
        }
    }

    /// Counts a vector store delete about to be executed while `failed`
    /// attachments of the chat are `failed` (nothing when there are none).
    fn count_failed_attachments(&self, payload: &ChatCleanupPayload, failed: u64) {
        if failed == 0 {
            return;
        }
        self.metrics.with_failed_attachments.add(1, &[]);
        warn!(
            target: TARGET,
            chat_id = %payload.chat_id,
            failed,
            "vector store delete proceeds with failed attachment cleanups"
        );
    }

    /// `Retry` after a failed store delete; `Reject` (row kept) on the
    /// delivery that reaches `max_attempts`.
    fn store_delete_failed(
        &self,
        payload: &ChatCleanupPayload,
        attempt: u32,
        err: &dyn std::fmt::Display,
    ) -> MessageResult {
        self.metrics
            .retry(VECTOR_STORE, "vector_store_delete_failed");
        if attempt >= self.max_attempts {
            self.metrics.failed(VECTOR_STORE);
            error!(
                target: TARGET,
                chat_id = %payload.chat_id,
                attempt,
                error = %err,
                "vector store delete failed on the last attempt; dead-lettering"
            );
            return MessageResult::Reject(format!(
                "vector store delete: max attempts ({}) reached",
                self.max_attempts
            ));
        }
        warn!(
            target: TARGET,
            chat_id = %payload.chat_id,
            attempt,
            error = %err,
            "vector store delete failed; retrying"
        );
        MessageResult::Retry
    }

    async fn find_store(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRecord>, DomainError> {
        let conn = self.repo.db().conn()?;
        vector_store_repo::find(&conn, scope, chat_id).await
    }

    async fn remove_row(
        &self,
        payload: &ChatCleanupPayload,
        scope: &AccessScope,
        row: &VectorStoreRecord,
    ) -> MessageResult {
        let removed = match self.repo.db().conn() {
            Ok(conn) => vector_store_repo::delete_row(&conn, scope, payload.chat_id, row.id).await,
            Err(err) => Err(DomainError::from(err)),
        };
        match removed {
            Ok(_) => {
                self.metrics.completed(VECTOR_STORE);
                info!(target: TARGET, chat_id = %payload.chat_id, "chat provider cleanup done");
                MessageResult::Ok
            }
            Err(err) => db_retry(payload, &err),
        }
    }

    /// A placeholder has no provider store yet: removing it makes the
    /// in-flight creator delete the store it creates.
    async fn remove_placeholder(
        &self,
        payload: &ChatCleanupPayload,
        scope: &AccessScope,
        row: &VectorStoreRecord,
    ) -> MessageResult {
        let removed = match self.repo.db().conn() {
            Ok(conn) => {
                vector_store_repo::delete_placeholder(&conn, scope, payload.chat_id, row.id).await
            }
            Err(err) => Err(DomainError::from(err)),
        };
        match removed {
            Ok(true) => MessageResult::Ok,
            // The creator set the id meanwhile: the next delivery deletes it.
            Ok(false) => MessageResult::Retry,
            Err(err) => db_retry(payload, &err),
        }
    }
}

/// The batch loop of the per-message leased handler, with the cancel point
/// of the batch lease passed to every message (module docs, "Delivery
/// budget"). Stops starting messages once the lease is spent or the outbox
/// is shutting down.
#[async_trait]
impl LeasedHandler for ChatCleanupHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        // The cancel point is fixed for the whole batch.
        let lease_end = Instant::now() + batch.remaining();
        while let Some(msg) = batch.next_msg() {
            match self.handle_message(msg, Some(lease_end)).await {
                MessageResult::Ok => batch.ack(),
                MessageResult::Retry => {
                    return HandlerResult::Retry {
                        reason: "chat cleanup message returned Retry".to_owned(),
                    };
                }
                MessageResult::Reject(reason) => batch.reject(reason),
            }
            if batch.should_stop() {
                break;
            }
        }
        HandlerResult::Success
    }
}

#[cfg(test)]
#[path = "chat_cleanup_handler_tests.rs"]
mod tests;
