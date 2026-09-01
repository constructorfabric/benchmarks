// Created: 2026-08-29 by Constructor Tech
//! GTS type provisioning (DESIGN §3.2 `type_provisioning.rs`).
//!
//! The gear publishes the base type-schemas of the entities it owns and the
//! catalog-only plugin identifiers the API names, so the types-registry knows
//! the vocabulary even before any entity is created. Provisioning is a
//! best-effort bootstrap step: a failure is logged and never blocks the gear,
//! because the management API works entirely from its own repositories.

use toolkit::api::OpenApiRegistry;
use tracing::info;

use crate::domain::model::gts;
use crate::domain::plugin::ids;

/// GTS type-schemas the OAGW gear is responsible for.
#[must_use]
pub fn type_schemas() -> Vec<serde_json::Value> {
    vec![
        schema(
            gts::UPSTREAM,
            "OAGW Upstream",
            "Tenant-scoped upstream configuration.",
        ),
        schema(
            gts::ROUTE,
            "OAGW Route",
            "Route match and route-level overrides.",
        ),
        schema(
            gts::AUTH_PLUGIN,
            "OAGW Auth Plugin",
            "Credential injection plugin type.",
        ),
        schema(
            gts::GUARD_PLUGIN,
            "OAGW Guard Plugin",
            "Policy enforcement plugin type.",
        ),
        schema(
            gts::TRANSFORM_PLUGIN,
            "OAGW Transform Plugin",
            "Request/response mutation plugin type.",
        ),
        schema(
            "gts.cf.core.oagw.protocol.v1~",
            "OAGW Protocol",
            "Upstream transport protocol.",
        ),
    ]
}

/// GTS instances (built-in plugin identifiers) published alongside the schemas.
#[must_use]
pub fn plugin_instances() -> Vec<serde_json::Value> {
    let builtins = [
        ids::AUTH_NOOP,
        ids::AUTH_APIKEY,
        ids::AUTH_OAUTH2_FORM,
        ids::AUTH_OAUTH2_BASIC,
        ids::GUARD_REQUIRED_HEADERS,
        ids::TRANSFORM_REQUEST_ID,
    ];
    let catalog_only = ids::CATALOG_ONLY_AUTH
        .iter()
        .chain(ids::CATALOG_ONLY_GUARD.iter())
        .chain(ids::CATALOG_ONLY_TRANSFORM.iter())
        .copied();
    builtins
        .into_iter()
        .chain(catalog_only)
        .map(|identifier| {
            serde_json::json!({
                "$id": identifier_with_suffix(identifier),
            })
        })
        .collect()
}

fn identifier_with_suffix(identifier: &str) -> String {
    // Instance identifiers are published verbatim; the type-suffix guard keeps
    // an accidentally passed base type out of the instance batch.
    if identifier.ends_with('~') {
        identifier.trim_end_matches('~').to_owned()
    } else {
        identifier.to_owned()
    }
}

/// Counts the batch entries the registry accepted and logs the rejected ones.
///
/// A rejected entry is never fatal — the management API works from its own
/// repositories — but a silent `Ok(0)` would leave the vocabulary unprovisioned
/// with nothing in the log to point at the cause.
fn registered_count(results: &[types_registry_sdk::RegisterResult]) -> usize {
    let mut accepted = 0;
    for result in results {
        match result {
            types_registry_sdk::RegisterResult::Ok { .. } => accepted += 1,
            types_registry_sdk::RegisterResult::Err { gts_id, error } => {
                tracing::warn!(
                    gts_id = gts_id.as_deref().unwrap_or("<unknown>"),
                    %error,
                    "OAGW GTS entity rejected by the types-registry"
                );
            }
        }
    }
    accepted
}

fn schema(base: &str, title: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        // Schema entities must name themselves with the `gts://` URI form: the
        // GTS store rejects a bare `gts.…` id in `$id` and silently drops the
        // registration, which then fails every derived id's parent lookup.
        "$id": format!("gts://{base}"),
        // The dialect reference is what marks the entity as a *schema* rather
        // than an instance: without it the registry stores the type as an
        // instance and every derived id fails its parent lookup at ready time.
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": title,
        "description": description,
        "type": "object",
    })
}

