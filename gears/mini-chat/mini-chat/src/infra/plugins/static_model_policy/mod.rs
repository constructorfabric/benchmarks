//! Bundled static model policy plugin
//! (`cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`).

pub mod config;
pub mod gear;
pub mod service;

pub use config::{StaticModelPolicyConfig, StaticModelPolicyConfigError};
pub use gear::StaticModelPolicyPlugin;
pub use service::StaticModelPolicyService;
