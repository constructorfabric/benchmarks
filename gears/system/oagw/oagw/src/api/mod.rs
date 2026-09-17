//! API layer ([DESIGN.md](../../docs/DESIGN.md) `cpt-cf-oagw-design-layers`).
//!
//! The transport-facing half of the gear: [`rest`] adapts the management
//! [`Service`](crate::domain::service::Service) onto HTTP. It owns no rule of
//! its own beyond DTO conversion and response shaping.

pub mod rest;
