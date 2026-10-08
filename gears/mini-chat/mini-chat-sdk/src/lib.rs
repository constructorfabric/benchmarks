//! Mini Chat SDK.
//!
//! Public contracts of the `mini-chat` gear:
//! - the model-policy plugin contract ([`MiniChatModelPolicyPluginClientV1`]) with the
//!   policy snapshot, model catalog, kill switch and user-limit models;
//! - the audit plugin contract ([`MiniChatAuditPluginClientV1`]) and its events;
//! - the usage settlement event ([`UsageEvent`]) published through the outbox;
//! - the GTS plugin specifications used for plugin discovery in types-registry.

#![forbid(unsafe_code)]

pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, LatencyMs, MiniChatAuditEvent, ModelApiParams,
    ModelCatalogEntry, ModelFeatures, ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints,
    ModelTier, ModelToolSupport, PolicyDecisions, PolicySnapshot, PolicyVersionInfo,
    QuotaPolicyDecision, TierLimits, ToolCalls, TurnAuditEvent, TurnMutationAuditEvent, UsageEvent,
    UsageTokens, UserLicenseStatus, UserLimits, WebSearchContextSize,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
