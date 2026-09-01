//! In-crate test support for the conversion domain layer.
//!
//! Mirrors the layout used by [`crate::domain::tenant::test_support`]:
//! the fake repo lives in its own submodule, the parent re-exports the
//! public surface, and the tests of the fake itself live under
//! [`repo_tests`] (compiled only under `#[cfg(test)]`).
//!
//! Gated on `#[cfg(test)]` at the parent (`domain::conversion::mod`)
//! site, matching `domain::tenant::test_support` — production binaries
//! do not ship these types.

pub mod repo;

pub use repo::FakeConversionRepo;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "repo_tests.rs"]
mod repo_tests;
