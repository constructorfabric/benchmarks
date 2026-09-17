//! REST adapters of the management API
//! ([DESIGN.md](../../../docs/DESIGN.md) `cpt-cf-oagw-interface-api`).
//!
//! [`dto`] carries the wire shapes, [`handlers`] the endpoint functions and
//! [`routes`] the `OperationBuilder` registration of the upstream and route
//! operations; [`plugin_handlers`] and [`plugin_routes`] do the same for the
//! plugin management operations. `register_routes` and `register_plugin_routes`
//! are the two entry points the gear's `RestApiCapability` calls.

mod dto;
mod handlers;
mod plugin_handlers;
mod plugin_routes;
mod proxy;
mod proxy_routes;
mod routes;

pub use plugin_handlers::PluginApiState;
pub use plugin_routes::register_plugin_routes;
pub use proxy::ProxyState;
pub use proxy_routes::register_proxy_routes;
pub use routes::register_routes;
