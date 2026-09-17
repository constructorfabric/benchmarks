//! Domain services for the OAGW control and data planes.
//!
//! * [`control_plane`] — CRUD for upstreams, routes and plugins, plus
//!   the alias rules of DOCS §2.
//! * [`data_plane`] — the proxy-facing resolution pipeline: alias
//!   lookup, route matching, header/rate-limit/plugin resolution.
//! * [`alias`] — alias derivation and validation.
//! * [`list`] — OData-style listing (`$filter`, `$top`, `$skip`).

pub mod alias;
pub mod control_plane;
pub mod data_plane;
pub mod list;
