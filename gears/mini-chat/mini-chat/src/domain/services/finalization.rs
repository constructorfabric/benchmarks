//! Turn finalization (DESIGN §5.7–5.9, spec §8.3): the single CAS-guarded
//! transaction that moves a `running` turn to its terminal state, persists the
//! assistant message, settles the quota reserve and enqueues the usage and audit
//! events (plus the optional thread-summary task).
//!
//! The shared steps — [`derive_billing`], [`compute_settlement`],
//! [`build_usage_event`] and [`build_turn_audit`] — are public so the orphan
//! watchdog reuses them instead of building its own billing logic.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent,
    ModelCatalogEntry, ModelTier, TurnAuditEvent, TurnAuditEventType, UsageEvent, UsageTokens,
};
use toolkit_db::DBProvider;
use tracing::{error, warn};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::estimation::period_starts;
use crate::domain::model::{
    BillingOutcome, DowngradeReason, MessageRole, QuotaDecision, SettlementMethod, TurnState,
    error_codes,
};
use crate::domain::sanitize::sanitize_provider_message;
use crate::domain::services::quota::{
    QuotaService, Settlement, committed_credits, estimated_credits,
};
use crate::domain::services::quota_status::{TierStatus, compute_tier_status};
use crate::infra::db::entities::{chat_turn, message};
use crate::infra::db::repos::turn::{TerminalUpdate, TurnCounters};
use crate::infra::db::repos::{MessageRepo, QuotaRepo, TurnRepo};
use crate::infra::db::tx::{TxRetryError, with_tx_retry};
use crate::infra::gateways::model_policy::ModelPolicyGateway;
use crate::infra::llm::types::LlmUsage;
use crate::infra::outbox::payloads::{
    AUDIT_PAYLOAD_TYPE, THREAD_SUMMARY_PAYLOAD_TYPE, ThreadSummaryPayload, USAGE_PAYLOAD_TYPE,
};
use crate::infra::outbox::{OutboxEnqueuer, PendingWakes, QueueKind};

/// `requester_type` of user turns in usage events.
const REQUESTER_USER: &str = "user";

/// `chat_turns.error_detail` of a turn whose assistant message could not be stored.
const PERSISTENCE_FAILED_DETAIL: &str = "assistant message could not be persisted";

/// Error codes of failures after the provider call started: settled `actual`
/// when the provider reported usage, else `estimated` (DESIGN §5.8 table).
const POST_PROVIDER_CODES: [&str; 8] = [
    error_codes::PROVIDER_ERROR,
    error_codes::PROVIDER_TIMEOUT,
    error_codes::RATE_LIMITED,
    error_codes::WEB_SEARCH_CALLS_EXCEEDED,
    error_codes::CODE_INTERPRETER_CALLS_EXCEEDED,
    error_codes::AGENTIC_ITERATIONS_EXCEEDED,
    error_codes::UNEXPECTED_TOOL_USE,
    error_codes::MESSAGE_PERSISTENCE_FAILED,
];

/// Error codes of failures before the provider call: the reserve is released.
const PRE_PROVIDER_CODES: [&str; 4] = [
    error_codes::CONTEXT_LENGTH_EXCEEDED,
    error_codes::VALIDATION_ERROR,
    error_codes::INPUT_TOO_LONG,
    error_codes::TURN_SETUP_FAILED,
];

/// How the stream of a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalKind {
    /// Provider `response.completed` or `response.incomplete` (with its reason,
    /// which is only logged).
    Completed { incomplete_reason: Option<String> },
    /// Terminal error; `detail` is stored (sanitized) in `chat_turns.error_detail`.
    Failed { code: String, detail: String },
    /// Client disconnect.
    Cancelled,
}

impl TerminalKind {
    const fn state(&self) -> TurnState {
        match self {
            Self::Completed { .. } => TurnState::Completed,
            Self::Failed { .. } => TurnState::Failed,
            Self::Cancelled => TurnState::Cancelled,
        }
    }

    fn error_code(&self) -> Option<&str> {
        match self {
            Self::Failed { code, .. } => Some(code),
            Self::Completed { .. } | Self::Cancelled => None,
        }
    }
}

