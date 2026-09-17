//! GTS catalogue for the OAGW gear.
//!
//! Contributes three things to the process-wide `toolkit-gts`
//! inventory (seeded into `types-registry` at boot, so consumers can
//! discover and validate OAGW resources through the registry):
//!
//! 1. **Type Schemas** — upstream / route / `{auth,guard,transform}_plugin`
//!    base types, declared via `#[gts_type_schema(...)]`. Every record
//!    managed by the OAGW control plane is a GTS Instance of one of
//!    these types with an anonymous identifier
//!    `gts.cf.core.oagw.{type}.v1~{uuid}`.
//! 2. **Builtin plugin catalogue** — well-known Instances for the
//!    named (reserved) plugins. `noop`, `apikey`, `oauth2_client_cred*`
//!    are resolvable in-process; `basic` / `bearer` (auth),
//!    `timeout` / `cors` (guard), `logging` / `metrics` (transform)
//!    are catalog-only identifiers with no backing implementation
//!    (see `docs/DESIGN.md` "Plugin Identification Model").
//! 3. **Permissions** — well-known `AuthzPermissionV1` Instances for
//!    the management + proxy surfaces, so an AuthZ admin UI can list
//!    what OAGW gates on.
//!
//! Error type identifiers (RFC 9457 `type` values, GTS form) live here
//! as consts so the error renderer and the OpenAPI contract share one
//! source of truth.

use gts::GtsInstanceId;
use toolkit_gts::{AuthzPermissionV1, gts_id, gts_instance, gts_type_schema};

// =====================================================================
//                              Type Schemas
// =====================================================================

/// Type Schema for OAGW upstream Instances.
///
/// GTS Type Identifier: `gts.cf.core.oagw.upstream.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.upstream.v1~"),
    description = "OAGW upstream service configuration",
    properties = "id,alias,enabled,tags,protocol",
    base = true
)]
pub struct UpstreamInstanceV1 {
    /// Anonymous GTS Instance Identifier
    /// (`gts.cf.core.oagw.upstream.v1~{uuid}`).
    pub id: GtsInstanceId,
    /// Routing alias (`/oagw/v1/proxy/{alias}/...`).
    pub alias: String,
    /// Whether the upstream is enabled (default `true`).
    pub enabled: bool,
    /// Categorization tags (effective tags are additive per hierarchy).
    pub tags: Vec<String>,
    /// Protocol identifier
    /// (`gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`).
    pub protocol: String,
}

/// Type Schema for OAGW route Instances.
///
/// GTS Type Identifier: `gts.cf.core.oagw.route.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.route.v1~"),
    description = "OAGW route configuration",
    properties = "id,upstream_id,tags,protocol",
    base = true
)]
pub struct RouteInstanceV1 {
    /// Anonymous GTS Instance Identifier
    /// (`gts.cf.core.oagw.route.v1~{uuid}`).
    pub id: GtsInstanceId,
    /// Referenced upstream instance UUID.
    pub upstream_id: uuid::Uuid,
    /// Categorization tags.
    pub tags: Vec<String>,
    /// Protocol identifier (matches the referenced upstream).
    pub protocol: String,
}

/// Type Schema for OAGW auth plugin Instances.
///
/// GTS Type Identifier: `gts.cf.core.oagw.auth_plugin.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.auth_plugin.v1~"),
    description = "OAGW auth plugin catalogue entry",
    properties = "id,name,description",
    base = true
)]
pub struct AuthPluginCatalogV1 {
    /// GTS Instance Identifier for the plugin
    /// (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.{name}.v1`).
    pub id: GtsInstanceId,
    /// Plugin handle (e.g. `noop`, `apikey`).
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
}

/// Type Schema for OAGW guard plugin Instances.
///
/// GTS Type Identifier: `gts.cf.core.oagw.guard_plugin.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.guard_plugin.v1~"),
    description = "OAGW guard plugin catalogue entry",
    properties = "id,name,description",
    base = true
)]
pub struct GuardPluginCatalogV1 {
    /// GTS Instance Identifier for the plugin.
    pub id: GtsInstanceId,
    /// Plugin handle (e.g. `required_headers`).
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
}

