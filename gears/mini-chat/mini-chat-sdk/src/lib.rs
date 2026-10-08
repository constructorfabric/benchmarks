//! SDK of the `mini-chat` gear.
//!
//! Defines the plugin contracts the gear consumes (model policy and audit)
//! and the models shared between the gear and its plugins: the model
//! catalog, kill switches, per-user credit limits, usage events and audit
//! events.

pub mod audit;
pub mod gts;
pub mod models;
pub mod plugin_api;
pub mod usage;

pub use audit::{
    AuditPluginError, MiniChatAuditEvent, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, TurnMutationAuditEvent,
};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    ApiParams, EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelTier, ModelToolSupport, PolicySnapshot,
    PolicyVersionInfo, SupportedEndpoints, TierLimits, UserLicenseStatus, UserLimits,
    WebSearchContextSize,
};
pub use plugin_api::{
    MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1, PolicyPluginError,
    PublishError,
};
pub use usage::{UsageEvent, UsageTokens};