/// Everything the finalization of one turn needs.
#[derive(Debug, Clone)]
pub struct FinalizeInput {
    /// The turn row with its preflight (reserve) fields.
    pub turn: chat_turn::Model,
    /// Selected model of the chat (`chats.model`).
    pub chat_model: String,
    /// The requester.
    pub user_id: Uuid,
    pub terminal: TerminalKind,
    /// Accumulated assistant text.
    pub text: String,
    /// Provider-reported usage, if any.
    pub usage: Option<LlmUsage>,
    pub provider_response_id: Option<String>,
    /// Pre-allocated id of the assistant message (`stream_started.message_id`).
    pub assistant_message_id: Uuid,
    pub counters: TurnCounters,
    /// File search calls for the events (provider `file_search` plus `search_knowledge`).
    pub file_search_calls: u32,
    /// Quota periods of the preflight.
    pub daily_start: NaiveDate,
    pub monthly_start: NaiveDate,
    /// Preflight quota decision; `None` is reported as `unknown`.
    pub decision: Option<(QuotaDecision, Option<DowngradeReason>)>,
    pub latency_ms: i64,
    /// Thread-summary task to enqueue with a completed turn.
    pub thread_summary: Option<ThreadSummaryPayload>,
}

/// Outcome of [`FinalizationService::finalize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeResult {
    /// This call won the CAS and committed `state`; `quota_status` is the
    /// user's quota status after the commit (empty when it could not be read).
    Won {
        state: TurnState,
        quota_status: Vec<TierStatus>,
    },
    /// Another finalizer already moved the turn out of `running`; nothing changed.
    Lost,
    /// The completed turn's assistant message could not be stored; the turn was
    /// finalized `failed` with `message_persistence_failed`.
    PersistenceFailed,
    /// The finalization transaction failed; the turn is still `running`.
    TxFailed,
}

/// Billing outcome and settlement method of a terminal turn (DESIGN §5.8
/// "Normative Billing Outcome Derivation"). For failed turns, usage is known
/// only when it has a non-zero input or output count; a completed turn always
/// settles `actual`; cancelled and orphan-timeout turns are `aborted` and
/// always `estimated`; an unknown error code is `failed` / `estimated`.
#[must_use]
pub fn derive_billing(
    state: TurnState,
    error_code: Option<&str>,
    usage: Option<&LlmUsage>,
) -> (BillingOutcome, SettlementMethod) {
    match state {
        TurnState::Completed => (BillingOutcome::Completed, SettlementMethod::Actual),
        TurnState::Cancelled => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        TurnState::Failed => match error_code {
            Some(error_codes::ORPHAN_TIMEOUT) => {
                (BillingOutcome::Aborted, SettlementMethod::Estimated)
            }
            Some(code) if POST_PROVIDER_CODES.contains(&code) => {
                let method = if usage_known(usage) {
                    SettlementMethod::Actual
                } else {
                    SettlementMethod::Estimated
                };
                (BillingOutcome::Failed, method)
            }
            Some(code) if PRE_PROVIDER_CODES.contains(&code) => {
                (BillingOutcome::Failed, SettlementMethod::Released)
            }
            other => {
                error!(
                    error_code = other.unwrap_or("<none>"),
                    "unknown_error_code: settling the failed turn as estimated"
                );
                (BillingOutcome::Failed, SettlementMethod::Estimated)
            }
        },
        TurnState::Running => {
            error!("billing derivation for a running turn; settling as failed/estimated");
            (BillingOutcome::Failed, SettlementMethod::Estimated)
        }
    }
}

/// Usage is "known" when it has a non-zero input or output count (DESIGN §5.7).
fn usage_known(usage: Option<&LlmUsage>) -> bool {
    usage.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

pub(crate) fn tokens(u: &LlmUsage) -> UsageTokens {
    UsageTokens {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_write_input_tokens: u.cache_write_input_tokens,
        reasoning_tokens: u.reasoning_tokens,
    }
}

/// Usage event `dedupe_key`: `{tenant}/{turn}/{request}` in simple UUID form
/// (DESIGN §5.6).
#[must_use]
pub fn dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.simple(),
        turn_id.simple(),
        request_id.simple()
    )
}

