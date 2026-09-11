//! GTS type catalog registration.
//!
//! OAGW publishes its resource types and the full plugin catalog to the
//! types-registry so operators can discover them — including the
//! catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`,
//! `metrics`) that name capabilities implemented as core Data Plane logic
//! rather than as plugins.
//!
//! Provisioning is descriptive metadata, not a correctness gate: a registry
//! that rejects an entry is logged loudly and startup continues, because the
//! proxy and management surfaces work regardless.

use serde_json::{Value, json};
use std::sync::Arc;
use types_registry_sdk::TypesRegistryClient;

use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, AUTH_PLUGIN_TYPE, BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID,
    CORS_GUARD_PLUGIN_ID, GUARD_PLUGIN_TYPE, LOGGING_TRANSFORM_PLUGIN_ID,
    METRICS_TRANSFORM_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, PROXY_TYPE, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID, ROUTE_TYPE, TIMEOUT_GUARD_PLUGIN_ID,
    TRANSFORM_PLUGIN_TYPE, UPSTREAM_TYPE,
};

/// JSON Schema dialect the catalog entries are authored in.
const DIALECT: &str = "http://json-schema.org/draft-07/schema#";

/// The type schemas OAGW owns.
#[must_use]
pub fn type_schemas() -> Vec<Value> {
    vec![
        schema(UPSTREAM_TYPE, "OAGW upstream service configuration"),
        schema(ROUTE_TYPE, "OAGW route configuration"),
        schema(PROXY_TYPE, "OAGW proxy invocation capability"),
        schema(AUTH_PLUGIN_TYPE, "OAGW auth plugin"),
        schema(GUARD_PLUGIN_TYPE, "OAGW guard plugin"),
        schema(TRANSFORM_PLUGIN_TYPE, "OAGW transform plugin"),
    ]
}

/// The built-in and catalog-only plugin identifiers, as derived type schemas
/// under their base plugin type.
#[must_use]
pub fn plugin_catalog() -> Vec<Value> {
    vec![
        schema(
            &derived(NOOP_AUTH_PLUGIN_ID),
            "No authentication (built-in)",
        ),
        schema(
            &derived(APIKEY_AUTH_PLUGIN_ID),
            "API key injection, header or query (built-in)",
        ),
        schema(
            &derived(OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID),
            "OAuth2 client credentials, form client auth (built-in)",
        ),
        schema(
            &derived(OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID),
            "OAuth2 client credentials, basic client auth (built-in)",
        ),
        schema(
            &derived(BASIC_AUTH_PLUGIN_ID),
            "HTTP Basic authentication (reserved identifier; no implementation)",
        ),
        schema(
            &derived(BEARER_AUTH_PLUGIN_ID),
            "Bearer token injection (reserved identifier; no implementation)",
        ),
        schema(
            &derived(REQUIRED_HEADERS_GUARD_PLUGIN_ID),
            "Required header enforcement, request and response (built-in)",
        ),
        schema(
            &derived(TIMEOUT_GUARD_PLUGIN_ID),
            "Request timeout enforcement (core Data Plane configuration)",
        ),
        schema(
            &derived(CORS_GUARD_PLUGIN_ID),
            "CORS validation (core Data Plane logic; configure via Upstream.cors)",
        ),
        schema(
            &derived(REQUEST_ID_TRANSFORM_PLUGIN_ID),
            "X-Request-ID propagation (built-in)",
        ),
        schema(
            &derived(LOGGING_TRANSFORM_PLUGIN_ID),
            "Request/response logging (core Data Plane instrumentation)",
        ),
        schema(
            &derived(METRICS_TRANSFORM_PLUGIN_ID),
            "Prometheus metrics collection (core Data Plane instrumentation)",
        ),
    ]
}

/// A plugin identifier is registered as a *derived type* — it names a kind of
/// plugin, not one configured instance — so the catalog entry carries the
/// trailing `~` that marks a type schema.
fn derived(plugin_id: &str) -> String {
    format!("{plugin_id}~")
}

fn schema(id: &str, description: &str) -> Value {
    json!({
        "$id": format!("gts://{id}"),
        "$schema": DIALECT,
        "description": description,
        "type": "object",
    })
}

/// Publish the catalog. Failures are logged, never fatal.
pub async fn provision(registry: &Arc<dyn TypesRegistryClient>) {
    let mut entities = type_schemas();
    entities.extend(plugin_catalog());
    let total = entities.len();

    match registry.register_type_schemas(entities).await {
        Ok(results) => {
            let failed: Vec<String> = results
                .iter()
                .filter_map(|result| match result {
                    types_registry_sdk::RegisterResult::Err { gts_id, error } => Some(format!(
                        "{}: {error}",
                        gts_id.as_deref().unwrap_or("<unidentified>")
                    )),
                    types_registry_sdk::RegisterResult::Ok { .. } => None,
                })
                .collect();
            if failed.is_empty() {
                tracing::info!(
                    target: "oagw.types",
                    registered = total,
                    "OAGW GTS type catalog registered"
                );
            } else {
                tracing::warn!(
                    target: "oagw.types",
                    registered = total - failed.len(),
                    rejected = failed.len(),
                    detail = %failed.join("; "),
                    "part of the OAGW GTS type catalog was rejected"
                );
            }
        }
        Err(err) => tracing::warn!(
            target: "oagw.types",
            error = %err,
            "could not reach the types-registry; OAGW type catalog not published"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resource_type_is_a_type_schema() {
        for entity in type_schemas() {
            let id = entity["$id"].as_str().expect("$id");
            assert!(id.starts_with("gts://"), "{id} must be a gts URI");
            assert!(id.ends_with('~'), "{id} must be a type schema");
        }
    }

    #[test]
    fn the_catalog_covers_builtins_and_reserved_identifiers() {
        let ids: Vec<String> = plugin_catalog()
            .iter()
            .filter_map(|e| e["$id"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(ids.len(), 12);
        for reserved in [
            BASIC_AUTH_PLUGIN_ID,
            BEARER_AUTH_PLUGIN_ID,
            TIMEOUT_GUARD_PLUGIN_ID,
            CORS_GUARD_PLUGIN_ID,
            LOGGING_TRANSFORM_PLUGIN_ID,
            METRICS_TRANSFORM_PLUGIN_ID,
        ] {
            assert!(
                ids.iter().any(|id| id.contains(reserved)),
                "{reserved} must be catalogued even though it has no implementation"
            );
        }
    }
}
