//! Turn finalization (DESIGN 5.7 `FinalizeTurn` invariant, "Content durability
//! invariant").
//!
//! One transaction per terminal outcome: the CAS (`WHERE state = 'running'`)
//! comes first and decides the winner; only the winner persists the assistant
//! message, settles the quota and enqueues the usage and audit messages, all in
//! the same transaction (`finalize_turn_tx`). A loser changes nothing. The
//! charge is computed before the transaction (pure): an out-of-range usage
//! leaves the turn unfinalized, exactly like a failing transaction.

use std::sync::Arc;

use toolkit_db::outbox::Wake;
use toolkit_db::secure::TxConfig;
use toolkit_db::{DbError, DbTx};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::StreamService;
use super::run::TurnRun;
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::messages::{NewMessage, SummaryFrontier};
use crate::domain::ports::{
    AuditPort, MessageRepository, ThreadSummaryPort, ThreadSummaryRepository, TurnRepository,
    UsagePort,
};
use crate::domain::provider::TokenBreakdown;
use crate::domain::quota::QuotaWarning;
use crate::domain::settlement::{
    OutboxPorts, Settlement, SettlementScopes, TerminalFacts, TurnLedger, charge_for, settle_turn,
};
use crate::domain::thread_summary::ThreadSummaryTask;
use crate::domain::turns::{TurnFinalization, TurnState};

/// `chat_turns.error_code` of a turn whose answer could not be stored.
const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";

/// The terminal outcome to commit.
pub(super) enum Outcome {
    /// `completed`: the assistant message is mandatory.
    Completed {
        message: NewMessage,
        /// Provider usage of the answer.
        usage: (u64, u64),
        /// Cache / reasoning breakdown of that usage.
        breakdown: TokenBreakdown,
    },
    /// `failed` with a streaming error code; partial text is not stored.
    Failed {
        error_code: String,
        /// Usage the provider reported with the failure, when any.
        usage: Option<(u64, u64)>,
        /// Cache / reasoning breakdown of that usage.
        breakdown: TokenBreakdown,
    },
    /// `cancelled`: the partial answer is stored when there is one.
    Cancelled { message: Option<NewMessage> },
}

/// What the finalization achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Finalization {
    /// This task committed the outcome it asked for (and settled the quota).
    Won {
        /// Post-settlement quota warnings of a completed turn.
        warnings: Vec<QuotaWarning>,
    },
    /// Another finalizer already moved the turn out of `running`.
    Lost,
    /// A `completed` outcome was downgraded to `failed`
    /// (`message_persistence_failed`) because the answer could not be stored.
    MessageNotPersisted,
    /// The transaction failed; the turn stays `running` until the watchdog.
    Unavailable,
}

/// A finalization transaction failure, by step.
enum FinalizeError {
    /// The CAS (or the transaction itself) failed.
    Turn(DomainError),
    /// The assistant message insert failed.
    Message(DomainError),
    /// The settlement, or the usage / audit enqueue, failed.
    Settlement(DomainError),
}

impl FinalizeError {
    fn error(&self) -> &DomainError {
        match self {
            Self::Turn(error) | Self::Message(error) | Self::Settlement(error) => error,
        }
    }
}

impl From<DbError> for FinalizeError {
    fn from(error: DbError) -> Self {
        Self::Turn(error.into())
    }
}

/// What a committed finalization transaction reports.
enum Committed {
    /// Another finalizer won the CAS.
    Lost,
    /// This transaction won; the wake of its outbox messages is fired once the
    /// commit is durable.
    Won {
        wake: Wake,
        warnings: Vec<QuotaWarning>,
    },
}

/// A thread-summary work item the finalization of a completed turn schedules
/// (the trigger fired): the port and what the frozen target needs.
#[derive(Clone)]
struct SummaryJob {
    port: Arc<dyn ThreadSummaryPort>,
    scope: AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    /// The turn being finalized: its messages are never the target.
    request_id: Uuid,
    /// Frontier of the summary the work item builds on.
    base: Option<SummaryFrontier>,
}

/// Everything one finalization attempt needs (cloned per transaction retry).
#[derive(Clone)]
struct FinalizeCtx {
    scopes: SettlementScopes,
    ledger: TurnLedger,
    facts: TerminalFacts,
    settlement: Settlement,
    warning_threshold_pct: u8,
    usage: Option<Arc<dyn UsagePort>>,
    audit: Option<Arc<dyn AuditPort>>,
    turn: TurnFinalization,
    message: Option<NewMessage>,
    summary: Option<SummaryJob>,
}

