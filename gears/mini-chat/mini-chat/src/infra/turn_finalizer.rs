//! The turn finalization port over the database provider (slices S1-setup-path and
//! S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`): one transaction in which
//! `QuotaService::finalize_turn` performs the CAS on `running`, the billing derivation, the
//! settlement and the usage message enqueue; the stream finalization adds the assistant (or
//! partial) message, the turn audit event and the thread-summary work, and the wakes and the
//! metrics follow the commit.
//!
//! A failed message insert is handled by running the transaction again without the message: a
//! failed statement aborts a `PostgreSQL` transaction, and the public transaction API has no
//! savepoints, so the whole attempt rolls back (nothing of it remains) and the next attempt
//! commits the outcome that does not depend on the message.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, QuotaDecision, TurnAuditEvent, UsageTokens};
use time::{Duration, OffsetDateTime};
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind, StreamErrorCode};
use crate::domain::quota::{
    FinalizeOutcome, FinalizeRequest, Finalizer, QuotaService, Settlement, SettlementResult,
    TerminalState, ToolCallCounts, TurnTerminalUpdate,
};
use crate::domain::streaming::audit::build_stream_audit;
use crate::domain::streaming::provider_errors::generic_message;
use crate::domain::streaming::summary::{
    Frontier, SummaryTriggerInput, ThreadSummaryWork, evaluate_trigger, has_summary_work,
};
use crate::domain::streaming::{
    CommittedTurn, ProviderCallPlan, StreamFinalization, StreamOutcome, StreamTerminal,
    TurnFinalization, TurnFinalizer, TurnOutbox, TurnOutcome, TurnTarget,
};
use crate::domain::turns::TurnState;
use crate::infra::metrics::{MiniChatMetrics, current_trace_id};
use crate::infra::storage::message_repo::{MessageRepo, NewAssistantMessage};
use crate::infra::storage::repo_impl::ChatRepo;
use crate::infra::storage::stream_store::time_storage;
use crate::infra::storage::turn_repo::TurnRepo;

/// The generic `error_detail` of a turn that failed without a provider response.
const FAILURE_DETAIL: &str = "the provider is not available";

/// `thread_summary_worker.enabled` and `thread_summary_worker.compression_threshold_pct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummarySettings {
    pub enabled: bool,
    pub compression_threshold_pct: u32,
}

/// What the stream finalization of slice S2 adds to the quota service: the audit and
/// thread-summary queues, the summary settings and the metrics.
#[derive(Clone)]
pub struct StreamFinalizationDeps {
    pub outbox: Arc<dyn TurnOutbox>,
    pub summary: SummarySettings,
    pub metrics: Arc<MiniChatMetrics>,
}

/// The finalization of committed turns through the quota service and its database provider.
#[derive(Clone)]
pub struct SqlTurnFinalizer {
    quota: Arc<QuotaService>,
    stream: Option<StreamFinalizationDeps>,
}

/// What a committed attempt leaves for the steps after the commit.
struct Won {
    committed: CommittedTurn,
    /// The settlement result (metrics) of the CAS winner; `None` for a code without a billing
    /// outcome.
    settlement: Option<SettlementResult>,
    /// The outbox wakes of the settlement and the enqueues: fired after the commit.
    wakes: Vec<Wake>,
    /// `scheduled` or `not_needed` when the summary trigger was evaluated.
    summary_result: Option<&'static str>,
}

/// The result of one transaction attempt.
enum Attempt {
    /// Another finalizer won the CAS: nothing was written.
    Lost,
    Won(Box<Won>),
}

/// Why an attempt failed.
enum AttemptError {
    /// The assistant (or partial) message could not be inserted: the attempt rolled back.
    MessageInsert(DomainError),
    /// Any other failure: the attempt rolled back and the turn stays `running`.
    Other(DomainError),
}

/// `time` truncated to microseconds, the precision every supported database keeps.
fn micros(time: OffsetDateTime) -> OffsetDateTime {
    time.replace_microsecond(time.microsecond()).unwrap_or(time)
}

