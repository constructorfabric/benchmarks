//! Mini Chat gear: multi-tenant AI chat with SSE streaming, attachments and
//! quotas. The public plugin contracts live in `mini-chat-sdk`.

pub use mini_chat_sdk;

pub(crate) mod api;
pub mod config;
pub(crate) mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
