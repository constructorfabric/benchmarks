//! Management and proxy REST API (DESIGN §3.3).
//!
//! The management plane is tenant-scoped CRUD for upstreams, routes and
//! custom plugins, exposed under `/oagw/v1/...`. The data plane is the
//! catch-all proxy route `{METHOD} /oagw/v1/proxy/{alias}/{*suffix}`.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod routes;
