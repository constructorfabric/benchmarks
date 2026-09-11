//! REST route modules for the `oagw` gear, one file per DECOMPOSITION
//! entry. [`routes::register_routes`] is the fixed-order aggregator every
//! later feature's submodule is mounted through; see its doc comment for
//! the call order.

mod page_params;
mod plugins;
mod proxy;
mod route_api;
pub mod routes;
mod upstreams;
