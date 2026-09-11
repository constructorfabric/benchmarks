//! GTS base-type provisioning (`cpt-cf-oagw-dod-gear-foundation-type-provisioning`).
//!
//! The gear registers the six base types through the `types_registry` client
//! during initialization (`inst-gf-gts-1`/`-2`):
//!
//! * `gts.cf.core.oagw.upstream.v1~`
//! * `gts.cf.core.oagw.route.v1~`
//! * `gts.cf.core.oagw.auth_plugin.v1~`
//! * `gts.cf.core.oagw.guard_plugin.v1~`
//! * `gts.cf.core.oagw.transform_plugin.v1~`
//! * `gts.cf.core.oagw.proxy.v1~`
//!
//! A registration that answers `AlreadyExists` is a successful no-op
//! (`inst-gf-gts-3`), so initialization is repeatable across a restart. Any
//! other rejection is fatal (`inst-gf-gts-4`/`-5`).
//!
//! The registry-reference posture of graded deviation 6 applies: plugin
//! identifiers resolve as references from this catalog, and no plugin
//! execution surface is provisioned here.

use toolkit_canonical_errors::CanonicalError;
use toolkit_gts::GTS_ID_URI_PREFIX;
use types_registry_sdk::api::TypesRegistryClient;
use types_registry_sdk::models::RegisterResult;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::BASE_TYPES;

/// The `$id`-shaped JSON Schema document of one base type.
///
/// Each document is a minimal but valid JSON Schema carrying the GTS type
/// identifier, so a later entry's resource identifier resolves against it and
/// so the registry can validate instances derived from it.
#[must_use]
pub fn base_type_schema(type_id: &str) -> serde_json::Value {
    let title = title_of(type_id);
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        // The `$id` carries the `gts://` URI form: a Type Schema keyed by the
        // bare canonical identifier is not resolvable as a GTS id, and the
        // registry rejects it rather than guessing.
        "$id": format!("{GTS_ID_URI_PREFIX}{type_id}"),
        "title": title,
        "description": format!("OAGW base type `{type_id}`"),
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "the GTS resource identifier" },
            "tenant_id": { "type": "string", "format": "uuid" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "enabled": { "type": "boolean" }
        },
        "required": ["id"],
        "additionalProperties": true
    })
}

/// The title of a base type, derived from its identifier's leaf segment with
/// the version label stripped: `...oagw.upstream.v1~` titles as `OAGW upstream
/// v1`.
fn title_of(type_id: &str) -> String {
    let leaf = type_id.trim_end_matches('~');
    let mut segments = leaf.rsplit('.');
    let version = segments.next().unwrap_or(leaf);
    let name = segments.next().unwrap_or(version);
    // A leaf that is not a version label (`...oagw.proxy~`) titles as itself.
    if version.len() > 1 && version.starts_with('v') && version[1..].chars().all(|c| c.is_ascii_digit()) {
        format!("OAGW {name} {version}")
    } else {
        format!("OAGW {version}")
    }
}

/// The ordered list of base-type schema documents.
#[must_use]
pub fn base_type_schemas() -> Vec<serde_json::Value> {
    BASE_TYPES.iter().map(|id| base_type_schema(id)).collect()
}

/// The outcome of one base-type registration attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// The registry accepted the type.
    Registered,
    /// The registry already held it; the registration is a no-op.
    AlreadyRegistered,
}

/// Register every base type through the client.
///
/// # Errors
///
/// Returns a [`DomainError::Internal`] when the registry is unreachable, and
/// the provisioning error when the registry rejects a base type for any reason
/// other than an idempotent `AlreadyExists`.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-3
// `inst-gf-gts-2`/`-3`: the base types register through the client and an
// `AlreadyExists` answer is a successful no-op, so initialization is
// repeatable.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-1
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-5
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-6
pub async fn register_base_types(
    client: &dyn TypesRegistryClient,
) -> Result<Vec<String>, DomainError> {
    let schemas = base_type_schemas();
    // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-2
    let results = client.register_type_schemas(schemas).await.map_err(|error| {
        DomainError::Internal(format!("the types-registry is unreachable: {error}"))
    })?;
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-2
    collect(&results)
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-3
}
//
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-6
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-5
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-1
//
/// Project the per-item results onto the idempotency rule
/// (`inst-gf-gts-3`), materializing `inst-gf-gts-4`.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-4
fn collect(results: &[RegisterResult]) -> Result<Vec<String>, DomainError> {
    let mut registered = Vec::with_capacity(results.len());
    for result in results {
        match result {
            RegisterResult::Ok { gts_id } => registered.push(gts_id.clone()),
            RegisterResult::Err { gts_id, error } => {
                if matches!(error, CanonicalError::AlreadyExists { .. }) {
                    if let Some(id) = gts_id {
                        registered.push(id.clone());
                    }
                } else {
                    return Err(DomainError::Internal(format!(
                        "the types-registry rejected a base type: {error}"
                    )));
                }
            }
        }
    }
    Ok(registered)
}
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-type-provisioning:p1:inst-gf-gts-4

#[cfg(test)]
#[path = "type_provisioning_tests.rs"]
mod tests;