impl StreamService {
    pub(super) async fn finalize(&self, run: &TurnRun, outcome: Outcome) -> Finalization {
        match outcome {
            Outcome::Completed {
                message,
                usage,
                breakdown,
            } => {
                self.finalize_completed(run, message, (usage, breakdown))
                    .await
            }
            Outcome::Failed {
                error_code,
                usage,
                breakdown,
            } => {
                let failed = self
                    .finalize_tx(
                        run,
                        TurnState::Failed,
                        Some(error_code),
                        usage.map(|usage| (usage, breakdown)),
                        None,
                    )
                    .await;
                Self::won_or_lost(run, failed)
            }
            Outcome::Cancelled { message } => self.finalize_cancelled(run, message).await,
        }
    }

    async fn finalize_completed(
        &self,
        run: &TurnRun,
        message: NewMessage,
        usage: ((u64, u64), TokenBreakdown),
    ) -> Finalization {
        let completed = self
            .finalize_tx(run, TurnState::Completed, None, Some(usage), Some(message))
            .await;
        if let Err(FinalizeError::Message(error)) = &completed {
            tracing::error!(turn_id = %run.turn_id, %error,
                "assistant message not persisted; failing the turn");
            return self.fail_persistence(run, usage).await;
        }
        Self::won_or_lost(run, completed)
    }

    /// The assistant message of a completed turn could not be stored: the turn
    /// is finalized `failed` instead (content durability invariant), settled on
    /// the usage the provider reported.
    async fn fail_persistence(
        &self,
        run: &TurnRun,
        usage: ((u64, u64), TokenBreakdown),
    ) -> Finalization {
        let failed = self
            .finalize_tx(
                run,
                TurnState::Failed,
                Some(MESSAGE_PERSISTENCE_FAILED.to_owned()),
                Some(usage),
                None,
            )
            .await;
        match Self::won_or_lost(run, failed) {
            Finalization::Won { .. } => Finalization::MessageNotPersisted,
            other => other,
        }
    }

    /// A cancelled turn keeps its partial answer on a best-effort basis: when
    /// the insert fails the turn is still cancelled, without a message.
    async fn finalize_cancelled(&self, run: &TurnRun, message: Option<NewMessage>) -> Finalization {
        let with_message = message.is_some();
        let cancelled = self
            .finalize_tx(run, TurnState::Cancelled, None, None, message)
            .await;
        match cancelled {
            Err(FinalizeError::Message(error)) if with_message => {
                tracing::warn!(turn_id = %run.turn_id, %error,
                    "partial answer not persisted; cancelling without it");
                let plain = self
                    .finalize_tx(run, TurnState::Cancelled, None, None, None)
                    .await;
                Self::won_or_lost(run, plain)
            }
            other => Self::won_or_lost(run, other),
        }
    }

    fn won_or_lost(
        run: &TurnRun,
        result: Result<Option<Vec<QuotaWarning>>, FinalizeError>,
    ) -> Finalization {
        match result {
            Ok(Some(warnings)) => Finalization::Won { warnings },
            Ok(None) => Finalization::Lost,
            Err(error) => {
                tracing::error!(turn_id = %run.turn_id, error = %error.error(),
                    "turn finalization failed");
                Finalization::Unavailable
            }
        }
    }

    /// The thread-summary work item of a turn that ends `completed`, when the
    /// service has the port, the worker is enabled and the trigger fires
    /// (DESIGN 3.6 "Trigger timing"): proactive at `compression_threshold_pct`
    /// of the effective budget without a summary, urgent on a truncated context.
    fn summary_job(&self, run: &TurnRun, state: TurnState) -> Option<SummaryJob> {
        let worker = &self.config.thread_summary_worker;
        let port = self.thread_summary.as_ref()?;
        if state != TurnState::Completed
            || !worker.enabled
            || !run.trigger.fires(worker.compression_threshold_pct)
        {
            return None;
        }
        Some(SummaryJob {
            port: Arc::clone(port),
            scope: run.scope.clone(),
            tenant_id: run.request.tenant_id,
            chat_id: run.chat_id,
            request_id: run.request_id,
            base: run.trigger.base,
        })
    }

