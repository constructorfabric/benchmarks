//! Bundled static model policy plugin (`static-mini-chat-model-policy-plugin`):
//! serves a fixed policy snapshot (version 1) and fixed per-user limits from
//! its own configuration; `publish_usage` only logs.

pub mod config;
mod gear;
pub mod service;

pub use gear::StaticModelPolicyPlugin;