/// Reserve columns of a turn (all set at preflight, never changed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: u64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i64,
}

impl TurnReserve {
    /// The reserve of `turn`; `None` when any reserve column is NULL.
    #[must_use]
    pub fn of(turn: &chat_turn::Model) -> Option<Self> {
        Some(Self {
            reserve_tokens: turn.reserve_tokens?,
            max_output_tokens_applied: i64::from(turn.max_output_tokens_applied?),
            reserved_credits_micro: turn.reserved_credits_micro?,
            policy_version_applied: u64::try_from(turn.policy_version_applied?).ok()?,
            effective_model: turn.effective_model.clone()?,
            minimal_generation_floor_applied: i64::from(turn.minimal_generation_floor_applied?),
        })
    }
}

/// Settlement of a turn (DESIGN §5.4.4, §5.4.5, §5.8, §5.9). `entry` is the
/// turn's effective model in the snapshot of its `policy_version_applied`
/// (multipliers and tier).
#[derive(Debug, Clone)]
pub struct SettlementInput<'a> {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub daily_start: NaiveDate,
    pub monthly_start: NaiveDate,
    pub reserve: &'a TurnReserve,
    pub entry: &'a ModelCatalogEntry,
    pub method: SettlementMethod,
    pub usage: Option<&'a LlmUsage>,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
    pub overshoot_tolerance_factor: f64,
}

/// Build the [`Settlement`] of a turn: committed credits per `method` (actual
/// with the overshoot cap; estimated `credits(reserve - max_out, floor)`;
/// released 0), bucket `tier:premium` when the entry is premium.
///
/// # Errors
/// `Internal` when the credits cannot be computed or the method is `none`.
pub fn compute_settlement(s: &SettlementInput<'_>) -> DomainResult<Settlement> {
    let r = s.reserve;
    let (input_tokens, output_tokens) = s
        .usage
        .map_or((0, 0), |u| (u.input_tokens, u.output_tokens));
    let committed = match s.method {
        SettlementMethod::Actual => {
            committed_credits(
                input_tokens,
                output_tokens,
                r.reserve_tokens,
                r.reserved_credits_micro,
                s.overshoot_tolerance_factor,
                s.entry,
            )?
            .0
        }
        SettlementMethod::Estimated => estimated_credits(
            r.reserve_tokens,
            r.max_output_tokens_applied,
            r.minimal_generation_floor_applied,
            s.entry,
        )?,
        SettlementMethod::Released => 0,
        SettlementMethod::None => {
            return Err(DomainError::internal(
                "settlement method `none` is not a turn settlement",
            ));
        }
    };
    Ok(Settlement {
        tenant_id: s.tenant_id,
        user_id: s.user_id,
        daily_start: s.daily_start,
        monthly_start: s.monthly_start,
        premium: s.entry.tier == ModelTier::Premium,
        turn_reserved_credits_micro: r.reserved_credits_micro,
        committed_credits_micro: committed,
        actual_input_tokens: input_tokens,
        actual_output_tokens: output_tokens,
        web_search_calls: s.web_search_calls,
        code_interpreter_calls: s.code_interpreter_calls,
        method: s.method,
    })
}

/// Facts shared by the usage and audit events of one finalized turn.
#[derive(Debug, Clone)]
pub struct TurnEventFacts<'a> {
    pub turn: &'a chat_turn::Model,
    /// The requester; `None` only on the watchdog path of a turn whose
    /// `requester_user_id` is NULL.
    pub user_id: Option<Uuid>,
    /// Selected model (the chat model; the effective model on the watchdog path).
    pub selected_model: &'a str,
    /// Committed terminal state.
    pub state: TurnState,
    pub error_code: Option<&'a str>,
    /// Provider-reported usage, if any.
    pub usage: Option<&'a LlmUsage>,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    pub now: DateTime<Utc>,
}

impl TurnEventFacts<'_> {
    fn effective_model(&self) -> String {
        self.turn.effective_model.clone().unwrap_or_default()
    }

    fn timestamp(&self) -> String {
        self.now.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
}

