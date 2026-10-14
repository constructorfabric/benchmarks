//! The background indexing task
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing`): rounds of 20 s with
//! a heartbeat, polls of 250 ms doubling to 5 s, a 10-minute limit, the guarded `ready` write
//! with 4 attempts, the one-transaction failure with its cleanup message, and a silent stop on
//! deletion, chat deletion or gear stop. It never holds an upload permit and never deletes the
//! provider file inline.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::indexing::{IndexStatus, classify_status};
use crate::domain::attachments::metrics::AttachmentMetrics;
use crate::domain::attachments::provider::{ProviderFailure, StorageProvider, StorageTarget};
use crate::domain::attachments::store::{AttachmentCleanupQueue, AttachmentStore};
use crate::domain::attachments::timing::{
    BACKGROUND_LIMIT, BACKGROUND_ROUND, BACKGROUND_WAIT_INITIAL, BACKGROUND_WAIT_MAX,
    SET_READY_BACKOFF, Timer,
};
use crate::domain::quota::Clock;

/// The collaborators of the task.
#[domain_model]
pub struct BackgroundDeps {
    pub provider: Arc<dyn StorageProvider>,
    pub store: Arc<dyn AttachmentStore>,
    pub cleanup: Arc<dyn AttachmentCleanupQueue>,
    pub clock: Arc<dyn Clock>,
    pub timer: Arc<dyn Timer>,
    pub metrics: Arc<dyn AttachmentMetrics>,
}

/// The input of the task.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub vector_store_id: String,
    pub provider_file_id: String,
    pub target: StorageTarget,
}

/// How the polling ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ready,
    Failed,
    Timeout,
}

/// How one polling round ended.
enum RoundEnd {
    /// An outcome was set.
    Outcome(Outcome),
    /// The round time is over: the next round starts with a heartbeat.
    Next,
    /// The task stops without changing anything.
    Stop,
}

/// The transient read errors of the task: the first is logged at `warn`, the last one is
/// remembered for the timeout log.
#[derive(Default)]
struct Transients {
    first_logged: bool,
    last: Option<ProviderFailure>,
}

impl Transients {
    fn note(&mut self, failure: ProviderFailure) {
        if !self.first_logged {
            self.first_logged = true;
            tracing::warn!(
                ?failure,
                "a background indexing read failed transiently; polling on"
            );
        }
        self.last = Some(failure);
    }
}

/// Run the task to its outcome (or to a silent stop). Every wait goes through the injected timer
/// and every await that can last is raced against the gear cancellation token.
pub async fn run_background_indexing(
    deps: &BackgroundDeps,
    task: &BackgroundTask,
    cancel: &CancellationToken,
) {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-start
    // the overall limit is 10 minutes after the task start
    let started = deps.timer.monotonic();
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-start
    let mut transients = Transients::default();
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-loop
    let outcome = loop {
        match poll_round(deps, task, cancel, started, &mut transients).await {
            RoundEnd::Outcome(outcome) => break outcome,
            RoundEnd::Next => {}
            RoundEnd::Stop => return,
        }
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-loop
    conclude(deps, task, cancel, outcome, &transients).await;
}

/// Write the outcome of the polling.
async fn conclude(
    deps: &BackgroundDeps,
    task: &BackgroundTask,
    cancel: &CancellationToken,
    outcome: Outcome,
    transients: &Transients,
) {
    if outcome == Outcome::Timeout {
        tracing::warn!(
            last_error = ?transients.last,
            "the background indexing reached its 10-minute limit"
        );
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-match
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-background
    // `mini_chat_attachment_background_indexing{result}` is recorded once per task that reaches an
    // outcome: `ready`, `failed`, `timeout` or `set_ready_failed`
    match outcome {
        Outcome::Ready => write_ready(deps, task, cancel).await,
        Outcome::Failed => write_failed(deps, task, "failed").await,
        Outcome::Timeout => write_failed(deps, task, "timeout").await,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-background
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-match
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-return
    tracing::debug!(
        attachment_id = %task.attachment_id,
        "the background indexing task finished"
    );
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-return
}

/// One round: the heartbeat, then reads of the vector store file for 20 s (250 ms doubling to
/// 5 s apart) until an outcome is set.
async fn poll_round(
    deps: &BackgroundDeps,
    task: &BackgroundTask,
    cancel: &CancellationToken,
    started: Duration,
    transients: &mut Transients,
) -> RoundEnd {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-gear-stop
    // the gear stops: no change, the row stays `uploaded` and the upload reaper later marks it
    // `upload_abandoned`
    if cancel.is_cancelled() {
        return RoundEnd::Stop;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-gear-stop
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-timeout
    if deps.timer.monotonic().saturating_sub(started) >= BACKGROUND_LIMIT {
        return RoundEnd::Outcome(Outcome::Timeout);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-timeout
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-heartbeat
    let heartbeat = deps
        .store
        .heartbeat(task.tenant_id, task.attachment_id, deps.clock.now())
        .await;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-heartbeat
    match heartbeat {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-row-gone
        // the attachment was deleted, its chat was deleted or it is no longer `uploaded`: such a
        // row never becomes `ready`
        Ok(false) => return RoundEnd::Stop,
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-row-gone
        Ok(true) => {}
        Err(error) => tracing::warn!(%error, "the background heartbeat failed"),
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-round
    let round_start = deps.timer.monotonic();
    let mut wait = BACKGROUND_WAIT_INITIAL;
    loop {
        let now = deps.timer.monotonic();
        if now.saturating_sub(round_start) >= BACKGROUND_ROUND
            || now.saturating_sub(started) >= BACKGROUND_LIMIT
        {
            return RoundEnd::Next;
        }
        tokio::select! {
            () = cancel.cancelled() => return RoundEnd::Stop,
            () = deps.timer.sleep(wait) => {}
        }
        wait = (wait * 2).min(BACKGROUND_WAIT_MAX);
        let read = tokio::select! {
            () = cancel.cancelled() => return RoundEnd::Stop,
            read = deps.provider.read_vector_store_file(
                &task.target,
                &task.vector_store_id,
                &task.provider_file_id,
                BACKGROUND_ROUND,
            ) => read,
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-round
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-outcome
        match read {
            Ok(text) => match classify_status(text.as_deref()) {
                IndexStatus::Completed => return RoundEnd::Outcome(Outcome::Ready),
                IndexStatus::Failed => return RoundEnd::Outcome(Outcome::Failed),
                IndexStatus::InProgress => {}
            },
            Err(failure @ ProviderFailure::Transient) => transients.note(failure),
            Err(ProviderFailure::Other | ProviderFailure::NotFound) => {
                return RoundEnd::Outcome(Outcome::Failed);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-outcome
    }
}

/// The `ready` outcome: the guarded write, 4 attempts in total, 1, 2 and 4 s apart.
async fn write_ready(deps: &BackgroundDeps, task: &BackgroundTask, cancel: &CancellationToken) {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-case-ready
    for attempt in 0..=SET_READY_BACKOFF.len() {
        match deps
            .store
            .set_ready_guarded(task.tenant_id, task.attachment_id, deps.clock.now())
            .await
        {
            Ok(true) => {
                deps.metrics.background_indexing("ready");
                return;
            }
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(%error, attempt, "the guarded ready write failed");
                if let Some(pause) = SET_READY_BACKOFF.get(attempt) {
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = deps.timer.sleep(*pause) => {}
                    }
                }
            }
        }
    }
    // every attempt failed: the row stays `uploaded`
    deps.metrics.background_indexing("set_ready_failed");
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-case-ready
}

/// The `failed` and `timeout` outcomes: one transaction with the guarded `failed` update and the
/// cleanup message, whose outbox wake fires after the commit. The provider file is never deleted
/// inline from this task.
async fn write_failed(deps: &BackgroundDeps, task: &BackgroundTask, label: &str) {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-case-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-no-inline-delete
    let failed = deps
        .store
        .fail_indexing(
            task.tenant_id,
            task.chat_id,
            task.attachment_id,
            deps.clock.now(),
            Arc::clone(&deps.cleanup),
        )
        .await;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-no-inline-delete
    match failed {
        Ok(true) => deps.metrics.background_indexing(label),
        Ok(false) => {}
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-tx-failed
        // the row stays `uploaded` so that the upload reaper finishes it
        Err(error) => tracing::warn!(%error, "the background failure transaction failed"),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-tx-failed
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-case-failed
}

#[cfg(test)]
#[path = "background_tests.rs"]
mod background_tests;
