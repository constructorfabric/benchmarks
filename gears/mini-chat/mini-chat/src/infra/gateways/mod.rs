//! Gear-side gateways to the model policy and audit plugins.

pub mod audit;
pub mod model_policy;
mod plugin_select;

#[cfg(test)]
#[path = "gateways_tests.rs"]
mod gateways_tests;
