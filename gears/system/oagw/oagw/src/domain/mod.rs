//! Domain layer: pure business logic, no `axum`/`hyper`/network types.

pub mod alias;
pub mod dto;
pub mod error;
pub mod gts_helpers;
pub mod plugin;
pub mod repo;
pub mod services;