/// Type Schema for OAGW transform plugin Instances.
///
/// GTS Type Identifier: `gts.cf.core.oagw.transform_plugin.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.transform_plugin.v1~"),
    description = "OAGW transform plugin catalogue entry",
    properties = "id,name,description",
    base = true
)]
pub struct TransformPluginCatalogV1 {
    /// GTS Instance Identifier for the plugin.
    pub id: GtsInstanceId,
    /// Plugin handle (e.g. `request_id`).
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
}

// =====================================================================
//                       Builtin plugin catalogue
// =====================================================================

gts_instance! {
    AuthPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"),
        name: "noop".to_owned(),
        description: Some("No authentication".to_owned()),
    }
}

gts_instance! {
    AuthPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
        name: "apikey".to_owned(),
        description: Some("API key injection (header/query)".to_owned()),
    }
}

gts_instance! {
    AuthPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1"),
        name: "oauth2_client_cred".to_owned(),
        description: Some("OAuth2 client credentials flow (Form)".to_owned()),
    }
}

gts_instance! {
    AuthPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"),
        name: "oauth2_client_cred_basic".to_owned(),
        description: Some("OAuth2 client credentials flow (Basic auth)".to_owned()),
    }
}

// Reserved catalog identifiers — `basic.v1` / `bearer.v1` have no
// backing `AuthPlugin` implementation; using either as an auth plugin
// type fails with "unknown auth plugin".
gts_instance! {
    AuthPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"),
        name: "basic".to_owned(),
        description: Some("HTTP Basic authentication (catalog identifier only)".to_owned()),
    }
}

gts_instance! {
    AuthPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1"),
        name: "bearer".to_owned(),
        description: Some("Bearer token injection (catalog identifier only)".to_owned()),
    }
}

gts_instance! {
    GuardPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
        name: "required_headers".to_owned(),
        description: Some("Required header enforcement (request/response)".to_owned()),
    }
}

// Catalog-only — request timeout is core Data Plane configuration and
// cannot be bound via `plugins.items`.
gts_instance! {
    GuardPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"),
        name: "timeout".to_owned(),
        description: Some("Request timeout enforcement (core config, not bindable)".to_owned()),
    }
}

// Catalog-only — CORS is core Data Plane configuration (`cors` field)
// and cannot be bound via `plugins.items`.
gts_instance! {
    GuardPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1"),
        name: "cors".to_owned(),
        description: Some("CORS preflight validation (core config, not bindable)".to_owned()),
    }
}

gts_instance! {
    TransformPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
        name: "request_id".to_owned(),
        description: Some("X-Request-ID injection/propagation".to_owned()),
    }
}

// Catalog-only — structured logging is core Data Plane instrumentation
// and is not resolvable via `TransformPluginRegistry`.
gts_instance! {
    TransformPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1"),
        name: "logging".to_owned(),
        description: Some("Request/response logging (core instrumentation)".to_owned()),
    }
}

// Catalog-only — metrics collection is core Data Plane instrumentation
// and is not resolvable via `TransformPluginRegistry`.
gts_instance! {
    TransformPluginCatalogV1 {
        id: gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1"),
        name: "metrics".to_owned(),
        description: Some("Metrics collection (core instrumentation)".to_owned()),
    }
}

// =====================================================================
//                             Permissions
// =====================================================================

