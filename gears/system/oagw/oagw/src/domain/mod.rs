//! Domain layer of the `oagw` gear (DDD-Light).
//!
//! Nothing in here may depend on a transport (`axum`, `http`) or persistence
//! crate: the module owns the model, the alias and validation rules, the plugin
//! contracts, the repository traits and the control-plane service. Infrastructure
//! implementations live in [`crate::infra`], the transport in [`crate::api`].

pub mod alias;
pub mod dto;
pub mod error;
pub mod gts_helpers;
pub mod model;
pub mod plugin;
pub mod repo;
pub mod services;
pub mod validation;
