//! Domain layer: models, validation rules and the two internal services.
//!
//! Nothing here depends on the infrastructure layer; the ports in
//! [`repo`] and [`ports`] are the only way out.

pub mod alias;
pub mod error;
pub mod gts_helpers;
pub mod model;
pub mod plugin;
pub mod ports;
pub mod repo;
pub mod services;
pub mod timeutil;
pub mod validate;
