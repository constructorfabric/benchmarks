//! GTS type provisioning.
//!
//! `cpt-cf-oagw-contract-types-registry`: OAGW registers the type schemas for
//! its resources and the instances for every plugin identifier it catalogues.
//! Registration is what makes a resource type authorizable — RBAC validates a
//! role's `target_type` against the registry — and it is also what lets an
//! operator discover the plugin identifiers that exist without reading the
//! source.
//!
//! Catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`,
//! `metrics`) are registered here **and nowhere else**: they exist so the
//! registry can name them, while `PluginRegistries` deliberately refuses to
//! resolve them (`DESIGN.md` § *Plugin Identification Model*).

use std::sync::Arc;

use serde_json::{Value, json};
use types_registry_sdk::TypesRegistryClient;

use crate::domain::gts_helpers::{
    AUTH_PLUGIN_TYPE, CATALOG_PLUGIN_IDS, GUARD_PLUGIN_TYPE, PROTOCOL_GRPC, PROTOCOL_HTTP,
    PROXY_TYPE, ROUTE_TYPE, TRANSFORM_PLUGIN_TYPE, UPSTREAM_TYPE,
};

/// JSON Schema dialect the base envelopes are authored in.
const DIALECT: &str = "http://json-schema.org/draft-07/schema#";

/// Base type for the `protocol` enumeration.
const PROTOCOL_TYPE: &str = "gts.cf.core.oagw.protocol.v1~";

/// Render a GTS URI from a bare identifier.
fn uri(id: &str) -> String {
    format!("gts://{id}")
}

/// One permissive base type schema.
///
/// The wire contract for upstreams and routes is the JSON Schema shipped in
/// `docs/schemas/`, enforced by the DTO layer at write time. The registry
/// entry exists to make the *type* known and authorizable, so it stays open
/// rather than duplicating a schema that would then have two owners.
fn type_schema(id: &str, description: &str) -> Value {
    json!({
        "$id": uri(id),
        "$schema": DIALECT,
        "description": description,
        "type": "object",
    })
}

/// One catalogued instance.
fn instance(id: &str, description: &str) -> Value {
    json!({
        "$id": uri(id),
        "description": description,
    })
}

/// Every type schema OAGW owns, parents before children.
#[must_use]
pub fn type_schemas() -> Vec<Value> {
    vec![
        type_schema(
            UPSTREAM_TYPE,
            "OAGW upstream service — tenant-scoped root configuration object",
        ),
        type_schema(ROUTE_TYPE, "OAGW route — an API path on an upstream"),
        type_schema(
            PROXY_TYPE,
            "OAGW proxy invocation — permission target for outbound calls",
        ),
        type_schema(PROTOCOL_TYPE, "OAGW upstream wire protocol"),
        type_schema(
            AUTH_PLUGIN_TYPE,
            "OAGW auth plugin — outbound credential injection",
        ),
        type_schema(
            GUARD_PLUGIN_TYPE,
            "OAGW guard plugin — request/response validation and policy enforcement",
        ),
        type_schema(
            TRANSFORM_PLUGIN_TYPE,
            "OAGW transform plugin — request/response mutation",
        ),
    ]
}

/// Every instance OAGW catalogues: the two protocols and all twelve plugin
/// identifiers, implemented or catalog-only.
#[must_use]
pub fn instances() -> Vec<Value> {
    let mut entities = vec![
        instance(PROTOCOL_HTTP, "HTTP upstream protocol"),
        instance(
            PROTOCOL_GRPC,
            "gRPC upstream protocol (catalogued; proxy path is phase 3)",
        ),
    ];
    for id in CATALOG_PLUGIN_IDS {
        entities.push(instance(id, plugin_description(id)));
    }
    entities
}

