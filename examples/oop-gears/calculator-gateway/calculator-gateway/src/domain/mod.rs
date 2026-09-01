//! Domain layer for calculator_gateway gear
//!
//! Contains business logic and service orchestration.

pub mod service;

pub use service::{Service, ServiceError};
