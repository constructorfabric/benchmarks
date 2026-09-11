//! HTTP surface of the `oagw` gear.
//!
//! `rest` carries the error-response contract and the route shell registered
//! under the `/oagw/v1` prefix (`cpt-cf-oagw-flow-error-response`,
//! `cpt-cf-oagw-dod-gear-registration`); `control_plane` carries the 15
//! management handlers that answer those shells
//! (`cpt-cf-oagw-feature-management-api`).

pub mod control_plane;
pub mod rest;
