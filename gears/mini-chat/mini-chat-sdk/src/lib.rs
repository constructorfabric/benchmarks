//! SDK of the mini-chat gear: policy snapshot, usage and audit models, plugin
//! API traits and GTS plugin specifications.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-sdk-crate:p1

pub mod gts;
pub mod models;
pub mod plugin_api;

pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    ApiParams, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, AuditUsage,
    BillingOutcome, DecodeError, EncodeError, EstimationBudgets, KillSwitches, MiniChatAuditEvent,
    ModelCatalogEntry, ModelFeatures, ModelGeneralConfig, ModelPreference, ModelTier,
    ModelToolSupport, PolicySnapshot, PolicyVersionInfo, RequesterType, SettlementMethod,
    SupportedEndpoints, SystemTaskType, TerminalState, TierLimits, TurnAuditEvent,
    TurnAuditEventType, TurnMutationAuditEvent, UsageEvent, UsageTokens, UserLicenseStatus,
    UserLimits, WebSearchContextSize, decode_audit_event, decode_model_catalog_entry,
    decode_policy_snapshot, decode_usage_event, encode_audit_event, encode_usage_event,
    system_task_dedupe_key, usage_dedupe_key,
};
pub use plugin_api::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    PolicyPluginError, PublishError,
};
