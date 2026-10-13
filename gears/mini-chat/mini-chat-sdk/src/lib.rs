//! Public contracts of the mini-chat gear.
//!
//! * [`catalog`] - the model catalog entry and its nested types,
//! * [`policy`] - policy snapshot, kill switches, per-user limits,
//! * [`usage`] - the usage event published to the model-policy plugin,
//! * [`audit`] - the audit events delivered to the audit plugin,
//! * [`plugin`] - the plugin client traits and their error types,
//! * [`gts`] - the GTS plugin specs used for plugin discovery.
//!
//! The crate carries no implementation: the gear (`cf-gears-mini-chat`) and
//! the plugins depend on it, never on each other.

pub mod audit;
pub mod catalog;
pub mod gts;
pub mod plugin;
pub mod policy;
pub mod usage;

pub use audit::{AuditEvent, TurnAuditEvent};
pub use catalog::{
    EstimationBudgets, ModelApiParams, ModelCatalogEntry, ModelFeatures, ModelGeneralConfig,
    ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport, WebSearchContextSize,
};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use plugin::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    ModelPolicyPluginError, PublishError,
};
pub use policy::{
    KillSwitches, PolicySnapshot, PolicyVersionInfo, TierLimits, UserLicenseStatus, UserLimits,
};
pub use usage::{
    BillingOutcome, RequesterType, SettlementMethod, SystemTaskType, TerminalState, UsageEvent,
    UsageTokens, system_task_dedupe_key, turn_dedupe_key,
};
