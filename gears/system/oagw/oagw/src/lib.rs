//! OAGW — outbound API gateway.
//!
//! The gear exposes a management API for upstreams, routes and plugins, and a
//! data plane that proxies requests to the upstream selected by alias.
//! See `gears/system/oagw/docs/DESIGN.md`.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod domain;
pub mod error;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use error::{ErrorKind, ErrorSource, OagwError, OagwResult};
pub use gear::OagwGear;
