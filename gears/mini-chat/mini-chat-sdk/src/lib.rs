//! SDK of the `mini-chat` gear: the transport-agnostic contract shared by the
//! gear and its plugins (model policy and audit), and the models they exchange.

pub mod audit;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use audit::{
    AuditEvent, LatencyMs, PolicyDecisions, QuotaDecisionAudit, ToolCalls, TurnAuditEvent,
    TurnMutationAuditEvent,
};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, TierLimits, UsageEvent, UsageTokens, UserLicenseStatus, UserLimits,
};
pub use plugin_api::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, PublishError,
};

#[cfg(test)]
mod tests;
