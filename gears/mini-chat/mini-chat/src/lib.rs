//! `mini-chat` gear: multi-tenant AI chat with SSE streaming, attachments and quotas.

pub mod api;
pub mod clock;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

#[cfg(test)]
pub mod testing;

pub use gear::MiniChatGear;
