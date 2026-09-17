//! `oagw` — the Constructor Fabric outbound API gateway gear.
//!
//! The gear is split into a **control plane** (management of upstreams, routes
//! and plugin bindings, plus alias/route resolution) and a **data plane** (the
//! `/oagw/v1/proxy/...` request path: plugin chain, header transformation,
//! streaming and CORS). Configuration comes from `gears.oagw.config`, storage
//! is in-memory for this build.
//!
//! See `gears/system/oagw/docs/` for the specification this implementation
//! follows.
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts_helpers;
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;
