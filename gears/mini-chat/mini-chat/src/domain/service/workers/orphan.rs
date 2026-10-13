//! The orphan finalization inputs
//! (`cpt-cf-mini-chat-algo-background-workers-finalize-orphan`): the candidate
//! row of the scan and the `turn_failed` audit event built from it.

use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, AuditUsage, TurnAuditEvent,
    TurnAuditEventType,
};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// One `running` turn whose durable progress is stale, as the scan reads it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanCandidate {
    /// Turn id.
    pub id: Uuid,
    /// Tenant of the turn.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Request id.
    pub request_id: Uuid,
    /// Requesting user; `None` for a corrupt row.
    pub requester_user_id: Option<Uuid>,
    /// Effective model; `None` before the reserve.
    pub effective_model: Option<String>,
    /// `policy_version_applied`; `None` before the reserve.
    pub policy_version_applied: Option<i64>,
    /// `started_at`.
    pub started_at: OffsetDateTime,
    /// `web_search_completed_count`.
    pub web_search_completed_count: i32,
    /// `file_search_completed_count`.
    pub file_search_completed_count: i32,
}

/// The stale-progress cutoff of a scan: `now` minus `timeout_secs`.
#[must_use]
pub fn stale_cutoff(now: OffsetDateTime, timeout_secs: u64) -> OffsetDateTime {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-watchdog-cutoff
    now - time::Duration::seconds(i64::try_from(timeout_secs).unwrap_or(i64::MAX))
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-watchdog-cutoff
}

/// The `turn_failed` audit event of an orphan finalization; `None` for a
/// candidate without `requester_user_id` (no audit event is enqueued).
/// `selected_model` and `effective_model` are the persisted effective model
/// (empty when NULL), `usage` is 0 and 0, `latency_ms` runs from `started_at`
/// to `now`, the tool calls come from the two persisted counters and the quota
/// decision is `unknown`.
#[must_use]
pub fn orphan_audit_event(
    candidate: &OrphanCandidate,
    now: OffsetDateTime,
) -> Option<TurnAuditEvent> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-precompute-orphan-audit
    let requester_user_id = candidate.requester_user_id?;
    let model = candidate.effective_model.clone().unwrap_or_default();
    let latency = (now - candidate.started_at).whole_milliseconds().max(0);
    Some(TurnAuditEvent {
        event_type: TurnAuditEventType::TurnFailed,
        tenant_id: candidate.tenant_id,
        requester_user_id,
        chat_id: candidate.chat_id,
        turn_id: candidate.id,
        request_id: candidate.request_id,
        selected_model: model.clone(),
        effective_model: model,
        usage: AuditUsage {
            input_tokens: 0,
            output_tokens: 0,
        },
        latency_ms: u64::try_from(latency).unwrap_or(u64::MAX),
        tool_calls: AuditToolCalls {
            web_search: u32::try_from(candidate.web_search_completed_count).unwrap_or(0),
            file_search: u32::try_from(candidate.file_search_completed_count).unwrap_or(0),
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: "unknown".to_owned(),
            },
        },
        timestamp: now,
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: None,
        quota_scope: None,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-precompute-orphan-audit
}

#[cfg(test)]
#[path = "orphan_tests.rs"]
mod tests;
