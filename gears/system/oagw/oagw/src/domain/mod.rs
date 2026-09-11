//! Domain layer for the OAGW gear.
//!
//! Holds the entities, the domain errors and the repository contracts. The
//! layer has no dependency on `infra` and no dependency on HTTP types: the
//! transport layer (`crate::api::rest`) maps between HTTP and these types.

pub mod alias;
pub mod identity;
pub mod plugin;
pub mod query;
pub mod sharing;
pub mod error;
pub mod model;
pub mod repo;
pub mod services;
pub mod stream;
pub mod validation;

pub use error::DomainError;
pub use repo::UpstreamRepository;
