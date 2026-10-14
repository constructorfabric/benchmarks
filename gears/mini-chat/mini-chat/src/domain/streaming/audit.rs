//! The turn audit event (slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`,
//! algorithm `cpt-cf-mini-chat-algo-message-streaming-turn-audit-event`): the payload enqueued
//! in the finalization transaction of the stream path and of the orphan watchdog path.

use mini_chat_sdk::{AuditEvent, QuotaDecision, TurnAuditEvent};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::quota::ToolCallCounts;
use crate::domain::streaming::finalizer::{StreamOutcome, StreamTerminal};
use crate::domain::streaming::plan::ProviderCallPlan;

/// The facts of an orphan-finalized turn, read from its row.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanTurnFacts {
    pub tenant_id: Uuid,
    /// `requester_user_id`.
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    /// The persisted `effective_model`, empty when the turn has no reserve columns.
    pub effective_model: String,
    pub started_at: OffsetDateTime,
    /// The durable completed counts of the row.
    pub tool_counts: ToolCallCounts,
}

/// The audit event of a stream finalization (`terminal` is the outcome actually committed):
/// `turn_completed` for a completed outcome and `turn_failed` for every other one (`cancelled`
/// included; there is no `turn_cancelled`), with `terminal_state` `completed`, `failed` or
/// `cancelled`;
/// `selected_model` from `chats.model`, `effective_model` and the quota decision with its
/// downgrade facts from the preflight decision; `usage` the provider counts or `None`;
/// `latency_ms` from `plan.started_at` to `finished_at`; the tool counts; `requester_type`
/// `user`; no `prompt`, `response`, `attachments`, `license` or `quota_scope`; no provider id.
#[must_use]
pub fn build_stream_audit(
    plan: &ProviderCallPlan,
    terminal: &StreamTerminal,
    finished_at: OffsetDateTime,
    trace_id: Option<String>,
) -> AuditEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-type
    let (terminal_state, error_code, completed) = match &terminal.outcome {
        StreamOutcome::Completed => ("completed", None, true),
        StreamOutcome::Failed { code, .. } => ("failed", Some(code.as_str().to_owned()), false),
        StreamOutcome::Cancelled => ("cancelled", None, false),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-type
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-stream-models
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-empty-content
    // no content reaches the audit plugin: `prompt`, `response`, `attachments`, `license` and
    // `quota_scope` stay empty and no provider identifier is carried
    let event = TurnAuditEvent {
        tenant_id: plan.tenant_id,
        user_id: plan.user_id,
        chat_id: plan.chat_id,
        turn_id: plan.turn_id,
        request_id: plan.request_id,
        requester_type: "user".to_owned(),
        selected_model: plan.selected_model.clone(),
        effective_model: plan.decision.effective_model.clone(),
        terminal_state: terminal_state.to_owned(),
        error_code,
        usage: terminal.usage,
        latency_ms: latency_ms(plan.started_at, finished_at),
        web_search_calls: terminal.tool_counts.web_search,
        code_interpreter_calls: terminal.tool_counts.code_interpreter,
        file_search_calls: terminal.tool_counts.file_search,
        quota_decision: plan.decision.quota_decision,
        downgrade_from: plan.decision.downgrade_from.clone(),
        downgrade_reason: plan
            .decision
            .downgrade_reason
            .map(|reason| reason.as_str().to_owned()),
        trace_id,
        timestamp: finished_at,
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: None,
        quota_scope: None,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-empty-content
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-stream-models
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-return
    if completed {
        AuditEvent::TurnCompleted(event)
    } else {
        AuditEvent::TurnFailed(event)
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-return
}

/// The audit event of the orphan path: `turn_failed`, `terminal_state` `failed`, `error_code`
/// `orphan_timeout`, `selected_model` equal to the effective model, the quota decision
/// `unknown`, no usage.
#[must_use]
pub fn build_orphan_audit(
    turn: &OrphanTurnFacts,
    finished_at: OffsetDateTime,
    trace_id: Option<String>,
) -> AuditEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-orphan-models
    // the orphan path has no preflight decision: `selected_model` is the persisted effective
    // model (empty without reserve columns) and the quota decision is `unknown`
    AuditEvent::TurnFailed(TurnAuditEvent {
        tenant_id: turn.tenant_id,
        user_id: turn.user_id,
        chat_id: turn.chat_id,
        turn_id: turn.turn_id,
        request_id: turn.request_id,
        requester_type: "user".to_owned(),
        selected_model: turn.effective_model.clone(),
        effective_model: turn.effective_model.clone(),
        terminal_state: "failed".to_owned(),
        error_code: Some(ORPHAN_TIMEOUT.to_owned()),
        usage: None,
        latency_ms: latency_ms(turn.started_at, finished_at),
        web_search_calls: turn.tool_counts.web_search,
        code_interpreter_calls: turn.tool_counts.code_interpreter,
        file_search_calls: turn.tool_counts.file_search,
        quota_decision: QuotaDecision::Unknown,
        downgrade_from: None,
        downgrade_reason: None,
        trace_id,
        timestamp: finished_at,
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: None,
        quota_scope: None,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-audit-event:p1:inst-audit-orphan-models
}

/// The error code stored on an orphan-finalized turn.
pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// Milliseconds from `started_at` to `finished_at`, 0 when the clock went backwards.
fn latency_ms(started_at: OffsetDateTime, finished_at: OffsetDateTime) -> u64 {
    let millis = (finished_at - started_at).whole_milliseconds();
    u64::try_from(millis).unwrap_or(0)
}
