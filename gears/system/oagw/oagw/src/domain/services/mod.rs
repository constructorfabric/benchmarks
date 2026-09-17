//! Domain services of the OAGW gear (DESIGN §3.2).
//!
//! `control_plane` owns configuration management (CRUD, alias resolution,
//! validation); `data_plane` executes proxied requests against that
//! configuration.

pub mod control_plane;
pub mod data_plane;
