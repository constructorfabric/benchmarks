//! Mini Chat SDK.
//!
//! Public, transport-agnostic surface of the `mini-chat` gear:
//! - the model policy plugin contract ([`MiniChatModelPolicyPluginClientV1`])
//!   with the policy snapshot, model catalog, kill switches and user limits;
//! - the audit plugin contract ([`MiniChatAuditPluginClientV1`]) and audit
//!   event types;
//! - the usage event published after every settled turn ([`UsageEvent`]);
//! - the GTS plugin specs used to register and discover plugin instances.

pub mod audit;
pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use audit::{
    AuditEvent, AuditUsage, LatencyMs, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, TurnMutationAuditEvent,
};
pub use error::{AuditPluginError, PolicyError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, TierLimits, UsageEvent, UsageTokens, UserLicenseStatus,
    UserLimits, WebSearchContextSize,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_accepts_capitalized() {
        let t: ModelTier = serde_json::from_str("\"Premium\"").unwrap();
        assert_eq!(t, ModelTier::Premium);
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"premium\"");
    }

    #[test]
    fn catalog_entry_defaults() {
        let e: ModelCatalogEntry =
            serde_json::from_value(serde_json::json!({"id": "m", "tier": "standard"})).unwrap();
        assert!(!e.enabled);
        assert_eq!(e.max_tool_calls, 2);
        assert_eq!(e.estimation_budgets.bytes_per_token_conservative, 4);
    }

    #[test]
    fn default_model_prefers_is_default() {
        let snap: PolicySnapshot = serde_json::from_value(serde_json::json!({
            "policy_version": 1,
            "model_catalog": [
                {"id": "a", "tier": "premium", "enabled": true},
                {"id": "b", "tier": "standard", "enabled": true, "preference": {"is_default": true, "sort_order": 0}},
                {"id": "c", "tier": "standard", "enabled": false, "preference": {"is_default": true, "sort_order": 0}}
            ]
        }))
        .unwrap();
        assert_eq!(snap.default_model().unwrap().id, "b");
    }

    #[test]
    fn usage_event_omits_user_for_system() {
        let ev = UsageEvent {
            tenant_id: uuid::Uuid::nil(),
            user_id: None,
            chat_id: uuid::Uuid::nil(),
            turn_id: None,
            request_id: uuid::Uuid::nil(),
            effective_model: "m".into(),
            selected_model: "m".into(),
            terminal_state: "completed".into(),
            billing_outcome: "system_task".into(),
            usage: Some(UsageTokens::default()),
            actual_credits_micro: 0,
            settlement_method: "none".into(),
            policy_version_applied: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
            requester_type: "system".into(),
            dedupe_key: "k".into(),
            system_task_type: Some("thread_summary_update".into()),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert!(v.get("user_id").is_none());
        assert!(v.get("turn_id").is_none());
        assert_eq!(v["system_task_type"], "thread_summary_update");
    }
}
