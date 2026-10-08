//! Turn finalization (DESIGN section 5.7 "Turn Finalization Contract", 5.8,
//! 5.9, section 4 "Orphan Turn Watchdog", section 3.6 thread summary trigger).
//!
//! Every terminal path of a turn that took a quota reserve runs one database
//! transaction that performs the CAS state transition, the quota settlement
//! and the outbox enqueues (usage, audit and, for completed turns, the thread
//! summary task). The CAS (`... WHERE id = ? AND state = 'running'`) is the
//! only arbitration between competing finalizers: the loser (`rows_affected =
//! 0`) does nothing else.
//!
//! # Statement order inside the transaction (Ruling R5)
//!
//! 1. CAS `UPDATE chat_turns` — the first statement, so `SQLite` takes the
//!    write lock before any read (no `BUSY_SNAPSHOT`), and it doubles as the
//!    "won?" check.
//! 2. Assistant message `INSERT` (completed; best-effort for cancelled with
//!    text). It is in the same transaction as the CAS, so a `completed` state
//!    is never observable without its message (content durability, forbidden
//!    pattern 1).
//! 3. Quota settlement (`QuotaService::settle_in_tx`).
//! 4. Outbox enqueues (usage, audit, thread summary).
//!
//! If the message insert fails, the closure returns the error and the whole
//! transaction rolls back (the turn is `running` again). A second transaction
//! then finalizes the turn as `failed` / `message_persistence_failed`
//! (completed) or as `cancelled` without a message (cancelled, best-effort),
//! with the same settlement and outbox steps; its CAS again guarantees
//! exactly-once. Outbox wakes are fired after commit.
//!
//! The policy snapshot (multipliers and tier of the effective model at the
//! turn's `policy_version_applied`) is fetched before the transaction opens.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mini_chat_sdk::{
    MiniChatAuditEvent, ModelCatalogEntry, ModelTier, PolicyDecisions, QuotaPolicyDecision,
    ToolCalls, TurnAuditEvent, UsageEvent, UsageTokens,
};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use uuid::Uuid;

use crate::domain::billing::{
    BillingOutcome, SettlementMethod, TurnTerminalState, derive_billing, is_known_error_code,
};
use crate::domain::enums::{MessageRole, RequesterType, TurnState};
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyProvider;
use crate::domain::services::quota_service::{
    PeriodStarts, QuotaService, QuotaWarningView, SettlementInput,
};
use crate::domain::time::{db_now, db_ts};
use crate::infra::db::entities::chat_turn;
use crate::infra::db::repos::message_repo::{self, NewMessage};
use crate::infra::db::repos::turn_repo::{self, ORPHAN_TIMEOUT, TerminalUpdate};
use crate::infra::outbox::MiniChatOutbox;
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// `chat_turns.error_code` when the assistant message of a completed stream
/// could not be persisted.
pub const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";
const THREAD_SUMMARY_TASK: &str = "thread_summary_update";
const AUDIT_TURN_COMPLETED: &str = "turn_completed";
const AUDIT_TURN_FAILED: &str = "turn_failed";
/// Quota decision recorded by the orphan watchdog (the preflight decision is
/// not persisted).
const QUOTA_DECISION_UNKNOWN: &str = "unknown";

// ── Public types ─────────────────────────────────────────────────────────────

/// How the stream of a turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// Provider `response.completed` or `response.incomplete`
    /// (`incomplete_reason` set; logged only, never stored as an error code).
    Completed {
        text: String,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    /// Terminal error. `partial_text` is not persisted.
    Failed {
        error_code: String,
        error_detail: Option<String>,
        usage: Option<UsageTokens>,
        partial_text: String,
    },
    /// Client disconnect. Non-empty `partial_text` is persisted best-effort.
    Cancelled { partial_text: String },
}

/// Completed tool calls of the turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolCounts {
    pub web_search: u32,
    pub code_interpreter: u32,
    pub file_search: u32,
}

