//! The plugin catalog the types-registry holds
//! (`cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
//!
//! Two sets of identifiers live here:
//!
//! * the six **built-in** plugin identifiers, which have a backing
//!   implementation in `infra/plugin/` and are bindable;
//! * the six **catalog-only** identifiers (`basic`, `bearer`, `timeout`,
//!   `cors`, `logging`, `metrics`), which the types-registry catalogs so a
//!   reference to them *resolves as a reference* but which no plugin registry
//!   resolves, so binding one is rejected at binding time and a binding never
//!   reaches the core timeout, CORS, logging, or metrics behavior (graded
//!   deviations 6 and 10).
//!
//! The catalog is provisioned as GTS **instances** under the three plugin
//! base types, which is why it is registered *after*
//! [`crate::infra::type_provisioning::register_base_types`]: an instance's
//! declaring type-schema must already be registered. It is a separate call
//! from the base-type provisioning, so the base-type catalog of entry 2.1 is
//! unchanged.

use serde_json::{json, Value};
use types_registry_sdk::api::TypesRegistryClient;
use types_registry_sdk::models::RegisterResult;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, AUTH_PLUGIN_BASE_TYPE, BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID,
    CATALOG_ONLY_PLUGIN_IDS, CORS_GUARD_PLUGIN_ID, GUARD_PLUGIN_BASE_TYPE,
    LOGGING_TRANSFORM_PLUGIN_ID, METRICS_TRANSFORM_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    REQUEST_ID_TRANSFORM_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID,
    TRANSFORM_PLUGIN_BASE_TYPE,
};

/// One catalog entry: a plugin identifier the types-registry holds.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    /// The concrete plugin GTS identifier.
    pub gts_id: &'static str,
    /// The plugin base type the identifier derives from.
    pub base_type: &'static str,
    /// The short label the instance segment carries: `noop`, `apikey`, …
    pub label: &'static str,
    /// Whether a plugin registry can resolve the identifier. `false` for the
    /// catalog-only set, which resolves as a reference only.
    pub bindable: bool,
    /// What the plugin does; catalog metadata only.
    pub summary: &'static str,
    /// The JSON schema an instance configuration must validate against before
    /// a binding row is stored.
    pub config_schema: Option<Value>,
}

/// The six built-in entries, in identifier order.
///
/// The two OAuth2 variants declare the same configuration shape and differ
/// only in `auth_method`, so their schemas are identical by construction.
#[must_use]
pub fn builtin_catalog() -> Vec<CatalogEntry> {
    vec![
        CatalogEntry {
            gts_id: NOOP_AUTH_PLUGIN_ID,
            base_type: AUTH_PLUGIN_BASE_TYPE,
            label: "noop",
            bindable: true,
            summary: "No authentication; the request passes through unauthenticated.",
            config_schema: Some(object_schema()),
        },
        CatalogEntry {
            gts_id: APIKEY_AUTH_PLUGIN_ID,
            base_type: AUTH_PLUGIN_BASE_TYPE,
            label: "apikey",
            bindable: true,
            summary: "Injects an API key resolved from `cred_store` into a header or a query parameter.",
            config_schema: Some(json!({
                "type": "object",
                "properties": {
                    "api_key_ref": {
                        "type": "string",
                        "format": "cred-reference",
                        "description": "the `cred://` reference the API key resolves from"
                    },
                    "location": {
                        "type": "string",
                        "description": "`header` (default) or `query`"
                    },
                    "name": {
                        "type": "string",
                        "description": "the header or query name, default `x-api-key`"
                    }
                },
                "required": ["api_key_ref"],
                "additionalProperties": false
            })),
        },
        CatalogEntry {
            gts_id: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            base_type: AUTH_PLUGIN_BASE_TYPE,
            label: "oauth2_client_cred",
            bindable: true,
            summary: "OAuth2 client credentials with the Form client-auth method at the token endpoint.",
            config_schema: Some(oauth2_schema()),
        },
        CatalogEntry {
            gts_id: OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            base_type: AUTH_PLUGIN_BASE_TYPE,
            label: "oauth2_client_cred_basic",
            bindable: true,
            summary: "OAuth2 client credentials with the Basic client-auth method at the token endpoint.",
            config_schema: Some(oauth2_schema()),
        },
        CatalogEntry {
            gts_id: REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            base_type: GUARD_PLUGIN_BASE_TYPE,
            label: "required_headers",
            bindable: true,
            summary: "Rejects a request or an upstream response that lacks a required header, presence-only.",
            config_schema: Some(json!({
                "type": "object",
                "properties": {
                    "required_request_headers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "comma-separated request header names, case-insensitive, presence-only"
                    },
                    "required_response_headers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "comma-separated response header names, case-insensitive, presence-only"
                    }
                },
                "additionalProperties": false
            })),
        },
        CatalogEntry {
            gts_id: REQUEST_ID_TRANSFORM_PLUGIN_ID,
            base_type: TRANSFORM_PLUGIN_BASE_TYPE,
            label: "request_id",
            bindable: true,
            summary: "Propagates the proxy-entry correlation identifier as `X-Request-ID`.",
            config_schema: Some(object_schema()),
        },
    ]
}

