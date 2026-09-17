//! GTS type provisioning: registers the OAGW type-schemas and the built-in
//! plugin/protocol instances (the "catalog") with the types-registry gear.
//!
//! Registration is **best-effort and non-fatal**: the types-registry may
//! already hold these entities (duplicate registration then yields
//! per-item `AlreadyExists` results, which are tolerated), and a
//! temporarily unavailable registry must not prevent the OAGW gear from
//! becoming live. Per-item failures are logged at `warn`.
//!
//! See DESIGN §"Key Domain Entities" and §"Plugin Schemas" for the
//! identifier list this mirrors.

use serde_json::{Value, json};
use tracing::warn;
use types_registry_sdk::TypesRegistryClient;

use crate::domain::gts as g;

const JSON_SCHEMA_DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";

/// A root type-schema (GTS id ends with `~`).
///
/// The `$id` field must use the `gts://` URI form — per the GTS spec, the bare
/// canonical `gts.` form is deliberately rejected by `GtsEntity::extract_type_ids`
/// ("Do not place the canonical gts. string directly in `$id`"); the registry
/// strips the `gts://` prefix and keys the schema by `gts.cf.core.oagw.<type>.v1~`.
fn schema(id: &str, title: &str, description: &str) -> Value {
    json!({
        "$id": format!("gts://{id}"),
        "$schema": JSON_SCHEMA_DRAFT_07,
        "type": "object",
        "title": title,
        "description": description,
    })
}

/// A well-known instance of a base type (GTS id does not end with `~`).
fn instance(id: &str, parent: &str, title: &str, description: &str) -> Value {
    json!({
        "id": id,
        "type": parent,
        "title": title,
        "description": description,
    })
}

/// The full catalog of OAGW GTS entities.
#[must_use]
pub fn oagw_entities() -> Vec<Value> {
    let mut entities = Vec::new();

    // Root type-schemas.
    entities.push(schema(
        g::UPSTREAM_TYPE,
        "Upstream",
        "Outbound API gateway upstream configuration (server, auth, headers, plugins, rate limits, CORS).",
    ));
    entities.push(schema(
        g::ROUTE_TYPE,
        "Route",
        "Route bound to an upstream: HTTP match rules, priority, and per-route overrides.",
    ));
    entities.push(schema(
        g::AUTH_PLUGIN_TYPE,
        "Auth Plugin",
        "Outbound credential-injection plugin (one per upstream).",
    ));
    entities.push(schema(
        g::GUARD_PLUGIN_TYPE,
        "Guard Plugin",
        "Request/response validation plugin.",
    ));
    entities.push(schema(
        g::TRANSFORM_PLUGIN_TYPE,
        "Transform Plugin",
        "Request/response mutation plugin.",
    ));
    entities.push(schema(
        g::PROTOCOL_TYPE,
        "Protocol",
        "Upstream transport protocol classification.",
    ));
    entities.push(schema(
        g::PROXY_TYPE,
        "Proxy",
        "Data-plane proxy invocation resource (authorized via `:invoke`).",
    ));

    // Protocol instances.
    entities.push(instance(
        g::PROTOCOL_HTTP_TYPE,
        g::PROTOCOL_TYPE,
        "HTTP",
        "HTTP/1.x upstream transport.",
    ));
    entities.push(instance(
        g::PROTOCOL_GRPC_TYPE,
        g::PROTOCOL_TYPE,
        "gRPC",
        "gRPC upstream transport (Phase 3 — no proxy code path yet).",
    ));

    // Auth plugin instances (resolvable built-ins).
    for (id, name, desc) in [
        (
            g::AUTH_NOOP,
            "Noop",
            "No credential injection.",
        ),
        (
            g::AUTH_APIKEY,
            "API Key",
            "API key injection via header and/or query parameter.",
        ),
        (
            g::AUTH_OAUTH2_CLIENT_CRED,
            "OAuth2 Client Credentials (Form)",
            "RFC 6749 §4.4 client-credentials flow with credentials in the request body.",
        ),
        (
            g::AUTH_OAUTH2_CLIENT_CRED_BASIC,
            "OAuth2 Client Credentials (Basic)",
            "RFC 6749 §4.4 client-credentials flow with HTTP Basic client authentication.",
        ),
    ] {
        entities.push(instance(id, g::AUTH_PLUGIN_TYPE, name, desc));
    }
    // Auth plugin instances (catalog-only: reserved, not resolvable).
    entities.push(instance(
        g::AUTH_BASIC,
        g::AUTH_PLUGIN_TYPE,
        "Basic",
        "HTTP Basic auth (reserved — no backing implementation; binding fails).",
    ));
    entities.push(instance(
        g::AUTH_BEARER,
        g::AUTH_PLUGIN_TYPE,
        "Bearer",
        "Static bearer token auth (reserved — no backing implementation; binding fails).",
    ));

    // Guard plugin instances.
    entities.push(instance(
        g::GUARD_REQUIRED_HEADERS,
        g::GUARD_PLUGIN_TYPE,
        "Required Headers",
        "Enforces presence of configured headers on requests and/or responses.",
    ));
    entities.push(instance(
        g::GUARD_TIMEOUT,
        g::GUARD_PLUGIN_TYPE,
        "Timeout",
        "Request timeout (core data-plane functionality — catalog-only).",
    ));
    entities.push(instance(
        g::GUARD_CORS,
        g::GUARD_PLUGIN_TYPE,
        "CORS",
        "Cross-origin resource sharing (core data-plane functionality — catalog-only).",
    ));

    // Transform plugin instances.
    entities.push(instance(
        g::TRANSFORM_REQUEST_ID,
        g::TRANSFORM_PLUGIN_TYPE,
        "Request ID",
        "X-Request-ID injection and propagation.",
    ));
    entities.push(instance(
        g::TRANSFORM_LOGGING,
        g::TRANSFORM_PLUGIN_TYPE,
        "Logging",
        "Data-plane instrumentation (core functionality — catalog-only).",
    ));
    entities.push(instance(
        g::TRANSFORM_METRICS,
        g::TRANSFORM_PLUGIN_TYPE,
        "Metrics",
        "Data-plane metrics (core functionality — catalog-only).",
    ));

    entities
}