/// The usage event of a finalized turn (DESIGN §5.6, §5.7 "Usage accounting
/// rules per outcome", §5.8, §5.9). `usage` carries the provider counts on an
/// actual settlement (`null` when the provider reported none), zeros on a
/// released settlement and `null` on an estimated one.
#[must_use]
pub fn build_usage_event(
    f: &TurnEventFacts<'_>,
    outcome: BillingOutcome,
    method: SettlementMethod,
    actual_credits_micro: i64,
) -> UsageEvent {
    let turn = f.turn;
    let usage = match method {
        SettlementMethod::Actual => f.usage.map(tokens),
        SettlementMethod::Released => Some(UsageTokens::default()),
        SettlementMethod::Estimated | SettlementMethod::None => None,
    };
    UsageEvent {
        tenant_id: turn.tenant_id,
        user_id: f.user_id,
        chat_id: turn.chat_id,
        turn_id: Some(turn.id),
        request_id: turn.request_id,
        effective_model: f.effective_model(),
        selected_model: f.selected_model.to_owned(),
        terminal_state: f.state.as_str().to_owned(),
        billing_outcome: outcome.as_str().to_owned(),
        usage,
        actual_credits_micro,
        settlement_method: method.as_str().to_owned(),
        policy_version_applied: turn
            .policy_version_applied
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0),
        web_search_calls: f.web_search_calls,
        code_interpreter_calls: f.code_interpreter_calls,
        file_search_calls: f.file_search_calls,
        timestamp: f.timestamp(),
        requester_type: REQUESTER_USER.to_owned(),
        dedupe_key: dedupe_key(turn.tenant_id, turn.id, turn.request_id),
        system_task_type: None,
    }
}

/// The turn audit event (DESIGN §3.9 "Audit Events for Turn Mutations", PRD
/// "Audit Events"): `turn_completed` for a completed turn, `turn_failed`
/// otherwise; the quota decision is `unknown` when `decision` is `None`;
/// `downgrade_from` (= selected model) and `downgrade_reason` only on a
/// downgrade. Content fields stay empty (ADR-0009).
#[must_use]
pub fn build_turn_audit(
    f: &TurnEventFacts<'_>,
    decision: Option<(QuotaDecision, Option<DowngradeReason>)>,
    latency_ms: i64,
) -> TurnAuditEvent {
    let turn = f.turn;
    let quota = match decision {
        Some((d, reason)) => {
            let downgrade = d == QuotaDecision::Downgrade;
            AuditQuotaDecision {
                decision: d.as_str().to_owned(),
                downgrade_from: downgrade.then(|| f.selected_model.to_owned()),
                downgrade_reason: reason.filter(|_| downgrade).map(|r| r.as_str().to_owned()),
            }
        }
        None => AuditQuotaDecision {
            decision: "unknown".to_owned(),
            downgrade_from: None,
            downgrade_reason: None,
        },
    };
    let completed = f.state == TurnState::Completed;
    TurnAuditEvent {
        event_type: if completed {
            TurnAuditEventType::TurnCompleted
        } else {
            TurnAuditEventType::TurnFailed
        },
        tenant_id: turn.tenant_id,
        // The audit contract has no "no user": a NULL requester is the nil UUID.
        user_id: f.user_id.unwrap_or_else(Uuid::nil),
        chat_id: turn.chat_id,
        turn_id: turn.id,
        request_id: turn.request_id,
        selected_model: f.selected_model.to_owned(),
        effective_model: f.effective_model(),
        usage: f.usage.map(tokens).unwrap_or_default(),
        latency_ms: u64::try_from(latency_ms).unwrap_or(0),
        tool_calls: AuditToolCalls {
            web_search_calls: f.web_search_calls,
            file_search_calls: f.file_search_calls,
        },
        policy_decisions: AuditPolicyDecisions { quota },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: None,
        quota_scope: None,
        error_code: if completed {
            None
        } else {
            f.error_code.map(str::to_owned)
        },
        timestamp: f.timestamp(),
    }
}

/// Infrastructure of [`FinalizationService`].
pub struct FinalizationDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub quota: Arc<QuotaService>,
}

