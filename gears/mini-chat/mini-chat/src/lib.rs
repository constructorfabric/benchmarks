//! `mini-chat` gear: multi-tenant AI chat REST + SSE API.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

#[doc(hidden)]
pub mod testing;

pub use gear::MiniChatGear;
