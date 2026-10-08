//! `static-mini-chat-model-policy-plugin`: serves a fixed policy snapshot (version 1)
//! from its own configuration; `publish_usage` only logs.

mod config;
mod gear;
mod service;

pub use config::{StaticKillSwitches, StaticModelPolicyConfig};
pub use gear::StaticMiniChatModelPolicyPlugin;
pub use service::StaticModelPolicyService;