/// Thread summary trigger decision computed at context assembly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SummaryCandidate {
    pub trigger: bool,
}

/// Stream finalization input (values persisted on `chat_turns` at preflight
/// plus the stream's terminal outcome).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizeInput {
    pub turn_id: Uuid,
    pub chat_id: Uuid,
    pub tenant_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub policy_version: u64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub max_output_tokens_applied: i64,
    pub minimal_generation_floor_applied: i64,
    pub periods: PeriodStarts,
    /// Preflight's tier of the effective model. Settlement uses the tier of
    /// the snapshot entry (same snapshot version); a mismatch is logged.
    pub premium: bool,
    pub assistant_message_id: Uuid,
    pub outcome: TerminalOutcome,
    pub tool_counts: ToolCounts,
    pub quota_decision: QuotaPolicyDecision,
    pub latency_ms: u64,
    pub summary_candidate: Option<SummaryCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizeResult {
    /// This finalizer won the CAS (only then may a terminal SSE event be sent).
    pub won: bool,
    /// The committed state: this finalizer's when `won` (`failed` after a
    /// message persistence failure), else the state another finalizer left.
    pub state: TurnState,
    /// `quota_warnings` for the SSE `done` event (won and completed only;
    /// `None` when they could not be computed).
    pub quota_warnings: Option<Vec<QuotaWarningView>>,
}

// ── Transaction plan ─────────────────────────────────────────────────────────

/// The guarded update that opens the finalization transaction.
#[derive(Clone, Debug)]
enum Cas {
    Stream(TerminalUpdate),
    Orphan { cutoff: OffsetDateTime },
}

/// Everything one finalization transaction writes, computed before it opens.
#[derive(Clone, Debug)]
struct Plan {
    tenant_id: Uuid,
    chat_id: Uuid,
    turn_id: Uuid,
    request_id: Uuid,
    user_id: Option<Uuid>,
    requester_type: String,
    cas: Cas,
    state: TurnState,
    error_code: Option<String>,
    message: Option<NewMessage>,
    /// `None`: settlement skipped (orphan without reserve fields).
    settlement: Option<SettlementInput>,
    billing: (BillingOutcome, SettlementMethod),
    /// `usage` of the usage and audit events.
    event_usage: Option<UsageTokens>,
    selected_model: String,
    effective_model: String,
    policy_version: u64,
    tools: ToolCounts,
    quota_decision: QuotaPolicyDecision,
    latency_ms: Option<u64>,
    trace_id: Option<String>,
    summary: bool,
    now: OffsetDateTime,
}

enum TxOutcome {
    Won(Wake),
    Lost(Option<TurnState>),
}

enum Attempt {
    Done(TxOutcome),
    /// The assistant message insert failed; the transaction rolled back.
    MessageFailed,
}

/// The terminal outcome reduced to what the CAS and the events need.
#[derive(Clone, Debug)]
struct Resolved {
    state: TurnState,
    error_code: Option<String>,
    error_detail: Option<String>,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
    /// Assistant message content to insert.
    message_text: Option<String>,
}

impl Resolved {
    fn from_outcome(outcome: &TerminalOutcome) -> Self {
        match outcome {
            TerminalOutcome::Completed {
                text,
                usage,
                response_id,
                ..
            } => Self {
                state: TurnState::Completed,
                error_code: None,
                error_detail: None,
                usage: *usage,
                response_id: response_id.clone(),
                // Always inserted for completed, even when empty.
                message_text: Some(text.clone()),
            },
            TerminalOutcome::Failed {
                error_code,
                error_detail,
                usage,
                ..
            } => Self {
                state: TurnState::Failed,
                error_code: Some(error_code.clone()),
                error_detail: error_detail.clone(),
                usage: *usage,
                response_id: None,
                message_text: None,
            },
            TerminalOutcome::Cancelled { partial_text } => Self {
                state: TurnState::Cancelled,
                error_code: None,
                error_detail: None,
                usage: None,
                response_id: None,
                message_text: (!partial_text.is_empty()).then(|| partial_text.clone()),
            },
        }
    }

