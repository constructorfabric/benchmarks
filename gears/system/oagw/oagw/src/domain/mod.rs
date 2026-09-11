// Updated: 2026-09-01 by Constructor Tech
//! The domain layer: models, the error vocabulary and the management service.
//!
//! Nothing here depends on axum, on storage or on any external gear. The
//! services take their collaborators as traits, so the REST layer and the
//! in-memory store can be swapped without touching a rule.

pub mod dto;
pub mod error;
pub mod plugin;
pub mod repo;
pub mod services;
