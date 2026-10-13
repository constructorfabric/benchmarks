//! `mini-chat` gear: multi-tenant AI chat with SSE streaming, attachments and quotas.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
