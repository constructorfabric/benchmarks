//! `mini-chat` gear: multi-tenant AI chat (see `gears/mini-chat/docs`).

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
