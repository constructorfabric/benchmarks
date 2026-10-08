//! SDK of the `mini-chat` gear: plugin contracts (model policy, audit), the
//! policy snapshot / catalog / limits types and the usage and audit events.

pub mod error;
pub mod events;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use events::{
    AuditUsage, MiniChatAuditEvent, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, TurnMutationAuditEvent, UsageEvent, UsageTokens,
};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, TierLimits, UserLicenseStatus, UserLimits, VISION_INPUT,
    WebSearchContextSize,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
