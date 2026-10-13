//! Mini-chat SDK.
//!
//! - Models: [`ModelCatalogEntry`], [`PolicySnapshot`], [`KillSwitches`], limits
//! - [`UsageEvent`] - usage settlement event
//! - [`MiniChatAuditEvent`] - audit events
//! - [`MiniChatModelPolicyPluginClientV1`], [`MiniChatAuditPluginClientV1`] - plugin traits
//! - [`MiniChatModelPolicyPluginSpecV1`], [`MiniChatAuditPluginSpecV1`] - GTS plugin specs

// Wire shapes are fixed by the DESIGN docs: flag-heavy structs and `*_tokens` field names are intended.
#![allow(
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    clippy::large_enum_variant
)]

pub mod audit;
pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;
pub mod usage;

pub use audit::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent, TurnAuditEvent,
    TurnAuditEventType, TurnMutationAuditEvent,
};
pub use error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    PolicySnapshot, PolicyVersionInfo, TierLimits, UserLicenseStatus, UserLimits,
};
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
pub use usage::{UsageEvent, UsageTokens};
