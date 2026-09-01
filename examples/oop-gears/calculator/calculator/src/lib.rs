//! Calculator Gear
//!
//! A trivial example gRPC service that performs addition.
//! This gear demonstrates the OoP (out-of-process) gear pattern.
//!
//! ## Architecture
//!
//! - `domain/service.rs` - Core business logic
//! - `api/grpc/server.rs` - gRPC server implementation
//! - `gear.rs` - Gear registration and lifecycle
//!
//! External consumers should use `calculator-sdk` crate which provides
//! the gRPC client and `wire_client()` for ClientHub integration.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// === MODULE DEFINITION ===
mod gear;
pub use gear::CalculatorGear;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod domain;
