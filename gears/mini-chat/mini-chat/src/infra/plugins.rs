//! Plugins bundled with the gear. Each one is its own gear (registered under
//! its own name) that registers a GTS plugin instance in the types-registry
//! and a scoped client in the `ClientHub`.

pub mod static_audit;
pub mod static_model_policy;
