//! Mini-chat SDK.
//!
//! Public, transport-agnostic contracts of the mini-chat gear:
//!
//! - [`plugin_api`] — plugin client traits resolved by the gear through
//!   types-registry and scoped `ClientHub`:
//!   [`MiniChatModelPolicyPluginClientV1`] (policy snapshots, user limits,
//!   usage publication) and [`MiniChatAuditPluginClientV1`] (audit delivery),
//!   with their error types.
//! - [`models`] — policy snapshot and model catalog types, per-user limits,
//!   the usage outbox payload [`UsageEvent`] with its dedupe key helpers, and
//!   the audit outbox payload [`AuditEvent`].
//! - [`gts`] — GTS plugin specs ([`MiniChatModelPolicyPluginSpecV1`],
//!   [`MiniChatAuditPluginSpecV1`]) and the gear's GTS resource-type ids
//!   ([`resource_types`]).
//!
//! A plugin registers a `PluginV1<Spec>` instance and its scoped client:
//!
//! ```ignore
//! let instance_id = MiniChatAuditPluginSpecV1::gts_make_instance_id(
//!     "constructorfabric.mini_chat.static_audit.v1",
//! );
//! ctx.client_hub().register_scoped::<dyn MiniChatAuditPluginClientV1>(
//!     ClientScope::gts_id(&instance_id),
//!     plugin,
//! );
//! ```

pub mod gts;
pub mod models;
pub mod plugin_api;

pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1, resource_types};
pub use models::{
    AuditEvent, EstimationBudgets, KillSwitches, ModelApiParams, ModelCatalogEntry,
    ModelGeneralConfig, ModelPreference, ModelTier, ModelToolSupport, PolicyDecisions,
    PolicySnapshot, QuotaDecision, TierLimits, ToolCalls, TurnAuditEvent, TurnAuditEventType,
    TurnMutationAuditEvent, TurnMutationAuditEventType, UsageEvent, UsageTokens, UserLicenseStatus,
    UserLimits, dedupe_key_for_system_task, dedupe_key_for_turn,
};
pub use plugin_api::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, PublishError,
};
