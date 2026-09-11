//! Catalogue rows: the JSON entities handed to the types-registry.
//!
//! Realizes `cpt-cf-oagw-dod-gts-type-catalog`: 7 base type schemas, 2
//! protocol instances, and 21 distinct error instances covering the 22
//! `ErrorKind` variants. Realizes the types-registry half of
//! `cpt-cf-oagw-dod-builtin-catalogue`: the twelve plugin instances of
//! `crate::gts::plugin_catalog`, backed and catalog-only alike, registered
//! during the post-init phase.

use serde_json::{Value, json};
use toolkit_gts::gts_uri;

use crate::domain::error::ErrorKind;
use crate::gts::{
    AUTH_PLUGIN_TYPE, ERR_ALIAS_CONFLICT, ERR_AUTH_FAILED, ERR_CIRCUIT_BREAKER_OPEN,
    ERR_DOWNSTREAM_ERROR, ERR_INVALID_TARGET_HOST, ERR_LINK_UNAVAILABLE, ERR_MATCH_CONFLICT,
    ERR_MISSING_TARGET_HOST, ERR_PAYLOAD_TOO_LARGE, ERR_PLUGIN_IN_USE, ERR_PLUGIN_NOT_FOUND,
    ERR_PROTOCOL_ERROR, ERR_RATE_LIMIT_EXCEEDED, ERR_ROUTE_NOT_FOUND, ERR_SECRET_NOT_FOUND,
    ERR_STREAM_ABORTED, ERR_TIMEOUT_CONNECTION, ERR_TIMEOUT_IDLE, ERR_TIMEOUT_REQUEST,
    ERR_UNKNOWN_TARGET_HOST, ERR_VALIDATION, ERROR_TYPE, GUARD_PLUGIN_TYPE, PROTOCOL_GRPC,
    PROTOCOL_HTTP, PROTOCOL_TYPE, ROUTE_TYPE, TRANSFORM_PLUGIN_TYPE, UPSTREAM_TYPE,
};
use crate::gts::plugin_catalog::{
    AUTH_APIKEY, AUTH_NOOP, AUTH_OAUTH2_CLIENT_CRED, AUTH_OAUTH2_CLIENT_CRED_BASIC,
    CATALOG_ONLY_AUTH_BASIC, CATALOG_ONLY_AUTH_BEARER, CATALOG_ONLY_GUARD_CORS,
    CATALOG_ONLY_GUARD_TIMEOUT, CATALOG_ONLY_TRANSFORM_LOGGING, CATALOG_ONLY_TRANSFORM_METRICS,
    GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID,
};

/// Frozen upstream aggregate schema, mirrored byte for byte.
const UPSTREAM_SCHEMA_JSON: &str = include_str!("../../../docs/schemas/upstream.v1.schema.json");
/// Frozen route aggregate schema, mirrored byte for byte.
const ROUTE_SCHEMA_JSON: &str = include_str!("../../../docs/schemas/route.v1.schema.json");

/// JSON Schema `draft-07` meta-schema URI used by every base type schema.
const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";

/// The 21 distinct error instance rows, paired with the `ErrorKind` whose
/// status, title, and Retriable flag the row carries. `RouteError` and
/// `ValidationError` share `cf.oagw.validation.error.v1`, so 21 identifiers
/// cover the 22 variants; `AliasConflict` and `MatchConflict` are the two
/// §1.5 additions.
const ERROR_KIND_ROWS: [(&str, ErrorKind); 21] = [
    (ERR_VALIDATION, ErrorKind::ValidationError),
    (ERR_MISSING_TARGET_HOST, ErrorKind::MissingTargetHost),
    (ERR_INVALID_TARGET_HOST, ErrorKind::InvalidTargetHost),
    (ERR_UNKNOWN_TARGET_HOST, ErrorKind::UnknownTargetHost),
    (ERR_AUTH_FAILED, ErrorKind::AuthenticationFailed),
    (ERR_ROUTE_NOT_FOUND, ErrorKind::RouteNotFound),
    (ERR_PLUGIN_IN_USE, ErrorKind::PluginInUse),
    (ERR_ALIAS_CONFLICT, ErrorKind::AliasConflict),
    (ERR_MATCH_CONFLICT, ErrorKind::MatchConflict),
    (ERR_PAYLOAD_TOO_LARGE, ErrorKind::PayloadTooLarge),
    (ERR_RATE_LIMIT_EXCEEDED, ErrorKind::RateLimitExceeded),
    (ERR_SECRET_NOT_FOUND, ErrorKind::SecretNotFound),
    (ERR_PROTOCOL_ERROR, ErrorKind::ProtocolError),
    (ERR_DOWNSTREAM_ERROR, ErrorKind::DownstreamError),
    (ERR_STREAM_ABORTED, ErrorKind::StreamAborted),
    (ERR_LINK_UNAVAILABLE, ErrorKind::LinkUnavailable),
    (ERR_CIRCUIT_BREAKER_OPEN, ErrorKind::CircuitBreakerOpen),
    (ERR_PLUGIN_NOT_FOUND, ErrorKind::PluginNotFound),
    (ERR_TIMEOUT_CONNECTION, ErrorKind::ConnectionTimeout),
    (ERR_TIMEOUT_REQUEST, ErrorKind::RequestTimeout),
    (ERR_TIMEOUT_IDLE, ErrorKind::IdleTimeout),
];

