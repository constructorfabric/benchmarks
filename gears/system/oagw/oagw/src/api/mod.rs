// Updated: 2026-09-01 by Constructor Tech
//! The API layer: axum handlers and the OpenAPI document they describe.
//!
//! Handlers are thin. They extract the caller's [`SecurityContext`], call a
//! domain service, and map the result onto the wire. Every rule about tenancy,
//! authorization or validation lives below this layer.

pub mod rest;
