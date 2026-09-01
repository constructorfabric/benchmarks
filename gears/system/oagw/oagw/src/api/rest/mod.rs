//! REST surface of the OAGW gear.
//!
//! Slice 2 installs the management routes (`/oagw/v1/upstreams`,
//! `/oagw/v1/routes`, `/oagw/v1/plugins`) and slice 4 the proxy routes, both
//! layered through [`error_source_layer`].

pub mod dto;
pub mod error_layer;
pub mod extractors;
pub mod handlers;
pub mod routes;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

pub use error_layer::error_source_layer;
