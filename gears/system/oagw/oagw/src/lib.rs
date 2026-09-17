//! OAGW — Outbound API Gateway gear.
//!
//! Discovered and routed by the Constructor Fabric single-host server.
//! The control plane (upstream / route / plugin CRUD) and the data
//! plane (`/oagw/v1/proxy/...`) follow `docs/` (PRD, DESIGN, ADRs,
//! schemas).

#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts_helpers;
pub mod infra;
pub mod type_catalog;

pub use gear::Oagw;
