//! HTTP API of the `oagw` gear.
//!
//! [`rest`] is the management plane; [`proxy`] mounts the proxy data plane
//! under `/oagw/v1/proxy/...`.
pub mod proxy;
pub mod rest;
