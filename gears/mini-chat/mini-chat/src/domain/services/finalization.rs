//! Stream finalization (S§6.3, D§5.7 "Turn Finalization Contract"): one
//! transaction with the CAS on `chat_turns`, the assistant message, the quota
//! settlement, the usage + audit outbox events and the summary hook.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use mini_chat_sdk::{
    AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent,
    ModelTier, RequesterType, TerminalState, TurnAuditEvent, TurnAuditEventType, UsageEvent,
    UsageTokens, UserLimits,
};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::billing::{self, SettlementMethod, TurnReserve, settle_amount};
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    OutboxPort, PendingWakes, SummaryHook, SummaryHookInput, SummaryTriggerInput,
    SummaryTriggerResult,
};
use crate::domain::services::QuotaService;
use crate::domain::services::quota_service::{
    PeriodStarts, QuotaDecision, QuotaWarning, SettleInput,
};
use crate::infra::db::entity::message;
use crate::infra::db::repos::{MessageRepo, TurnRepo, TurnTerminal};
use crate::infra::db::tx::with_retry;
use crate::infra::metrics::MiniChatMetrics;

/// Error code of a completed stream whose assistant message could not be
/// persisted.
pub const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";

/// How the provider stream ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Terminal {
    /// `response.completed`, or `response.incomplete` with its reason.
    Completed {
        text: String,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    /// Provider error, tool-limit breach, unexpected tool use.
    Failed {
        code: String,
        /// Sanitized message (stored as `error_detail`).
        detail: String,
        usage: Option<UsageTokens>,
    },
    /// Client disconnect.
    Cancelled { partial_text: String },
}

/// Tool calls completed during the turn (`done` events).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCounts {
    pub web_search: u32,
    pub code_interpreter: u32,
    pub file_search: u32,
}

/// Everything the finalization of one turn needs (persisted preflight
/// values of the turn plus the stream outcome).
#[derive(Debug, Clone)]
pub struct FinalizeInput {
    pub turn_id: Uuid,
    pub tenant: Uuid,
    pub user: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub terminal: Terminal,
    pub assistant_message_id: Uuid,
    pub effective_model: String,
    /// `chats.model`.
    pub selected_model: String,
    pub effective_tier: ModelTier,
    /// Credit multipliers of the effective model in `policy_version`.
    pub multipliers: (i64, i64),
    pub periods: PeriodStarts,
    pub reserve: TurnReserve,
    pub policy_version: u64,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<&'static str>,
    pub tool_counts: ToolCounts,
    pub summary_trigger: SummaryTriggerInput,
    pub latency: AuditLatency,
    /// Limits of `policy_version` (quota warnings of a completed turn).
    pub limits: UserLimits,
    /// `chat_turns.started_at` (= the user message's `created_at`).
    pub started_at: OffsetDateTime,
}

/// Result of [`FinalizationService::finalize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    /// This finalizer won the CAS and committed `committed_state`
    /// (`Failed` also when a completed stream's message could not be
    /// persisted). `quota_warnings` is filled for completed turns only.
    Won {
        committed_state: TerminalState,
        quota_warnings: Vec<QuotaWarning>,
    },
    /// Another finalizer already moved the turn out of `running`.
    Lost,
}

/// Values written by one finalization attempt (built before the transaction).
#[derive(Clone)]
struct Plan {
    terminal: TurnTerminal,
    message: Option<message::Model>,
    settle: SettleInput,
    usage_event: UsageEvent,
    audit_event: MiniChatAuditEvent,
    summary: Option<SummaryHookInput>,
    state: TerminalState,
}

/// Stream finalization (CAS winner writes settlement + outbox; losers no-op).
pub struct FinalizationService {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    quota: Arc<QuotaService>,
    outbox: Arc<dyn OutboxPort>,
    summary: Arc<dyn SummaryHook>,
    config: Arc<MiniChatConfig>,
    metrics: Arc<MiniChatMetrics>,
}

