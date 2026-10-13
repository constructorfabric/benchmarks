//! Stream turn finalization (DESIGN section 5.7 "Turn Finalization Contract",
//! "`FinalizeTurn` Invariant", "Terminal SSE Event Emission Guard", "Content
//! durability invariant", "Usage accounting rules per outcome"; section 3.6
//! "Thread Summary" durable scheduling).
//!
//! Every terminal path of a streamed turn (completed / incomplete, provider
//! failure, adapter failure before the outbound call, tool-limit breach,
//! client disconnect) runs one transaction (the reserve release of a
//! pre-provider failure included, section 5.8 "Pre-Provider Failure
//! Handling"):
//!
//! 1. the CAS `UPDATE chat_turns ... WHERE id = ? AND state = 'running'`,
//!    the first statement (it takes the write lock on `SQLite`); no row
//!    returned means another finalizer won and nothing else is written;
//! 2. the assistant message (completed: always, even when empty; cancelled:
//!    only with non-empty partial text);
//! 3. the quota settlement ([`QuotaService::settle_in_tx`], billing derived by
//!    [`crate::domain::billing::derive_billing`]) from the persisted turn row;
//! 4. the usage event and the turn audit event;
//! 5. for a completed turn whose context triggered a summary: the
//!    thread-summary work item (frozen target = latest live message outside
//!    the causing turn; skipped without such a message or when the target
//!    equals the stored frontier).
//!
//! The outbox wake fires after the commit. When the assistant message cannot
//! be inserted, the transaction rolls back and is run again as `failed`
//! (`message_persistence_failed`) for a completed turn, or as `cancelled`
//! without a message for a cancelled turn (best effort).

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, UsageTokens};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_db::{Db, DbError};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::events::MESSAGE_PERSISTENCE_FAILED;
use crate::domain::billing::{
    TURN_SETUP_FAILED, TerminalState, TurnAuditInput, TurnIdentity, TurnUsageInput,
    turn_audit_event, turn_usage_event,
};
use crate::domain::error::DomainError;
use crate::domain::llm::LlmUsage;
use crate::domain::ports::{
    FinalizedTurnRecord, NewAssistantMessage, QuotaOwner, Repositories, ThreadSummaryTask,
    ToolCallCounts, TurnFinalization, storage_now,
};
use crate::domain::quota_service::{PreflightDecision, QuotaService, SettlementInput};

/// Everything finalization needs to know about a running turn.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub chat_id: Uuid,
    pub tenant_id: Uuid,
    /// Requesting user.
    pub user_id: Uuid,
    /// PEP scope of the parent chat.
    pub scope: AccessScope,
    /// Pre-allocated assistant message id.
    pub assistant_message_id: Uuid,
    /// `chats.model`.
    pub selected_model: String,
    /// The preflight decision the reserve was booked with.
    pub decision: PreflightDecision,
    /// The context of the turn triggered a thread summary (proactive or
    /// urgent, worker enabled).
    pub summary_due: bool,
    /// Trace id of the originating request, for the audit event.
    pub trace_id: Option<String>,
}

impl TurnContext {
    /// `(tenant, requesting user)` of the turn's quota rows.
    #[must_use]
    pub const fn owner(&self) -> QuotaOwner {
        QuotaOwner {
            tenant_id: self.tenant_id,
            user_id: self.user_id,
        }
    }
}

/// How the provider phase of a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutcome {
    /// `response.completed` or `response.incomplete`.
    Completed {
        text: String,
        usage: Option<LlmUsage>,
        response_id: Option<String>,
    },
    /// Provider failure or tool-limit breach. `code` is the streaming error
    /// code, also stored as `chat_turns.error_code` when the request was
    /// sent; an adapter failure before any outbound call (`request_sent =
    /// false`) is stored as `turn_setup_failed` without usage, so the
    /// settlement releases the reserve (`released`, zero charge).
    Failed {
        code: String,
        message: String,
        usage: Option<LlmUsage>,
        request_sent: bool,
    },
    /// Client disconnect; `text` is the partial answer.
    Cancelled { text: String },
}

