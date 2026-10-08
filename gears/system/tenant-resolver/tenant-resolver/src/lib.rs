//! Tenant Resolver Gear
//!
//! This gear discovers tenant resolver plugins via types-registry
//! and routes API calls to the selected plugin based on vendor configuration.
//!
//! The gear provides the `TenantResolverClient` trait registered
//! in `ClientHub` for consumption by other gears.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod domain;
pub mod gear;
