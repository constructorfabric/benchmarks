//! `static-mini-chat-model-policy-plugin`: serves a model catalog, kill
//! switches and per-user limits from its own config section (identical for
//! every user, policy version `1`).

mod config;
mod gear;
mod service;

pub use config::{StaticKillSwitches, StaticModelPolicyConfig};
pub use gear::StaticModelPolicyGear;
pub use service::{StaticModelPolicyError, StaticModelPolicyService};
