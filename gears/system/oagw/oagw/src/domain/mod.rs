//! OAGW domain layer (DESIGN §3.1–§3.3).
//!
//! * `models` — wire + stored record shapes (both JSON schemas).
//! * `alias` — alias derivation/normalization/enforcement.
//! * `merge` — hierarchical configuration merging at proxy time.
//! * `error` — domain errors mapped to RFC 9457 + GTS type ids.
//! * `dto` — in-process request/response DTOs shared with the data plane.
//! * `list` — OData-style list-query parsing.
//! * `scopes` — permission-scope identifiers and checks.
//! * `plugin` — plugin trait definitions.
//! * `services` — control-plane service + data-plane trait.

pub mod alias;
pub mod dto;
pub mod error;
pub mod list;
pub mod merge;
pub mod models;
pub mod plugin;
pub mod scopes;
pub mod services;

#[cfg(test)]
pub mod test_util;
