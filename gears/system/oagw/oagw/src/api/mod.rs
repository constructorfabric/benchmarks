// Created: 2026-09-01 by Constructor Tech
//! The REST surface.
//!
//! * [`routes`] — the operation catalogue, registered at `/oagw/v1/...`.
//! * [`handlers`] — management API handlers (Control Plane).
//! * [`proxy`] — the Data Plane entry point.
//! * [`dto`] — the wire shapes.
//! * [`state`] — what every handler is handed.

pub mod dto;
pub mod handlers;
pub mod proxy;
pub mod routes;
pub mod state;
