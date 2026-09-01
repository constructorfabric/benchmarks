//! Calculator Gateway Gear
//!
//! An in-process gear that exposes a REST API for addition.
//! It delegates the actual computation to the calculator service via gRPC.
//!
//! ## Architecture
//!
//! - `Service` contains the domain logic
//! - REST handlers call Service directly
//! - External consumers use the SDK (`calculator_gateway-sdk`) which provides
//!   `CalculatorGatewayClient` trait and `wire_client()` for ClientHub integration

// === MODULE DEFINITION ===
mod gear;
pub use gear::CalculatorGateway;

// === PUBLIC EXPORTS (for SDK) ===
pub mod domain;
pub use domain::{Service, ServiceError};

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
