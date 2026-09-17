//! GTS type-schema and instance catalogue published to the types-registry
//! (DESIGN §3.1, §3.6).
//!
//! Everything the gear catalogs: the persistent management-entity schemas
//! (verbatim from `docs/schemas/` with the GTS type id injected as `$id`),
//! the remaining resource type-schemas declared inline, the upstream
//! connection protocols, and every well-known plugin identifier — bindable
//! builtins and catalog-only ids alike (see `gts_helpers.rs`).
//!
//! GTS registration is strict in ready mode: every type-schema must carry
//! its id in `gts://` URI form and every instance must conform to its
//! type-schema (the id chain picks the parent type). The catalogue is
//! therefore built self-consistently — type-schemas first, minimal instance
//! documents second — so the registry's ready-commit never sees an orphaned
//! instance. Registration is **best-effort** per item: the catalogue is
//! discovery metadata; the management and data planes resolve plugins,
//! protocols and validation in-process and never depend on the registry. A
//! per-item rejection (for example a not-yet-registered chain parent in a
//! fresh registry) is logged and skipped rather than aborting gear startup.

use serde_json::{Value, json};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::gts_helpers;

/// Persistent upstream schema, verbatim from the docs.
const UPSTREAM_SCHEMA: &str = include_str!("../../docs/schemas/upstream.v1.schema.json");
/// Persistent route schema, verbatim from the docs.
const ROUTE_SCHEMA: &str = include_str!("../../docs/schemas/route.v1.schema.json");

/// All type-schema documents OAGW publishes (GTS ids end with `~`).
#[must_use]
pub fn type_schemas() -> Vec<Value> {
    vec![
        schema_from_file(gts_helpers::UPSTREAM_TYPE_ID, UPSTREAM_SCHEMA),
        schema_from_file(gts_helpers::ROUTE_TYPE_ID, ROUTE_SCHEMA),
        inline_schema(
            gts_helpers::PROXY_TYPE_ID,
            "OAGW Proxy",
            "Data-plane resource type; permission target for proxy invocation.",
            &[],
        ),
        inline_schema(
            gts_helpers::PLUGIN_TYPE_ID,
            "OAGW Custom Plugin",
            "A stored custom plugin definition parameterizing a builtin implementation.",
            &[],
        ),
        inline_schema(
            gts_helpers::AUTH_PLUGIN_TYPE_ID,
            "OAGW Auth Plugin",
            "Upstream authentication plugin (noop / apikey / OAuth2 client-credentials / catalog-only basic & bearer).",
            &[],
        ),
        inline_schema(
            gts_helpers::GUARD_PLUGIN_TYPE_ID,
            "OAGW Guard Plugin",
            "Inbound request guard (required_headers; catalog-only timeout & cors).",
            &[],
        ),
        inline_schema(
            gts_helpers::TRANSFORM_PLUGIN_TYPE_ID,
            "OAGW Transform Plugin",
            "Request/response transform (request_id; catalog-only logging & metrics).",
            &[],
        ),
        inline_schema(
            gts_helpers::PROTOCOL_TYPE_ID,
            "OAGW Upstream Protocol",
            "Upstream connection protocol namespace (http, grpc).",
            &["name"],
        ),
        plugin_spec_schema(),
    ]
}

