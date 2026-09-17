//! GTS type schemas and well-known instances for the OAGW domain.
//!
//! Declares the seven OAGW resource types the GTS type-provisioning
//! registers (`gts.cf.core.oagw.{upstream,route,auth_plugin,guard_plugin,
//! transform_plugin,proxy,protocol}.v1~`), the OAGW plugin spec type that
//! derives from the toolkit `PluginV1` base, and the inventory of the twelve
//! plugin instances that the type provisioning materializes.
//!
//! Every `#[gts_type_schema(...)]`-annotated type contributes an
//! [`InventoryTypeSchema`]; every `gts_instance!` contributes an
//! [`InventoryInstance`] — both via the process-wide `toolkit-gts`
//! inventory, which `register_gts_catalog` uploads to the types registry at
//! gear init.

use serde_json::Value;

use toolkit::gts::PluginV1;
use toolkit_gts::{gts_instance, gts_type_schema};

/// OAGW **upstream** resource (`gts.cf.core.oagw.upstream.v1~`).
///
/// A proxying target: host, port, scheme and optional path prefix. Also the
/// PEP resource type for the management surface's upstream CRUD.
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.upstream.v1~"),
    description = "OAGW upstream target — proxying backend host, port and scheme",
    properties = "id,name,host,port,scheme,path_prefix,enabled",
    base = true
)]
pub struct UpstreamV1 {
    /// Required by the `gts-macros` base-struct contract; runtime upstream
    /// ids are opaque UUIDs minted by the control plane.
    pub id: gts::GtsInstanceId,
    /// Stable upstream name (unique within the serving tenant).
    #[schemars(length(min = 1, max = 255))]
    pub name: String,
    /// Resolvable target host (hostname or literal IP).
    #[schemars(length(min = 1, max = 255))]
    pub host: String,
    /// Target port; omitted when the scheme default applies.
    pub port: Option<u16>,
    /// URL scheme (`http` or `https`).
    #[schemars(length(min = 1, max = 16))]
    pub scheme: String,
    /// Optional base path prepended to every proxied request URI.
    #[schemars(length(min = 0, max = 1024))]
    pub path_prefix: Option<String>,
    /// Whether the upstream accepts traffic (drives route validation).
    pub enabled: bool,
}

/// OAGW **route** resource (`gts.cf.core.oagw.route.v1~`).
///
/// A route binds an alias (the public proxy name) to an upstream with an
/// optional HTTP method/path match and an option to disable.
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.route.v1~"),
    description = "OAGW route — public alias bound to an upstream target",
    properties = "id,alias,name,upstream_alias,methods,path,enabled",
    base = true
)]
pub struct RouteV1 {
    /// Required by the `gts-macros` base-struct contract; runtime route ids
    /// are opaque UUIDs minted by the control plane.
    pub id: gts::GtsInstanceId,
    /// Public alias under which the route is proxied (`/oagw/v1/proxy/{alias}`).
    #[schemars(length(min = 1, max = 255))]
    pub alias: String,
    /// Optional descriptive name.
    #[schemars(length(min = 1, max = 255))]
    pub name: Option<String>,
    /// Name of the bound upstream.
    #[schemars(length(min = 1, max = 255))]
    pub upstream_alias: String,
    /// HTTP methods this route answers (empty = all methods).
    pub methods: Vec<String>,
    /// Optional path match applied on top of the alias prefix.
    #[schemars(length(min = 0, max = 1024))]
    pub path: Option<String>,
    /// Whether the route accepts traffic.
    pub enabled: bool,
}

/// OAGW **auth plugin** resource (`gts.cf.core.oagw.auth_plugin.v1~`).
///
/// Authenticates proxied requests before forwarding. Executable kinds:
/// `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`.
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.auth_plugin.v1~"),
    description = "OAGW auth plugin — authenticates proxied requests before forwarding",
    properties = "id,alias,kind,enabled,config",
    base = true
)]
pub struct AuthPluginV1 {
    /// Required by the `gts-macros` base-struct contract.
    pub id: gts::GtsInstanceId,
    /// Plugin alias used in route/upstream bindings and the L1 key.
    #[schemars(length(min = 1, max = 255))]
    pub alias: String,
    /// Plugin kind discriminator (`auth_plugin`).
    #[schemars(length(min = 1, max = 64))]
    pub kind: String,
    /// Whether the plugin participates in the request path.
    pub enabled: bool,
    /// Plugin-specific configuration consumed by the data plane.
    pub config: Value,
}

/// OAGW **guard plugin** resource (`gts.cf.core.oagw.guard_plugin.v1~`).
///
/// Validates request prerequisites before forwarding. Executable kind:
/// `required_headers`.
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.guard_plugin.v1~"),
    description = "OAGW guard plugin — validates request prerequisites before forwarding",
    properties = "id,alias,kind,enabled,config",
    base = true
)]
pub struct GuardPluginV1 {
    /// Required by the `gts-macros` base-struct contract.
    pub id: gts::GtsInstanceId,
    /// Plugin alias used in route/upstream bindings and the L1 key.
    #[schemars(length(min = 1, max = 255))]
    pub alias: String,
    /// Plugin kind discriminator (`guard_plugin`).
    #[schemars(length(min = 1, max = 64))]
    pub kind: String,
    /// Whether the plugin participates in the request path.
    pub enabled: bool,
    /// Plugin-specific configuration consumed by the data plane.
    pub config: Value,
}

