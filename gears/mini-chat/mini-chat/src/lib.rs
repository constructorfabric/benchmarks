//! Mini-chat gear: multi-tenant AI chat with SSE streaming, attachments and
//! quotas.

pub use mini_chat_sdk;

#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod gear;
#[doc(hidden)]
pub mod infra;

pub use gear::MiniChatGear;
