//! Mini Chat gear: multi-tenant AI chat with SSE streaming, attachments,
//! quotas and thread summaries (see `gears/mini-chat/docs`).

pub use mini_chat_sdk;

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
