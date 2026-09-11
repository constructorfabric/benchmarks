//! The gear's GTS type catalog.
//!
//! Three types are owned here — upstream, route and plugin — and the plugin type carries
//! the built-in identifiers as well-known instances. Two groups of plugin identifiers
//! exist: the ones the gateway binds and executes, and the ones the catalog reserves for
//! completeness (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) that fail
//! when selected, because the behaviour they name is core Data Plane logic rather than a
//! plugin implementation.
//!
//! Every instance is submitted to the process-wide `toolkit-gts` inventory, which the
//! types-registry gear aggregates at boot, so the catalog carries the gear's types and the
//! reserved identifiers without a registration round trip.

use toolkit_gts::{gts_id, gts_instance_raw};

/// GTS type identifier of the upstream entity.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1";
/// GTS type identifier of the route entity.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1";
/// GTS type identifier of the plugin entity.
pub const PLUGIN_TYPE: &str = "gts.cf.core.oagw.plugin.v1";
/// GTS type identifier of the HTTP protocol binding.
pub const HTTP_PROTOCOL_TYPE: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// GTS type identifier of the WebSocket protocol binding.
pub const WEBSOCKET_PROTOCOL_TYPE: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.websocket.v1";
/// GTS type identifier of the Server-Sent Events protocol binding.
pub const SSE_PROTOCOL_TYPE: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.sse.v1";

/// GTS type identifier of the error kind the gateway raises.
pub const ERROR_TYPE: &str = "gts.cf.core.errors.err.v1";

/// The plugin identifiers the gateway binds and executes.
pub const BINDABLE_PLUGINS: [&str; 6] = [
    "noop",
    "apikey",
    "oauth2_client_cred",
    "oauth2_client_cred_basic",
    "required_headers",
    "request_id",
];

/// The plugin identifiers the catalog reserves but the gateway does not implement.
pub const CATALOG_ONLY_PLUGINS: [&str; 6] = [
    "basic",
    "bearer",
    "timeout",
    "cors",
    "logging",
    "metrics",
];

/// The instance identifier of one built-in plugin.
#[must_use]
pub fn plugin_instance_id(name: &str) -> String {
    format!("{PLUGIN_TYPE}~cf.core.oagw.plugin_{name}.v1")
}

/// The catalog entries: one well-known instance per built-in plugin, tagged with whether
/// the gateway can bind it.
///
/// The order is stable so the catalog renders identically on every boot.
#[must_use]
pub fn catalog() -> Vec<PluginCatalogEntry> {
    let bindable = BINDABLE_PLUGINS
        .iter()
        .map(|name| PluginCatalogEntry {
            name: (*name).to_owned(),
            instance_id: plugin_instance_id(name),
            bindable: true,
        });
    let reserved = CATALOG_ONLY_PLUGINS
        .iter()
        .map(|name| PluginCatalogEntry {
            name: (*name).to_owned(),
            instance_id: plugin_instance_id(name),
            bindable: false,
        });
    bindable.chain(reserved).collect()
}

/// One entry of the gear's plugin catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCatalogEntry {
    /// The binding name a configuration uses.
    pub name: String,
    /// The GTS instance identifier it is catalogued under.
    pub instance_id: String,
    /// Whether the gateway binds and executes it.
    pub bindable: bool,
}

// The reserved identifiers are catalogued as instances of the base toolkit plugin type so
// a configuration that names one is rejected by the gateway's own validation rather than
// silently dropped by the registry.
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_noop.v1"),
    "vendor": "cf.core.oagw",
    "priority": 100,
    "properties": { "kind": "auth", "bindable": true },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_apikey.v1"),
    "vendor": "cf.core.oagw",
    "priority": 100,
    "properties": { "kind": "auth", "bindable": true },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_oauth2_client_cred.v1"),
    "vendor": "cf.core.oagw",
    "priority": 100,
    "properties": { "kind": "auth", "bindable": true },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_oauth2_client_cred_basic.v1"),
    "vendor": "cf.core.oagw",
    "priority": 100,
    "properties": { "kind": "auth", "bindable": true },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_required_headers.v1"),
    "vendor": "cf.core.oagw",
    "priority": 100,
    "properties": { "kind": "guard", "bindable": true },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_request_id.v1"),
    "vendor": "cf.core.oagw",
    "priority": 100,
    "properties": { "kind": "transform", "bindable": true },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_basic.v1"),
    "vendor": "cf.core.oagw",
    "priority": 200,
    "properties": { "kind": "auth", "bindable": false },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_bearer.v1"),
    "vendor": "cf.core.oagw",
    "priority": 200,
    "properties": { "kind": "auth", "bindable": false },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_timeout.v1"),
    "vendor": "cf.core.oagw",
    "priority": 200,
    "properties": { "kind": "guard", "bindable": false },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_cors.v1"),
    "vendor": "cf.core.oagw",
    "priority": 200,
    "properties": { "kind": "guard", "bindable": false },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_logging.v1"),
    "vendor": "cf.core.oagw",
    "priority": 200,
    "properties": { "kind": "transform", "bindable": false },
});
gts_instance_raw!({
    "id": gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin_metrics.v1"),
    "vendor": "cf.core.oagw",
    "priority": 200,
    "properties": { "kind": "transform", "bindable": false },
});