/// Everything the finalization transaction writes, computed before it starts
/// (cloned per transaction attempt).
#[derive(Clone)]
struct TxPlan {
    turn_id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    /// Assistant message and whether its insert is mandatory (completed) or
    /// best-effort (cancelled with partial text).
    message: Option<(message::Model, bool)>,
    terminal: TerminalUpdate,
    settlement: Option<Settlement>,
    usage_event: UsageEvent,
    audit_event: TurnAuditEvent,
    thread_summary: Option<ThreadSummaryPayload>,
}

/// Why the finalization transaction rolled back.
#[derive(Debug)]
enum TxError {
    /// The CAS found the turn no longer `running`.
    Lost,
    /// The mandatory assistant message insert failed.
    MessagePersistence(String),
    Domain(DomainError),
}

impl From<DomainError> for TxError {
    fn from(e: DomainError) -> Self {
        Self::Domain(e)
    }
}

impl From<toolkit_db::DbError> for TxError {
    fn from(e: toolkit_db::DbError) -> Self {
        Self::Domain(DomainError::from(e))
    }
}

impl TxRetryError for TxError {
    fn is_contention(&self) -> bool {
        matches!(self, Self::Domain(e) if e.is_contention())
    }
}

/// CAS-guarded turn finalization (DESIGN §5.7 "`FinalizeTurn` Invariant").
pub struct FinalizationService {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyGateway>,
    outbox: Arc<OutboxEnqueuer>,
    quota: Arc<QuotaService>,
}

impl FinalizationService {
    #[must_use]
    pub fn new(deps: FinalizationDeps) -> Self {
        let FinalizationDeps {
            config,
            db,
            policy,
            outbox,
            quota,
        } = deps;
        Self {
            cfg: config,
            db,
            policy,
            outbox,
            quota,
        }
    }

    /// Finalize a running turn in one transaction: assistant message insert
    /// (completed; cancelled with non-empty text, best effort) → CAS `running →
    /// terminal` → quota settlement → usage + audit enqueue → thread-summary
    /// enqueue (completed only) → commit → fire the outbox wakes.
    ///
    /// A failed mandatory message insert rolls back and finalizes the turn
    /// `failed` / `message_persistence_failed` in a new transaction. Every other
    /// failure rolls back and leaves the turn `running` (`TxFailed`).
    pub async fn finalize(&self, input: FinalizeInput) -> FinalizeResult {
        if let TerminalKind::Completed {
            incomplete_reason: Some(reason),
        } = &input.terminal
        {
            warn!(turn_id = %input.turn.id, %reason, "stream incomplete");
        }
        match self.plan_and_run(&input).await {
            Ok(wakes) => {
                wakes.fire();
                FinalizeResult::Won {
                    state: input.terminal.state(),
                    quota_status: self.quota_status(&input).await,
                }
            }
            Err(TxError::MessagePersistence(reason)) => {
                warn!(turn_id = %input.turn.id, %reason, "assistant message persistence failed; finalizing the turn as failed");
                self.finalize_persistence_failure(input).await
            }
            Err(err) => not_won(input.turn.id, err),
        }
    }

    async fn finalize_persistence_failure(&self, input: FinalizeInput) -> FinalizeResult {
        let failed = FinalizeInput {
            terminal: TerminalKind::Failed {
                code: error_codes::MESSAGE_PERSISTENCE_FAILED.to_owned(),
                detail: PERSISTENCE_FAILED_DETAIL.to_owned(),
            },
            thread_summary: None,
            ..input
        };
        match self.plan_and_run(&failed).await {
            Ok(wakes) => {
                wakes.fire();
                FinalizeResult::PersistenceFailed
            }
            Err(err) => not_won(failed.turn.id, err),
        }
    }

    async fn plan_and_run(&self, input: &FinalizeInput) -> Result<PendingWakes, TxError> {
        let plan = self.plan(input).await?;
        self.run(plan).await
    }

