//! Static model policy plugin (`static-mini-chat-model-policy-plugin`): serves a fixed
//! snapshot (`policy_version` 1) from its own configuration.

pub mod config;
pub mod gear;
pub mod service;

pub use config::StaticModelPolicyConfig;
pub use gear::StaticMiniChatModelPolicyPlugin;
pub use service::StaticModelPolicyService;
