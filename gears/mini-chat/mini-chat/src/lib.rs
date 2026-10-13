//! `mini-chat` gear: multi-tenant AI chat with SSE streaming, attachments and credit quotas.
//!
//! The crate also hosts the bundled plugins `static-mini-chat-model-policy-plugin`
//! (`infra::plugins::static_model_policy`) and `static-mini-chat-audit-plugin`
//! (`infra::plugins::static_audit`).

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
