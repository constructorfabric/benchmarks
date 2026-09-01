//! REST transport of the management and proxy APIs.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod routes;

pub use error::OagwProblem;
pub use handlers::proxy;
pub use routes::register_routes;
