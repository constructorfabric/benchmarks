//! The background indexing task (DESIGN "File Upload", B.9.5).
//!
//! A document still `in_progress` at the request deadline is answered as `uploaded`; this task
//! keeps polling for up to `background_timeout`, in rounds of `background_round`. Every round first
//! refreshes `updated_at`, so the upload reaper does not take the row. The task stops without
//! changes when the row is deleted, is no longer `uploaded`, belongs to a deleted chat or the gear
//! stops. A row left `uploaded` is finished by the upload reaper.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_security::{AccessScope, SecurityContext};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::model::{IndexingTimings, ReadyUpdate};
use super::ports::{
    AttachmentRepository, IndexingStatus, StorageError, StorageGateway, StorageRoute,
};
use crate::domain::cleanup::AttachmentCleanupPayload;
use crate::domain::clock;
use crate::domain::metrics::Metrics;

/// Pauses before the second, third and fourth attempt to set `ready` (7 s in total).
const READY_RETRY_PAUSES: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// How one status read ended.
enum Poll {
    Completed,
    Failed,
    Running,
    Transient(StorageError),
    Aborted,
}

/// How the task ends.
enum Verdict {
    /// The document is searchable.
    Ready,
    /// The provider reported a failure or a status read failed for good.
    Failed,
    /// `background_timeout` elapsed.
    TimedOut,
    /// Gear stop or the row is no longer ours: nothing to write.
    Stopped,
}

/// The state shared by the rounds of one task.
struct Rounds {
    give_up: Instant,
    wait: Duration,
    last_error: Option<StorageError>,
}

/// One background indexing task.
pub(super) struct BackgroundIndexing {
    pub ctx: SecurityContext,
    pub scope: AccessScope,
    pub route: StorageRoute,
    pub repo: Arc<dyn AttachmentRepository>,
    pub storage: Arc<dyn StorageGateway>,
    pub metrics: Arc<dyn Metrics>,
    pub timings: IndexingTimings,
    pub shutdown: CancellationToken,
    pub attachment_id: Uuid,
    pub vector_store: String,
    pub provider_file: String,
    /// The cleanup message written with the `failed` transition.
    pub failure: AttachmentCleanupPayload,
}

impl BackgroundIndexing {
    /// Runs the task to its end.
    pub async fn run(self) {
        let mut rounds = Rounds {
            give_up: Instant::now() + self.timings.background_timeout,
            wait: self.timings.poll_initial,
            last_error: None,
        };
        match self.poll_until_done(&mut rounds).await {
            Verdict::Ready => self.set_ready().await,
            Verdict::Failed => self.fail("failed", rounds.last_error.as_ref()).await,
            Verdict::TimedOut => self.fail("timeout", rounds.last_error.as_ref()).await,
            Verdict::Stopped => self.log_stopped(),
        }
    }

    /// The rounds: refresh the heartbeat, then read the status with a doubling wait until the round
    /// ends.
    async fn poll_until_done(&self, rounds: &mut Rounds) -> Verdict {
        loop {
            if !self.heartbeat().await {
                return Verdict::Stopped;
            }
            let round_end = (Instant::now() + self.timings.background_round).min(rounds.give_up);
            while Instant::now() < round_end {
                if let Some(verdict) = self.step(rounds, round_end).await {
                    return verdict;
                }
            }
            if Instant::now() >= rounds.give_up {
                return Verdict::TimedOut;
            }
        }
    }

    /// Refreshes `updated_at`; `false` when the row no longer qualifies.
    async fn heartbeat(&self) -> bool {
        match self
            .repo
            .touch_uploaded(&self.scope, self.attachment_id, clock::now())
            .await
        {
            Ok(alive) => alive,
            Err(err) => {
                self.log_error("the heartbeat of the background indexing failed", &err);
                true
            }
        }
    }

    /// One wait and one status read; `Some` ends the task.
    async fn step(&self, rounds: &mut Rounds, round_end: Instant) -> Option<Verdict> {
        let pause = rounds
            .wait
            .min(round_end.saturating_duration_since(Instant::now()));
        if !self.pause(pause).await {
            return Some(Verdict::Stopped);
        }
        let verdict = match self.poll(rounds.give_up).await {
            Poll::Completed => Some(Verdict::Ready),
            Poll::Failed => Some(Verdict::Failed),
            Poll::Aborted => Some(Verdict::Stopped),
            Poll::Running => None,
            Poll::Transient(err) => {
                self.note_transient(rounds.last_error.as_ref(), &err);
                rounds.last_error = Some(err);
                None
            }
        };
        rounds.wait = rounds
            .wait
            .saturating_mul(2)
            .min(self.timings.background_poll_max);
        verdict
    }

