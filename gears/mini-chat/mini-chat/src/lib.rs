//! Mini Chat gear: multi-tenant AI chat with SSE streaming, attachments and
//! quotas, backed by in-process provider adapters through OAGW.
//!
//! The public SDK lives in `mini_chat_sdk` and is re-exported here.

pub use mini_chat_sdk::*;

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
