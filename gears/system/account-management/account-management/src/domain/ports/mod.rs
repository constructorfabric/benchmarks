//! Domain ports for outbound concerns (observability, audit, etc.).
//!
//! Ports defined here are pure-Rust traits with no infrastructure
//! dependency. Infra adapters live under [`crate::infra`] and are
//! constructed at gear-init time in [`crate::gear`].

pub mod metrics;