/// Why the catalogue could not be assembled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// A frozen schema file does not parse as JSON.
    #[error("frozen schema for {type_id} is not valid JSON: {message}")]
    FrozenSchema {
        /// GTS identifier of the type whose frozen schema failed to parse.
        type_id: String,
        /// The parse failure.
        message: String,
    },
}

/// Builds one base type schema entity: `{"$id": <gts uri>, ...body}`.
fn schema_entity(type_id: &str, body: Value) -> Value {
    let mut entity = serde_json::Map::new();
    entity.insert(String::from("$id"), Value::String(gts_uri!(type_id)));
    if let Value::Object(fields) = body {
        for (key, value) in fields {
            entity.insert(key, value);
        }
    }
    Value::Object(entity)
}

/// Builds a permissive minimal base type schema: no `properties`, so
/// instances carry arbitrary fields.
fn minimal_schema(title: &str, description: &str) -> Value {
    json!({
        "$schema": DRAFT_07,
        "type": "object",
        "title": title,
        "description": description
    })
}

/// Wraps a frozen JSON Schema file as a type-schema entity, injecting `$id`
/// as the first key so the registered entry mirrors the frozen input.
fn frozen_schema_entity(type_id: &str, frozen: &str) -> Result<Value, CatalogError> {
    let parsed: Value = serde_json::from_str(frozen).map_err(|e| CatalogError::FrozenSchema {
        type_id: type_id.to_owned(),
        message: e.to_string(),
    })?;
    Ok(schema_entity(type_id, parsed))
}

/// Builds one instance entity: `{"id": <gts id>, ...content}`.
fn instance_entity(gts_id: &str, content: Value) -> Value {
    let mut entity = serde_json::Map::new();
    entity.insert(String::from("id"), Value::String(gts_id.to_owned()));
    if let Value::Object(fields) = content {
        for (key, value) in fields {
            entity.insert(key, value);
        }
    }
    Value::Object(entity)
}

/// Builds one error instance entity from its catalogue metadata.
fn error_instance(gts_id: &str, kind: ErrorKind) -> Value {
    instance_entity(
        gts_id,
        json!({
            "title": kind.title(),
            "http_status": kind.http_status(),
            "retriable": kind.is_retriable()
        }),
    )
}

/// The 7 base type schemas, parents first. The two aggregates mirror their
/// frozen schema files; the five other base types are permissive minimal
/// schemas.
///
/// # Errors
///
/// Returns [`CatalogError::FrozenSchema`] when a frozen schema file does not
/// parse as JSON.
pub fn base_type_schemas() -> Result<Vec<Value>, CatalogError> {
    Ok(vec![
        frozen_schema_entity(UPSTREAM_TYPE, UPSTREAM_SCHEMA_JSON)?,
        frozen_schema_entity(ROUTE_TYPE, ROUTE_SCHEMA_JSON)?,
        schema_entity(
            PROTOCOL_TYPE,
            minimal_schema(
                "OAGW Protocol",
                "Protocol used to connect to an upstream service.",
            ),
        ),
        schema_entity(
            AUTH_PLUGIN_TYPE,
            minimal_schema("OAGW Auth Plugin", "Authentication plugin base type."),
        ),
        schema_entity(
            GUARD_PLUGIN_TYPE,
            minimal_schema("OAGW Guard Plugin", "Guard plugin base type."),
        ),
        schema_entity(
            TRANSFORM_PLUGIN_TYPE,
            minimal_schema("OAGW Transform Plugin", "Transform plugin base type."),
        ),
        schema_entity(
            ERROR_TYPE,
            minimal_schema(
                "OAGW Gateway Error",
                "Namespace of every OAGW problem type.",
            ),
        ),
    ])
}

