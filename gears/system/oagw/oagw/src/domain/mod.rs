//! Business logic: models, validation, service traits and plugin contracts.
//!
//! Nothing in this module depends on Axum, SeaORM or the proxy engine — the
//! DDD-Light rule from `docs/DESIGN.md` §1.3.

pub mod alias;
pub mod cors;
pub mod error;
pub mod gts_helpers;
pub mod merge;
pub mod model;
pub mod plugin;
pub mod ratelimit;
pub mod repo;
pub mod services;

pub use error::{DomainError, DomainResult, ErrorSource};
