//! Mini Chat SDK
//!
//! This crate provides the public API surface of the mini-chat gear:
//!
//! - [`MiniChatModelPolicyPluginClientV1`] — model policy plugin contract
//!   (policy snapshots, per-user limits, usage publication)
//! - [`MiniChatAuditPluginClientV1`] — audit plugin contract
//! - Policy models ([`PolicySnapshot`], [`ModelCatalogEntry`],
//!   [`KillSwitches`], [`UserLimits`]) and the [`UsageEvent`] settlement
//!   payload
//! - [`TurnAuditEvent`] audit payload
//! - GTS plugin specifications ([`MiniChatModelPolicyPluginSpecV1`],
//!   [`MiniChatAuditPluginSpecV1`]) for types-registry discovery
//!
//! Plugins register a GTS instance with the types-registry and a scoped
//! client with the `ClientHub`; the gear resolves them at runtime.

#![forbid(unsafe_code)]

pub mod api;
pub mod errors;
pub mod gts;
pub mod models;

pub use api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
pub use errors::{AuditError, ModelPolicyError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::{
    ApiParams, EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelFeatures,
    ModelGeneralConfig, ModelPreference, ModelTier, ModelToolSupport, PolicyDecisions,
    PolicySnapshot, QuotaAuditDecision, SupportedEndpoints, TierLimits, ToolCalls,
    TurnAuditEvent, UsageEvent, UsageTokens, UserLimits, WebSearchContextSize,
};