/// Tool calls of a streamed turn at finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnToolCalls {
    /// Completed calls, persisted in `chat_turns.*_completed_count`.
    pub counts: ToolCallCounts,
    /// `file_search_calls` of the usage and audit events: the in-memory
    /// count of provider `file_search` completions and `search_knowledge`
    /// retrievals, failed retrievals included (DESIGN section 4 "Knowledge
    /// Search", "Accounting").
    pub file_search_calls: u32,
}

/// Result of a finalization attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finalized {
    /// This finalizer won the CAS and the transaction committed.
    Committed {
        terminal: TerminalState,
        error_code: Option<String>,
    },
    /// The turn was no longer running: nothing was written.
    Lost,
}

/// Provider usage in the SDK form (all counters).
#[must_use]
pub fn usage_tokens(usage: &LlmUsage) -> UsageTokens {
    UsageTokens {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    }
}

/// What one finalization transaction writes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    terminal: TerminalState,
    error_code: Option<String>,
    /// Assistant message content to persist.
    message: Option<String>,
    usage: Option<UsageTokens>,
    provider_response_id: Option<String>,
    enqueue_summary: bool,
}

impl Plan {
    fn of(outcome: &StreamOutcome, summary_due: bool) -> Self {
        match outcome {
            StreamOutcome::Completed {
                text,
                usage,
                response_id,
            } => Self {
                terminal: TerminalState::Completed,
                error_code: None,
                message: Some(text.clone()),
                usage: usage.as_ref().map(usage_tokens),
                provider_response_id: response_id.clone(),
                enqueue_summary: summary_due,
            },
            StreamOutcome::Failed {
                code,
                usage,
                request_sent: true,
                ..
            } => Self {
                terminal: TerminalState::Failed,
                error_code: Some(code.clone()),
                message: None,
                usage: usage.as_ref().map(usage_tokens),
                provider_response_id: None,
                enqueue_summary: false,
            },
            StreamOutcome::Failed {
                request_sent: false,
                ..
            } => Self {
                terminal: TerminalState::Failed,
                error_code: Some(TURN_SETUP_FAILED.to_owned()),
                message: None,
                usage: None,
                provider_response_id: None,
                enqueue_summary: false,
            },
            StreamOutcome::Cancelled { text } => Self {
                terminal: TerminalState::Cancelled,
                error_code: None,
                message: (!text.is_empty()).then(|| text.clone()),
                usage: None,
                provider_response_id: None,
                enqueue_summary: false,
            },
        }
    }

    /// The plan to commit when the assistant message could not be inserted.
    fn without_message(self) -> Self {
        match self.terminal {
            TerminalState::Completed => Self {
                terminal: TerminalState::Failed,
                error_code: Some(MESSAGE_PERSISTENCE_FAILED.to_owned()),
                message: None,
                enqueue_summary: false,
                ..self
            },
            TerminalState::Failed | TerminalState::Cancelled => Self {
                message: None,
                ..self
            },
        }
    }
}

/// Failure of a finalization transaction.
#[derive(Debug)]
enum TxError {
    /// The assistant message insert failed.
    Message(DomainError),
    /// Any other failure (CAS, settlement, outbox, commit).
    Other(DomainError),
}

impl From<DbError> for TxError {
    fn from(err: DbError) -> Self {
        Self::Other(DomainError::from(err))
    }
}

impl From<DomainError> for TxError {
    fn from(err: DomainError) -> Self {
        Self::Other(err)
    }
}

/// Runs the finalization transaction of streamed turns.
#[derive(Clone)]
pub struct Finalizer {
    db: Db,
    repos: Repositories,
    quota: Arc<QuotaService>,
}

impl std::fmt::Debug for Finalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Finalizer").finish_non_exhaustive()
    }
}

impl Finalizer {
    /// Finalizer over the gear database, the persistence ports and the
    /// quota service.
    #[must_use]
    pub const fn new(db: Db, repos: Repositories, quota: Arc<QuotaService>) -> Self {
        Self { db, repos, quota }
    }