/// All instance documents OAGW publishes — protocols plus the well-known
/// plugin identifiers (builtins and catalog-only ids alike).
#[must_use]
pub fn instances() -> Vec<Value> {
    let mut out = vec![
        protocol_instance(gts_helpers::PROTOCOL_HTTP_ID, "HTTP"),
        protocol_instance(gts_helpers::PROTOCOL_GRPC_ID, "gRPC"),
        plugin_instance(gts_helpers::AUTH_PLUGIN_NOOP),
        plugin_instance(gts_helpers::AUTH_PLUGIN_APIKEY),
        plugin_instance(gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED),
        plugin_instance(gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC),
        plugin_instance(gts_helpers::AUTH_PLUGIN_BASIC_CATALOG),
        plugin_instance(gts_helpers::AUTH_PLUGIN_BEARER_CATALOG),
        plugin_instance(gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS),
        plugin_instance(gts_helpers::GUARD_PLUGIN_TIMEOUT_CATALOG),
        plugin_instance(gts_helpers::GUARD_PLUGIN_CORS_CATALOG),
        plugin_instance(gts_helpers::TRANSFORM_PLUGIN_REQUEST_ID),
        plugin_instance(gts_helpers::TRANSFORM_PLUGIN_LOGGING_CATALOG),
        plugin_instance(gts_helpers::TRANSFORM_PLUGIN_METRICS_CATALOG),
    ];
    out.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(b.get("id").and_then(Value::as_str).unwrap_or_default())
    });
    out
}

/// All catalogue documents (type-schemas first, then instances) — the exact
/// batch [`register_catalog`] publishes.
#[must_use]
pub fn catalog() -> Vec<Value> {
    let mut documents = type_schemas();
    documents.extend(instances());
    documents
}

/// Publish the catalogue to the types-registry.
///
/// Best-effort per item: a failed schema/instance only logs a warning so a
/// registry hiccup never blocks OAGW startup or the data plane.
///
/// # Errors
///
/// Returns an error only for catastrophic transport failures (registry
/// unavailable), never for per-item rejections.
pub async fn register_catalog(client: &dyn TypesRegistryClient) -> anyhow::Result<()> {
    let results = client.register(catalog()).await?;
    let mut failed = 0;
    for result in &results {
        if let RegisterResult::Err {
            gts_id: Some(gts_id),
            error,
        } = result
        {
            failed += 1;
            tracing::warn!(gts_id = %gts_id, %error, "OAGW GTS catalogue entry not registered");
        }
    }
    tracing::info!(
        registered = results.iter().filter(|r| r.is_ok()).count(),
        failed,
        "Published OAGW GTS catalogue to types-registry"
    );
    Ok(())
}

/// Load a `docs/schemas/*.v1.schema.json` file and inject the GTS type id
/// as its `$id`, in `gts://` URI form (the schema files omit it — it is
/// assigned at registration; gts-rust rejects a bare `gts.` id in `$id`).
fn schema_from_file(gts_type_id: &str, raw: &str) -> Value {
    let mut schema =
        serde_json::from_str(raw).unwrap_or_else(|_| json!({ "type": "object", "title": gts_type_id }));
    if let Some(object) = schema.as_object_mut() {
        object.insert("$id".to_owned(), json!(gts_uri(gts_type_id)));
    }
    schema
}

/// Minimal inline type-schema for a resource type. Structure is validated by
/// the JSON Schema dialect; semantic validation happens in-process.
/// `extra_properties` are declared alongside the mandatory `id` member.
fn inline_schema(
    gts_type_id: &str,
    title: &str,
    description: &str,
    extra_properties: &[&str],
) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "id".to_owned(),
        json!({ "type": "string", "description": "GTS instance identifier." }),
    );
    for extra in extra_properties {
        properties.insert((*extra).to_owned(), json!({ "type": "string" }));
    }
    json!({
        "$id": gts_uri(gts_type_id),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": title,
        "description": description,
        "type": "object",
        "properties": properties,
        "required": ["id"],
        "additionalProperties": false
    })
}

