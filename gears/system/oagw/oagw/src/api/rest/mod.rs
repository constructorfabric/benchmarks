//! REST API surface of the gear.

pub mod dto;
pub mod error;
pub mod extract;
pub mod handlers;
pub mod problem;
pub mod routes;

pub use error::{ErrorContext, OagwError, ProblemBody};
pub use routes::register_routes;