    /// Finalizes `turn` with `outcome` and its tool calls `calls` (module
    /// docs).
    ///
    /// # Errors
    ///
    /// The [`DomainError`] of a failed finalization transaction; the turn
    /// then stays `running` for the orphan watchdog.
    pub async fn finalize(
        &self,
        turn: &Arc<TurnContext>,
        outcome: &StreamOutcome,
        calls: TurnToolCalls,
    ) -> Result<Finalized, DomainError> {
        let plan = Plan::of(outcome, turn.summary_due);
        match self.run(turn, plan.clone(), calls).await {
            Ok(finalized) => Ok(finalized),
            Err(TxError::Other(err)) => Err(err),
            Err(TxError::Message(err)) => {
                tracing::warn!(
                    target: "mini_chat.stream",
                    turn_id = %turn.turn_id,
                    terminal = plan.terminal.as_str(),
                    error = %err,
                    "assistant message could not be persisted; finalizing without it"
                );
                match self.run(turn, plan.without_message(), calls).await {
                    Ok(finalized) => Ok(finalized),
                    Err(TxError::Other(err) | TxError::Message(err)) => Err(err),
                }
            }
        }
    }

    async fn run(
        &self,
        turn: &Arc<TurnContext>,
        plan: Plan,
        calls: TurnToolCalls,
    ) -> Result<Finalized, TxError> {
        let turn = Arc::clone(turn);
        let repos = self.repos.clone();
        let quota = Arc::clone(&self.quota);
        let (finalized, wake) = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let unit = FinalizationUnit {
                        tx,
                        repos: &repos,
                        quota: &quota,
                        turn: &turn,
                    };
                    unit.execute(plan, calls).await
                })
            })
            .await?;
        wake.fire();
        Ok(finalized)
    }
}

/// One finalization transaction.
struct FinalizationUnit<'t, 'a> {
    tx: &'t DbTx<'a>,
    repos: &'t Repositories,
    quota: &'t QuotaService,
    turn: &'t TurnContext,
}

impl FinalizationUnit<'_, '_> {
    async fn execute(
        &self,
        plan: Plan,
        calls: TurnToolCalls,
    ) -> Result<(Finalized, Wake), TxError> {
        let now = storage_now();
        let turn = self.turn;
        let assistant_message_id = plan.message.as_ref().map(|_| turn.assistant_message_id);
        let Some(row) = self
            .repos
            .turns
            .finalize_in_tx(
                self.tx,
                &turn.scope,
                TurnFinalization {
                    turn_id: turn.turn_id,
                    state: plan.terminal.turn_state(),
                    error_code: plan.error_code.clone(),
                    assistant_message_id,
                    provider_response_id: plan.provider_response_id.clone(),
                    counts: calls.counts,
                    at: now,
                },
            )
            .await?
        else {
            return Ok((Finalized::Lost, Wake::empty()));
        };

        if let Some(content) = &plan.message {
            self.repos
                .messages
                .insert_assistant_in_tx(
                    self.tx,
                    &turn.scope,
                    NewAssistantMessage {
                        id: turn.assistant_message_id,
                        tenant_id: row.tenant_id,
                        chat_id: row.chat_id,
                        request_id: row.request_id,
                        content: content.clone(),
                        model: effective_model(turn, &row),
                        usage: plan.usage.unwrap_or_default(),
                        provider_response_id: plan.provider_response_id.clone(),
                        created_at: now,
                    },
                )
                .await
                .map_err(TxError::Message)?;
        }

        let settlement = settlement_input(turn, &row, &plan);
        let settled = self.quota.settle_in_tx(self.tx, &settlement).await?;
        let identity = TurnIdentity {
            tenant_id: row.tenant_id,
            user_id: settlement.owner.user_id,
            chat_id: row.chat_id,
            turn_id: row.id,
            request_id: row.request_id,
        };
        let usage_event = turn_usage_event(TurnUsageInput {
            identity,
            selected_model: turn.selected_model.clone(),
            effective_model: settlement.effective_model.clone(),
            terminal: plan.terminal,
            billing: settled.billing,
            usage: plan.usage,
            actual_credits_micro: settled.committed_credits_micro,
            policy_version_applied: settlement.policy_version_applied,
            web_search_calls: row.counts.web_search,
            code_interpreter_calls: row.counts.code_interpreter,
            file_search_calls: calls.file_search_calls,
            timestamp: now,
        });
        let audit_event = AuditEvent::from(turn_audit_event(TurnAuditInput {
            identity,
            selected_model: turn.selected_model.clone(),
            effective_model: settlement.effective_model.clone(),
            terminal: plan.terminal,
            error_code: plan.error_code.clone(),
            usage: plan.usage,
            latency_ms: latency_ms(row.started_at, now),
            web_search_calls: row.counts.web_search,
            file_search_calls: calls.file_search_calls,
            quota_decision: turn.decision.audit_quota_decision(),
            trace_id: turn.trace_id.clone(),
            timestamp: now,
        }));

        let mut wake = self
            .repos
            .outbox
            .enqueue_usage_in_tx(self.tx, &usage_event)
            .await?;
        match self.enqueue_rest(&audit_event, &plan).await {
            Ok(rest) => wake += rest,
            Err(err) => {
                wake.discard();
                return Err(err);
            }
        }
        Ok((
            Finalized::Committed {
                terminal: plan.terminal,
                error_code: plan.error_code,
            },
            wake,
        ))
    }

