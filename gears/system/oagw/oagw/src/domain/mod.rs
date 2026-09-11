//! Domain layer: resource model, alias rules, validation, repository and
//! plugin contracts, and the control-plane / data-plane services.
//!
//! The domain layer has no transport or infrastructure dependency: it is
//! expressible without `axum`, without `hyper` and without any peer-gear SDK,
//! which is what makes the acceptance behaviour testable in isolation.

pub mod alias;
pub mod error;
pub mod gts_helpers;
pub mod model;
pub mod plugin;
pub mod repo;
pub mod services;
pub mod type_catalog;
pub mod validation;

pub use error::DomainError;
pub use model::{Endpoint, Plugin, Route, Upstream};
