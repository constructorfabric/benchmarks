//! Audit event payloads.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::usage::UsageTokens;

/// Time to first token and total turn duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLatency {
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool calls made during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision taken at preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaPolicyDecision {
    pub decision: String,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
}

/// Policy decisions applied to a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecisions {
    pub quota: QuotaPolicyDecision,
    pub license: Option<serde_json::Value>,
}

/// Audit record of a finished turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// `turn_completed | turn_failed`.
    pub event_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub tenant_id: Uuid,
    pub requester_type: String,
    pub actor_user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    pub error_code: Option<String>,
    pub usage: Option<UsageTokens>,
    pub latency: TurnLatency,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    pub prompt: String,
    pub response: String,
    pub attachments: Vec<serde_json::Value>,
    pub quota_scope: Option<String>,
    pub trace_id: Option<String>,
}

/// Audit record of a turn mutation (retry, edit, delete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    /// `turn_retry | turn_edit | turn_delete`.
    pub event_type: String,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    pub original_request_id: Option<Uuid>,
    pub new_request_id: Option<Uuid>,
    pub request_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event delivered to the audit plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AuditEvent {
    Turn(TurnAuditEvent),
    Mutation(TurnMutationAuditEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    #[test]
    fn audit_event_round_trip() {
        let turn = AuditEvent::Turn(TurnAuditEvent {
            event_type: "turn_completed".to_owned(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            tenant_id: Uuid::from_u128(1),
            requester_type: "user".to_owned(),
            actor_user_id: Some(Uuid::from_u128(2)),
            chat_id: Uuid::from_u128(3),
            turn_id: Uuid::from_u128(4),
            request_id: Uuid::from_u128(5),
            selected_model: "m".to_owned(),
            effective_model: "m".to_owned(),
            terminal_state: "completed".to_owned(),
            error_code: None,
            usage: None,
            latency: TurnLatency {
                ttft_ms: Some(10),
                total_ms: 100,
            },
            tool_calls: ToolCalls {
                web_search_calls: 1,
                file_search_calls: 0,
            },
            policy_decisions: PolicyDecisions {
                quota: QuotaPolicyDecision {
                    decision: "allow".to_owned(),
                    downgrade_from: None,
                    downgrade_reason: None,
                },
                license: None,
            },
            prompt: "hi".to_owned(),
            response: "hello".to_owned(),
            attachments: vec![],
            quota_scope: None,
            trace_id: Some("t".to_owned()),
        });
        let mutation = AuditEvent::Mutation(TurnMutationAuditEvent {
            event_type: "turn_retry".to_owned(),
            tenant_id: Uuid::from_u128(1),
            actor_user_id: Uuid::from_u128(2),
            chat_id: Uuid::from_u128(3),
            original_request_id: Some(Uuid::from_u128(5)),
            new_request_id: Some(Uuid::from_u128(6)),
            request_id: None,
            timestamp: OffsetDateTime::UNIX_EPOCH,
        });

        for (ev, kind) in [(turn, "turn"), (mutation, "mutation")] {
            let v = serde_json::to_value(&ev).unwrap();
            assert_eq!(v["kind"], kind);
            let back: AuditEvent = serde_json::from_value(v).unwrap();
            assert_eq!(back, ev);
        }
    }
}