    /// The outcome after the assistant message could not be inserted:
    /// completed becomes `failed` / `message_persistence_failed`; cancelled
    /// stays cancelled without a message.
    fn without_message(&self) -> Self {
        let mut next = self.clone();
        next.message_text = None;
        if self.state == TurnState::Completed {
            next.state = TurnState::Failed;
            next.error_code = Some(MESSAGE_PERSISTENCE_FAILED.to_owned());
        }
        next
    }
}

/// "Usage known" for a failed turn: at least one non-zero token count
/// (DESIGN section 5.7, usage accounting rule 3).
fn usage_known(u: Option<&UsageTokens>) -> bool {
    u.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

fn terminal(state: TurnState) -> TurnTerminalState {
    match state {
        TurnState::Completed => TurnTerminalState::Completed,
        TurnState::Cancelled => TurnTerminalState::Cancelled,
        TurnState::Failed | TurnState::Running => TurnTerminalState::Failed,
    }
}

/// Billing outcome and the events' `usage`: provider usage for completed and
/// for actual settlements, zeros for released, `null` for estimated.
fn billing_for(
    state: TurnState,
    error_code: Option<&str>,
    usage: Option<UsageTokens>,
) -> ((BillingOutcome, SettlementMethod), Option<UsageTokens>) {
    if state == TurnState::Failed
        && let Some(code) = error_code
        && !is_known_error_code(code)
    {
        tracing::error!(
            error_code = %code,
            "CRITICAL: unknown turn error code; billed as failed/estimated \
             (mini_chat_unknown_error_code_total)"
        );
    }
    let billing = derive_billing(terminal(state), error_code, usage_known(usage.as_ref()));
    let event_usage = match billing.1 {
        SettlementMethod::Actual => usage,
        SettlementMethod::Released => Some(UsageTokens::default()),
        SettlementMethod::Estimated => None,
    };
    (billing, event_usage)
}

/// Trace id of the current OpenTelemetry span, if there is a valid one.
fn current_trace_id() -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let cx = tracing::Span::current().context();
    let span = cx.span();
    let sc = span.span_context();
    sc.is_valid().then(|| sc.trace_id().to_string())
}

fn dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        turn_id.as_simple(),
        request_id.as_simple()
    )
}

