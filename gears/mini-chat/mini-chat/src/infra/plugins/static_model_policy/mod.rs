//! Static model policy plugin (gear `static-mini-chat-model-policy-plugin`):
//! serves a fixed policy snapshot (version 1) from its own configuration.

pub mod config;
mod gear;
pub mod service;

pub use gear::StaticModelPolicyPluginGear;