    /// The audit event and, when due, the thread-summary work item.
    async fn enqueue_rest(&self, audit: &AuditEvent, plan: &Plan) -> Result<Wake, TxError> {
        let mut wake = self
            .repos
            .outbox
            .enqueue_audit_in_tx(self.tx, audit)
            .await?;
        if plan.enqueue_summary {
            match self.thread_summary_task().await {
                Ok(Some(task)) => {
                    match self
                        .repos
                        .outbox
                        .enqueue_thread_summary_in_tx(self.tx, &task)
                        .await
                    {
                        Ok(summary) => wake += summary,
                        Err(err) => {
                            wake.discard();
                            return Err(err.into());
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    wake.discard();
                    return Err(err.into());
                }
            }
        }
        Ok(wake)
    }

    /// The thread-summary work item of the causing turn, or `None` when no
    /// earlier message exists or the target equals the stored frontier.
    async fn thread_summary_task(&self) -> Result<Option<ThreadSummaryTask>, DomainError> {
        let turn = self.turn;
        let base = self
            .repos
            .messages
            .summary_frontier_in_tx(self.tx, &turn.scope, turn.chat_id)
            .await?;
        let target = self
            .repos
            .messages
            .latest_key_excluding_request_in_tx(self.tx, &turn.scope, turn.chat_id, turn.request_id)
            .await?;
        Ok(target
            .filter(|target| Some(*target) != base)
            .map(|frozen_target| ThreadSummaryTask {
                tenant_id: turn.tenant_id,
                chat_id: turn.chat_id,
                system_request_id: Uuid::new_v4(),
                base_frontier: base,
                frozen_target,
            }))
    }
}

fn effective_model(turn: &TurnContext, row: &FinalizedTurnRecord) -> String {
    row.effective_model
        .clone()
        .unwrap_or_else(|| turn.decision.effective_model_id().to_owned())
}

/// The settlement of the finalized row; preflight columns the send path
/// always writes fall back to the turn's preflight decision.
fn settlement_input(turn: &TurnContext, row: &FinalizedTurnRecord, plan: &Plan) -> SettlementInput {
    let decision = &turn.decision;
    SettlementInput {
        owner: QuotaOwner {
            tenant_id: row.tenant_id,
            user_id: row.requester_user_id.unwrap_or(turn.user_id),
        },
        effective_model: effective_model(turn, row),
        policy_version_applied: row
            .policy_version_applied
            .and_then(|version| u64::try_from(version).ok())
            .unwrap_or(decision.policy_version_applied),
        reserve_tokens: row.reserve_tokens.unwrap_or(decision.reserve_tokens),
        max_output_tokens_applied: row
            .max_output_tokens_applied
            .map_or_else(|| i64::from(decision.max_output_tokens_applied), i64::from),
        reserved_credits_micro: row
            .reserved_credits_micro
            .unwrap_or(decision.reserved_credits_micro),
        minimal_generation_floor_applied: row.minimal_generation_floor_applied.map_or_else(
            || i64::from(decision.minimal_generation_floor_applied),
            i64::from,
        ),
        periods: decision.periods,
        terminal: plan.terminal,
        error_code: plan.error_code.clone(),
        usage: plan.usage,
        web_search_calls: row.counts.web_search,
        code_interpreter_calls: row.counts.code_interpreter,
    }
}

fn latency_ms(started_at: OffsetDateTime, now: OffsetDateTime) -> u64 {
    u64::try_from((now - started_at).whole_milliseconds()).unwrap_or(0)
}

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;
