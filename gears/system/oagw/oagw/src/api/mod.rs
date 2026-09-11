//! Transport layer for the OAGW gear.
//!
//! The only layer allowed to touch HTTP types: it maps between HTTP and the
//! domain types of `crate::domain` and owns the response contract.

pub mod rest;
