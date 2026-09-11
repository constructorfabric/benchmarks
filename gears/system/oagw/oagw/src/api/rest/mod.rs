//! Transport layer: DTOs, handlers, error rendering and route registration.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod query;
pub mod routes;
pub mod state;

pub use routes::register_routes;
pub use state::OagwState;
