//! Domain layer: model, validation, matching, merging and services.
//!
//! Nothing here depends on [`crate::infra`] or [`crate::api`].

pub mod alias;
pub mod error;
pub mod headers;
pub mod identifiers;
pub mod match_route;
pub mod merge;
pub mod model;
pub mod plugin;
pub mod ratelimit;
pub mod repo;
pub mod services;
pub mod validation;