/// Resource type for OAGW upstream resources (permission `resource_type`).
pub const UPSTREAM_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// Resource type for OAGW route resources.
pub const ROUTE_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// Resource type for OAGW proxy invocations.
pub const PROXY_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.proxy.v1~");

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.upstream_create.v1"),
        resource_type: UPSTREAM_RESOURCE_TYPE.to_owned(),
        action: "create".to_owned(),
        display_name: "Create upstream".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.upstream_override.v1"),
        resource_type: UPSTREAM_RESOURCE_TYPE.to_owned(),
        action: "override".to_owned(),
        display_name: "Replace upstream".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.upstream_read.v1"),
        resource_type: UPSTREAM_RESOURCE_TYPE.to_owned(),
        action: "read".to_owned(),
        display_name: "Read upstream".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.upstream_delete.v1"),
        resource_type: UPSTREAM_RESOURCE_TYPE.to_owned(),
        action: "delete".to_owned(),
        display_name: "Delete upstream".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.route_create.v1"),
        resource_type: ROUTE_RESOURCE_TYPE.to_owned(),
        action: "create".to_owned(),
        display_name: "Create route".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.route_override.v1"),
        resource_type: ROUTE_RESOURCE_TYPE.to_owned(),
        action: "override".to_owned(),
        display_name: "Replace route".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.route_read.v1"),
        resource_type: ROUTE_RESOURCE_TYPE.to_owned(),
        action: "read".to_owned(),
        display_name: "Read route".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.route_delete.v1"),
        resource_type: ROUTE_RESOURCE_TYPE.to_owned(),
        action: "delete".to_owned(),
        display_name: "Delete route".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.oagw.proxy_invoke.v1"),
        resource_type: PROXY_RESOURCE_TYPE.to_owned(),
        action: "invoke".to_owned(),
        display_name: "Proxy requests to upstreams".to_owned(),
    }
}

// =====================================================================
//                       Protocol identifiers
// =====================================================================

/// Protocol identifier for HTTP/1.1 upstreams.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// Protocol identifier for gRPC upstreams.
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

// =====================================================================
//                          Error type identifiers
// =====================================================================

/// `type` for 400 validation / route errors (RFC 9457 problem).
pub const ERR_VALIDATION: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");
/// `type` for a missing `X-OAGW-Target-Host` header (400).
pub const ERR_MISSING_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");
/// `type` for an invalid `X-OAGW-Target-Host` header (400).
pub const ERR_INVALID_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1");
/// `type` for an unknown `X-OAGW-Target-Host` value (400).
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1");
/// `type` for failed upstream authentication (401).
pub const ERR_AUTH_FAILED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
/// `type` for a token missing the required scope (403).
pub const ERR_PERMISSION_DENIED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.auth.permission_denied.v1");
/// `type` for a duplicated resource (409).
pub const ERR_ALREADY_EXISTS: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.validation.already_exists.v1");
/// `type` for unresolved alias / unmatched route (404).
pub const ERR_ROUTE_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
/// `type` for deleting a referenced plugin (409).
pub const ERR_PLUGIN_IN_USE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");
/// `type` for an oversized request payload (413).
pub const ERR_PAYLOAD_TOO_LARGE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
/// `type` for a refused cross-origin request (403).
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");
/// `type` for a refused cross-origin method (403).
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");
/// `type` for rate-limit rejection (429).
pub const ERR_RATE_LIMIT_EXCEEDED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
/// `type` for a missing referenced secret (500).
pub const ERR_SECRET_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1");
/// `type` for protocol-level errors (502).
pub const ERR_PROTOCOL_ERROR: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1");
/// `type` for upstream service errors (502; body passthrough).
pub const ERR_DOWNSTREAM_ERROR: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1");
/// `type` for aborted streams (502).
pub const ERR_STREAM_ABORTED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1");
/// `type` for an unavailable upstream (503).
pub const ERR_LINK_UNAVAILABLE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
/// `type` for an open circuit breaker (503).
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1");
/// `type` for an unresolvable plugin (503).
pub const ERR_PLUGIN_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1");
/// `type` for connection timeouts (504).
pub const ERR_TIMEOUT_CONNECTION: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1");
/// `type` for overall request timeouts (504).
pub const ERR_TIMEOUT_REQUEST: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1");
/// `type` for idle timeouts (504).
pub const ERR_TIMEOUT_IDLE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1");
