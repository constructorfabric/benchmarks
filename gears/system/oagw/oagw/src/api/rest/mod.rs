//! REST management surface (`/api/oagw/v1/...`).

pub mod common;
pub mod defaults;
pub mod dto;
pub mod error;
pub mod handlers;
pub mod query;
pub mod routes;

#[cfg(test)]
#[path = "rest_tests.rs"]
mod tests;
