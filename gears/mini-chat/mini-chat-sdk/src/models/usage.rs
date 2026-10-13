//! Usage event payload (DESIGN sections 3.2 and A.3).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Provider-reported token usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Settled usage of a turn or system task, handed to `publish_usage`.
///
/// `user_id` and `turn_id` are absent for system tasks; `system_task_type` is
/// absent for user turns; `usage` serializes as `null` when unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub tenant_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    pub request_id: Uuid,
    pub effective_model: String,
    pub selected_model: String,
    /// `completed | failed | cancelled`.
    pub terminal_state: String,
    /// `completed | failed | aborted | system_task`.
    pub billing_outcome: String,
    pub usage: Option<UsageTokens>,
    pub actual_credits_micro: i64,
    /// `actual | estimated | released | none`.
    pub settlement_method: String,
    pub policy_version_applied: i64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// `user | system`.
    pub requester_type: String,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn base_event() -> UsageEvent {
        UsageEvent {
            tenant_id: Uuid::from_u128(1),
            user_id: None,
            chat_id: Uuid::from_u128(2),
            turn_id: None,
            request_id: Uuid::from_u128(3),
            effective_model: "gpt-4.1-mini".to_owned(),
            selected_model: "gpt-4.1-mini".to_owned(),
            terminal_state: "completed".to_owned(),
            billing_outcome: "system_task".to_owned(),
            usage: Some(UsageTokens {
                input_tokens: 5000,
                output_tokens: 200,
                cache_read_input_tokens: 4000,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            }),
            actual_credits_micro: 0,
            settlement_method: "none".to_owned(),
            policy_version_applied: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: OffsetDateTime::UNIX_EPOCH,
            requester_type: "system".to_owned(),
            dedupe_key: "k".to_owned(),
            system_task_type: Some("thread_summary_update".to_owned()),
        }
    }

    #[test]
    fn system_task_event_omits_user_and_turn() {
        let ev = base_event();
        let v = serde_json::to_value(&ev).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("user_id"));
        assert!(!obj.contains_key("turn_id"));
        assert_eq!(v["system_task_type"], "thread_summary_update");
        assert_eq!(v["timestamp"], "1970-01-01T00:00:00Z");
        let back: UsageEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, ev);

        let user_turn = UsageEvent {
            user_id: Some(Uuid::from_u128(4)),
            turn_id: Some(Uuid::from_u128(5)),
            usage: None,
            requester_type: "user".to_owned(),
            system_task_type: None,
            ..ev
        };
        let v = serde_json::to_value(&user_turn).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("system_task_type"));
        assert!(obj.contains_key("user_id"));
        assert!(obj.contains_key("turn_id"));
        assert!(v["usage"].is_null());
        assert!(obj.contains_key("usage"));
    }
}
