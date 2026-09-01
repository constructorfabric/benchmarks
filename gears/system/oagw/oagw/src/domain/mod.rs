//! Domain layer: entities, business rules, error catalogue and repository
//! contracts.
//!
//! This module must never depend on [`crate::api`] or [`crate::infra`]; the
//! dependency direction is enforced by the module tree (`domain` is declared
//! before and independently of the other two).

pub mod alias;
pub mod body;
pub mod cors;
pub mod error;
pub mod merge;
pub mod headers;
pub mod models;
pub mod plugin;
pub mod rate_limit;
pub mod repo;
pub mod routing;
pub mod service;
pub mod target;
pub mod time;
