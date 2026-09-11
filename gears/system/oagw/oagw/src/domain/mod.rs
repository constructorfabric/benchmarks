//! Domain model of the gateway: types, validation, aliases, plugins, repositories.

pub mod alias;
pub mod error;
pub mod gts_helpers;
pub mod model;
pub mod plugin;
pub mod repo;
pub mod service;
pub mod validation;

pub use error::{DomainError, ProblemMeta};
pub use service::ControlPlane;
