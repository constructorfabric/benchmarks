//! REST surface of the OAGW gear.
//!
//! Routes are registered directly at `/oagw/v1/...` (the host router is
//! passed to the gear as-is; no gateway prefix is applied).
//!
//! - Control plane: `{POST,GET,PUT,DELETE} /oagw/v1/upstreams[/{id}]`,
//!   `{POST,GET,PUT,DELETE} /oagw/v1/routes[/{id}]`,
//!   `{POST,GET,DELETE} /oagw/v1/plugins[/{id}]`,
//!   `GET /oagw/v1/plugins/{id}/source`.
//! - Enable/disable: `POST /oagw/v1/upstreams/{id}/enable|disable`.
//! - Data plane: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`.
//!
//! Errors are RFC 9457 `application/problem+json` with GTS `type`
//! identifiers (DESIGN §3.3 error table) and the `X-OAGW-Error-Source`
//! header (gateway) — see [`error`].

pub mod dto;
pub mod error;
pub mod extractors;
pub mod handlers;
pub mod routes;
