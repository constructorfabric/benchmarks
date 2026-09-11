//! REST handlers, DTOs, problem responses and route registration.
pub mod dto;
pub mod error;
pub mod handlers;
pub mod routes;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod router_tests;
