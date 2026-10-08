//! Mini Chat SDK
//!
//! Transport-agnostic public surface of the `mini-chat` gear:
//!
//! - [`MiniChatModelPolicyPluginClientV1`] / [`MiniChatAuditPluginClientV1`] —
//!   plugin traits resolved by the gear through types-registry.
//! - [`PolicySnapshot`], [`ModelCatalogEntry`], [`UserLimits`] — policy models.
//! - [`UsageEvent`] — usage settlement event (usage outbox payload).
//! - [`AuditEvent`] — audit events (audit outbox payload).
//! - [`MiniChatModelPolicyPluginSpecV1`] / [`MiniChatAuditPluginSpecV1`] — GTS
//!   plugin specs.

pub mod audit;
pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;
pub mod usage;

pub use audit::{
    AuditEvent, AuditLatency, AuditUsage, PolicyDecisions, QuotaDecisionAudit, ToolCalls,
    TurnAuditEvent, TurnAuditEventType, TurnMutationAuditEvent, TurnMutationEventType,
};
pub use error::{AuditPluginError, PolicyError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, TierLimits, UserLicenseStatus, UserLimits, VISION_INPUT,
    WebSearchContextSize,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
pub use usage::{UsageEvent, UsageTokens, system_task_dedupe_key, turn_dedupe_key};
