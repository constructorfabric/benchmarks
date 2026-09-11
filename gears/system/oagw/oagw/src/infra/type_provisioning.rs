//! GTS catalog provisioning.
//!
//! The gear publishes its resource types and the built-in plugin catalog to the
//! types-registry during `SystemCapability::post_init`, so an operator browsing
//! the catalog sees every identifier the control plane can bind — including the
//! catalog-only ones (`basic`, `bearer`, `timeout`, `cors`, `logging`,
//! `metrics`) that have no backing implementation (DESIGN §"Catalog-only
//! identifiers").

use std::sync::Arc;

use serde_json::json;
use types_registry_sdk::TypesRegistryClient;

/// The resource types the gear speaks about.
const RESOURCE_TYPES: &[&str] = &[
    crate::domain::gts_helpers::UPSTREAM_TYPE,
    crate::domain::gts_helpers::ROUTE_TYPE,
    crate::domain::gts_helpers::AUTH_PLUGIN_TYPE,
    crate::domain::gts_helpers::GUARD_PLUGIN_TYPE,
    crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE,
    crate::domain::gts_helpers::PROXY_TYPE,
];

/// Every plugin identifier the catalog names, implemented or not.
const PLUGIN_CATALOG: &[&str] = &[
    crate::domain::gts_helpers::BUILTIN_AUTH_NOOP,
    crate::domain::gts_helpers::BUILTIN_AUTH_APIKEY,
    crate::domain::gts_helpers::BUILTIN_AUTH_OAUTH2_CC,
    crate::domain::gts_helpers::BUILTIN_AUTH_OAUTH2_CC_BASIC,
    crate::domain::gts_helpers::CATALOG_AUTH_BASIC,
    crate::domain::gts_helpers::CATALOG_AUTH_BEARER,
    crate::domain::gts_helpers::BUILTIN_GUARD_REQUIRED_HEADERS,
    crate::domain::gts_helpers::CATALOG_GUARD_TIMEOUT,
    crate::domain::gts_helpers::CATALOG_GUARD_CORS,
    crate::domain::gts_helpers::BUILTIN_TRANSFORM_REQUEST_ID,
    crate::domain::gts_helpers::CATALOG_TRANSFORM_LOGGING,
    crate::domain::gts_helpers::CATALOG_TRANSFORM_METRICS,
    crate::domain::gts_helpers::PROTOCOL_HTTP,
    crate::domain::gts_helpers::PROTOCOL_GRPC,
];

/// Registers the gear's types and catalog instances with the types-registry.
///
/// A registry that is not wired (a gear running without types-registry) is not
/// an error: the catalog is descriptive, not load-bearing.
pub async fn provision(registry: Option<Arc<dyn TypesRegistryClient>>) -> anyhow::Result<()> {
    let Some(registry) = registry else {
        tracing::info!("types-registry unavailable: the oagw catalog is not published");
        return Ok(());
    };

    let schemas = RESOURCE_TYPES
        .iter()
        .map(|t| type_schema(t))
        .collect::<Vec<_>>();
    if let Err(err) = registry.register_type_schemas(schemas).await {
        tracing::warn!(error = %err, "failed to publish the oagw resource types");
        return Ok(());
    }

    let instances = PLUGIN_CATALOG
        .iter()
        .map(|id| catalog_instance(id))
        .collect::<Vec<_>>();
    if let Err(err) = registry.register_instances(instances).await {
        tracing::warn!(error = %err, "failed to publish the oagw plugin catalog");
    }
    Ok(())
}

/// A minimal JSON-Schema envelope for a resource type.
fn type_schema(type_id: &str) -> serde_json::Value {
    json!({
        "$id": format!("gts://{type_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "description": "Constructor Fabric outbound API gateway resource type.",
        "type": "object",
    })
}

/// A catalog instance naming a built-in or catalog-only plugin.
fn catalog_instance(id: &str) -> serde_json::Value {
    let type_id = crate::domain::gts_helpers::type_part(id);
    json!({
        "$id": format!("gts://{id}"),
        "type": type_id,
        "description": format!(
            "OAGW {} catalog entry",
            crate::domain::gts_helpers::plugin_short_name(id)
        ),
    })
}