/// The 12 plugin instance rows: the six backed implementations and the six
/// catalog-only identifiers, all registered in the types-registry during the
/// post-init phase.
fn plugin_instances() -> Vec<Value> {
    let row = |gts_id: &str, name: &str, description: &str, backed: bool| {
        instance_entity(
            gts_id,
            json!({
                "name": name,
                "description": description,
                "backed": backed
            }),
        )
    };
    vec![
        row(
            AUTH_NOOP,
            "noop",
            "Authentication plugin that injects no credential.",
            true,
        ),
        row(
            AUTH_APIKEY,
            "apikey",
            "Authentication plugin that resolves an API key and injects it as a header.",
            true,
        ),
        row(
            AUTH_OAUTH2_CLIENT_CRED,
            "oauth2_client_cred",
            "OAuth2 Client Credentials plugin authenticating its client by form body.",
            true,
        ),
        row(
            AUTH_OAUTH2_CLIENT_CRED_BASIC,
            "oauth2_client_cred_basic",
            "OAuth2 Client Credentials plugin authenticating its client by basic header.",
            true,
        ),
        row(
            GUARD_REQUIRED_HEADERS,
            "required_headers",
            "Guard plugin that rejects a request or response missing a required header.",
            true,
        ),
        row(
            TRANSFORM_REQUEST_ID,
            "request_id",
            "Transform plugin that propagates the request identifier.",
            true,
        ),
        row(
            CATALOG_ONLY_AUTH_BASIC,
            "basic",
            "Reserved auth identifier: HTTP Basic is upstream transport configuration.",
            false,
        ),
        row(
            CATALOG_ONLY_AUTH_BEARER,
            "bearer",
            "Reserved auth identifier: a static bearer value is an upstream field, not a plugin.",
            false,
        ),
        row(
            CATALOG_ONLY_GUARD_TIMEOUT,
            "timeout",
            "Reserved guard identifier: timeouts are core data-plane behaviour.",
            false,
        ),
        row(
            CATALOG_ONLY_GUARD_CORS,
            "cors",
            "Reserved guard identifier: CORS is a dedicated aggregate field.",
            false,
        ),
        row(
            CATALOG_ONLY_TRANSFORM_LOGGING,
            "logging",
            "Reserved transform identifier: logging is core data-plane instrumentation.",
            false,
        ),
        row(
            CATALOG_ONLY_TRANSFORM_METRICS,
            "metrics",
            "Reserved transform identifier: metrics are core data-plane instrumentation.",
            false,
        ),
    ]
}

/// The 35 instances the base types own: 2 protocol values, 21 distinct error
/// identifiers, and the 12 plugin identifiers of the built-in and catalog-only
/// catalogue.
#[must_use]
pub fn instances() -> Vec<Value> {
    // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-collect
    let mut rows = vec![
        instance_entity(
            PROTOCOL_HTTP,
            json!({
                "title": "HTTP protocol",
                "description": "HTTP upstream protocol value."
            }),
        ),
        instance_entity(
            PROTOCOL_GRPC,
            json!({
                "title": "gRPC protocol",
                "description": "gRPC upstream protocol value."
            }),
        ),
    ];
    rows.extend(
        ERROR_KIND_ROWS
            .iter()
            .map(|(gts_id, kind)| error_instance(gts_id, *kind)),
    );
    rows.extend(plugin_instances());
    rows
    // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-collect
}

/// The whole catalogue batch, ordered so a parent type never follows its
/// children.
///
/// # Errors
///
/// Returns [`CatalogError::FrozenSchema`] when a frozen schema file does not
/// parse as JSON.
pub fn catalog_entities() -> Result<Vec<Value>, CatalogError> {
    let mut batch = base_type_schemas()?;
    batch.extend(instances());
    Ok(batch)
}
