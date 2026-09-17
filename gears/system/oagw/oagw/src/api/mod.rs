//! Transport layer of the OAGW gear.
//!
//! `rest/` holds the management API. The data plane (part 2) adds its own
//! transport under `proxy/` and reuses the domain layer.

pub mod rest;
