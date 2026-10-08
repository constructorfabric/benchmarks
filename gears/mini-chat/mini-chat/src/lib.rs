//! Mini Chat gear: multi-tenant AI chat (`gears/mini-chat/docs`).
//!
//! The crate also hosts the two bundled plugin gears
//! ([`infra::plugins::static_model_policy`], [`infra::plugins::static_audit`]).

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
#[doc(hidden)]
pub mod test_support;

pub use gear::MiniChatGear;
