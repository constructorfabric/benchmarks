//! Usage events published through the usage outbox to the model policy
//! plugin.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Provider-reported token counts (telemetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Usage settlement event of a turn (or a system task).
///
/// `user_id` and `turn_id` are omitted for system tasks; `system_task_type`
/// is omitted for user turns; `usage` is `null` when the provider reported
/// no usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// `completed`, `failed` or `cancelled`.
    pub terminal_state: String,
    /// `completed`, `failed`, `aborted` or `system_task`.
    pub billing_outcome: String,
    pub usage: Option<UsageTokens>,
    pub actual_credits_micro: i64,
    /// `actual`, `estimated`, `released` or `none`.
    pub settlement_method: String,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: time::OffsetDateTime,
    /// `user` or `system`.
    pub requester_type: String,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

/// Canonical usage dedupe key `{tenant}/{turn}/{request}` in simple UUID form.
#[must_use]
pub fn turn_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        turn_id.as_simple(),
        request_id.as_simple()
    )
}

/// System-task dedupe key `{tenant}/{task_type}/{system_request}`.
#[must_use]
pub fn system_task_dedupe_key(tenant_id: Uuid, task_type: &str, system_request_id: Uuid) -> String {
    format!(
        "{}/{task_type}/{}",
        tenant_id.as_simple(),
        system_request_id.as_simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(system: bool) -> UsageEvent {
        UsageEvent {
            tenant_id: Uuid::nil(),
            user_id: (!system).then(Uuid::nil),
            chat_id: Uuid::nil(),
            turn_id: (!system).then(Uuid::nil),
            request_id: Uuid::nil(),
            effective_model: "m".into(),
            selected_model: "m".into(),
            terminal_state: "completed".into(),
            billing_outcome: if system { "system_task" } else { "completed" }.into(),
            usage: None,
            actual_credits_micro: 0,
            settlement_method: if system { "none" } else { "actual" }.into(),
            policy_version_applied: 1,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
            requester_type: if system { "system" } else { "user" }.into(),
            dedupe_key: "k".into(),
            system_task_type: system.then(|| "thread_summary_update".to_owned()),
        }
    }

    #[test]
    fn system_task_omits_user_and_turn() {
        let v = serde_json::to_value(event(true)).unwrap();
        assert!(v.get("user_id").is_none());
        assert!(v.get("turn_id").is_none());
        assert_eq!(v["system_task_type"], "thread_summary_update");
        assert!(v["usage"].is_null());
    }

    #[test]
    fn user_turn_omits_system_task_type() {
        let v = serde_json::to_value(event(false)).unwrap();
        assert!(v.get("system_task_type").is_none());
        assert!(v.get("user_id").is_some());
        let back: UsageEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, event(false));
    }

    #[test]
    fn dedupe_keys_use_simple_form() {
        let t = Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").unwrap();
        let k = turn_dedupe_key(t, t, t);
        assert_eq!(k.len(), 32 * 3 + 2);
        assert!(!k.contains('-'));
        assert_eq!(
            system_task_dedupe_key(t, "thread_summary_update", t),
            "f47ac10b58cc4372a5670e02b2c3d479/thread_summary_update/f47ac10b58cc4372a5670e02b2c3d479"
        );
    }
}