/// A provider count as the signed column value.
fn column_count(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

impl SqlTurnFinalizer {
    /// Create the finalizer over the quota service (its database, ledger, queue, metrics and
    /// clock).
    #[must_use]
    pub fn new(quota: Arc<QuotaService>) -> Self {
        Self {
            quota,
            stream: None,
        }
    }

    /// Add the collaborators of the stream finalization of slice S2 (the audit and
    /// thread-summary queues, the summary settings and the metrics). With them, every
    /// finalization of this finalizer, the S1 outcomes included, enqueues its turn audit event.
    #[must_use]
    pub fn with_stream_deps(mut self, deps: StreamFinalizationDeps) -> Self {
        self.stream = Some(deps);
        self
    }

    /// The terminal columns of the CAS update of an outcome without a message.
    fn update(&self, outcome: TurnOutcome) -> Result<TurnTerminalUpdate, DomainError> {
        let completed_at = self.quota.clock.now();
        Ok(match outcome {
            TurnOutcome::Cancelled => TurnTerminalUpdate {
                state: TerminalState::Cancelled,
                error_code: None,
                error_detail: None,
                assistant_message_id: None,
                completed_at,
            },
            // the SSE-only codes are never stored
            TurnOutcome::Failed {
                code: StreamErrorCode::FinalizationFailed | StreamErrorCode::StreamInterrupted,
            } => {
                return Err(DomainError::internal(
                    "an SSE-only error code cannot be stored on a turn",
                ));
            }
            TurnOutcome::Failed { code } => TurnTerminalUpdate {
                state: TerminalState::Failed,
                error_code: Some(code.as_str().to_owned()),
                error_detail: Some(FAILURE_DETAIL.to_owned()),
                assistant_message_id: None,
                completed_at,
            },
        })
    }

    /// One transaction of the stream finalization: the CAS and the settlement first (a lost CAS
    /// writes nothing), then the provider response id, the assistant (or partial) message, the
    /// audit event and the thread-summary work.
    async fn attempt(
        &self,
        deps: &StreamFinalizationDeps,
        plan: &ProviderCallPlan,
        terminal: &StreamTerminal,
        with_message: bool,
    ) -> Result<Attempt, AttemptError> {
        let now = micros(self.quota.clock.now());
        let message_id = with_message.then_some(plan.assistant_message_id);
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-match
        let update = match &terminal.outcome {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-completed
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-completed
            StreamOutcome::Completed => TurnTerminalUpdate {
                state: TerminalState::Completed,
                // the incomplete reason is never written to `error_code`
                error_code: None,
                error_detail: None,
                assistant_message_id: message_id,
                completed_at: now,
            },
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-completed
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-completed
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-cancelled-partial
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-cancelled
            StreamOutcome::Cancelled => TurnTerminalUpdate {
                state: TerminalState::Cancelled,
                error_code: None,
                error_detail: None,
                // a partial message only when there is text to keep
                assistant_message_id: message_id.filter(|_| !terminal.text.is_empty()),
                completed_at: now,
            },
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-cancelled
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-cancelled-partial
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-failed-or-empty
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-failed
            StreamOutcome::Failed { code, detail } => TurnTerminalUpdate {
                state: TerminalState::Failed,
                error_code: Some(code.as_str().to_owned()),
                error_detail: Some(detail.clone()),
                // partial text of a failed turn is discarded
                assistant_message_id: None,
                completed_at: now,
            },
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-failed
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-failed-or-empty
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-match
        let committed = CommittedTurn {
            state: match update.state {
                TerminalState::Completed => TurnState::Completed,
                TerminalState::Failed => TurnState::Failed,
                TerminalState::Cancelled => TurnState::Cancelled,
            },
            error_code: update.error_code.clone(),
            assistant_message_id: update.assistant_message_id,
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-settle
        // the contract performs the CAS on `state` `running`, derives the billing outcome,
        // settles the buckets and enqueues the one usage event; no usage for a cancelled turn
        let request = FinalizeRequest {
            tenant_id: plan.tenant_id,
            turn_id: plan.turn_id,
            update,
            finalizer: Finalizer::Stream {
                selected_model: plan.selected_model.clone(),
                period_starts: plan.decision.period_starts,
            },
            usage: terminal.usage,
            tool_counts: terminal.tool_counts,
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-settle
        let insert_failed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&insert_failed);
        let quota = Arc::clone(&self.quota);
        let outbox = Arc::clone(&deps.outbox);
        let summary_enabled = deps.summary.enabled;
        let threshold = deps.summary.compression_threshold_pct;
        let storage = time_storage(&self.quota.db);
        let (plan, terminal) = (plan.clone(), terminal.clone());
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-begin
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-try
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-commit
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-forbidden
        // the state transition, the message, the settlement, the usage and audit events and the
        // summary work commit together or not at all: no message first and state later in a
        // separate transaction, no outbox message outside it, and a non-winning path writes
        // nothing
        let outcome = self
            .quota
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let settled = quota.finalize_turn(tx, &request).await?;
                    // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-terminal-immutable
                    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-cas-lost
                    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-return-noop
                    // the loser wrote nothing (its message is inserted only after the CAS), so
                    // the transaction commits empty; the relay sends `stream_interrupted`
                    let (settlement, mut wakes) = match settled {
                        FinalizeOutcome::AlreadyFinalized => return Ok(Attempt::Lost),
                        FinalizeOutcome::NoSettlement => (None, Vec::new()),
                        FinalizeOutcome::Settled(settlement) => {
                            let Settlement { result, wake } = *settlement;
                            (Some(result), vec![wake])
                        }
                    };
                    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-return-noop
                    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-cas-lost
                    // @cpt-end:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-terminal-immutable
                    // the provider response id stays internal: on the turn and on the message
                    if let Some(response_id) = &terminal.provider_response_id {
                        TurnRepo::set_provider_response_id(
                            tx,
                            plan.tenant_id,
                            plan.turn_id,
                            response_id,
                        )
                        .await?;
                    }
                    if let Some(id) = request.update.assistant_message_id {
                        insert_assistant(tx, &plan, &terminal, id, storage, quota.clock.now())
                            .await
                            .inspect_err(|_| flag.store(true, Ordering::SeqCst))?;
                    }
                    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-audit
                    // the audit event of the outcome actually committed, in the same transaction
                    let finished_at = request.update.completed_at;
                    let audit =
                        build_stream_audit(&plan, &terminal, finished_at, current_trace_id());
                    wakes.push(outbox.enqueue_turn_audit(tx, &audit).await?);
                    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-audit
                    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-summary-gate
                    let summary_result = if summary_enabled
                        && matches!(terminal.outcome, StreamOutcome::Completed)
                    {
                        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-summary
                        summary_trigger(tx, outbox.as_ref(), &plan, storage, threshold, &mut wakes)
                            .await?
                        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-summary
                    } else {
                        None
                    };
                    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-summary-gate
                    Ok(Attempt::Won(Box::new(Won {
                        committed,
                        settlement,
                        wakes,
                        summary_result,
                    })))
                })
            })
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-forbidden
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-commit
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-try
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-begin
        outcome.map_err(|error| {
            if insert_failed.load(Ordering::SeqCst) {
                AttemptError::MessageInsert(error)
            } else {
                AttemptError::Other(error)
            }
        })
    }

    /// The effects after a commit that won the CAS: the metrics of the settlement and of the
    /// finalization, then the outbox wakes.
    fn after_commit(&self, deps: &StreamFinalizationDeps, won: Won, started: Instant) {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-after-commit
        let Won {
            committed,
            settlement,
            wakes,
            summary_result,
        } = won;
        if let Some(result) = &settlement {
            result.record_after_commit(self.quota.metrics.as_ref());
        }
        deps.metrics.finalization_latency(started.elapsed());
        if let Some(result) = summary_result {
            deps.metrics.thread_summary_trigger(result);
        }
        if committed.state == TurnState::Cancelled {
            deps.metrics.streams_aborted("client_disconnect");
        }
        for wake in wakes {
            wake.fire();
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-after-commit
    }

    /// The audit event of an S1 outcome (a pre-provider cancellation or a provider task that
    /// could not start): the turn row supplies what the plan would.
    async fn enqueue_pre_provider_audit(
        &self,
        tx: &DbTx<'_>,
        outbox: &dyn TurnOutbox,
        target: &TurnTarget,
        update: &TurnTerminalUpdate,
    ) -> Result<Wake, DomainError> {
        let turn = self
            .quota
            .ledger
            .load_turn(tx, target.tenant_id, target.turn_id)
            .await?
            .ok_or_else(|| DomainError::internal("the finalized turn cannot be loaded"))?;
        let effective_model = turn.effective_model.clone().unwrap_or_default();
        let downgraded = !effective_model.is_empty() && effective_model != target.selected_model;
        let latency = (update.completed_at - turn.started_at).whole_milliseconds();
        let event = TurnAuditEvent {
            tenant_id: target.tenant_id,
            user_id: turn.requester_user_id.unwrap_or_else(Uuid::nil),
            chat_id: turn.chat_id,
            turn_id: turn.id,
            request_id: turn.request_id,
            requester_type: "user".to_owned(),
            selected_model: target.selected_model.clone(),
            effective_model,
            terminal_state: update.state.as_str().to_owned(),
            error_code: update.error_code.clone(),
            usage: None,
            latency_ms: u64::try_from(latency).unwrap_or(0),
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            quota_decision: if downgraded {
                QuotaDecision::Downgrade
            } else {
                QuotaDecision::Allow
            },
            downgrade_from: downgraded.then(|| target.selected_model.clone()),
            downgrade_reason: None,
            trace_id: current_trace_id(),
            timestamp: update.completed_at,
            prompt: String::new(),
            response: String::new(),
            attachments: Vec::new(),
            license: None,
            quota_scope: None,
        };
        let event = if update.state == TerminalState::Completed {
            AuditEvent::TurnCompleted(event)
        } else {
            AuditEvent::TurnFailed(event)
        };
        outbox.enqueue_turn_audit(tx, &event).await
    }
}

