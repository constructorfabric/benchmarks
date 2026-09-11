//! Transport layer: Axum handlers, route registration and error rendering.

pub mod error;
pub mod handlers;
pub mod routes;

pub use handlers::OagwState;
pub use routes::register_routes;