impl FinalizationService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        clock: Arc<dyn Clock>,
        quota: Arc<QuotaService>,
        outbox: Arc<dyn OutboxPort>,
        summary: Arc<dyn SummaryHook>,
        config: Arc<MiniChatConfig>,
        metrics: Arc<MiniChatMetrics>,
    ) -> Self {
        Self {
            db,
            clock,
            quota,
            outbox,
            summary,
            config,
            metrics,
        }
    }

    /// Finalize a turn in one transaction (CAS → assistant message →
    /// settlement → usage + audit events → summary hook). Outbox wakes fire
    /// after commit.
    ///
    /// A completed stream whose assistant message cannot be inserted is
    /// finalized as `failed` (`message_persistence_failed`); a cancelled
    /// turn whose partial message cannot be inserted is finalized
    /// `cancelled` without a message (best effort).
    ///
    /// # Errors
    /// Database / outbox / credit computation failures; the turn then stays
    /// `running` (the orphan watchdog finalizes it later).
    pub async fn finalize(&self, f: FinalizeInput) -> Result<FinalizeOutcome, DomainError> {
        let started = Instant::now();
        let result = self.finalize_turn(f).await;
        self.metrics.finalization_latency(started.elapsed());
        result
    }

    async fn finalize_turn(&self, f: FinalizeInput) -> Result<FinalizeOutcome, DomainError> {
        let plan = self.plan(&f, &f.terminal)?;
        let message_failed = Arc::new(AtomicBool::new(false));
        match self.commit(plan, Arc::clone(&message_failed)).await {
            Ok(outcome) => self.with_warnings(&f, outcome).await,
            Err(e) if message_failed.load(Ordering::SeqCst) => {
                let Some(retry) = without_message(&f, &e) else {
                    return Err(e);
                };
                let plan = self.plan(&f, &retry)?;
                let outcome = self.commit(plan, Arc::new(AtomicBool::new(false))).await?;
                self.with_warnings(&f, outcome).await
            }
            Err(e) => Err(e),
        }
    }

    /// Run one finalization transaction; fire the wakes after commit.
    async fn commit(
        &self,
        plan: Plan,
        message_failed: Arc<AtomicBool>,
    ) -> Result<FinalizeOutcome, DomainError> {
        let quota = Arc::clone(&self.quota);
        let outbox = Arc::clone(&self.outbox);
        let summary = Arc::clone(&self.summary);
        let scope = AccessScope::for_tenant(plan.usage_event.tenant_id);
        let turn_id = plan.usage_event.turn_id.unwrap_or_default();
        let (settle, model) = (plan.settle, plan.usage_event.effective_model.clone());
        let result = with_retry(&self.db, move |tx| {
            let (quota, outbox, summary, scope, plan, message_failed) = (
                Arc::clone(&quota),
                Arc::clone(&outbox),
                Arc::clone(&summary),
                scope.clone(),
                plan.clone(),
                Arc::clone(&message_failed),
            );
            // Per attempt: only the attempt that fails reports the message.
            message_failed.store(false, Ordering::SeqCst);
            Box::pin(async move {
                let won = TurnRepo
                    .finalize_cas(tx, &scope, turn_id, &plan.terminal)
                    .await?;
                if won == 0 {
                    return Ok(None);
                }
                if let Some(msg) = plan.message
                    && let Err(e) = MessageRepo.insert(tx, &scope, msg).await
                {
                    message_failed.store(true, Ordering::SeqCst);
                    return Err(DomainError::from(e));
                }
                quota.settle(tx, &plan.settle).await?;
                let mut wakes = PendingWakes::new();
                outbox
                    .enqueue_usage(tx, &plan.usage_event, &mut wakes)
                    .await?;
                outbox
                    .enqueue_audit(tx, &plan.audit_event, &mut wakes)
                    .await?;
                let trigger = match &plan.summary {
                    Some(input) => summary.maybe_enqueue(tx, input, &mut wakes).await?,
                    None => SummaryTriggerResult::NotEvaluated,
                };
                Ok(Some((plan.state, wakes, trigger)))
            })
        })
        .await?;
        Ok(match result {
            Some((state, wakes, trigger)) => {
                wakes.fire_all();
                if let Some(result) = trigger.label() {
                    debug!(%turn_id, result, "thread summary trigger evaluated");
                    self.metrics.summary_trigger(result);
                }
                if settle.method == SettlementMethod::Actual {
                    let s = settle.settlement;
                    self.metrics.quota_actual_settlement(
                        s.actual_tokens_for_telemetry.map(|(i, o)| i + o),
                        s.overshoot,
                        &model,
                        settle.code_interpreter_calls,
                    );
                }
                FinalizeOutcome::Won {
                    committed_state: state,
                    quota_warnings: Vec::new(),
                }
            }
            None => FinalizeOutcome::Lost,
        })
    }

    /// Add the quota warnings of a committed completed turn (read after
    /// commit; a read failure only drops the warnings).
    async fn with_warnings(
        &self,
        f: &FinalizeInput,
        outcome: FinalizeOutcome,
    ) -> Result<FinalizeOutcome, DomainError> {
        let FinalizeOutcome::Won {
            committed_state: TerminalState::Completed,
            ..
        } = outcome
        else {
            return Ok(outcome);
        };
        let warnings = match self.db.conn() {
            Ok(conn) => self
                .quota
                .warnings(&conn, f.tenant, f.user, &f.limits, self.clock.now())
                .await
                .unwrap_or_else(|e| {
                    warn!(turn_id = %f.turn_id, error = %e, "quota warnings unavailable");
                    Vec::new()
                }),
            Err(e) => {
                warn!(turn_id = %f.turn_id, error = %e, "quota warnings unavailable");
                Vec::new()
            }
        };
        Ok(FinalizeOutcome::Won {
            committed_state: TerminalState::Completed,
            quota_warnings: warnings,
        })
    }

    /// Build the rows / events of `terminal` for turn `f`.
    fn plan(&self, f: &FinalizeInput, terminal: &Terminal) -> Result<Plan, DomainError> {
        let now = self.clock.now();
        let (state, error_code, error_detail, usage, response_id, message_text) = match terminal {
            Terminal::Completed {
                text,
                usage,
                response_id,
                incomplete_reason,
            } => {
                if let Some(reason) = incomplete_reason {
                    warn!(turn_id = %f.turn_id, reason = %reason, "stream incomplete");
                }
                (
                    TerminalState::Completed,
                    None,
                    None,
                    *usage,
                    response_id.clone(),
                    Some(text.clone()),
                )
            }
            Terminal::Failed {
                code,
                detail,
                usage,
            } => (
                TerminalState::Failed,
                Some(code.clone()),
                Some(detail.clone()),
                *usage,
                None,
                None,
            ),
            Terminal::Cancelled { partial_text } => (
                TerminalState::Cancelled,
                None,
                None,
                None,
                None,
                (!partial_text.is_empty()).then(|| partial_text.clone()),
            ),
        };

        let (outcome, method) = billing::derive(state, error_code.as_deref(), usage.as_ref());
        let settlement = settle_amount(
            method,
            &f.reserve,
            usage.as_ref(),
            self.config.quota.overshoot_tolerance_factor,
            f.multipliers,
        )
        .map_err(|e| DomainError::Internal(format!("settlement credits: {e}")))?;
        if settlement.overshoot {
            warn!(turn_id = %f.turn_id, "actual usage exceeds the reserve");
        }

        let message = message_text.map(|content| {
            let u = usage.unwrap_or_default();
            assistant_message(f, content, &u, response_id.clone(), now)
        });
        let terminal_row = TurnTerminal {
            state: state_str(state),
            error_code: error_code.clone(),
            error_detail,
            assistant_message_id: message.as_ref().map(|m| m.id),
            provider_response_id: response_id,
            now,
        };
        let reported_usage =
            (method == SettlementMethod::Actual).then(|| usage.unwrap_or_default());
        let usage_event = UsageEvent {
            tenant_id: f.tenant,
            user_id: Some(f.user),
            chat_id: f.chat_id,
            turn_id: Some(f.turn_id),
            request_id: f.request_id,
            effective_model: f.effective_model.clone(),
            selected_model: f.selected_model.clone(),
            terminal_state: state,
            billing_outcome: outcome,
            usage: reported_usage,
            actual_credits_micro: settlement.committed_credits_micro,
            settlement_method: method,
            policy_version_applied: f.policy_version,
            web_search_calls: f.tool_counts.web_search,
            code_interpreter_calls: f.tool_counts.code_interpreter,
            file_search_calls: f.tool_counts.file_search,
            timestamp: now,
            requester_type: RequesterType::User,
            dedupe_key: dedupe_key(f.tenant, f.turn_id, f.request_id),
            system_task_type: None,
        };
        let downgraded = f.decision == QuotaDecision::Downgrade;
        let audit_event = MiniChatAuditEvent::Turn(TurnAuditEvent {
            event_type: if state == TerminalState::Completed {
                TurnAuditEventType::TurnCompleted
            } else {
                TurnAuditEventType::TurnFailed
            },
            tenant_id: f.tenant,
            user_id: f.user,
            chat_id: f.chat_id,
            turn_id: f.turn_id,
            request_id: f.request_id,
            selected_model: f.selected_model.clone(),
            effective_model: f.effective_model.clone(),
            terminal_state: state,
            error_code,
            usage,
            latency_ms: f.latency,
            tool_calls: AuditToolCalls {
                web_search_calls: f.tool_counts.web_search,
                file_search_calls: f.tool_counts.file_search,
            },
            policy_decisions: AuditPolicyDecisions {
                quota: AuditQuotaDecision {
                    decision: f.decision.as_str().to_owned(),
                    downgrade_from: downgraded.then(|| f.selected_model.clone()),
                    downgrade_reason: f.downgrade_reason.map(str::to_owned),
                },
                license: None,
            },
            prompt: String::new(),
            response: String::new(),
            attachments: Vec::new(),
            quota_scope: None,
            trace_id: None,
            timestamp: now,
        });
        let summary = (state == TerminalState::Completed).then_some(SummaryHookInput {
            tenant_id: f.tenant,
            chat_id: f.chat_id,
            turn_id: f.turn_id,
            request_id: f.request_id,
            trigger: f.summary_trigger,
        });
        Ok(Plan {
            terminal: terminal_row,
            message,
            settle: SettleInput {
                tenant: f.tenant,
                user: f.user,
                effective_tier: f.effective_tier,
                periods: f.periods,
                turn: f.reserve,
                method,
                settlement,
                web_search_calls: f.tool_counts.web_search,
                code_interpreter_calls: f.tool_counts.code_interpreter,
            },
            usage_event,
            audit_event,
            summary,
            state,
        })
    }
}

