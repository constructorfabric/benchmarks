//! The setup failure marking of a replacement turn
//! (`cpt-cf-mini-chat-algo-turn-mutations-fail-setup`).

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::ports::{MutationMetrics, MutationRepository};
use super::types::{MutationOp, MutationResult, SetupFailureCode};
use crate::domain::error::DomainError;

/// Attempts of the mark (the first try included).
pub const MARK_ATTEMPTS: u32 = 3;

/// Pauses between the attempts: after the first and after the second.
pub const MARK_BACKOFF: [Duration; 2] = [Duration::from_millis(20), Duration::from_millis(100)];

/// The turn a failed setup marks.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailedSetup {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The new turn.
    pub turn_id: Uuid,
    /// The new turn's `request_id`.
    pub request_id: Uuid,
    /// The code to mark it with.
    pub code: SetupFailureCode,
    /// The current time (`completed_at`, `updated_at`).
    pub at: OffsetDateTime,
}

/// Marks the new turn `failed`: a guarded autocommit UPDATE retried at most
/// [`MARK_ATTEMPTS`] times on an error; an update of 0 rows is benign (logged
/// at debug, not retried); an error after the last attempt is logged at error
/// and the turn stays `running` for the orphan watchdog. Never fails and never
/// records a metric: the service records the result once per request.
// @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-setup-failure:p1
pub async fn fail_setup(repo: &dyn MutationRepository, failed: &FailedSetup) {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-no-settlement
    for attempt in 1..=MARK_ATTEMPTS {
        if try_mark(repo, failed, attempt).await {
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-return-fail-setup
            return;
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-return-fail-setup
        }
        let pause = MARK_BACKOFF
            .get(usize::try_from(attempt - 1).unwrap_or(0))
            .copied()
            .unwrap_or_default();
        tokio::time::sleep(pause).await;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-no-settlement
}

/// One attempt of the mark; `true` when the mark is settled (done, benign, or
/// out of attempts and logged).
async fn try_mark(repo: &dyn MutationRepository, failed: &FailedSetup, attempt: u32) -> bool {
    let marked = repo
        .mark_setup_failed(
            failed.tenant_id,
            failed.turn_id,
            failed.code.as_str(),
            failed.at,
        )
        .await;
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-if-mark-failed-error
    match marked {
        Ok(0) => {
            log_already_final(failed);
            true
        }
        Ok(_) => true,
        Err(error) if attempt >= MARK_ATTEMPTS => {
            log_exhausted(failed, &error);
            true
        }
        Err(error) => {
            tracing::debug!(%error, attempt, "marking the replacement turn failed again");
            false
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-if-mark-failed-error
}

fn log_already_final(failed: &FailedSetup) {
    tracing::debug!(
        chat_id = %failed.chat_id,
        turn_id = %failed.turn_id,
        request_id = %failed.request_id,
        "the replacement turn was already finalized, nothing to mark"
    );
}

fn log_exhausted(failed: &FailedSetup, error: &DomainError) {
    tracing::error!(
        %error,
        chat_id = %failed.chat_id,
        turn_id = %failed.turn_id,
        request_id = %failed.request_id,
        "the replacement turn could not be marked failed; the orphan watchdog finalizes it"
    );
}

/// A drop guard armed after the replacement commit: when the setup task ends
/// without disarming it (a panic), the new turn is marked `failed` with
/// `turn_setup_failed` by a spawned task, and `setup_failed` is recorded.
pub struct SetupGuard {
    inner: Option<ArmedGuard>,
}

/// What an armed guard needs to mark the turn.
type ArmedGuard = (
    Arc<dyn MutationRepository>,
    Arc<dyn MutationMetrics>,
    MutationOp,
    FailedSetup,
);

impl SetupGuard {
    /// Arms the guard; `failed.code` is replaced by `turn_setup_failed` on a
    /// drop.
    #[must_use]
    pub fn arm(
        repo: Arc<dyn MutationRepository>,
        metrics: Arc<dyn MutationMetrics>,
        op: MutationOp,
        failed: FailedSetup,
    ) -> Self {
        Self {
            inner: Some((repo, metrics, op, failed)),
        }
    }

    /// The setup ended without a panic: nothing is marked on drop.
    pub fn disarm(mut self) {
        self.inner = None;
    }
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        let Some((repo, metrics, op, failed)) = self.inner.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                turn_id = %failed.turn_id,
                "no runtime to mark the replacement turn failed; the orphan watchdog finalizes it"
            );
            return;
        };
        runtime.spawn(async move {
            let failed = FailedSetup {
                code: SetupFailureCode::TurnSetupFailed,
                ..failed
            };
            fail_setup(&*repo, &failed).await;
            metrics.turn_mutation(op, MutationResult::SetupFailed);
        });
    }
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