    /// Quota settlement of the turn under `method`, with the multipliers and tier
    /// of its effective model in the snapshot of its `policy_version_applied`;
    /// `None` (with a warning) when the turn has no reserve fields.
    async fn settlement(
        &self,
        input: &FinalizeInput,
        method: SettlementMethod,
    ) -> DomainResult<Option<Settlement>> {
        let turn = &input.turn;
        let Some(reserve) = TurnReserve::of(turn) else {
            warn!(turn_id = %turn.id, "turn has no reserve fields; skipping quota settlement");
            return Ok(None);
        };
        let snapshot = self
            .policy
            .snapshot(input.user_id, reserve.policy_version_applied)
            .await?;
        let entry = snapshot.find(&reserve.effective_model).ok_or_else(|| {
            DomainError::internal(format!(
                "effective model `{}` missing from policy snapshot {}",
                reserve.effective_model, reserve.policy_version_applied
            ))
        })?;
        compute_settlement(&SettlementInput {
            tenant_id: turn.tenant_id,
            user_id: input.user_id,
            daily_start: input.daily_start,
            monthly_start: input.monthly_start,
            reserve: &reserve,
            entry,
            method,
            usage: input.usage.as_ref(),
            web_search_calls: i64::from(input.counters.web_search),
            code_interpreter_calls: i64::from(input.counters.code_interpreter),
            overshoot_tolerance_factor: self.cfg.quota.overshoot_tolerance_factor,
        })
        .map(Some)
    }

    /// Compute every row change and payload of the finalization (the policy
    /// snapshot is fetched here, outside the transaction).
    async fn plan(&self, input: &FinalizeInput) -> DomainResult<TxPlan> {
        let turn = &input.turn;
        let now = now_utc();
        let state = input.terminal.state();
        let error_code = input.terminal.error_code();
        let usage = input.usage.as_ref();
        let (outcome, method) = derive_billing(state, error_code, usage);

        let settlement = self.settlement(input, method).await?;

        let message = match state {
            TurnState::Completed => Some((assistant_message(input, now), true)),
            TurnState::Cancelled if !input.text.is_empty() => {
                Some((assistant_message(input, now), false))
            }
            _ => None,
        };

        let facts = TurnEventFacts {
            turn,
            user_id: Some(input.user_id),
            selected_model: &input.chat_model,
            state,
            error_code,
            usage,
            web_search_calls: u32::try_from(input.counters.web_search).unwrap_or(0),
            code_interpreter_calls: u32::try_from(input.counters.code_interpreter).unwrap_or(0),
            file_search_calls: input.file_search_calls,
            now,
        };
        let credits = settlement.as_ref().map_or(0, |s| s.committed_credits_micro);
        let usage_event = build_usage_event(&facts, outcome, method, credits);
        let audit_event = build_turn_audit(&facts, input.decision, input.latency_ms);

        let error_detail = match &input.terminal {
            TerminalKind::Failed { detail, .. } => Some(sanitize_provider_message(detail)),
            TerminalKind::Completed { .. } | TerminalKind::Cancelled => None,
        };
        let assistant_message_id = message.as_ref().map(|(m, _)| m.id);
        Ok(TxPlan {
            turn_id: turn.id,
            tenant_id: turn.tenant_id,
            chat_id: turn.chat_id,
            message,
            terminal: TerminalUpdate {
                state,
                error_code: error_code.map(str::to_owned),
                error_detail,
                assistant_message_id,
                provider_response_id: input.provider_response_id.clone(),
                counters: input.counters,
                now,
            },
            settlement,
            usage_event,
            audit_event,
            thread_summary: if state == TurnState::Completed {
                input.thread_summary.clone()
            } else {
                None
            },
        })
    }
}

/// Result of a finalization that did not win: `Lost`, or `TxFailed` (logged).
fn not_won(turn_id: Uuid, err: TxError) -> FinalizeResult {
    let reason = match err {
        TxError::Lost => return FinalizeResult::Lost,
        TxError::MessagePersistence(reason) => reason,
        TxError::Domain(err) => err.to_string(),
    };
    error!(%turn_id, %reason, "turn finalization transaction failed; turn left running");
    FinalizeResult::TxFailed
}

/// The assistant message row of a finalized turn (`id` = pre-allocated id,
/// `model` = effective model, token columns from the usage, 0 when absent).
fn assistant_message(input: &FinalizeInput, now: DateTime<Utc>) -> message::Model {
    let usage = input.usage.unwrap_or_default();
    message::Model {
        id: input.assistant_message_id,
        tenant_id: input.turn.tenant_id,
        chat_id: input.turn.chat_id,
        request_id: Some(input.turn.request_id),
        role: MessageRole::Assistant.as_str().to_owned(),
        content: input.text.clone(),
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: input.provider_response_id.clone(),
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!([]),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        model: input.turn.effective_model.clone(),
        is_compressed: false,
        created_at: now,
        deleted_at: None,
    }
}

