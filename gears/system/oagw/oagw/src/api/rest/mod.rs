//! REST surface for the OAGW gear (routes mounted at `/oagw/v1`).

pub mod error;
pub mod extractors;
pub mod handlers;
pub mod routes;
#[cfg(test)]
mod tests;