/// The six catalog-only entries, in identifier order.
#[must_use]
pub fn catalog_only_catalog() -> Vec<CatalogEntry> {
    vec![
        CatalogEntry {
            gts_id: BASIC_AUTH_PLUGIN_ID,
            base_type: AUTH_PLUGIN_BASE_TYPE,
            label: "basic",
            bindable: false,
            summary: "HTTP Basic authentication; catalog-only, no backing plugin (graded deviation 10).",
            config_schema: None,
        },
        CatalogEntry {
            gts_id: BEARER_AUTH_PLUGIN_ID,
            base_type: AUTH_PLUGIN_BASE_TYPE,
            label: "bearer",
            bindable: false,
            summary: "Bearer token injection; catalog-only, no backing plugin (graded deviation 10).",
            config_schema: None,
        },
        CatalogEntry {
            gts_id: TIMEOUT_GUARD_PLUGIN_ID,
            base_type: GUARD_PLUGIN_BASE_TYPE,
            label: "timeout",
            bindable: false,
            summary: "Request timeout is core Data Plane configuration; catalog-only (graded deviation 10).",
            config_schema: None,
        },
        CatalogEntry {
            gts_id: CORS_GUARD_PLUGIN_ID,
            base_type: GUARD_PLUGIN_BASE_TYPE,
            label: "cors",
            bindable: false,
            summary: "CORS is core Data Plane configuration on `Upstream.cors`; catalog-only (graded deviation 10).",
            config_schema: None,
        },
        CatalogEntry {
            gts_id: LOGGING_TRANSFORM_PLUGIN_ID,
            base_type: TRANSFORM_PLUGIN_BASE_TYPE,
            label: "logging",
            bindable: false,
            summary: "Core Data Plane instrumentation; catalog-only (graded deviation 10).",
            config_schema: None,
        },
        CatalogEntry {
            gts_id: METRICS_TRANSFORM_PLUGIN_ID,
            base_type: TRANSFORM_PLUGIN_BASE_TYPE,
            label: "metrics",
            bindable: false,
            summary: "Core Data Plane instrumentation; catalog-only (graded deviation 10).",
            config_schema: None,
        },
    ]
}

/// The whole plugin catalog: the built-ins followed by the catalog-only
/// identifiers.
#[must_use]
pub fn plugin_catalog() -> Vec<CatalogEntry> {
    let mut entries = builtin_catalog();
    entries.extend(catalog_only_catalog());
    entries
}

/// The catalog entry an identifier names, or `None` when the identifier is not
/// part of the OAGW plugin catalog.
#[must_use]
pub fn catalog_entry(gts_id: &str) -> Option<CatalogEntry> {
    plugin_catalog().into_iter().find(|entry| entry.gts_id == gts_id)
}

/// The `config_schema` a built-in identifier binds against, or `None` for an
/// identifier the catalog does not carry or that carries no schema.
///
/// A custom plugin's schema comes from its own `oagw_plugin` row instead.
#[must_use]
pub fn builtin_config_schema(gts_id: &str) -> Option<Value> {
    builtin_catalog()
        .into_iter()
        .find(|entry| entry.gts_id == gts_id)
        .and_then(|entry| entry.config_schema)
}

/// An empty, closed configuration schema: no keys are accepted.
fn object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

