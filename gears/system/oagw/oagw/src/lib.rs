//! OAGW — outbound API gateway gear.
//!
//! Control-plane phase: upstream/route management REST API, domain model,
//! alias rules, error taxonomy, plugin contracts and a minimal proxy path.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

// === MODULE DEFINITION ===
pub mod gear;
pub use gear::{OagwGear, OagwState};

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;
