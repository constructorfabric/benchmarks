//! SDK of the `mini-chat` gear.
//!
//! Holds the transport-agnostic contract shared between the gear and its
//! plugins: the model catalog and policy snapshot types served by the model
//! policy plugin, the usage and audit event payloads, the plugin client
//! traits and their GTS plugin specifications.

pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use error::{AuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use gts::{
    ATTACHMENT_RESOURCE_TYPE, CHAT_AUTHZ_RESOURCE_TYPE, CHAT_RESOURCE_TYPE,
    MESSAGE_RESOURCE_TYPE, MODEL_AUTHZ_RESOURCE_TYPE, MODEL_RESOURCE_TYPE,
    MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1, TURN_RESOURCE_TYPE,
    USER_QUOTA_AUTHZ_RESOURCE_TYPE,
};
pub use models::{
    ApiParams, AuditEvent, AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls,
    AuditUsage, EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, TierLimits, TurnAuditEvent, TurnMutationAuditEvent,
    UsageEvent, UsageTokens, UserLicenseStatus, UserLimits, WebSearchContextSize,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
