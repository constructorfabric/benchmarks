//! GTS type provisioning.
//!
//! Two mechanisms, matching the platform's split:
//!
//! * **Link time** — the authorization permissions OAGW grants are declared as
//!   `AuthzPermissionV1` instances via `gts_instance!`; `types-registry`
//!   collects them from the process-global inventory at boot, so there is no
//!   registration call for them.
//! * **Runtime** — the resource and plugin type schemas are published from
//!   [`register_catalog`] once the registry is in ready mode, using the
//!   schemas shipped in `docs/schemas/`.

use std::sync::Arc;

use toolkit_gts::{AuthzPermissionV1, gts_instance};
use tracing::{info, warn};
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::state::actions;
use crate::domain::gts;

/// The published upstream schema, so the registry serves exactly what the
/// management API validates against.
const UPSTREAM_SCHEMA: &str = include_str!("../../../docs/schemas/upstream.v1.schema.json");
/// The published route schema.
const ROUTE_SCHEMA: &str = include_str!("../../../docs/schemas/route.v1.schema.json");

// --- Management permissions (link-time inventory) --------------------------

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.upstream_create.v1"),
        resource_type: gts::UPSTREAM_BASE.to_owned(),
        action: actions::CREATE.to_owned(),
        display_name: "Create outbound upstream".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.upstream_read.v1"),
        resource_type: gts::UPSTREAM_BASE.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read outbound upstream".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.upstream_override.v1"),
        resource_type: gts::UPSTREAM_BASE.to_owned(),
        action: actions::OVERRIDE.to_owned(),
        display_name: "Replace outbound upstream".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.upstream_delete.v1"),
        resource_type: gts::UPSTREAM_BASE.to_owned(),
        action: actions::DELETE.to_owned(),
        display_name: "Delete outbound upstream".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.route_create.v1"),
        resource_type: gts::ROUTE_BASE.to_owned(),
        action: actions::CREATE.to_owned(),
        display_name: "Create outbound route".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.route_read.v1"),
        resource_type: gts::ROUTE_BASE.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read outbound route".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.route_override.v1"),
        resource_type: gts::ROUTE_BASE.to_owned(),
        action: actions::OVERRIDE.to_owned(),
        display_name: "Replace outbound route".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.route_delete.v1"),
        resource_type: gts::ROUTE_BASE.to_owned(),
        action: actions::DELETE.to_owned(),
        display_name: "Delete outbound route".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.oagw._.proxy_invoke.v1"),
        resource_type: gts::PROXY_BASE.to_owned(),
        action: actions::INVOKE.to_owned(),
        display_name: "Invoke the outbound proxy".to_owned(),
    }
}

// --- Type schemas (runtime registration) -----------------------------------

/// Publish the OAGW type schemas.
///
/// Never fails the caller: a registry that refuses an entry is a catalog
/// problem, not a reason to keep the gateway from serving traffic.
pub async fn register_catalog(client: &Arc<dyn TypesRegistryClient>) {
    let entities = catalog_entities();
    let count = entities.len();
    match client.register_type_schemas(entities).await {
        Ok(results) => {
            let failures: Vec<String> = results
                .iter()
                .filter_map(|result| match result {
                    types_registry_sdk::RegisterResult::Err { gts_id, error } => Some(format!(
                        "{}: {error}",
                        gts_id.as_deref().unwrap_or("<unknown>")
                    )),
                    types_registry_sdk::RegisterResult::Ok { .. } => None,
                })
                .collect();
            if failures.is_empty() {
                info!(target: "oagw.types", count, "registered OAGW type schemas");
            } else {
                warn!(
                    target: "oagw.types",
                    count,
                    failures = ?failures,
                    "some OAGW type schemas were rejected by the registry"
                );
            }
        }
        Err(err) => warn!(
            target: "oagw.types",
            error = %err,
            "could not register OAGW type schemas"
        ),
    }
}

/// The schema documents OAGW publishes, ordered parent-before-child.
#[must_use]
pub fn catalog_entities() -> Vec<serde_json::Value> {
    let mut entities = Vec::new();

    if let Some(schema) = published_schema(UPSTREAM_SCHEMA, gts::UPSTREAM_BASE) {
        entities.push(schema);
    }
    if let Some(schema) = published_schema(ROUTE_SCHEMA, gts::ROUTE_BASE) {
        entities.push(schema);
    }

    entities.push(plugin_base_schema(
        gts::AUTH_PLUGIN_BASE,
        "OAGW auth plugin — injects credentials on the outbound request",
    ));
    entities.push(plugin_base_schema(
        gts::GUARD_PLUGIN_BASE,
        "OAGW guard plugin — validates a request or response and may reject it",
    ));
    entities.push(plugin_base_schema(
        gts::TRANSFORM_PLUGIN_BASE,
        "OAGW transform plugin — mutates a request, response or error",
    ));
    entities.push(protocol_base_schema());

    entities
}

/// Attach a GTS `$id` to a published JSON Schema document.
fn published_schema(raw: &str, type_id: &str) -> Option<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object_mut()?;
    object.insert(
        "$id".to_owned(),
        serde_json::Value::String(format!("gts://{type_id}")),
    );
    Some(value)
}

fn plugin_base_schema(type_id: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "$id": format!("gts://{type_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "description": description,
        "type": "object",
        "properties": {
            "id": { "type": "string", "format": "gts-identifier" },
            "config": { "type": "object" }
        }
    })
}

fn protocol_base_schema() -> serde_json::Value {
    serde_json::json!({
        "$id": format!("gts://{}", gts::PROTOCOL_BASE),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "description": "Protocol spoken to an OAGW upstream service",
        "type": "object",
        "properties": {
            "id": { "type": "string", "format": "gts-identifier" }
        }
    })
}

#[cfg(test)]
#[path = "type_catalog_tests.rs"]
mod tests;
