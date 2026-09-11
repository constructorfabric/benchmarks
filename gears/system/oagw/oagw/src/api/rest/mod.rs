//! REST transport for the management and proxy APIs.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod proxy;
pub mod query;
pub mod routes;
pub mod state;

pub use routes::{BASE, register_routes};
pub use state::OagwState;
