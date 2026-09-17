//! Control-plane REST API for the OAGW gear (management surface).
//!
//! The data-plane proxy intake is registered alongside the management routes
//! in `crate::api::rest::routes` as a raw catch-all (`routing::any`) that
//! hands the unfiltered request to the `DataPlaneService`.

pub mod rest;