    /// Commits one terminal outcome. `Ok(Some(warnings))` when this call won
    /// the CAS, `Ok(None)` when it lost.
    async fn finalize_tx(
        &self,
        run: &TurnRun,
        state: TurnState,
        error_code: Option<String>,
        usage: Option<((u64, u64), TokenBreakdown)>,
        message: Option<NewMessage>,
    ) -> Result<Option<Vec<QuotaWarning>>, FinalizeError> {
        let now = now_utc();
        let facts = TerminalFacts {
            state,
            error_code: error_code.clone(),
            usage: usage.map(|(tokens, _)| tokens),
            breakdown: usage.map(|(_, breakdown)| breakdown).unwrap_or_default(),
            latency_ms: u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            now,
        };
        // Pure, before the transaction: an unusable usage or multiplier leaves
        // the turn unfinalized (DESIGN 5.3 "Overflow Protection").
        let settlement = charge_for(
            &run.ledger,
            &facts,
            self.config.quota.overshoot_tolerance_factor,
        )
        .map_err(|error| {
            tracing::warn!(turn_id = %run.turn_id, %error, "settlement refused the usage");
            FinalizeError::Settlement(DomainError::internal(format!(
                "settlement of turn {} failed: {error}",
                run.turn_id
            )))
        })?;
        let ctx = FinalizeCtx {
            scopes: SettlementScopes {
                turn: run.scope.clone(),
                quota: run.quota_scope.clone(),
            },
            ledger: run.ledger.clone(),
            facts,
            settlement,
            warning_threshold_pct: self.config.quota.warning_threshold_pct,
            usage: self.usage.clone(),
            audit: self.audit.clone(),
            turn: TurnFinalization {
                turn_id: run.turn_id,
                state,
                error_code,
                assistant_message_id: message.as_ref().map(|message| message.id),
                provider_response_id: message
                    .as_ref()
                    .and_then(|message| message.provider_response_id.clone()),
                now,
            },
            message,
            summary: self.summary_job(run, state),
        };
        let committed = self
            .db
            .db()
            .transaction_with_retry(
                TxConfig::default(),
                |error: &FinalizeError| error.error().db_cause(),
                |tx| {
                    let ctx = ctx.clone();
                    Box::pin(async move { finalize_turn_tx(tx, &ctx).await })
                },
            )
            .await?;
        Ok(match committed {
            Committed::Lost => None,
            Committed::Won { wake, warnings } => {
                // The rows are durable now: wake the outbox sequencers.
                wake.fire();
                Some(warnings)
            }
        })
    }
}

/// The finalization transaction body: the CAS first, then (winner only) the
/// assistant message, the quota settlement and the usage and audit messages.
async fn finalize_turn_tx(tx: &DbTx<'_>, ctx: &FinalizeCtx) -> Result<Committed, FinalizeError> {
    if !tx
        .finalize_turn(&ctx.scopes.turn, &ctx.turn)
        .await
        .map_err(FinalizeError::Turn)?
    {
        return Ok(Committed::Lost);
    }
    if let Some(message) = &ctx.message {
        tx.insert_message(&ctx.scopes.turn, message.clone())
            .await
            .map_err(FinalizeError::Message)?;
    }
    let settled = settle_turn(
        tx,
        &ctx.scopes,
        &ctx.ledger,
        &ctx.facts,
        &ctx.settlement,
        Some(ctx.warning_threshold_pct),
        OutboxPorts {
            usage: ctx.usage.as_deref(),
            audit: ctx.audit.as_deref(),
        },
    )
    .await
    .map_err(FinalizeError::Settlement)?;
    let mut wake = settled.wake;
    if let Some(job) = &ctx.summary {
        wake += enqueue_summary(tx, job)
            .await
            .map_err(FinalizeError::Settlement)?;
    }
    Ok(Committed::Won {
        wake,
        warnings: settled.warnings,
    })
}

/// Enqueues the thread-summary work item in the finalization transaction. The
/// frozen target is the latest non-deleted message of the chat outside the turn
/// being finalized (the causing turn is never summarized); nothing is enqueued
/// when there is none (first turn of the chat) or when it is not past the base
/// frontier (nothing new to summarize).
async fn enqueue_summary(tx: &DbTx<'_>, job: &SummaryJob) -> Result<Wake, DomainError> {
    let Some(target) = tx
        .latest_message_frontier(&job.scope, job.chat_id, job.request_id)
        .await?
    else {
        return Ok(Wake::empty());
    };
    if job.base.is_some_and(|base| {
        (target.created_at, target.message_id) <= (base.created_at, base.message_id)
    }) {
        return Ok(Wake::empty());
    }
    job.port
        .enqueue_thread_summary(
            tx,
            &ThreadSummaryTask {
                tenant_id: job.tenant_id,
                chat_id: job.chat_id,
                system_request_id: Uuid::new_v4(),
                base: job.base,
                target,
            },
        )
        .await
}
