//! HTTP layer: routes, DTOs, handlers, canonical error mapping. Extended by later tasks.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod license;
pub mod routes;
pub mod sse;
pub mod state;

#[cfg(test)]
mod access_tests;
