//! The data plane: header transformation, rate limiting, CORS and proxying.
pub mod cors;
pub mod headers;
pub mod rate_limit;
pub mod service;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod service_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod websocket_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod plugin_chain_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod auth_plugin_tests;
