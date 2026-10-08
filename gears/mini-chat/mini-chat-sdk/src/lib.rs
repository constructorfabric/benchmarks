//! Mini-chat SDK.
//!
//! Public, transport-agnostic contracts of the `mini-chat` gear:
//!
//! - the model-policy plugin contract ([`MiniChatModelPolicyPluginClientV1`])
//!   with the policy snapshot, model catalog, kill switches and per-user
//!   limits it serves, and the usage events it receives;
//! - the audit plugin contract ([`MiniChatAuditPluginClientV1`]) with the
//!   turn and turn-mutation audit events;
//! - the GTS plugin specifications used to discover plugin instances through
//!   the types-registry.

#![forbid(unsafe_code)]

pub mod audit;
pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;
pub mod usage;

pub use audit::{
    AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, AuditUsage,
    TurnAuditEvent, TurnAuditEventType, TurnMutationAuditEvent, TurnMutationKind,
};
pub use error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, TierLimits, UserLicenseStatus, UserLimits,
    WebSearchContextSize,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
pub use usage::{UsageEvent, UsageTokens};
