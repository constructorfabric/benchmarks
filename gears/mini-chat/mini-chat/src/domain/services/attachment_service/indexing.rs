//! Chat vector store creation (DESIGN section 3.7, `chat_vector_stores`
//! creation protocol) and document indexing: the in-request poll until 25 s
//! after the upload started, then the background task (section 3.6 "File
//! Upload", B.9.5).

use std::sync::Arc;
use std::time::Duration;

use tokio::time::{Instant, sleep, sleep_until, timeout_at};
use toolkit_db::DBProvider;
use uuid::Uuid;

use crate::domain::background::Background;
use crate::domain::enums::AttachmentKind;
use crate::domain::error::DomainError;
use crate::domain::ports::{IndexStatus, RagStorage, StorageError};
use crate::domain::time::db_now;
use crate::infra::db::entities::chat_vector_store;
use crate::infra::db::repos::{attachment_repo, vector_store_repo};
use crate::infra::llm::ResolvedStorage;
use crate::infra::outbox::MiniChatOutbox;
use crate::infra::outbox::payloads::AttachmentCleanupPayload;

/// Background round length: each round starts with the heartbeat.
const BG_ROUND_SECS: u64 = 20;
/// Minimum `upload_reaper.stale_after_secs`.
const MIN_REAPER_STALE_SECS: u64 = 60;
// The heartbeat must refresh the row well before the reaper may take it.
const _: () = assert!(BG_ROUND_SECS * 2 <= MIN_REAPER_STALE_SECS);

/// Loser path of the creation protocol: number of polls.
const STORE_POLLS: u32 = 5;
/// A placeholder older than this is reclaimed.
const STALE_PLACEHOLDER: time::Duration = time::Duration::seconds(120);

pub(super) const EVENT_INDEXING_FAILED: &str = "attachment_indexing_failed";
pub(super) const ERROR_INDEXING_FAILED: &str = "indexing_failed";

/// Waits of the indexing protocol (DESIGN section 3.6, B.9.5). The defaults
/// are normative; tests scale them down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexingTiming {
    /// The upload stops waiting this long after it started.
    pub deadline: Duration,
    /// In-request poll: first wait, cap (doubling).
    pub sync_first_wait: Duration,
    pub sync_max_wait: Duration,
    /// Background poll: round length (heartbeat), first wait, cap, total.
    pub bg_round: Duration,
    pub bg_first_wait: Duration,
    pub bg_max_wait: Duration,
    pub bg_total: Duration,
    /// Waits between the 4 attempts to set `ready` in the background task.
    pub set_ready_retry: [Duration; 3],
    /// Loser path of the vector store creation: first poll wait (doubling).
    pub store_first_wait: Duration,
}

impl Default for IndexingTiming {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(25),
            sync_first_wait: Duration::from_millis(250),
            sync_max_wait: Duration::from_secs(2),
            bg_round: Duration::from_secs(BG_ROUND_SECS),
            bg_first_wait: Duration::from_millis(250),
            bg_max_wait: Duration::from_secs(5),
            bg_total: Duration::from_secs(600),
            set_ready_retry: [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            store_first_wait: Duration::from_millis(100),
        }
    }
}

/// The document being indexed.
#[derive(Clone, Debug)]
pub(super) struct IndexTarget {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: String,
    pub vector_store_id: String,
    pub storage: ResolvedStorage,
}

/// Background poll state carried across rounds.
struct BgPoll {
    wait: Duration,
    last_transient: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Continue,
    Done,
}

/// Result of the in-request indexing wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SyncOutcome {
    Ready,
    Failed,
    /// Still `in_progress` at the deadline.
    Pending,
}

#[derive(Clone)]
pub(super) struct Indexer {
    pub db: Arc<DBProvider<DomainError>>,
    pub storage: Arc<dyn RagStorage>,
    pub outbox: Arc<MiniChatOutbox>,
    pub background: Background,
    pub timing: IndexingTiming,
}

impl Indexer {
    // ── Vector store creation protocol ───────────────────────────────────────

