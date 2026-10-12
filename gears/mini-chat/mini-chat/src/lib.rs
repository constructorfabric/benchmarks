//! `mini-chat`: multi-tenant AI chat gear.
//!
//! The public contracts (plugin traits, GTS specs, policy and usage models) live in
//! `cf-gears-mini-chat-sdk` and are re-exported here.

// === PUBLIC API (from the SDK) ===
pub use mini_chat_sdk::{
    AuditPluginError, BillingOutcome, EstimationBudgets, KillSwitches, MiniChatAuditPluginClientV1,
    MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1,
    ModelCatalogEntry, ModelGeneralConfig, ModelPolicyPluginError, ModelTier, PolicySnapshot,
    PublishError, SettlementMethod, TierLimits, TurnAuditEvent, TurnMutationAuditEvent, UsageEvent,
    UserLimits,
};

// === GEAR ===
pub mod gear;
pub use gear::MiniChatGear;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
pub mod config;
#[doc(hidden)]
pub mod domain;
pub mod infra;
pub mod services;

/// Scaffold for the black-box integration tests; not part of the public API.
#[doc(hidden)]
pub mod test_support;
