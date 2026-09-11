//! OAGW — the outbound API gateway gear.
//!
//! The control plane manages upstreams, routes and plugins; the data plane
//! forwards proxied requests to the resolved upstream after running the
//! plugin chain.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(clippy::module_name_repetitions)]
// `OagwError` carries an RFC 9457 problem document plus diagnostics; boxing it
// would spread an allocation across every error path for no measurable gain.
#![allow(clippy::result_large_err)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
