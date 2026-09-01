// Created: 2026-08-29 by Constructor Tech
//! OAGW domain layer: model, validation, alias derivation, hierarchical
//! merge, rate-limit maths, plugin contracts and the control-plane service.
//!
//! The domain never touches `axum::extract` / `toolkit` wiring; it only uses
//! `axum::http` primitives for status codes and header maps.

pub mod alias;
pub mod error;
pub mod merge;
pub mod model;
pub mod plugin;
pub mod ports;
pub mod rate_limit;
pub mod repo;
pub mod services;