    /// The chat's vector store id, created on first use.
    ///
    /// # Errors
    /// `ProviderMismatch` when the store belongs to another backend,
    /// `StorageUnavailable` when no store can be obtained, database failure.
    pub async fn chat_vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        st: &ResolvedStorage,
    ) -> Result<String, DomainError> {
        let conn = self.db.conn()?;
        // Two passes: the second runs after a stale placeholder was reclaimed.
        for _ in 0..2 {
            if let Some(row) = vector_store_repo::find(&conn, tenant_id, chat_id).await? {
                check_provider(&row, st)?;
                if let Some(vs) = row.vector_store_id {
                    return Ok(vs);
                }
                if row.created_at < db_now() - STALE_PLACEHOLDER {
                    tracing::warn!(%chat_id, "reclaiming stale vector store placeholder");
                    vector_store_repo::delete_stale_placeholder(
                        &conn,
                        tenant_id,
                        chat_id,
                        db_now() - STALE_PLACEHOLDER,
                    )
                    .await?;
                    continue;
                }
                return self.await_store(tenant_id, chat_id, st).await;
            }
            match vector_store_repo::insert_placeholder(
                &conn,
                tenant_id,
                chat_id,
                &st.backend_label,
                db_now(),
            )
            .await
            {
                Ok(row_id) => return self.create_store(tenant_id, chat_id, row_id, st).await,
                Err(DomainError::UniqueViolation) => {
                    return self.await_store(tenant_id, chat_id, st).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(DomainError::StorageUnavailable)
    }

    /// Winner path: create the provider store and CAS its id into the row.
    async fn create_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
        st: &ResolvedStorage,
    ) -> Result<String, DomainError> {
        let created = self
            .storage
            .create_vector_store(st, &format!("chat-{chat_id}"))
            .await;
        let conn = self.db.conn()?;
        let vs = match created {
            Ok(vs) => vs,
            Err(e) => {
                tracing::warn!(%chat_id, error = %e, "vector store creation failed");
                drop_placeholder(&conn, tenant_id, chat_id, row_id).await;
                return Err(DomainError::StorageUnavailable);
            }
        };
        if vector_store_repo::set_store_id(&conn, tenant_id, chat_id, row_id, &vs).await? == 1 {
            return Ok(vs);
        }
        // The placeholder was reclaimed meanwhile: drop our store, use theirs.
        tracing::warn!(%chat_id, "vector store placeholder lost; deleting the new store");
        self.delete_store_best_effort(st, vs);
        self.await_store(tenant_id, chat_id, st).await
    }

    fn delete_store_best_effort(&self, st: &ResolvedStorage, vs: String) {
        let storage = Arc::clone(&self.storage);
        let st = st.clone();
        self.background.tracker.spawn(async move {
            if let Err(e) = storage.delete_vector_store(&st, &vs).await {
                tracing::warn!(error = %e, "orphan vector store delete failed");
            }
        });
    }

    /// Loser path: poll until the winner sets the store id (5 polls).
    async fn await_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        st: &ResolvedStorage,
    ) -> Result<String, DomainError> {
        let conn = self.db.conn()?;
        let mut wait = self.timing.store_first_wait;
        for _ in 0..STORE_POLLS {
            sleep(wait).await;
            wait *= 2;
            if let Some(row) = vector_store_repo::find(&conn, tenant_id, chat_id).await? {
                check_provider(&row, st)?;
                if let Some(vs) = row.vector_store_id {
                    return Ok(vs);
                }
            }
        }
        tracing::warn!(%chat_id, "vector store not available after polling");
        Err(DomainError::StorageUnavailable)
    }

    // ── In-request indexing ──────────────────────────────────────────────────

    /// Adds the file to the store and polls its status until the deadline
    /// (`timing.deadline` after `started`).
    pub async fn index(&self, t: &IndexTarget, started: Instant) -> SyncOutcome {
        let deadline = started + self.timing.deadline;
        let added = timeout_at(
            deadline,
            self.storage.add_file_to_vector_store(
                &t.storage,
                &t.vector_store_id,
                &t.provider_file_id,
                t.attachment_id,
            ),
        )
        .await;
        match added {
            Err(_) => return SyncOutcome::Pending,
            Ok(Err(e)) => {
                tracing::warn!(attachment_id = %t.attachment_id, error = %e, "add to vector store failed");
                return SyncOutcome::Failed;
            }
            Ok(Ok(IndexStatus::Completed)) => return SyncOutcome::Ready,
            Ok(Ok(IndexStatus::Failed)) => return SyncOutcome::Failed,
            Ok(Ok(IndexStatus::InProgress)) => {}
        }
        let mut wait = self.timing.sync_first_wait;
        let mut warned = false;
        loop {
            let next = Instant::now() + wait;
            if next >= deadline {
                sleep_until(deadline).await;
                return SyncOutcome::Pending;
            }
            sleep_until(next).await;
            wait = (wait * 2).min(self.timing.sync_max_wait);
            let read = timeout_at(deadline, self.read_status(t)).await;
            match read {
                Err(_) => return SyncOutcome::Pending,
                Ok(Ok(IndexStatus::Completed)) => return SyncOutcome::Ready,
                Ok(Ok(IndexStatus::InProgress)) => {}
                Ok(Err(StorageError::Transient(e) | StorageError::Unavailable(e))) => {
                    if !warned {
                        warned = true;
                        tracing::warn!(attachment_id = %t.attachment_id, error = %e, "transient indexing status error");
                    }
                }
                Ok(Ok(IndexStatus::Failed) | Err(_)) => return SyncOutcome::Failed,
            }
        }
    }

    async fn read_status(&self, t: &IndexTarget) -> Result<IndexStatus, StorageError> {
        self.storage
            .vector_store_file_status(&t.storage, &t.vector_store_id, &t.provider_file_id)
            .await
    }

    /// Fire-and-forget provider file delete (not retried).
    pub fn delete_file_best_effort(&self, st: &ResolvedStorage, file_id: String) {
        let storage = Arc::clone(&self.storage);
        let st = st.clone();
        self.background.tracker.spawn(async move {
            if let Err(e) = storage.delete_file(&st, &file_id).await {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    // ── Background indexing ──────────────────────────────────────────────────

    /// Keeps polling in the background; stops on gear shutdown.
    pub fn spawn_background(&self, t: IndexTarget) {
        let this = self.clone();
        let cancel = self.background.cancel.clone();
        self.background.tracker.spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!(attachment_id = %t.attachment_id, "background indexing cancelled");
                }
                () = this.run_background(&t) => {}
            }
        });
    }

    async fn run_background(&self, t: &IndexTarget) {
        let end = Instant::now() + self.timing.bg_total;
        let mut poll = BgPoll {
            wait: self.timing.bg_first_wait,
            last_transient: None,
        };
        loop {
            if !self.heartbeat(t).await {
                return;
            }
            let round_end = (Instant::now() + self.timing.bg_round).min(end);
            if self.poll_round(t, round_end, end, &mut poll).await == Step::Done {
                return;
            }
            if Instant::now() >= end {
                tracing::warn!(
                    attachment_id = %t.attachment_id,
                    last_error = poll.last_transient.as_deref().unwrap_or("none"),
                    "background indexing timed out"
                );
                self.fail_for_cleanup(t).await;
                return;
            }
        }
    }

    /// One heartbeat round of status reads.
    async fn poll_round(
        &self,
        t: &IndexTarget,
        round_end: Instant,
        end: Instant,
        poll: &mut BgPoll,
    ) -> Step {
        loop {
            // A read cut by the overall deadline counts as still in progress.
            let read = timeout_at(end, self.read_status(t))
                .await
                .unwrap_or(Ok(IndexStatus::InProgress));
            if self.on_background_read(t, read, poll).await == Step::Done {
                return Step::Done;
            }
            let now = Instant::now();
            if now >= round_end {
                return Step::Continue;
            }
            sleep_until((now + poll.wait).min(round_end)).await;
            poll.wait = (poll.wait * 2).min(self.timing.bg_max_wait);
            if Instant::now() >= round_end {
                return Step::Continue;
            }
        }
    }

    async fn on_background_read(
        &self,
        t: &IndexTarget,
        read: Result<IndexStatus, StorageError>,
        poll: &mut BgPoll,
    ) -> Step {
        match read {
            Ok(IndexStatus::Completed) => {
                self.set_ready_with_retry(t).await;
                Step::Done
            }
            Ok(IndexStatus::InProgress) => Step::Continue,
            Err(StorageError::Transient(e) | StorageError::Unavailable(e)) => {
                if poll.last_transient.is_none() {
                    tracing::warn!(attachment_id = %t.attachment_id, error = %e, "transient indexing status error");
                }
                poll.last_transient = Some(e);
                Step::Continue
            }
            Ok(IndexStatus::Failed) | Err(_) => {
                tracing::warn!(attachment_id = %t.attachment_id, "background indexing failed");
                self.fail_for_cleanup(t).await;
                Step::Done
            }
        }
    }

    /// Refreshes `updated_at`; `false` when the row is no longer indexable
    /// (deleted, not `uploaded`, claimed by cleanup). A database failure keeps
    /// the task going.
    async fn heartbeat(&self, t: &IndexTarget) -> bool {
        let res = match self.db.conn() {
            Ok(conn) => {
                attachment_repo::heartbeat(&conn, t.tenant_id, t.attachment_id, db_now()).await
            }
            Err(e) => Err(e),
        };
        match res {
            Ok(n) if n > 0 => true,
            Ok(_) => {
                tracing::info!(attachment_id = %t.attachment_id, "attachment no longer indexable; background indexing stops");
                false
            }
            Err(e) => {
                tracing::warn!(attachment_id = %t.attachment_id, error = %e, "indexing heartbeat failed");
                true
            }
        }
    }

    async fn try_set_ready(&self, t: &IndexTarget) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        attachment_repo::set_ready(&conn, t.tenant_id, t.attachment_id, None, db_now()).await
    }

    /// 4 attempts, 1 / 2 / 4 s apart; a row that is no longer `uploaded` is
    /// left unchanged.
    async fn set_ready_with_retry(&self, t: &IndexTarget) {
        let id = t.attachment_id;
        let mut delays = self.timing.set_ready_retry.iter();
        loop {
            let err = match self.try_set_ready(t).await {
                Ok(n) => {
                    if n == 0 {
                        tracing::info!(attachment_id = %id, "indexed attachment no longer uploaded");
                    }
                    return;
                }
                Err(e) => e,
            };
            let Some(delay) = delays.next() else {
                log_set_ready_failed(id, &err);
                return;
            };
            tracing::warn!(attachment_id = %id, error = %err, "set ready failed; retrying");
            sleep(*delay).await;
        }
    }

    /// One transaction: `failed` / `indexing_failed` / cleanup `pending` and
    /// the `attachment_indexing_failed` cleanup event.
    async fn fail_for_cleanup(&self, t: &IndexTarget) {
        let outbox = Arc::clone(&self.outbox);
        let t_owned = t.clone();
        let res = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let now = db_now();
                    let t = t_owned;
                    if attachment_repo::fail_indexing_for_cleanup(
                        tx,
                        t.tenant_id,
                        t.attachment_id,
                        ERROR_INDEXING_FAILED,
                        now,
                    )
                    .await?
                        == 0
                    {
                        return Ok(None);
                    }
                    let payload = AttachmentCleanupPayload {
                        event_type: EVENT_INDEXING_FAILED.to_owned(),
                        tenant_id: t.tenant_id,
                        chat_id: t.chat_id,
                        attachment_id: t.attachment_id,
                        provider_file_id: Some(t.provider_file_id.clone()),
                        vector_store_id: None,
                        storage_backend: t.storage.backend_label.clone(),
                        attachment_kind: AttachmentKind::Document.as_str().to_owned(),
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    Ok(Some(outbox.enqueue_attachment_cleanup(tx, &payload).await?))
                })
            })
            .await;
        match res {
            Ok(Some(wake)) => wake.fire(),
            Ok(None) => {
                tracing::info!(attachment_id = %t.attachment_id, "attachment no longer uploaded; failure not recorded");
            }
            Err(e) => {
                tracing::error!(attachment_id = %t.attachment_id, error = %e, "could not record the background indexing failure");
            }
        }
    }
}

/// Final outcome of the ready retries: the row stays `uploaded` and the upload
/// reaper takes it (`mini_chat_attachment_background_indexing{result}`).
fn log_set_ready_failed(attachment_id: Uuid, err: &DomainError) {
    tracing::error!(
        %attachment_id,
        error = %err,
        result = "set_ready_failed",
        "could not mark the indexed attachment ready"
    );
}

/// Best-effort delete of a placeholder whose store could not be created.
async fn drop_placeholder(
    conn: &impl toolkit_db::secure::DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    row_id: Uuid,
) {
    if let Err(e) = vector_store_repo::delete_placeholder(conn, tenant_id, chat_id, row_id).await {
        tracing::warn!(%chat_id, error = %e, "placeholder delete failed");
    }
}

fn check_provider(row: &chat_vector_store::Model, st: &ResolvedStorage) -> Result<(), DomainError> {
    if row.provider == st.backend_label {
        Ok(())
    } else {
        Err(DomainError::ProviderMismatch)
    }
}
