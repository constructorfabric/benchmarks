//! REST API surface for the OAGW gear.
//!
//! Paths are gear-relative and prefixed `/oagw/v1`; the runtime nests
//! them (and the api-gateway's `prefix_path`, when set) outside this
//! crate. Control-plane families (`upstreams`, `routes`, `plugins`) plus
//! the data-plane `/oagw/v1/proxy/{*path}` catch-all live one level down
//! in [`rest`].

pub mod rest;