/// Derived type-schema for OAGW plugin instances, conforming to the toolkit
/// base [`PluginV1`](toolkit_gts::PluginV1) type
/// (`gts.cf.toolkit.plugins.plugin.v1~`).
///
/// Mirrors the canonical derived-schema shape produced by gts-rust's
/// `build_gts_allof_schema` (`allOf` over the base `$ref` plus the own
/// `properties`/`required`, no `additionalProperties`): the chain-compat
/// check rejects a derived schema that closes `additionalProperties` without
/// restating every ancestor property.
fn plugin_spec_schema() -> Value {
    json!({
        "$id": gts_uri(gts_helpers::OAGW_PLUGIN_SPEC_TYPE_ID),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "OAGW Plugin Spec",
        "description": "Conformance schema for OAGW builtin plugin instances (derived from the toolkit PluginV1 base type).",
        "type": "object",
        "allOf": [
            { "$ref": "gts://gts.cf.toolkit.plugins.plugin.v1~" },
            {
                "type": "object",
                "properties": {},
                "required": []
            }
        ]
    })
}

/// The `gts://` URI form of a GTS identifier, as schema `$id` fields must
/// carry it (per the GTS spec: "Do not place the canonical `gts.` string
/// directly in `$id`").
fn gts_uri(gts_type_id: &str) -> String {
    format!("gts://{gts_type_id}")
}

/// Instance document for an upstream connection protocol.
fn protocol_instance(gts_id: &str, name: &str) -> Value {
    json!({ "id": gts_id, "name": name })
}

/// Instance document for a well-known plugin identifier. The id chain picks
/// the parent type (`auth_plugin.v1~` / `guard_plugin.v1~` /
/// `transform_plugin.v1~`), whose schema admits exactly the `id` member.
fn plugin_instance(gts_id: &str) -> Value {
    json!({ "id": gts_id })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn every_type_schema_carries_its_gts_id() {
        let schemas = type_schemas();
        assert_eq!(schemas.len(), 9);
        for (index, schema) in schemas.iter().enumerate() {
            let gts_id = schema["$id"].as_str().unwrap_or_default();
            assert!(gts_id.ends_with('~'), "type schema id must end with '~': {gts_id}");
            assert!(
                gts_id.starts_with("gts://gts."),
                "schema $id is a gts:// URI: {gts_id}"
            );
            // All but the last are plain object schemas; the derived plugin
            // spec is composed via `allOf` over the toolkit base type.
            if index < schemas.len() - 1 {
                assert_eq!(schema["type"].as_str(), Some("object"));
            }
        }
        let derived = &schemas[schemas.len() - 1];
        assert!(derived.get("allOf").is_some(), "derived plugin spec composes its base");
        // The persistent schemas come verbatim from the docs (plus `$id`).
        let upstream = &schemas[0];
        assert_eq!(
            upstream["$id"].as_str(),
            Some("gts://gts.cf.core.oagw.upstream.v1~")
        );
        assert!(upstream.get("properties").is_some());
        assert_eq!(upstream["title"].as_str(), Some("OAGW Upstream Service"));
    }

    #[test]
    fn instances_conform_to_their_namespace() {
        let instances = instances();
        for instance in &instances {
            let gts_id = instance["id"].as_str().unwrap_or_default();
            assert!(!gts_id.ends_with('~'));
            assert!(gts_id.starts_with("gts.cf.core.oagw."));
        }
        // Plugin instances are bare id members — the shape their parent
        // type-schemas admit.
        let bearer = instances
            .iter()
            .find(|i| i["id"].as_str() == Some(gts_helpers::AUTH_PLUGIN_BEARER_CATALOG))
            .expect("bearer catalog instance present");
        assert_eq!(bearer, &json!({ "id": gts_helpers::AUTH_PLUGIN_BEARER_CATALOG }));
    }

    #[test]
    fn catalog_batch_is_parent_before_child() {
        // Type-schemas precede instances so in-process validation sees every
        // parent type before any of its instances.
        let documents = catalog();
        let mut saw_instance = false;
        for document in &documents {
            let is_schema = document["$id"]
                .as_str()
                .or_else(|| document["id"].as_str())
                .is_some_and(|id| id.ends_with('~'));
            if is_schema {
                assert!(!saw_instance, "type-schema after an instance in batch order");
            } else {
                saw_instance = true;
            }
        }
    }
}