/// Insert the assistant (or partial) message of the finalized turn after the latest message of
/// the chat, and move the chat activity in the same transaction.
async fn insert_assistant(
    tx: &DbTx<'_>,
    plan: &ProviderCallPlan,
    terminal: &StreamTerminal,
    id: Uuid,
    storage: crate::infra::storage::chat_page::TimeStorage,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-assistant-after-user
    // the application clock reading, moved to one microsecond after the latest message of the
    // chat when the clock is not later, so the assistant message follows its user message
    let now = micros(now);
    let created_at =
        match MessageRepo::latest_message(tx, plan.tenant_id, plan.chat_id, storage, None).await? {
            Some(latest) if latest.created_at >= now => {
                latest.created_at + Duration::microseconds(1)
            }
            _ => now,
        };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-assistant-after-user
    // a partial message carries no provider token counts
    let usage = match terminal.outcome {
        StreamOutcome::Completed => terminal.usage.unwrap_or_default(),
        _ => UsageTokens::default(),
    };
    MessageRepo::insert_assistant_message(
        tx,
        plan.tenant_id,
        plan.chat_id,
        &NewAssistantMessage {
            id,
            request_id: plan.request_id,
            content: terminal.text.clone(),
            model: plan.decision.effective_model.clone(),
            input_tokens: column_count(usage.input_tokens),
            output_tokens: column_count(usage.output_tokens),
            cache_read_input_tokens: column_count(usage.cache_read_input_tokens),
            cache_write_input_tokens: column_count(usage.cache_write_input_tokens),
            reasoning_tokens: column_count(usage.reasoning_tokens),
            provider_response_id: terminal.provider_response_id.clone(),
            created_at,
        },
    )
    .await?;
    // the message-activity contract: `chats.updated_at` moves in the same transaction; a chat
    // deleted while the stream ran has no activity to move
    let owner = AccessScope::for_tenant(plan.tenant_id).ensure_owner(plan.user_id);
    match ChatRepo::touch_chat(tx, &owner, plan.chat_id).await {
        Ok(_)
        | Err(DomainError::NotFound {
            resource: ResourceKind::Chat,
            ..
        }) => Ok(()),
        Err(other) => Err(other),
    }
}