/// The OAuth2 client credentials configuration schema, shared by the Form and
/// the Basic variant.
fn oauth2_schema() -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "client_id_ref": {
                "type": "string",
                "format": "cred-reference",
                "description": "the `cred://` reference the client identifier resolves from"
            },
            "client_secret_ref": {
                "type": "string",
                "format": "cred-reference",
                "description": "the `cred://` reference the client secret resolves from"
            },
            "token_endpoint": {
                "type": "string",
                "description": "the token endpoint URL"
            },
            "issuer_url": {
                "type": "string",
                "description": "the issuer URL the token endpoint is discovered from"
            },
            "scopes": {
                "type": "array",
                "items": { "type": "string" },
                "description": "the scope set to request"
            },
            "audience": { "type": "string", "description": "the requested audience" }
        },
        "required": ["client_id_ref", "client_secret_ref"],
        "additionalProperties": false
    });
    // The two key-relationship annotations are inserted by name so the schema
    // documents and the validator cannot drift apart.
    let object = schema.as_object_mut().expect("the schema is an object");
    object.insert(
        MUTUALLY_EXCLUSIVE.to_owned(),
        json!([["token_endpoint", "issuer_url"]]),
    );
    object.insert(
        REQUIRED_ALTERNATIVE.to_owned(),
        json!([["token_endpoint", "issuer_url"]]),
    );
    schema
}

/// Re-export the annotation names so the schema documents and the validator
/// cannot drift apart.
pub use crate::domain::plugin::schema::{MUTUALLY_EXCLUSIVE, REQUIRED_ALTERNATIVE};

/// The GTS instance documents of the catalog-only identifiers.
///
/// The built-in identifiers need no instance registration: they resolve
/// through the in-process registries, not through the catalog. The catalog-only
/// ones are registered so a reference to them *resolves as a reference* — and
/// is then rejected at binding time as unresolvable, which is the whole point
/// of carrying them in the catalog (graded deviations 6 and 10).
///
/// Each document is a **GTS instance**, not a schema: the identifier goes in
/// the `id` field (the well-known-instance form), and no `$schema` is set — a
/// non-empty `$schema` is what makes the GTS model classify a document as a
/// Type Schema, and a Type Schema keyed by an instance identifier (no
/// `gts://` URI form, segment past the `~`) is refused on ingest. The parent
/// base type has already been provisioned by
/// [`crate::infra::type_provisioning`], so the instance declares itself
/// against it and carries no tenant of its own: these are platform catalog
/// entries, not tenant resources.
#[must_use]
pub fn catalog_instance_documents() -> Vec<Value> {
    CATALOG_ONLY_PLUGIN_IDS
        .iter()
        .map(|gts_id| {
            let entry = catalog_entry(gts_id).expect("catalog-only identifier is cataloged");
            json!({
                "id": entry.gts_id,
                "title": format!("OAGW {} plugin", entry.label),
                "description": entry.summary,
                "base_type": entry.base_type,
                "bindable": entry.bindable,
                "x-oagw-catalog-only": true
            })
        })
        .collect()
}

/// Register the catalog-only plugin identifiers as GTS instances.
///
/// Called once during gear initialization, *after* the base types are
/// provisioned, because an instance's declaring type-schema must already be
/// registered. A registry that already holds an instance answers
/// `AlreadyExists`, which is a successful no-op, so initialization is
/// repeatable across a restart.
///
/// # Errors
///
/// Returns [`DomainError::Internal`] when the registry is unreachable, and the
/// provisioning error when the registry rejects an identifier for any reason
/// other than an idempotent `AlreadyExists`.
pub async fn register_plugin_catalog(
    client: &dyn TypesRegistryClient,
) -> Result<Vec<String>, DomainError> {
    let results = client
        .register_instances(catalog_instance_documents())
        .await
        .map_err(|error| DomainError::Internal(format!("the types-registry is unreachable: {error}")))?;
    let mut registered = Vec::with_capacity(results.len());
    for result in results {
        match result {
            RegisterResult::Ok { gts_id } => registered.push(gts_id),
            RegisterResult::Err { gts_id, error } => {
                if matches!(error, toolkit_canonical_errors::CanonicalError::AlreadyExists { .. }) {
                    if let Some(id) = gts_id {
                        registered.push(id);
                    }
                } else {
                    return Err(DomainError::Internal(format!(
                        "the types-registry rejected a plugin catalog identifier: {error}"
                    )));
                }
            }
        }
    }
    Ok(registered)
}
