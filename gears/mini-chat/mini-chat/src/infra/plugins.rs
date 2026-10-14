//! Bundled plugins. The modules stay public and reachable as
//! `mini_chat::infra::plugins::{static_audit, static_model_policy}` because the example
//! server imports both to link the plugin registrations.

pub mod static_audit;
pub mod static_model_policy;
