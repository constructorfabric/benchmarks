//! SDK for the mini-chat gear: plugin contracts (model policy, audit) and the
//! payload types shared with the gear and its bundled static plugins.

pub mod audit;
pub mod gts;
pub mod models;
pub mod plugin_api;
pub mod usage;

pub use audit::{
    MiniChatAuditEvent, PolicyDecisions, QuotaPolicyDecision, ToolCalls, TurnAuditEvent,
    TurnMutationAuditEvent,
};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, TierLimits, UserLicenseStatus, UserLimits,
};
pub use plugin_api::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, PublishError,
};
pub use usage::{UsageEvent, UsageTokens};
