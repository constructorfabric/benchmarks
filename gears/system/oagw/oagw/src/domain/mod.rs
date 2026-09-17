//! Business logic of the `oagw` gear.
//!
//! This layer is the *only* place that knows the OAGW rules (alias
//! enforcement, tenant scoping, configuration layering, plugin ordering). It
//! depends on nothing from `infra/` — infrastructure implements the traits
//! declared here.

pub mod dto;
pub mod error;
pub mod gts_helpers;
pub mod model;
pub mod plugin;
pub mod services;
