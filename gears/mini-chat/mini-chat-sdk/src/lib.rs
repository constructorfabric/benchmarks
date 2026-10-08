//! Mini Chat SDK
//!
//! Public contract of the `mini-chat` gear towards its plugins:
//!
//! - [`MiniChatModelPolicyPluginClientV1`] — model policy plugin (catalog,
//!   kill switches, per-user limits, usage ingestion)
//! - [`MiniChatAuditPluginClientV1`] — audit plugin
//! - models ([`PolicySnapshot`], [`ModelCatalogEntry`], [`UsageEvent`],
//!   [`MiniChatAuditEvent`], ...)
//! - errors ([`PolicyPluginError`], [`PublishError`], [`AuditPluginError`])
//! - GTS specs ([`MiniChatModelPolicyPluginSpecV1`], [`MiniChatAuditPluginSpecV1`])

pub mod errors;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use errors::{AuditPluginError, PolicyPluginError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    ApiParams, AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls,
    BillingOutcome, EstimationBudgets, KillSwitches, MiniChatAuditEvent, ModelCatalogEntry,
    ModelFeatures, ModelGeneralConfig, ModelPreference, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, RequesterType, SettlementMethod, SupportedEndpoints,
    TerminalState, TierLimits, TurnAuditEvent, TurnAuditEventType, TurnMutationAuditEvent,
    TurnMutationAuditEventType, UsageEvent, UsageTokens, UserLicenseStatus, UserLimits,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
