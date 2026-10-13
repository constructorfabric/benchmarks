//! `mini-chat` gear: multi-tenant AI chat with SSE streaming, attachments and
//! credit quotas (see `gears/mini-chat/docs`).

pub mod config;
pub mod domain;
pub mod infra;
pub mod api;
pub mod gear;

pub use gear::MiniChatGear;

#[cfg(test)]
pub(crate) mod test_support;
