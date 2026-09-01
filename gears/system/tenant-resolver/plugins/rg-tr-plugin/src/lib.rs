//! RG Tenant Resolver Plugin
//!
//! Resolves tenant hierarchy via the Resource Group gear's
//! `ResourceGroupReadHierarchy` trait. Production replacement for
//! `static-tr-plugin`: tenants are RG groups whose GTS type code starts
//! with `TENANT_RG_TYPE_PATH` (see `resource_group_sdk`). Metadata
//! contains `status` and `self_managed` fields.
//!
//! ## Configuration
//!
//! ```yaml
//! gears:
//!   rg_tr_plugin:
//!     config:
//!       vendor: "constructorfabric"
//!       priority: 50
//! ```
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod domain;
pub mod gear;

pub use gear::RgTrPlugin;
