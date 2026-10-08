//! Mini Chat SDK
//!
//! Public, transport-agnostic contract of the `mini-chat` gear:
//!
//! - [`MiniChatModelPolicyPluginClientV1`] — model policy plugin API (policy
//!   snapshot with the model catalog and kill switches, per-user limits, usage
//!   publication).
//! - [`MiniChatAuditPluginClientV1`] — audit plugin API (turn and turn-mutation
//!   audit events).
//! - [`MiniChatModelPolicyPluginSpecV1`], [`MiniChatAuditPluginSpecV1`] — GTS
//!   plugin specifications used for plugin discovery through types-registry.
//! - Models: [`PolicySnapshot`], [`ModelCatalogEntry`], [`KillSwitches`],
//!   [`UserLimits`], [`UsageEvent`], [`AuditEvent`].

pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
pub use gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
pub use models::*;
pub use plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
