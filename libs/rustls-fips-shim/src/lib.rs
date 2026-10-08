//! Empty shim. This crate exists solely to add `rustls/fips` and
//! `hyper-rustls/fips` to the workspace's resolved feature set on targets
//! where the AWS-LC FIPS backend applies (every OS except macOS).
//!
//! See `Cargo.toml` for the rationale.
