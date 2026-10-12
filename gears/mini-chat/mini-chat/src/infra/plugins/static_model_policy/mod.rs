//! Static model-policy plugin: gear `static-mini-chat-model-policy-plugin`.
//!
//! Serves a fixed policy snapshot (version 1) built from its own configuration: the model
//! catalog, the kill switches and identical default limits for every user. `publish_usage` only
//! logs.

mod config;
mod gear;
mod service;

pub use config::{KillSwitchesConfig, StaticModelPolicyConfig};
pub use gear::StaticModelPolicyPlugin;
pub use service::{STATIC_POLICY_VERSION, StaticModelPolicyService};