/// OAGW **transform plugin** resource (`gts.cf.core.oagw.transform_plugin.v1~`).
///
/// Mutates, logs and observes the proxied exchange. Executable kinds:
/// `request_id`, `logging`, `metrics`.
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.transform_plugin.v1~"),
    description = "OAGW transform plugin — mutates, logs and observes the proxied exchange",
    properties = "id,alias,kind,enabled,config",
    base = true
)]
pub struct TransformPluginV1 {
    /// Required by the `gts-macros` base-struct contract.
    pub id: gts::GtsInstanceId,
    /// Plugin alias used in route/upstream bindings and the L1 key.
    #[schemars(length(min = 1, max = 255))]
    pub alias: String,
    /// Plugin kind discriminator (`transform_plugin`).
    #[schemars(length(min = 1, max = 64))]
    pub kind: String,
    /// Whether the plugin participates in the request path.
    pub enabled: bool,
    /// Plugin-specific configuration consumed by the data plane.
    pub config: Value,
}

/// OAGW **proxy** resource (`gts.cf.core.oagw.proxy.v1~`).
///
/// The proxy surface itself — PEP resource type for invoking
/// `/oagw/v1/proxy/{alias}`. Body carries only the alias: tenant scope and
/// action are supplied by the PDP on the `invoke` decision.
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.proxy.v1~"),
    description = "OAGW proxy surface — PEP resource type for proxied invocations",
    properties = "id,alias",
    base = true
)]
pub struct ProxyV1 {
    /// Required by the `gts-macros` base-struct contract.
    pub id: gts::GtsInstanceId,
    /// Route alias being invoked.
    #[schemars(length(min = 1, max = 255))]
    pub alias: String,
}

/// OAGW **protocol** resource (`gts.cf.core.oagw.protocol.v1~`).
///
/// A protocol descriptor (`http`, `grpc`, `tcp`, …) an upstream speaks.
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.protocol.v1~"),
    description = "OAGW protocol descriptor — the wire protocol an upstream speaks",
    properties = "id,name",
    base = true
)]
pub struct ProtocolV1 {
    /// Required by the `gts-macros` base-struct contract.
    pub id: gts::GtsInstanceId,
    /// Protocol name (`http`, `grpc`, `tcp`, …).
    #[schemars(length(min = 1, max = 32))]
    pub name: String,
}

// ---------------------------------------------------------------------------
// OAGW plugin spec type (derives from the toolkit PluginV1 base)
// ---------------------------------------------------------------------------

/// OAGW plugin specification type.
///
/// Unit marker deriving from the toolkit [`PluginV1`] base so plugin
/// instances are discoverable through the standard toolkit plugin machinery.
/// The per-kind configuration lives in the instance `properties` of the
/// plugin-kind schemas; this spec type only marks the inventory family.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~"),
    description = "OAGW plugin specification type",
    properties = "",
)]
pub struct OagwPluginSpecV1;

// ---------------------------------------------------------------------------
// OAGW plugin inventory instances
// ---------------------------------------------------------------------------

/// Assembles and submits one `PluginV1<OagwPluginSpecV1>` inventory entry.
///
/// `$id` is the full instance-id suffix under the
/// `gts.cf.toolkit.plugins.plugin.v1~` base (the last `~`-segment of the
/// instance id derives the type id the instance conforms to — the OAGW
/// plugin spec type's id).
macro_rules! oagw_plugin_instance {
    ($id:literal) => {
        gts_instance! {
            PluginV1::<OagwPluginSpecV1> {
                id: gts_id!($id),
                vendor: "constructorfabric".to_owned(),
                priority: 100,
                properties: OagwPluginSpecV1,
            }
        }
    };
}

// Executable auth plugins.
oagw_plugin_instance!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.noop.v1");
oagw_plugin_instance!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.apikey.v1");
oagw_plugin_instance!(
    "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.oauth2_client_cred.v1"
);
oagw_plugin_instance!(
    "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
);
// Executable guard plugins.
oagw_plugin_instance!(
    "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.required_headers.v1"
);
// Executable transform plugins.
oagw_plugin_instance!(
    "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.request_id.v1"
);
oagw_plugin_instance!(
    "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.logging.v1"
);
oagw_plugin_instance!(
    "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.metrics.v1"
);
// Catalog-only plugins (discoverable, not executed in the request path).
oagw_plugin_instance!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.basic.v1");
oagw_plugin_instance!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.bearer.v1");
oagw_plugin_instance!(
    "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.timeout.v1"
);
oagw_plugin_instance!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.cors.v1");

#[cfg(test)]
mod tests {
    use super::*;
    use toolkit_gts::all_inventory_instances;

    #[test]
    fn oagw_plugin_catalog_is_materialized() {
        let instances = all_inventory_instances().expect("instances collect cleanly");
        let ids = [
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.noop.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.apikey.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.required_headers.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.request_id.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.logging.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.metrics.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.basic.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.bearer.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.timeout.v1",
            "cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~cf.core.oagw.cors.v1",
        ];
        for id in ids {
            let full = format!("gts.{}", id);
            assert!(
                instances
                    .iter()
                    .any(|v| v.get("id").and_then(Value::as_str) == Some(full.as_str())),
                "missing catalog plugin instance `{full}`"
            );
        }
    }
}
