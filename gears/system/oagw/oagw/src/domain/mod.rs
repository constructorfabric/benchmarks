//! OAGW domain layer: model, alias rules, error taxonomy, service operations
//! and repository ports.
//!
//! The layer is transport-agnostic — the REST handlers in
//! [`crate::api::rest`] translate axum primitives into these calls and back.

pub mod alias;
pub mod error;
pub mod models;
pub mod plugin;
pub mod repo;
pub mod services;

pub use error::DomainError;
pub use repo::Repositories;
