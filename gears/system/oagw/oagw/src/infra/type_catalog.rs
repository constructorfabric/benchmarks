//! GTS type provisioning (`cpt-cf-oagw-contract-types-registry`).
//!
//! OAGW owns a small type catalog: the base type-schemas for its
//! configuration objects and plugin families, plus one instance per protocol
//! and per built-in / reserved plugin identifier. The catalog is advisory —
//! it makes the identifiers discoverable — so a registration failure is
//! logged and never blocks startup.

use serde_json::{Value, json};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::domain::gts_helpers as gts;

/// JSON Schema dialect every OAGW type-schema is authored in.
const DIALECT: &str = "http://json-schema.org/draft-07/schema#";

fn gts_uri(id: &str) -> String {
    format!("gts://{id}")
}

/// Base type-schemas OAGW registers.
#[must_use]
pub fn type_schemas() -> Vec<Value> {
    vec![
        json!({
            "$id": gts_uri(gts::UPSTREAM_TYPE),
            "$schema": DIALECT,
            "title": "OAGW Upstream Service",
            "description": "External service target: endpoint pool, protocol, auth binding, \
                            header rules, rate limits, CORS and plugin chain. Unique per \
                            (tenant_id, alias).",
            "type": "object",
            "properties": {
                "id": { "type": "string", "format": "uuid", "readOnly": true },
                "enabled": { "type": "boolean", "default": true },
                "alias": { "type": "string", "pattern": "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$" },
                "tags": { "type": "array", "items": { "type": "string", "pattern": "^[a-z0-9_-]+$" } },
                "server": { "type": "object" },
                "protocol": { "type": "string", "format": "gts-identifier" },
                "auth": { "type": "object" },
                "headers": { "type": "object" },
                "plugins": { "type": "object" },
                "rate_limit": { "type": "object" },
                "cors": { "type": "object" }
            }
        }),
        json!({
            "$id": gts_uri(gts::ROUTE_TYPE),
            "$schema": DIALECT,
            "title": "OAGW Route",
            "description": "API path on an upstream: match rules, priority and route-level \
                            overrides for rate limits, CORS and plugins.",
            "type": "object",
            "properties": {
                "id": { "type": "string", "format": "uuid", "readOnly": true },
                "enabled": { "type": "boolean", "default": true },
                "priority": { "type": "integer", "default": 0 },
                "upstream_id": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string", "pattern": "^[a-z0-9_-]+$" } },
                "match": { "type": "object" },
                "plugins": { "type": "object" },
                "rate_limit": { "type": "object" },
                "cors": { "type": "object" }
            }
        }),
        json!({
            "$id": gts_uri("gts.cf.core.oagw.protocol.v1~"),
            "$schema": DIALECT,
            "title": "OAGW Upstream Protocol",
            "description": "Protocol used to connect to an upstream service.",
            "type": "object"
        }),
        json!({
            "$id": gts_uri(gts::AUTH_PLUGIN_TYPE),
            "$schema": DIALECT,
            "title": "OAGW Auth Plugin",
            "description": "Credential-injection plugin. One per upstream, executed before \
                            guards.",
            "type": "object"
        }),
        json!({
            "$id": gts_uri(gts::GUARD_PLUGIN_TYPE),
            "$schema": DIALECT,
            "title": "OAGW Guard Plugin",
            "description": "Validation / policy-enforcement plugin; may reject a request or a \
                            response.",
            "type": "object"
        }),
        json!({
            "$id": gts_uri(gts::TRANSFORM_PLUGIN_TYPE),
            "$schema": DIALECT,
            "title": "OAGW Transform Plugin",
            "description": "Request / response / error mutation plugin.",
            "type": "object"
        }),
        json!({
            "$id": gts_uri(gts::PROXY_TYPE),
            "$schema": DIALECT,
            "title": "OAGW Proxy",
            "description": "Pseudo-resource guarding the proxy data plane; the subject of the \
                            `:invoke` permission.",
            "type": "object"
        }),
    ]
}

/// Instances OAGW registers: the two protocols and every plugin identifier,
/// including the reserved catalog-only ones.
#[must_use]
pub fn instances() -> Vec<Value> {
    let mut out = vec![
        json!({
            "$id": gts_uri(gts::PROTOCOL_HTTP),
            "description": "HTTP/HTTPS upstream protocol."
        }),
        json!({
            "$id": gts_uri(gts::PROTOCOL_GRPC),
            "description": "gRPC upstream protocol (catalogued; proxying is Phase 3)."
        }),
    ];
    for id in gts::catalog_plugin_ids() {
        out.push(json!({
            "$id": gts_uri(id),
            "description": describe_plugin(id)
        }));
    }
    out
}

