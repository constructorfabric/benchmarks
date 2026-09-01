//! Transport layer of the gear.
//!
//! Only this layer knows about `axum`; it binds requests, calls the control
//! plane, and renders the wire envelope.

pub mod rest;