    /// Sleeps; `false` when the gear is stopping.
    async fn pause(&self, duration: Duration) -> bool {
        tokio::select! {
            () = self.shutdown.cancelled() => false,
            () = tokio::time::sleep(duration) => true,
        }
    }

    /// One status read, bounded by the end of the task and interrupted by the gear stop.
    async fn poll(&self, give_up: Instant) -> Poll {
        let left = give_up.saturating_duration_since(Instant::now());
        let read = self.storage.file_status(
            &self.ctx,
            &self.route,
            &self.vector_store,
            &self.provider_file,
        );
        let result = tokio::select! {
            () = self.shutdown.cancelled() => return Poll::Aborted,
            result = tokio::time::timeout(left, read) => result,
        };
        match result {
            Err(_elapsed) => Poll::Running,
            Ok(Ok(status)) => classify(status),
            Ok(Err(err)) if err.transient => Poll::Transient(err),
            Ok(Err(err)) => {
                self.log_error("the status read failed for good", &err);
                Poll::Failed
            }
        }
    }

    /// `uploaded` to `ready`: four attempts, 1, 2 and 4 s apart. A row that no longer qualifies
    /// ends the task silently; if every attempt fails the reaper finishes the row later.
    async fn set_ready(&self) {
        let mut pauses = READY_RETRY_PAUSES.into_iter();
        loop {
            let attempt = self
                .repo
                .mark_ready(
                    &self.scope,
                    self.attachment_id,
                    &ReadyUpdate::plain(),
                    clock::now(),
                )
                .await;
            match attempt {
                Ok(true) => {
                    self.metrics.attachment_background_indexing("ready");
                    return;
                }
                Ok(false) => return self.log_stopped(),
                Err(err) => {
                    let Some(pause) = pauses.next() else {
                        self.log_error(
                            "the attachment could not be set `ready`; the upload reaper will fail it",
                            &err,
                        );
                        self.metrics
                            .attachment_background_indexing("set_ready_failed");
                        return;
                    };
                    if !self.pause(pause).await {
                        return;
                    }
                }
            }
        }
    }

    /// `uploaded` to `failed` with the cleanup event in one transaction; the outbox cleanup
    /// deletes the provider file.
    async fn fail(&self, result: &str, last_error: Option<&StorageError>) {
        let recorded = self
            .repo
            .fail_indexing(&self.scope, &self.failure, clock::now())
            .await;
        match recorded {
            Ok(true) => {
                self.metrics.attachment_background_indexing(result);
                self.log_failed(result, last_error);
            }
            Ok(false) => self.log_stopped(),
            Err(err) => self.log_error(
                "the indexing failure could not be recorded; the upload reaper will fail the row",
                &err,
            ),
        }
    }

    fn note_transient(&self, previous: Option<&StorageError>, err: &StorageError) {
        if previous.is_none() {
            warn!(attachment_id = %self.attachment_id, error = %err, "a transient status read error during the background indexing");
        }
    }

    fn log_stopped(&self) {
        debug!(attachment_id = %self.attachment_id, "the background indexing stopped without changes");
    }

    fn log_failed(&self, result: &str, last_error: Option<&StorageError>) {
        warn!(
            attachment_id = %self.attachment_id,
            result,
            last_error = last_error.map(ToString::to_string),
            "the background indexing failed"
        );
    }

    fn log_error(&self, message: &str, err: &dyn std::fmt::Display) {
        warn!(attachment_id = %self.attachment_id, error = %err, "{message}");
    }
}

/// The poll outcome of a status the provider reported.
fn classify(status: IndexingStatus) -> Poll {
    match status {
        IndexingStatus::Completed => Poll::Completed,
        IndexingStatus::InProgress => Poll::Running,
        IndexingStatus::Failed(reason) => {
            info!(reason = %reason, "the vector store reports the file as not indexable");
            Poll::Failed
        }
    }
}
