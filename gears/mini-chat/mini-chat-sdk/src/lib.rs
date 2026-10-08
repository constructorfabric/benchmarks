//! SDK of the `mini-chat` gear: model catalog, policy snapshot, usage and audit events and the
//! plugin contracts shared between the gear and its policy / audit plugins.

pub mod events;
pub mod gts;
pub mod models;
pub mod plugin_api;
pub mod policy;

pub use events::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent, TurnAuditEvent,
    TurnMutationAuditEvent, UsageEvent, UsageTokens,
};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, ModelApiParams, ModelCatalogEntry, ModelFeatures, ModelGeneralConfig, ModelPreference,
    ModelSupportedEndpoints, ModelTier, ModelToolSupport, VISION_INPUT, WebSearchContextSize,
};
pub use plugin_api::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1, PolicyPluginError,
    PublishError,
};
pub use policy::{KillSwitches, PolicySnapshot, PolicyVersionInfo, TierLimits, UserLicenseStatus, UserLimits};
