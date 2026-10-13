#![doc = include_str!("../README.md")]

pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use error::{AuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    AuditEvent, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, EstimationBudgets,
    KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures, ModelGeneralConfig,
    ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport, PolicySnapshot,
    PolicyVersionInfo, TierLimits, TurnAuditEvent, TurnMutationAuditEvent, UsageEvent,
    UsageTokens, UserLicenseStatus, UserLimits, VISION_INPUT,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};

#[cfg(test)]
mod models_tests;