/// Human-readable description for a catalogued plugin identifier.
fn plugin_description(id: &str) -> &'static str {
    use crate::domain::gts_helpers as gts;
    match id {
        _ if id == gts::NOOP_AUTH_PLUGIN_ID => "No authentication",
        _ if id == gts::APIKEY_AUTH_PLUGIN_ID => "API key injection (header/query)",
        _ if id == gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID => {
            "OAuth2 client credentials flow (credentials in the request body)"
        }
        _ if id == gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID => {
            "OAuth2 client credentials flow (credentials in the Authorization header)"
        }
        _ if id == gts::BASIC_AUTH_PLUGIN_ID => {
            "HTTP Basic authentication; catalog identifier only, no backing implementation"
        }
        _ if id == gts::BEARER_AUTH_PLUGIN_ID => {
            "Bearer token injection; catalog identifier only, no backing implementation"
        }
        _ if id == gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID => {
            "Required header enforcement (request/response)"
        }
        _ if id == gts::TIMEOUT_GUARD_PLUGIN_ID => {
            "Request timeout enforcement; core Data Plane configuration, not plugins-bindable"
        }
        _ if id == gts::CORS_GUARD_PLUGIN_ID => {
            "CORS validation; core Data Plane logic via Upstream.cors, not plugins-bindable"
        }
        _ if id == gts::REQUEST_ID_TRANSFORM_PLUGIN_ID => "X-Request-ID propagation",
        _ if id == gts::LOGGING_TRANSFORM_PLUGIN_ID => {
            "Request/response logging; core Data Plane instrumentation"
        }
        _ if id == gts::METRICS_TRANSFORM_PLUGIN_ID => {
            "Prometheus metrics collection; core Data Plane instrumentation"
        }
        _ => "OAGW plugin",
    }
}

/// Register OAGW's type catalogue with the types-registry.
///
/// Per-item failures are logged and skipped. A registry that rejects one
/// descriptive entry must not stop the gateway from serving traffic — the
/// entries are for discovery and authorization metadata, not for the request
/// path.
pub async fn provision(registry: &Arc<dyn TypesRegistryClient>) {
    register_batch(registry, type_schemas(), "type schema").await;
    register_batch(registry, instances(), "instance").await;
}

async fn register_batch(registry: &Arc<dyn TypesRegistryClient>, entities: Vec<Value>, kind: &str) {
    let count = entities.len();
    match registry.register(entities).await {
        Ok(results) => {
            let mut failed = 0;
            for result in &results {
                if let types_registry_sdk::RegisterResult::Err { gts_id, error } = result {
                    failed += 1;
                    tracing::warn!(
                        target: "oagw.types",
                        gts_id = gts_id.as_deref().unwrap_or("<unknown>"),
                        error = %error,
                        "could not register an OAGW {kind}"
                    );
                }
            }
            tracing::info!(
                target: "oagw.types",
                registered = count - failed,
                failed,
                "registered OAGW {kind}s with the types-registry"
            );
        }
        Err(err) => tracing::warn!(
            target: "oagw.types",
            error = %err,
            "types-registry rejected the OAGW {kind} batch"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_schema_ids_end_with_a_tilde() {
        for schema in type_schemas() {
            let id = schema["$id"].as_str().expect("$id");
            assert!(id.starts_with("gts://gts."), "{id}");
            assert!(id.ends_with('~'), "a type schema id must end with ~: {id}");
            assert_eq!(schema["$schema"], DIALECT);
        }
    }

    #[test]
    fn instance_ids_do_not_end_with_a_tilde() {
        for entity in instances() {
            let id = entity["$id"].as_str().expect("$id");
            assert!(id.starts_with("gts://gts."), "{id}");
            assert!(
                !id.ends_with('~'),
                "an instance id must not end with ~: {id}"
            );
        }
    }

    #[test]
    fn every_catalogued_plugin_is_registered_with_a_description() {
        let ids: Vec<String> = instances()
            .iter()
            .map(|entity| {
                entity["$id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("gts://")
                    .to_owned()
            })
            .collect();
        for id in CATALOG_PLUGIN_IDS {
            assert!(ids.contains(&(*id).to_owned()), "{id} is not catalogued");
            assert_ne!(
                plugin_description(id),
                "OAGW plugin",
                "{id} needs its own description"
            );
        }
        assert_eq!(CATALOG_PLUGIN_IDS.len(), 12);
    }

    #[test]
    fn catalog_includes_both_protocols() {
        let ids: Vec<String> = instances()
            .iter()
            .map(|e| e["$id"].as_str().unwrap().to_owned())
            .collect();
        assert!(ids.contains(&uri(PROTOCOL_HTTP)));
        assert!(ids.contains(&uri(PROTOCOL_GRPC)));
    }
}
