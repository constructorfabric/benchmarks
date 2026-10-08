//! Mini Chat gear: multi-tenant AI chat with SSE streaming, attachments and quotas.
//!
//! The public plugin contracts and models live in `mini-chat-sdk` and are
//! re-exported here.

pub use mini_chat_sdk;
pub use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1, ModelCatalogEntry,
    PolicySnapshot, UsageEvent,
};

pub mod gear;
pub use gear::MiniChatGear;

#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;
