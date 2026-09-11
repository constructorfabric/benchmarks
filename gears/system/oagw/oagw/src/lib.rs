//! The Outbound API Gateway (`oagw`): a gear that lets a tenant's workloads call an
//! external service through a tenant-scoped alias instead of a raw endpoint.
//!
//! The control plane (`/oagw/v1/upstreams`, `/routes`, `/plugins`) is a CRUD surface over
//! the gateway's configuration. The data plane (`/oagw/v1/proxy/{alias}/…`) resolves an
//! alias against the caller's tenant chain, matches a route, validates and rate-limits the
//! request, runs the plugin chain and relays the result — streaming for server-sent events
//! and spliced end to end for WebSocket upgrades.
//!
//! Credential material never appears in configuration, in a log record, in an error
//! document or in an API response: the configuration carries a `cred://` reference and the
//! material is fetched just before the upstream call.

pub mod api;
pub mod config;
pub mod domain;
pub mod error;
pub mod gear;
pub mod plugins;
pub mod proxy;
pub mod ratelimit;
pub mod security;
pub mod store;
pub mod types;

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;

#[cfg(test)]
#[path = "types_tests.rs"]
mod types_tests;
