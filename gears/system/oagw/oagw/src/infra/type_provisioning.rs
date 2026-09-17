//! GTS type provisioning for the `oagw` gear.
//!
//! The gear owns six GTS type-schemas — the two resource types, the three
//! plugin types and the data-plane protocol type — and registers them with the
//! `types-registry` during `init`. Registration is idempotent: a restart
//! re-submits the same documents and an `AlreadyExists` answer is accepted.

use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

/// JSON Schema dialect every document declares.
const DRAFT: &str = "https://json-schema.org/draft-07/schema#";

/// The GTS base type of an upstream resource.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// The GTS base type of a route resource.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// The GTS base type of an auth plugin.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// The GTS base type of a guard plugin.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// The GTS base type of a transform plugin.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// The GTS base type of a data-plane protocol.
pub const PROTOCOL_TYPE: &str = "gts.cf.core.oagw.protocol.v1~";

/// Every type-schema the gear provisions, in registration order.
pub const PROVISIONED_TYPES: [&str; 6] = [
    UPSTREAM_TYPE,
    ROUTE_TYPE,
    AUTH_PLUGIN_TYPE,
    GUARD_PLUGIN_TYPE,
    TRANSFORM_PLUGIN_TYPE,
    PROTOCOL_TYPE,
];

/// Register the oagw type-schemas with `registry`.
///
/// # Errors
///
/// Returns an error when the registry itself fails, or when a document is
/// rejected for any reason other than being registered already.
pub async fn provision(registry: &dyn TypesRegistryClient) -> anyhow::Result<()> {
    let results = registry.register_type_schemas(documents()).await?;
    for result in &results {
        match result {
            RegisterResult::Ok { gts_id } => {
                tracing::debug!(gts_id, "oagw GTS type-schema registered");
            }
            RegisterResult::Err { gts_id, error } if already_registered(error) => {
                tracing::debug!(
                    gts_id = gts_id.as_deref().unwrap_or_default(),
                    "oagw GTS type-schema already registered"
                );
            }
            RegisterResult::Err { gts_id, error } => {
                return Err(anyhow::anyhow!(
                    "oagw type provisioning failed for `{}`: {error}",
                    gts_id.as_deref().unwrap_or("<unknown>")
                ));
            }
        }
    }
    Ok(())
}

/// Whether `error` only says the schema is already in the registry.
fn already_registered(error: &CanonicalError) -> bool {
    matches!(error, CanonicalError::AlreadyExists { .. })
}

/// The type-schema documents of the gear.
fn documents() -> Vec<serde_json::Value> {
    vec![
        schema(
            UPSTREAM_TYPE,
            "Outbound API gateway upstream",
            "alias,description,server,protocol,auth,plugins,rate_limit,cors,headers",
        ),
        schema(
            ROUTE_TYPE,
            "Outbound API gateway route",
            "upstream_id,path,methods,priority,rate_limit,cors,headers,plugins",
        ),
        schema(
            AUTH_PLUGIN_TYPE,
            "Outbound API gateway auth plugin",
            "id,vendor,priority,properties",
        ),
        schema(
            GUARD_PLUGIN_TYPE,
            "Outbound API gateway guard plugin",
            "id,vendor,priority,properties",
        ),
        schema(
            TRANSFORM_PLUGIN_TYPE,
            "Outbound API gateway transform plugin",
            "id,vendor,priority,properties",
        ),
        schema(
            PROTOCOL_TYPE,
            "Outbound API gateway upstream protocol",
            "id,name,transports",
        ),
    ]
}

/// One JSON Schema document for `type_id`, admitting the named properties.
///
/// The `$id` carries the `gts://` URI form: the registry refuses a document
/// that spells its own id with the bare `gts.` prefix.
fn schema(type_id: &str, description: &str, properties: &str) -> serde_json::Value {
    let properties: serde_json::Map<String, serde_json::Value> = properties
        .split(',')
        .map(|name| {
            (
                name.trim().to_owned(),
                serde_json::json!({ "type": "string" }),
            )
        })
        .collect();
    serde_json::json!({
        "$id": format!("gts://{type_id}"),
        "$schema": DRAFT,
        "title": description,
        "description": description,
        "type": "object",
        "properties": properties,
    })
}

#[cfg(test)]
#[path = "type_provisioning_tests.rs"]
mod type_provisioning_tests;
