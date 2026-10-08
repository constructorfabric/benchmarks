//! Mini Chat SDK
//!
//! Public, transport-agnostic surface of the `mini-chat` gear:
//! - plugin traits (`MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`)
//! - GTS plugin specs used for plugin discovery through types-registry
//! - policy snapshot / model catalog / user limit models
//! - usage and audit event payloads

#![forbid(unsafe_code)]

pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, MiniChatAuditEvent, ModelApiParams, ModelCatalogEntry,
    ModelFeatures, ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier,
    ModelToolSupport, PolicyDecisions, PolicySnapshot, PolicyVersionInfo, QuotaPolicyDecision,
    TierLimits, ToolCalls, TurnAuditEvent, TurnMutationAuditEvent, UsageEvent, UsageTokens,
    UserLicenseStatus, UserLimits,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