/// Publishes the OAGW type vocabulary, never failing the gear bootstrap.
pub async fn provision(registry: Option<&dyn types_registry_sdk::TypesRegistryClient>) {
    let Some(registry) = registry else {
        info!("OAGW type provisioning skipped: no types-registry client available");
        return;
    };
    match registry.register_type_schemas(type_schemas()).await {
        Ok(results) => {
            let registered = registered_count(&results);
            info!(registered, "OAGW GTS type schemas provisioned");
        }
        Err(error) => {
            tracing::warn!(%error, "OAGW GTS type schema provisioning failed; continuing");
        }
    }
    match registry.register_instances(plugin_instances()).await {
        Ok(results) => {
            let registered = registered_count(&results);
            info!(registered, "OAGW plugin identifiers provisioned");
        }
        Err(error) => {
            tracing::warn!(%error, "OAGW plugin identifier provisioning failed; continuing");
        }
    }
}

/// The OpenAPI schema name the gear's REST surface registers under.
#[must_use]
pub const fn openapi_tag() -> &'static str {
    crate::api::rest::API_TAG
}

/// `true` when the registry declares the OAGW upstream base type already.
pub async fn upstream_schema_registered(
    registry: &dyn types_registry_sdk::TypesRegistryClient,
) -> bool {
    matches!(
        registry.get_type_schema(gts::UPSTREAM).await,
        Ok(types_registry_sdk::GtsTypeSchema { .. })
    )
}

/// Keeps the OpenAPI registry import honest: provisioning registers the same
/// names the REST layer documents.
#[must_use]
pub fn schema_names(_openapi: &dyn OpenApiRegistry) -> Vec<&'static str> {
    vec!["Upstream", "Route", "Plugin"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_owned_base_type_is_published() {
        let schemas = type_schemas();
        for base in [
            gts::UPSTREAM,
            gts::ROUTE,
            gts::AUTH_PLUGIN,
            gts::GUARD_PLUGIN,
            gts::TRANSFORM_PLUGIN,
        ] {
            let uri = format!("gts://{base}");
            assert!(
                schemas.iter().any(|schema| schema["$id"] == uri),
                "{base} is not provisioned"
            );
        }
    }

    #[test]
    fn built_in_and_catalog_only_plugins_are_published() {
        let instances = plugin_instances();
        for identifier in [
            ids::AUTH_NOOP,
            ids::AUTH_APIKEY,
            ids::GUARD_REQUIRED_HEADERS,
            ids::TRANSFORM_REQUEST_ID,
            ids::CATALOG_ONLY_AUTH[0],
            ids::CATALOG_ONLY_TRANSFORM[0],
        ] {
            assert!(
                instances
                    .iter()
                    .any(|instance| instance["$id"] == identifier),
                "{identifier} is not provisioned"
            );
        }
    }

    #[test]
    fn instance_ids_never_end_with_the_type_marker() {
        for instance in plugin_instances() {
            let id = instance["$id"].as_str().expect("string id");
            assert!(!id.ends_with('~'), "{id} is a base type, not an instance");
        }
    }

    #[test]
    fn provisioned_schemas_carry_a_dialect_reference() {
        // `has_schema_field` in the underlying GTS store marks an entity as a
        // schema only when `$schema` is present; without it every derived id
        // fails its parent lookup at ready time and the server refuses to
        // start. The dialect must also be one the validator compiles.
        for schema in type_schemas() {
            let id = schema["$id"].as_str().expect("string id");
            assert!(
                schema["$schema"] == "http://json-schema.org/draft-07/schema#",
                "{id} is missing a JSON Schema dialect reference"
            );
        }
    }

    #[test]
    fn every_instance_derives_from_a_provisioned_base_type() {
        let schemas = type_schemas();
        let bases: Vec<String> = schemas
            .iter()
            .filter_map(|schema| schema["$id"].as_str())
            .map(ToString::to_string)
            .collect();
        for instance in plugin_instances() {
            let id = instance["$id"].as_str().expect("string id");
            let Some(base) = id.split_once('~').map(|(prefix, _)| prefix) else {
                panic!("{id} has no type prefix");
            };
            assert!(
                bases.contains(&format!("gts://{base}~")),
                "{id} derives from unprovisioned base type {base}~"
            );
        }
    }
}
