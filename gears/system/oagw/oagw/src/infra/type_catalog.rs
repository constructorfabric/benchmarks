//! GTS type provisioning (`cpt-cf-oagw-contract-types-registry`).
//!
//! Publishes the gear's base type-schemas so other components can resolve
//! `gts.cf.core.oagw.*` identifiers. Provisioning is best-effort: a
//! types-registry hiccup degrades discovery, it must not stop the gateway from
//! serving traffic.

use std::sync::Arc;

use serde_json::{Value, json};
use types_registry_sdk::TypesRegistryClient;

use crate::domain::gts_helpers;

/// The base type-schemas OAGW owns, parents first.
#[must_use]
pub fn base_type_schemas() -> Vec<Value> {
    vec![
        type_schema(
            gts_helpers::UPSTREAM_TYPE,
            "OAGW upstream service configuration.",
        ),
        type_schema(gts_helpers::ROUTE_TYPE, "OAGW route configuration."),
        type_schema(gts_helpers::PROXY_TYPE, "OAGW proxy invocation resource."),
        type_schema(
            gts_helpers::AUTH_PLUGIN_TYPE,
            "OAGW auth plugin: credential injection.",
        ),
        type_schema(
            gts_helpers::GUARD_PLUGIN_TYPE,
            "OAGW guard plugin: validation and policy enforcement.",
        ),
        type_schema(
            gts_helpers::TRANSFORM_PLUGIN_TYPE,
            "OAGW transform plugin: request/response mutation.",
        ),
    ]
}

fn type_schema(gts_id: &str, description: &str) -> Value {
    json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "description": description,
        "type": "object",
    })
}

/// Register the base type-schemas, logging per-item failures.
///
/// Never returns an error: see the module docs for why provisioning is
/// best-effort.
pub async fn provision(registry: &Arc<dyn TypesRegistryClient>) {
    let schemas = base_type_schemas();
    let count = schemas.len();
    match registry.register_type_schemas(schemas).await {
        Ok(results) => {
            let failures: Vec<String> = results
                .iter()
                .filter_map(|r| match r {
                    types_registry_sdk::RegisterResult::Err { gts_id, error } => Some(format!(
                        "{}: {error}",
                        gts_id.as_deref().unwrap_or("<unknown>")
                    )),
                    types_registry_sdk::RegisterResult::Ok { .. } => None,
                })
                .collect();
            if failures.is_empty() {
                tracing::info!(
                    target: "oagw.types",
                    count,
                    "registered OAGW base type-schemas"
                );
            } else {
                tracing::warn!(
                    target: "oagw.types",
                    failures = ?failures,
                    "some OAGW type-schemas were rejected by the types-registry"
                );
            }
        }
        Err(err) => tracing::warn!(
            target: "oagw.types",
            error = %err,
            "types-registry unavailable; OAGW type-schemas were not published"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_base_type_is_published_with_a_gts_uri() {
        let schemas = base_type_schemas();
        assert_eq!(schemas.len(), 6);
        for schema in &schemas {
            let id = schema["$id"].as_str().expect("$id");
            assert!(id.starts_with("gts://gts.cf.core.oagw."), "{id}");
            assert!(id.ends_with('~'), "type-schema ids end with '~': {id}");
        }
    }

    #[test]
    fn the_three_plugin_base_types_are_present() {
        let ids: Vec<String> = base_type_schemas()
            .iter()
            .map(|s| s["$id"].as_str().unwrap_or_default().to_owned())
            .collect();
        for base in [
            gts_helpers::AUTH_PLUGIN_TYPE,
            gts_helpers::GUARD_PLUGIN_TYPE,
            gts_helpers::TRANSFORM_PLUGIN_TYPE,
        ] {
            assert!(ids.contains(&format!("gts://{base}")), "{base} missing");
        }
    }
}