/// Register the OAGW catalog with the types-registry. Never fails the
/// caller — per-item and transport failures are logged and absorbed.
///
/// # Errors
///
/// No error is returned; registration problems are surfaced via `tracing`.
pub async fn register_oagw_types(registry: &dyn TypesRegistryClient) {
    let entities = oagw_entities();
    match registry.register(entities.clone()).await {
        Ok(results) => {
            let mut failures = 0usize;
            for result in results {
                if let types_registry_sdk::RegisterResult::Err { gts_id, error } = result {
                    failures += 1;
                    warn!(
                        gts_id = ?gts_id,
                        error = %error,
                        "types-registry rejected OAGW entity"
                    );
                }
            }
            if failures > 0 {
                warn!(
                    failures,
                    total = entities.len(),
                    "types-registry registration completed with per-item failures"
                );
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                total = entities.len(),
                "types-registry registration failed (gear continues)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_contains_expected_ids() {
        let entities = oagw_entities();
        // Instance `id` values are bare canonical ids; schema `$id` values use
        // the `gts://` URI form (registry strips the prefix). Normalize both.
        let owned: Vec<String> = entities
            .iter()
            .filter_map(|e| e.get("$id").or_else(|| e.get("id")).and_then(Value::as_str))
            .map(|s| s.strip_prefix("gts://").unwrap_or(s).to_owned())
            .collect();
        assert!(owned.iter().any(|id| id == g::UPSTREAM_TYPE));
        assert!(owned.iter().any(|id| id == g::AUTH_OAUTH2_CLIENT_CRED_BASIC));
        assert!(owned.iter().any(|id| id == g::GUARD_REQUIRED_HEADERS));
        assert!(owned.iter().any(|id| id == g::TRANSFORM_REQUEST_ID));
        assert!(owned.iter().any(|id| id == g::PROTOCOL_HTTP_TYPE));
        // Catalog-only ids are present too.
        assert!(owned.iter().any(|id| id == g::AUTH_BEARER));
        assert!(owned.iter().any(|id| id == g::TRANSFORM_LOGGING));
        // Exactly the 7 root schemas end with `~`.
        let schemas = entities
            .iter()
            .filter(|e| e.get("$id").is_some())
            .count();
        assert_eq!(schemas, 7);
    }

    #[test]
    fn instances_carry_their_parent_type() {
        let entities = oagw_entities();
        let auth_instance = entities
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(g::AUTH_APIKEY))
            .expect("apikey instance");
        assert_eq!(
            auth_instance.get("type").and_then(Value::as_str),
            Some(g::AUTH_PLUGIN_TYPE)
        );
    }

    #[test]
    fn catalog_has_exactly_fourteen_instances_and_seven_schemas() {
        let entities = oagw_entities();
        let schemas = entities
            .iter()
            .filter(|e| e.get("$id").is_some())
            .count();
        let instances = entities
            .iter()
            .filter(|e| e.get("id").is_some())
            .count();
        assert_eq!(schemas, 7);
        assert_eq!(instances, 14);
        assert_eq!(entities.len(), 21);
    }

    #[test]
    fn schema_ids_normalize_to_canonical_type_ids() {
        let entities = oagw_entities();
        let schema_ids: Vec<String> = entities
            .iter()
            .filter_map(|e| e.get("$id").and_then(Value::as_str))
            .map(|s| s.strip_prefix("gts://").unwrap_or(s).to_owned())
            .collect();
        for expected in [
            g::UPSTREAM_TYPE,
            g::ROUTE_TYPE,
            g::AUTH_PLUGIN_TYPE,
            g::GUARD_PLUGIN_TYPE,
            g::TRANSFORM_PLUGIN_TYPE,
            g::PROTOCOL_TYPE,
            g::PROXY_TYPE,
        ] {
            assert!(
                schema_ids.iter().any(|id| id == expected),
                "missing root schema {expected}"
            );
        }
    }

    #[test]
    fn every_catalog_only_id_is_registered() {
        let entities = oagw_entities();
        let owned: Vec<String> = entities
            .iter()
            .filter_map(|e| e.get("$id").or_else(|| e.get("id")).and_then(Value::as_str))
            .map(|s| s.strip_prefix("gts://").unwrap_or(s).to_owned())
            .collect();
        for expected in [
            g::AUTH_BASIC,
            g::AUTH_BEARER,
            g::GUARD_TIMEOUT,
            g::GUARD_CORS,
            g::TRANSFORM_LOGGING,
            g::TRANSFORM_METRICS,
            g::PROTOCOL_GRPC_TYPE,
        ] {
            assert!(owned.iter().any(|id| id == expected), "missing {expected}");
        }
    }
}