fn describe_plugin(id: &str) -> &'static str {
    match id {
        gts::NOOP_AUTH_PLUGIN_ID => "No authentication.",
        gts::APIKEY_AUTH_PLUGIN_ID => "API key injection (header or query).",
        gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID => {
            "OAuth2 client credentials flow, credentials in the form body."
        }
        gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID => {
            "OAuth2 client credentials flow, credentials in the Authorization header."
        }
        gts::BASIC_AUTH_PLUGIN_ID => {
            "HTTP Basic authentication; catalog identifier only, no backing implementation."
        }
        gts::BEARER_AUTH_PLUGIN_ID => {
            "Bearer token injection; catalog identifier only, no backing implementation."
        }
        gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID => {
            "Required header enforcement on the request and response phases."
        }
        gts::TIMEOUT_GUARD_PLUGIN_ID => {
            "Request timeout enforcement; core Data Plane config, not plugins-bindable."
        }
        gts::CORS_GUARD_PLUGIN_ID => {
            "CORS validation; core Data Plane config via Upstream.cors, not plugins-bindable."
        }
        gts::REQUEST_ID_TRANSFORM_PLUGIN_ID => "X-Request-ID propagation.",
        gts::LOGGING_TRANSFORM_PLUGIN_ID => {
            "Request/response logging; core instrumentation, not registry-resolvable."
        }
        gts::METRICS_TRANSFORM_PLUGIN_ID => {
            "Prometheus metrics collection; core instrumentation, not registry-resolvable."
        }
        _ => "OAGW plugin identifier.",
    }
}

/// Register the catalog. Failures are logged, never propagated: a missing
/// catalog entry makes an identifier undiscoverable, which must not keep the
/// gateway from serving traffic.
pub async fn provision(registry: &dyn TypesRegistryClient) {
    let mut entities = type_schemas();
    entities.extend(instances());
    let total = entities.len();
    match registry.register(entities).await {
        Ok(results) => {
            let failures: Vec<String> = results
                .iter()
                .filter_map(|result| match result {
                    RegisterResult::Ok { .. } => None,
                    RegisterResult::Err { gts_id, error } => Some(format!(
                        "{}: {error}",
                        gts_id.as_deref().unwrap_or("<unknown>")
                    )),
                })
                .collect();
            if failures.is_empty() {
                tracing::info!(
                    target: "oagw.types",
                    entities = total,
                    "OAGW GTS type catalog registered"
                );
            } else {
                tracing::warn!(
                    target: "oagw.types",
                    entities = total,
                    failed = failures.len(),
                    details = %failures.join("; "),
                    "part of the OAGW GTS type catalog could not be registered"
                );
            }
        }
        Err(err) => tracing::warn!(
            target: "oagw.types",
            error = %err,
            "OAGW GTS type catalog registration failed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{instances, type_schemas};
    use crate::domain::gts_helpers as gts;

    #[test]
    fn every_type_schema_id_ends_with_the_type_marker() {
        for schema in type_schemas() {
            let id = schema["$id"].as_str().expect("$id");
            assert!(id.starts_with("gts://"), "{id} must be a GTS URI");
            assert!(id.ends_with('~'), "{id} must be a type-schema id");
            assert!(schema["type"].is_string(), "{id} must declare a type");
            assert!(schema["$schema"].is_string(), "{id} must name a dialect");
        }
    }

    #[test]
    fn instances_cover_every_catalogued_plugin_and_protocol() {
        let ids: Vec<String> = instances()
            .iter()
            .map(|i| i["$id"].as_str().unwrap_or_default().to_owned())
            .collect();
        for id in gts::catalog_plugin_ids() {
            assert!(
                ids.contains(&format!("gts://{id}")),
                "{id} must be catalogued"
            );
        }
        assert!(ids.contains(&format!("gts://{}", gts::PROTOCOL_HTTP)));
        assert!(ids.contains(&format!("gts://{}", gts::PROTOCOL_GRPC)));
    }

    #[test]
    fn no_instance_id_is_mistaken_for_a_type_schema() {
        for instance in instances() {
            let id = instance["$id"].as_str().expect("$id");
            assert!(!id.ends_with('~'), "{id} is an instance, not a type");
        }
    }

    #[test]
    fn every_instance_carries_a_description() {
        for instance in instances() {
            let description = instance["description"].as_str().unwrap_or_default();
            assert!(
                !description.is_empty(),
                "{} must be described",
                instance["$id"]
            );
        }
    }
}