impl FinalizationService {
    /// Run `plan` in one transaction; returns the wakes to fire after commit.
    async fn run(&self, plan: TxPlan) -> Result<PendingWakes, TxError> {
        let quota = Arc::clone(&self.quota);
        let outbox = Arc::clone(&self.outbox);
        with_tx_retry(&self.db, "turn finalization", move |tx| {
            let (mut plan, quota, outbox) =
                (plan.clone(), Arc::clone(&quota), Arc::clone(&outbox));
            Box::pin(async move {
                    if let Some((row, required)) = plan.message.take() {
                        let id = row.id;
                        let reason = match MessageRepo::insert_if_absent(tx, row).await {
                            Ok(true) => None,
                            Ok(false) => Some(format!("message id {id} already exists")),
                            // Contention: the whole transaction is retried.
                            Err(e) if e.is_contention() => return Err(TxError::Domain(e)),
                            Err(e) => Some(e.to_string()),
                        };
                        if let Some(reason) = reason {
                            if required {
                                return Err(TxError::MessagePersistence(reason));
                            }
                            warn!(turn_id = %plan.turn_id, %reason, "partial assistant message not persisted; cancelling without it");
                            plan.terminal.assistant_message_id = None;
                        }
                    }
                    if !TurnRepo::cas_finalize(tx, plan.turn_id, &plan.terminal).await? {
                        return Err(TxError::Lost);
                    }
                    if let Some(s) = &plan.settlement {
                        quota.settle_in_tx(tx, s).await?;
                    }
                    let mut wakes = PendingWakes::new();
                    wakes.push(
                        outbox
                            .enqueue_json(
                                tx,
                                QueueKind::Usage,
                                plan.tenant_id,
                                USAGE_PAYLOAD_TYPE,
                                &plan.usage_event,
                            )
                            .await?,
                    );
                    wakes.push(
                        outbox
                            .enqueue_json(
                                tx,
                                QueueKind::Audit,
                                plan.tenant_id,
                                AUDIT_PAYLOAD_TYPE,
                                &MiniChatAuditEvent::Turn(plan.audit_event),
                            )
                            .await?,
                    );
                    if let Some(payload) = &plan.thread_summary {
                        wakes.push(
                            outbox
                                .enqueue_json(
                                    tx,
                                    QueueKind::ThreadSummary,
                                    plan.chat_id,
                                    THREAD_SUMMARY_PAYLOAD_TYPE,
                                    payload,
                                )
                                .await?,
                        );
                    }
                    Ok(wakes)
            })
        })
        .await
    }

    /// Quota status of the requester after the commit, from the current-period
    /// bucket rows and the limits of the turn's policy version. Read failures are
    /// logged and yield an empty list (the turn is already finalized).
    async fn quota_status(&self, input: &FinalizeInput) -> Vec<TierStatus> {
        let turn = &input.turn;
        let Some(version) = turn
            .policy_version_applied
            .and_then(|v| u64::try_from(v).ok())
        else {
            return Vec::new();
        };
        let limits = match self.policy.user_limits(input.user_id, version).await {
            Ok(limits) => limits,
            Err(err) => {
                warn!(turn_id = %turn.id, %err, "user limits unavailable after finalization");
                return Vec::new();
            }
        };
        let now = now_utc();
        let (daily, monthly) = period_starts(now);
        let rows = match self.db.conn() {
            Ok(conn) => {
                QuotaRepo::rows_for_periods(&conn, turn.tenant_id, input.user_id, daily, monthly)
                    .await
            }
            Err(err) => Err(err),
        };
        match rows {
            Ok(rows) => {
                compute_tier_status(&limits, &rows, now, self.cfg.quota.warning_threshold_pct)
            }
            Err(err) => {
                warn!(turn_id = %turn.id, %err, "quota rows unavailable after finalization");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
#[path = "finalization_tests.rs"]
mod finalization_tests;
