//! The proxy data plane's HTTP transport.
//!
//! * [`error`] — [`ProxyFailure`] → RFC 9457 `application/problem+json`.
//! * [`handler`] — the one handler behind `/oagw/v1/proxy/{*proxy_path}`.
//! * [`routes`] — registration inside the gear's `register_rest`.
pub mod error;
pub mod handler;
pub mod routes;

use std::sync::Arc;

use crate::infra::proxy::DataPlane;

/// Handle shared with the handler through an axum `Extension`.
pub type SharedDataPlane = Arc<DataPlane>;