/// Evaluate the thread-summary trigger and enqueue the work in the open transaction; the result
/// is `None` when the trigger did not fire.
async fn summary_trigger(
    tx: &DbTx<'_>,
    outbox: &dyn TurnOutbox,
    plan: &ProviderCallPlan,
    storage: crate::infra::storage::chat_page::TimeStorage,
    threshold: u32,
    wakes: &mut Vec<Wake>,
) -> Result<Option<&'static str>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-no-fire
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-return-no-fire
    // the estimate already computed by the context assembly; no result and no metric when the
    // trigger does not fire
    let input = SummaryTriggerInput {
        had_summary: plan.context.plan.thread_summary.is_some(),
        assembled_context_tokens: plan.context.assembled_context_tokens,
        effective_budget: plan.context.effective_budget,
        messages_truncated: plan.context.messages_truncated,
        compression_threshold_pct: threshold,
    };
    if evaluate_trigger(&input).is_none() {
        return Ok(None);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-return-no-fire
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-no-fire
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-base
    let base = MessageRepo::summary_of(tx, plan.tenant_id, plan.chat_id)
        .await?
        .map(|row| Frontier {
            created_at: row.frontier_created_at,
            message_id: row.frontier_message_id,
        });
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-base
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-target
    // the latest non-deleted message that does not belong to the causing turn: the latest turn
    // may still be replaced by retry, edit or delete
    let target = MessageRepo::latest_message(
        tx,
        plan.tenant_id,
        plan.chat_id,
        storage,
        Some(plan.request_id),
    )
    .await?
    .map(|latest| Frontier {
        created_at: latest.created_at,
        message_id: latest.id,
    });
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-target
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-return-not-needed
    let Some(frozen_target) = target.filter(|_| has_summary_work(base, target)) else {
        return Ok(Some(NOT_NEEDED));
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-return-not-needed
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-non-blocking
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-return-scheduled
    // the work item is enqueued here and consumed later: it never blocks or changes this turn
    wakes.push(
        outbox
            .enqueue_thread_summary(
                tx,
                &ThreadSummaryWork {
                    tenant_id: plan.tenant_id,
                    chat_id: plan.chat_id,
                    system_request_id: Uuid::new_v4(),
                    base_frontier: base,
                    frozen_target,
                },
            )
            .await?,
    );
    Ok(Some(SCHEDULED))
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-return-scheduled
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-non-blocking
}

/// The `result` of `mini_chat_thread_summary_trigger` for an enqueued work item.
const SCHEDULED: &str = "scheduled";

/// The `result` of `mini_chat_thread_summary_trigger` when there is nothing to schedule.
const NOT_NEEDED: &str = "not_needed";

#[async_trait]
impl TurnFinalizer for SqlTurnFinalizer {
    async fn finalize(
        &self,
        target: &TurnTarget,
        outcome: TurnOutcome,
    ) -> Result<TurnFinalization, DomainError> {
        // no usage and no tool calls: the settlement is derived from the terminal state and the
        // error code alone (`estimated`; the reserve moves to spent and is released)
        let update = self.update(outcome)?;
        let request = FinalizeRequest {
            tenant_id: target.tenant_id,
            turn_id: target.turn_id,
            update: update.clone(),
            finalizer: Finalizer::Stream {
                selected_model: target.selected_model.clone(),
                period_starts: target.period_starts,
            },
            usage: None,
            tool_counts: ToolCallCounts::default(),
        };
        let quota = Arc::clone(&self.quota);
        let this = self.clone();
        let audit_target = target.clone();
        let settled = self
            .quota
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let settled = quota.finalize_turn(tx, &request).await?;
                    // with the stream dependencies the turn audit event joins the transaction
                    let audit_wake = match (&this.stream, &settled) {
                        (
                            Some(deps),
                            FinalizeOutcome::NoSettlement | FinalizeOutcome::Settled(_),
                        ) => Some(
                            this.enqueue_pre_provider_audit(
                                tx,
                                deps.outbox.as_ref(),
                                &audit_target,
                                &request.update,
                            )
                            .await?,
                        ),
                        _ => None,
                    };
                    Ok((settled, audit_wake))
                })
            })
            .await?;
        // the transaction committed: the metrics of the settlement, then the outbox wakes
        let (settled, audit_wake) = settled;
        if let Some(wake) = audit_wake {
            wake.fire();
        }
        match settled {
            FinalizeOutcome::AlreadyFinalized => Ok(TurnFinalization::AlreadyFinalized),
            FinalizeOutcome::NoSettlement => Ok(TurnFinalization::Finalized),
            FinalizeOutcome::Settled(settlement) => {
                let Settlement { result, wake } = *settlement;
                result.record_after_commit(self.quota.metrics.as_ref());
                wake.fire();
                if let (Some(deps), TerminalState::Cancelled) = (&self.stream, update.state) {
                    deps.metrics.streams_aborted("client_disconnect");
                }
                Ok(TurnFinalization::Finalized)
            }
        }
    }

    async fn finalize_stream(
        &self,
        plan: &ProviderCallPlan,
        terminal: &StreamTerminal,
    ) -> Result<StreamFinalization, DomainError> {
        let deps = self.stream.as_ref().ok_or_else(|| {
            DomainError::internal("the stream finalization dependencies are not wired")
        })?;
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-store
        // the SSE-only codes `finalization_failed` and `stream_interrupted` are never stored
        if let StreamOutcome::Failed {
            code: StreamErrorCode::FinalizationFailed | StreamErrorCode::StreamInterrupted,
            ..
        } = &terminal.outcome
        {
            return Err(DomainError::internal(
                "an SSE-only error code cannot be stored on a turn",
            ));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-store
        let started = Instant::now();
        // a message is written for a completed outcome and for a cancelled one with text
        let wants_message = match terminal.outcome {
            StreamOutcome::Completed => true,
            StreamOutcome::Cancelled => !terminal.text.is_empty(),
            StreamOutcome::Failed { .. } => false,
        };
        let first = self.attempt(deps, plan, terminal, wants_message).await;
        let attempt = match first {
            Err(AttemptError::MessageInsert(cause)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-catch-message
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-retry-as-failed
                tracing::warn!(
                    turn_id = %plan.turn_id,
                    %cause,
                    "the assistant message could not be persisted"
                );
                if terminal.outcome == StreamOutcome::Cancelled {
                    // a partial message that cannot be stored: still `cancelled`, without it
                    self.attempt(deps, plan, terminal, false)
                        .await
                        .map_err(into_error)?
                } else {
                    // `completed` is never observable without its message: the turn fails with
                    // the same usage (settled `actual` when it is known)
                    let failed = StreamTerminal {
                        outcome: StreamOutcome::Failed {
                            code: StreamErrorCode::MessagePersistenceFailed,
                            detail: generic_message(StreamErrorCode::MessagePersistenceFailed)
                                .to_owned(),
                        },
                        text: String::new(),
                        ..terminal.clone()
                    };
                    self.attempt(deps, plan, &failed, false)
                        .await
                        .map_err(into_error)?
                }
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-retry-as-failed
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-catch-message
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-catch-other
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-return-failure
            // the transaction rolled back: the turn stays `running` with its reserve until the
            // orphan watchdog finalizes it
            Err(AttemptError::Other(error)) => return Err(error),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-return-failure
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-catch-other
            Ok(attempt) => attempt,
        };
        match attempt {
            Attempt::Lost => Ok(StreamFinalization::AlreadyFinalized),
            Attempt::Won(won) => {
                let committed = won.committed.clone();
                self.after_commit(deps, *won, started);
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-return
                Ok(StreamFinalization::Committed(committed))
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-finalization:p1:inst-fin-return
            }
        }
    }
}

fn into_error(error: AttemptError) -> DomainError {
    match error {
        AttemptError::MessageInsert(error) | AttemptError::Other(error) => error,
    }
}

#[cfg(test)]
#[path = "turn_finalizer_tests.rs"]
mod turn_finalizer_tests;
