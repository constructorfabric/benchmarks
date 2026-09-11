//! GTS type provisioning.
//!
//! The gear publishes its resource type schemas (`upstream`, `route`, the
//! plugin types and the proxy permission) to the types-registry at start-up so
//! the instances it stores resolve to a registered type. Registration is
//! best-effort and idempotent: an already-registered type is not an error, and
//! a registry that is unavailable at start-up only delays the publication, it
//! never blocks the gear from serving.

use crate::gts_helpers;
use std::sync::Arc;
use types_registry_sdk::TypesRegistryClient;

/// The type schemas the gear owns, in dependency order (parents first).
#[must_use]
pub fn owned_type_schemas() -> Vec<serde_json::Value> {
    vec![
        // Upstream resource type.
        serde_json::json!({
            "$id": format!("{}cf.core.oagw.upstream.v1", gts_helpers::UPSTREAM_TYPE.trim_end_matches('~')),
            "gtsId": format!("{}cf.core.oagw.upstream.v1", gts_helpers::UPSTREAM_TYPE.trim_end_matches('~')),
            "type": "object",
            "title": "OAGW Upstream",
            "properties": {
                "alias": {"type": "string"},
                "protocol": {"type": "string"},
                "server": {"type": "object"},
                "auth": {"type": "object"},
                "headers": {"type": "object"},
                "rateLimit": {"type": "object"},
                "cors": {"type": "object"},
                "plugins": {"type": "object"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "enabled": {"type": "boolean"}
            },
            "required": ["alias", "protocol", "server"]
        }),
        // Route resource type.
        serde_json::json!({
            "$id": format!("{}cf.core.oagw.route.v1", gts_helpers::ROUTE_TYPE.trim_end_matches('~')),
            "gtsId": format!("{}cf.core.oagw.route.v1", gts_helpers::ROUTE_TYPE.trim_end_matches('~')),
            "type": "object",
            "title": "OAGW Route",
            "properties": {
                "upstreamId": {"type": "string"},
                "priority": {"type": "integer"},
                "match": {"type": "object"},
                "rateLimit": {"type": "object"},
                "cors": {"type": "object"},
                "plugins": {"type": "object"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "enabled": {"type": "boolean"}
            },
            "required": ["upstreamId", "match"]
        }),
        // Auth plugin type.
        serde_json::json!({
            "$id": format!("{}cf.core.oagw.auth_plugin.v1", gts_helpers::AUTH_PLUGIN_TYPE.trim_end_matches('~')),
            "gtsId": format!("{}cf.core.oagw.auth_plugin.v1", gts_helpers::AUTH_PLUGIN_TYPE.trim_end_matches('~')),
            "type": "object",
            "title": "OAGW Auth Plugin"
        }),
        // Guard plugin type.
        serde_json::json!({
            "$id": format!("{}cf.core.oagw.guard_plugin.v1", gts_helpers::GUARD_PLUGIN_TYPE.trim_end_matches('~')),
            "gtsId": format!("{}cf.core.oagw.guard_plugin.v1", gts_helpers::GUARD_PLUGIN_TYPE.trim_end_matches('~')),
            "type": "object",
            "title": "OAGW Guard Plugin"
        }),
        // Transform plugin type.
        serde_json::json!({
            "$id": format!("{}cf.core.oagw.transform_plugin.v1", gts_helpers::TRANSFORM_PLUGIN_TYPE.trim_end_matches('~')),
            "gtsId": format!("{}cf.core.oagw.transform_plugin.v1", gts_helpers::TRANSFORM_PLUGIN_TYPE.trim_end_matches('~')),
            "type": "object",
            "title": "OAGW Transform Plugin"
        }),
        // Proxy permission.
        serde_json::json!({
            "$id": format!("{}:invoke", gts_helpers::PROXY_PERMISSION),
            "gtsId": format!("{}:invoke", gts_helpers::PROXY_PERMISSION),
            "type": "object",
            "title": "OAGW Proxy Permission"
        }),
    ]
}

/// Publishes the gear's types, reporting each outcome.
///
/// # Errors
///
/// Returns the registry's own error only when the whole batch fails.
pub async fn provision(registry: &Arc<dyn TypesRegistryClient>) -> Result<usize, String> {
    let schemas = owned_type_schemas();
    let total = schemas.len();
    match registry.register_type_schemas(schemas).await {
        Ok(results) => Ok(report_outcome(total, &results)),
        Err(error) => {
            tracing::warn!(target: "oagw::types", error = %error, "type provisioning deferred");
            Err(error.to_string())
        }
    }
}

/// Logs how much of the batch registered and returns the registered count.
fn report_outcome(total: usize, results: &[types_registry_sdk::RegisterResult]) -> usize {
    let registered = results
        .iter()
        .filter(|result| matches!(result, types_registry_sdk::RegisterResult::Ok { .. }))
        .count();
    let skipped = total - registered;
    if skipped > 0 {
        tracing::info!(target: "oagw::types", registered, skipped, "types already present");
    } else {
        tracing::info!(target: "oagw::types", registered, "types provisioned");
    }
    registered
}
