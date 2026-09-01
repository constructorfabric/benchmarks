//! Domain layer for the Types Registry gear.
//!
//! Contains business logic, error types, and repository traits.

pub mod error;
pub mod model;
pub mod repo;
pub mod service;
// === LOCAL CLIENT ===
pub mod local_client;

pub use error::DomainError;
pub use repo::GtsRepository;
pub use service::TypesRegistryService;