/// Terminal of the second attempt after the assistant message could not be
/// inserted: a completed stream becomes `failed`
/// (`message_persistence_failed`), a cancelled one keeps no message.
fn without_message(f: &FinalizeInput, e: &DomainError) -> Option<Terminal> {
    match &f.terminal {
        Terminal::Completed { usage, .. } => {
            error!(turn_id = %f.turn_id, error = %e, "assistant message persistence failed; finalizing as failed");
            Some(Terminal::Failed {
                code: MESSAGE_PERSISTENCE_FAILED.to_owned(),
                detail: "The response could not be saved".to_owned(),
                usage: *usage,
            })
        }
        Terminal::Cancelled { .. } => {
            warn!(turn_id = %f.turn_id, error = %e, "partial assistant message not persisted");
            Some(Terminal::Cancelled {
                partial_text: String::new(),
            })
        }
        Terminal::Failed { .. } => None,
    }
}

/// `dedupe_key` of a turn's usage event (`{tenant_id}/{turn_id}/{request_id}`,
/// D§5.7), shared with the orphan finalization.
pub(crate) fn dedupe_key(tenant: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant.simple(),
        turn_id.simple(),
        request_id.simple()
    )
}

const fn state_str(state: TerminalState) -> &'static str {
    match state {
        TerminalState::Completed => "completed",
        TerminalState::Failed => "failed",
        TerminalState::Cancelled => "cancelled",
    }
}

/// The assistant message row of a turn. `created_at` stays after the user
/// message (`started_at`) even when the clock did not advance.
fn assistant_message(
    f: &FinalizeInput,
    content: String,
    usage: &UsageTokens,
    response_id: Option<String>,
    now: OffsetDateTime,
) -> message::Model {
    message::Model {
        id: f.assistant_message_id,
        tenant_id: f.tenant,
        chat_id: f.chat_id,
        request_id: Some(f.request_id),
        role: "assistant".to_owned(),
        content,
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: response_id,
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!([]),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        model: Some(f.effective_model.clone()),
        is_compressed: false,
        created_at: now.max(f.started_at + time::Duration::microseconds(1)),
        deleted_at: None,
    }
}