fn count_u32(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

impl Plan {
    fn usage_event(&self, charged: i64) -> UsageEvent {
        UsageEvent {
            tenant_id: self.tenant_id,
            user_id: self.user_id,
            chat_id: self.chat_id,
            turn_id: Some(self.turn_id),
            request_id: self.request_id,
            effective_model: self.effective_model.clone(),
            selected_model: self.selected_model.clone(),
            terminal_state: self.state.as_str().to_owned(),
            billing_outcome: self.billing.0.as_str().to_owned(),
            usage: self.event_usage,
            actual_credits_micro: charged,
            settlement_method: self.billing.1.as_str().to_owned(),
            policy_version_applied: self.policy_version,
            web_search_calls: self.tools.web_search,
            code_interpreter_calls: self.tools.code_interpreter,
            file_search_calls: self.tools.file_search,
            timestamp: self.now,
            requester_type: self.requester_type.clone(),
            dedupe_key: dedupe_key(self.tenant_id, self.turn_id, self.request_id),
            system_task_type: None,
        }
    }

    fn audit_event(&self) -> MiniChatAuditEvent {
        let event_type = if self.state == TurnState::Completed {
            AUDIT_TURN_COMPLETED
        } else {
            AUDIT_TURN_FAILED
        };
        MiniChatAuditEvent::Turn(TurnAuditEvent {
            event_type: event_type.to_owned(),
            tenant_id: self.tenant_id,
            chat_id: self.chat_id,
            turn_id: self.turn_id,
            request_id: self.request_id,
            requester_type: self.requester_type.clone(),
            actor_user_id: self.user_id,
            selected_model: self.selected_model.clone(),
            effective_model: self.effective_model.clone(),
            terminal_state: self.state.as_str().to_owned(),
            error_code: self.error_code.clone(),
            usage: self.event_usage,
            latency_ms: self.latency_ms,
            tool_calls: ToolCalls {
                web_search_calls: self.tools.web_search,
                file_search_calls: self.tools.file_search,
            },
            policy_decisions: PolicyDecisions {
                quota: self.quota_decision.clone(),
                license: None,
                quota_scope: None,
            },
            prompt: None,
            response: None,
            attachments: vec![],
            trace_id: self.trace_id.clone(),
            timestamp: self.now,
        })
    }
}

// ── Service ──────────────────────────────────────────────────────────────────

pub struct FinalizationService {
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn PolicyProvider>,
    quota: Arc<QuotaService>,
    outbox: Arc<MiniChatOutbox>,
}

impl FinalizationService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<dyn PolicyProvider>,
        quota: Arc<QuotaService>,
        outbox: Arc<MiniChatOutbox>,
    ) -> Self {
        Self {
            db,
            policy,
            quota,
            outbox,
        }
    }

    /// The catalog entry of `model` in the user's snapshot of `version`.
    async fn model_entry(
        &self,
        user_id: Uuid,
        version: u64,
        model: &str,
    ) -> Result<ModelCatalogEntry, DomainError> {
        let snapshot = self.policy.snapshot(user_id, version).await?;
        snapshot.find(model).cloned().ok_or_else(|| {
            tracing::error!(
                model = %model,
                policy_version = version,
                "finalization: effective model missing from the policy snapshot; turn left running"
            );
            DomainError::Internal(format!(
                "model '{model}' missing from policy snapshot version {version}"
            ))
        })
    }

    /// Stream finalization of a turn (completed, incomplete, failed,
    /// cancelled): see the module documentation for the transaction.
    ///
    /// # Errors
    /// `Internal` when the effective model is missing from the snapshot or the
    /// settlement is not computable, policy plugin or database failure. On
    /// error nothing was committed: the turn stays `running` (the orphan
    /// watchdog finalizes it later).
    pub async fn finalize(&self, input: FinalizeInput) -> Result<FinalizeResult, DomainError> {
        let entry = self
            .model_entry(
                input.requester_user_id,
                input.policy_version,
                &input.effective_model,
            )
            .await?;
        log_input_signals(&input, &entry);
        let (outcome, state) = self.attempt_with_fallback(&input, &entry).await?;
        match outcome {
            TxOutcome::Lost(current) => Ok(FinalizeResult {
                won: false,
                state: current.unwrap_or(state),
                quota_warnings: None,
            }),
            TxOutcome::Won(wake) => {
                wake.fire();
                let quota_warnings = if state == TurnState::Completed {
                    self.warnings(&input).await
                } else {
                    None
                };
                Ok(FinalizeResult {
                    won: true,
                    state,
                    quota_warnings,
                })
            }
        }
    }

    /// First attempt; after a message insert failure, a second transaction
    /// without the message (DESIGN section 5.7 content durability). Returns
    /// the transaction outcome and the state it committed.
    async fn attempt_with_fallback(
        &self,
        input: &FinalizeInput,
        entry: &ModelCatalogEntry,
    ) -> Result<(TxOutcome, TurnState), DomainError> {
        let now = db_now();
        let trace_id = current_trace_id();
        let resolved = Resolved::from_outcome(&input.outcome);
        let first = stream_plan(input, &resolved, entry, now, trace_id.clone());
        if let Attempt::Done(o) = self.run(first).await? {
            return Ok((o, resolved.state));
        }
        let fallback = resolved.without_message();
        let plan = stream_plan(input, &fallback, entry, now, trace_id);
        match self.run(plan).await? {
            Attempt::Done(o) => Ok((o, fallback.state)),
            Attempt::MessageFailed => Err(DomainError::Internal(
                "finalization retry without a message failed on a message insert".to_owned(),
            )),
        }
    }

    /// `quota_warnings` after commit; a failure is logged and yields `None`.
    async fn warnings(&self, input: &FinalizeInput) -> Option<Vec<QuotaWarningView>> {
        let res = async {
            let limits = self
                .policy
                .user_limits(input.requester_user_id, input.policy_version)
                .await?;
            self.quota
                .warnings(
                    input.tenant_id,
                    input.requester_user_id,
                    &limits,
                    OffsetDateTime::now_utc(),
                )
                .await
        }
        .await;
        res.inspect_err(|e| tracing::warn!(error = %e, "quota warnings not computed"))
            .ok()
    }

    /// Plain CAS to `failed` with `error_code` for a retry/edit turn whose
    /// setup failed before the reserve: no settlement, no outbox event. A turn
    /// that is no longer running is left untouched.
    ///
    /// # Errors
    /// Database failure.
    pub async fn finalize_unstarted(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        error_code: &str,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        let update = TerminalUpdate {
            state: TurnState::Failed,
            error_code: Some(error_code.to_owned()),
            error_detail: None,
            provider_response_id: None,
            assistant_message_id: None,
            now: db_now(),
        };
        if turn_repo::cas_finalize(&conn, tenant_id, turn_id, &update).await? == 0 {
            tracing::debug!(turn_id = %turn_id, "unstarted turn already finalized");
        }
        Ok(())
    }

    /// Orphan watchdog finalization of one candidate (DESIGN section 4):
    /// guarded CAS re-checking the stale-progress predicate against `cutoff`,
    /// estimated settlement (periods from `started_at`), usage event
    /// (`aborted` / `estimated`, `selected_model` = effective model) and audit
    /// `turn_failed` with quota decision `"unknown"`. Settlement is skipped
    /// with a warning (the events then carry `actual_credits_micro = 0`) when
    /// the reserve fields or `requester_user_id` are NULL, or when the
    /// effective model is missing from the policy snapshot (or the snapshot
    /// version itself no longer exists), so no turn stays
    /// running indefinitely (DESIGN section 5.8). Returns whether this call
    /// finalized the turn.
    ///
    /// # Errors
    /// Policy plugin or database failure; the turn then stays `running` and a
    /// later scan retries it.
    pub async fn finalize_orphan(
        &self,
        turn: &chat_turn::Model,
        cutoff: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let reserve = OrphanReserve::from_turn(turn);
        let entry = if let Some(r) = &reserve {
            self.orphan_entry(turn.id, r).await?
        } else {
            tracing::warn!(
                turn_id = %turn.id,
                "orphan turn without reserve fields or requester; settlement skipped"
            );
            None
        };
        let plan = orphan_plan(
            turn,
            reserve.as_ref(),
            entry.as_ref(),
            db_ts(cutoff),
            db_now(),
        );
        match self.run(plan).await? {
            Attempt::Done(TxOutcome::Won(wake)) => {
                wake.fire();
                Ok(true)
            }
            Attempt::Done(TxOutcome::Lost(_)) => Ok(false),
            Attempt::MessageFailed => Err(DomainError::Internal(
                "orphan finalization inserted no message".to_owned(),
            )),
        }
    }

    /// The catalog entry an orphan turn is priced with. `None` (settlement
    /// skipped, warning logged) when the model is missing from the snapshot or
    /// the plugin no longer has the policy version at all, which is permanent
    /// so retrying would keep the turn running forever. Transient policy
    /// errors propagate (the next scan retries).
    async fn orphan_entry(
        &self,
        turn_id: Uuid,
        r: &OrphanReserve,
    ) -> Result<Option<ModelCatalogEntry>, DomainError> {
        match self.policy.snapshot(r.user_id, r.policy_version).await {
            Ok(snapshot) => {
                let entry = snapshot.find(&r.effective_model).cloned();
                if entry.is_none() {
                    tracing::warn!(
                        turn_id = %turn_id,
                        model = %r.effective_model,
                        policy_version = r.policy_version,
                        "orphan turn: effective model missing from the policy snapshot; settlement skipped"
                    );
                }
                Ok(entry)
            }
            Err(DomainError::PolicySnapshotGone(cause)) => {
                tracing::warn!(
                    turn_id = %turn_id,
                    policy_version = r.policy_version,
                    %cause,
                    "orphan turn: policy snapshot version no longer exists; settlement skipped"
                );
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Runs one finalization transaction for `plan`.
    async fn run(&self, plan: Plan) -> Result<Attempt, DomainError> {
        let message_failed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&message_failed);
        let quota = Arc::clone(&self.quota);
        let outbox = Arc::clone(&self.outbox);
        let res = self
            .db
            .transaction(move |tx| {
                Box::pin(async move { apply(tx, &quota, &outbox, &plan, &flag).await })
            })
            .await;
        match res {
            Ok(outcome) => Ok(Attempt::Done(outcome)),
            Err(e) if message_failed.load(Ordering::SeqCst) => {
                tracing::error!(error = %e, "assistant message not persisted at finalization");
                Ok(Attempt::MessageFailed)
            }
            Err(e) => {
                tracing::error!(error = %e, "turn finalization transaction failed");
                Err(e)
            }
        }
    }
}

/// The transaction body: CAS, message, settlement, outbox (in this order).
async fn apply(
    tx: &DbTx<'_>,
    quota: &QuotaService,
    outbox: &MiniChatOutbox,
    plan: &Plan,
    message_failed: &AtomicBool,
) -> Result<TxOutcome, DomainError> {
    let rows = match &plan.cas {
        Cas::Stream(update) => {
            turn_repo::cas_finalize(tx, plan.tenant_id, plan.turn_id, update).await?
        }
        Cas::Orphan { cutoff } => {
            turn_repo::cas_finalize_orphan(tx, plan.tenant_id, plan.turn_id, *cutoff, plan.now)
                .await?
        }
    };
    if rows == 0 {
        let current = turn_repo::find_by_id(tx, plan.tenant_id, plan.turn_id)
            .await?
            .and_then(|t| TurnState::parse(&t.state));
        return Ok(TxOutcome::Lost(current));
    }
    if let Some(msg) = &plan.message
        && let Err(e) = message_repo::insert(tx, msg.clone()).await
    {
        message_failed.store(true, Ordering::SeqCst);
        return Err(e);
    }
    let charged = match &plan.settlement {
        Some(s) => quota.settle_in_tx(tx, s).await?.charged_credits_micro,
        None => 0,
    };
    let mut wake = outbox.enqueue_usage(tx, &plan.usage_event(charged)).await?;
    wake += outbox
        .enqueue_audit(tx, plan.tenant_id, &plan.audit_event())
        .await?;
    if plan.summary
        && let Some(payload) = summary_payload(tx, plan).await?
    {
        wake += outbox.enqueue_thread_summary(tx, &payload).await?;
    }
    Ok(TxOutcome::Won(wake))
}

/// Thread summary task of a completed turn (DESIGN section 3.6): frozen
/// target = latest live message strictly before the turn's user message and
/// not belonging to the turn (its `request_id`); base = the current summary
/// frontier. `None` when there is no earlier message or the target is already
/// covered (`target <= base`).
async fn summary_payload(
    tx: &DbTx<'_>,
    plan: &Plan,
) -> Result<Option<ThreadSummaryPayload>, DomainError> {
    let Some(user_msg) =
        message_repo::find_turn_user_message(tx, plan.tenant_id, plan.chat_id, plan.request_id)
            .await?
    else {
        return Ok(None);
    };
    let Some(target) = message_repo::latest_live_before(
        tx,
        plan.tenant_id,
        plan.chat_id,
        (user_msg.created_at, user_msg.id),
        plan.request_id,
    )
    .await?
    else {
        return Ok(None);
    };
    let base = message_repo::summary_frontier(tx, plan.tenant_id, plan.chat_id).await?;
    if base.is_some_and(|b| target <= b) {
        return Ok(None);
    }
    Ok(Some(ThreadSummaryPayload {
        tenant_id: plan.tenant_id,
        chat_id: plan.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: base.map(|b| b.0),
        base_frontier_message_id: base.map(|b| b.1),
        frozen_target_created_at: target.0,
        frozen_target_message_id: target.1,
        system_task_type: THREAD_SUMMARY_TASK.to_owned(),
    }))
}

/// Logs a tier mismatch between preflight and the snapshot, and the
/// incomplete reason of a completed stream (DESIGN section 5.7: logged only).
fn log_input_signals(input: &FinalizeInput, entry: &ModelCatalogEntry) {
    if (entry.tier == ModelTier::Premium) != input.premium {
        tracing::warn!(
            model = %input.effective_model,
            "finalization: preflight tier differs from the snapshot tier; settling by the snapshot"
        );
    }
    if let TerminalOutcome::Completed {
        incomplete_reason: Some(reason),
        ..
    } = &input.outcome
    {
        tracing::warn!(reason = %reason, turn_id = %input.turn_id, "stream incomplete");
    }
}

/// Plan of a stream finalization attempt for `r`.
fn stream_plan(
    input: &FinalizeInput,
    r: &Resolved,
    entry: &ModelCatalogEntry,
    now: OffsetDateTime,
    trace_id: Option<String>,
) -> Plan {
    let (billing, event_usage) = billing_for(r.state, r.error_code.as_deref(), r.usage);
    let message = r.message_text.as_ref().map(|text| {
        // Token columns carry provider usage of completed turns only.
        let u = if r.state == TurnState::Completed {
            r.usage.unwrap_or_default()
        } else {
            UsageTokens::default()
        };
        NewMessage {
            id: input.assistant_message_id,
            tenant_id: input.tenant_id,
            chat_id: input.chat_id,
            request_id: input.request_id,
            role: MessageRole::Assistant,
            content: text.clone(),
            request_kind: "chat".to_owned(),
            features_used: serde_json::json!([]),
            provider_response_id: r.response_id.clone(),
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_write_input_tokens: u.cache_write_input_tokens,
            reasoning_tokens: u.reasoning_tokens,
            model: Some(input.effective_model.clone()),
            created_at: now,
        }
    });
    let settlement = SettlementInput {
        tenant_id: input.tenant_id,
        user_id: input.requester_user_id,
        premium: entry.tier == ModelTier::Premium,
        periods: input.periods,
        reserve_tokens: input.reserve_tokens,
        reserved_credits_micro: input.reserved_credits_micro,
        max_output_tokens_applied: input.max_output_tokens_applied,
        minimal_generation_floor_applied: input.minimal_generation_floor_applied,
        in_mult: entry.input_tokens_credit_multiplier_micro,
        out_mult: entry.output_tokens_credit_multiplier_micro,
        method: billing.1,
        usage: r.usage,
        web_search_calls: input.tool_counts.web_search,
        code_interpreter_calls: input.tool_counts.code_interpreter,
    };
    Plan {
        tenant_id: input.tenant_id,
        chat_id: input.chat_id,
        turn_id: input.turn_id,
        request_id: input.request_id,
        user_id: Some(input.requester_user_id),
        requester_type: RequesterType::User.as_str().to_owned(),
        cas: Cas::Stream(TerminalUpdate {
            state: r.state,
            error_code: r.error_code.clone(),
            error_detail: r.error_detail.clone(),
            provider_response_id: r.response_id.clone(),
            assistant_message_id: message.as_ref().map(|m| m.id),
            now,
        }),
        state: r.state,
        error_code: r.error_code.clone(),
        message,
        settlement: Some(settlement),
        billing,
        event_usage,
        selected_model: input.selected_model.clone(),
        effective_model: input.effective_model.clone(),
        policy_version: input.policy_version,
        tools: input.tool_counts,
        quota_decision: input.quota_decision.clone(),
        latency_ms: Some(input.latency_ms),
        trace_id,
        summary: r.state == TurnState::Completed
            && input.summary_candidate.is_some_and(|c| c.trigger),
        now,
    }
}

/// Reserve fields of an orphan turn (all required for settlement).
struct OrphanReserve {
    user_id: Uuid,
    effective_model: String,
    policy_version: u64,
    reserve_tokens: i64,
    reserved_credits_micro: i64,
    max_output_tokens_applied: i64,
    minimal_generation_floor_applied: i64,
}

impl OrphanReserve {
    fn from_turn(t: &chat_turn::Model) -> Option<Self> {
        Some(Self {
            user_id: t.requester_user_id?,
            effective_model: t.effective_model.clone()?,
            policy_version: u64::try_from(t.policy_version_applied?).ok()?,
            reserve_tokens: t.reserve_tokens?,
            reserved_credits_micro: t.reserved_credits_micro?,
            max_output_tokens_applied: i64::from(t.max_output_tokens_applied?),
            minimal_generation_floor_applied: i64::from(t.minimal_generation_floor_applied?),
        })
    }
}

/// Plan of an orphan finalization. Settlement needs both the reserve fields
/// and the snapshot entry; without the reserve the events carry empty models
/// and policy version 0, without the entry the turn's model and version.
fn orphan_plan(
    turn: &chat_turn::Model,
    reserve: Option<&OrphanReserve>,
    entry: Option<&ModelCatalogEntry>,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Plan {
    let (billing, event_usage) = billing_for(TurnState::Failed, Some(ORPHAN_TIMEOUT), None);
    let tools = ToolCounts {
        web_search: count_u32(turn.web_search_completed_count),
        code_interpreter: count_u32(turn.code_interpreter_completed_count),
        file_search: count_u32(turn.file_search_completed_count),
    };
    let settlement = reserve.zip(entry).map(|(r, entry)| SettlementInput {
        tenant_id: turn.tenant_id,
        user_id: r.user_id,
        premium: entry.tier == ModelTier::Premium,
        periods: PeriodStarts::from_started_at(turn.started_at),
        reserve_tokens: r.reserve_tokens,
        reserved_credits_micro: r.reserved_credits_micro,
        max_output_tokens_applied: r.max_output_tokens_applied,
        minimal_generation_floor_applied: r.minimal_generation_floor_applied,
        in_mult: entry.input_tokens_credit_multiplier_micro,
        out_mult: entry.output_tokens_credit_multiplier_micro,
        method: billing.1,
        usage: None,
        web_search_calls: tools.web_search,
        code_interpreter_calls: tools.code_interpreter,
    });
    let (model, policy_version) = reserve.map_or((String::new(), 0), |r| {
        (r.effective_model.clone(), r.policy_version)
    });
    Plan {
        tenant_id: turn.tenant_id,
        chat_id: turn.chat_id,
        turn_id: turn.id,
        request_id: turn.request_id,
        user_id: turn.requester_user_id,
        requester_type: turn.requester_type.clone(),
        cas: Cas::Orphan { cutoff },
        state: TurnState::Failed,
        error_code: Some(ORPHAN_TIMEOUT.to_owned()),
        message: None,
        settlement,
        billing,
        event_usage,
        selected_model: model.clone(),
        effective_model: model,
        policy_version,
        tools,
        quota_decision: QuotaPolicyDecision {
            decision: QUOTA_DECISION_UNKNOWN.to_owned(),
            downgrade_from: None,
            downgrade_reason: None,
        },
        latency_ms: None,
        trace_id: current_trace_id(),
        summary: false,
        now,
    }
}

#[cfg(test)]
#[path = "finalization_service_tests.rs"]
mod finalization_service_tests;
