//! Link-time GTS catalog for the OAGW gear (DESIGN §3.2 `type_provisioning`).
//!
//! Every identifier registered here is contributed to the process-wide
//! `toolkit-gts` inventory (`InventoryTypeSchema` / `InventoryInstance`),
//! which `types-registry` aggregates at boot. No per-gear registration code
//! is needed — these macros submit the entries at link time.
//!
//! The catalog mirrors `domain/gts_helpers.rs` (the single source of truth
//! for the identifier strings; keep the two in lock-step):
//!
//! - Type schemas: `oagw.upstream.v1~`, `oagw.route.v1~`,
//!   `oagw.auth_plugin.v1~`, `oagw.guard_plugin.v1~`,
//!   `oagw.transform_plugin.v1~`, `oagw.protocol.v1~`
//! - Instances: protocol instances (`http`, `https`, `wss`, `wt`, `grpc`)
//!   and built-in plugin instances.
//!
//! The RFC 9457 problem `type` ids (DESIGN error catalog, `cf.core.errors.
//! err.v1~cf.oagw.*.v1`) are intentionally **not** registered here: they are
//! catalog strings emitted in problem bodies (see `domain/gts_helpers.rs`),
//! and the platform does not provide the `cf.core.errors.err.v1~` base schema
//! in the `toolkit-gts` inventory — registering instances under an unknown
//! base would fail `types-registry`'s ready-commit at boot.

use toolkit_gts::{gts_id, gts_instance_raw, gts_type_schema};

// ---------------------------------------------------------------------------
// Type schemas
// ---------------------------------------------------------------------------

/// Upstream configuration resource.
///
/// GTS Type Identifier: `gts.cf.core.oagw.upstream.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.upstream.v1~"),
    description = "OAGW upstream configuration",
    properties = "id,alias,enabled,protocol",
    base = true
)]
pub struct UpstreamSchemaV1 {
    /// Anonymous GTS Instance Identifier.
    pub id: gts::GtsInstanceId,
    /// Routing alias.
    pub alias: String,
    /// Whether the upstream is enabled.
    pub enabled: bool,
    /// Protocol instance id.
    pub protocol: String,
}

/// Route configuration resource.
///
/// GTS Type Identifier: `gts.cf.core.oagw.route.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.route.v1~"),
    description = "OAGW route configuration",
    properties = "id,upstream_id,enabled",
    base = true
)]
pub struct RouteSchemaV1 {
    /// Anonymous GTS Instance Identifier.
    pub id: gts::GtsInstanceId,
    /// Owning upstream reference.
    pub upstream_id: String,
    /// Whether the route may match requests.
    pub enabled: bool,
}

/// Base type for auth plugins.
///
/// GTS Type Identifier: `gts.cf.core.oagw.auth_plugin.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.auth_plugin.v1~"),
    description = "OAGW auth plugin",
    properties = "id,name",
    base = true
)]
pub struct AuthPluginSchemaV1 {
    /// Full GTS Instance Identifier.
    pub id: gts::GtsInstanceId,
    /// Human-readable plugin name.
    pub name: String,
}

/// Base type for guard plugins.
///
/// GTS Type Identifier: `gts.cf.core.oagw.guard_plugin.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.guard_plugin.v1~"),
    description = "OAGW guard plugin",
    properties = "id,name",
    base = true
)]
pub struct GuardPluginSchemaV1 {
    /// Full GTS Instance Identifier.
    pub id: gts::GtsInstanceId,
    /// Human-readable plugin name.
    pub name: String,
}

/// Base type for transform plugins.
///
/// GTS Type Identifier: `gts.cf.core.oagw.transform_plugin.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.transform_plugin.v1~"),
    description = "OAGW transform plugin",
    properties = "id,name",
    base = true
)]
pub struct TransformPluginSchemaV1 {
    /// Full GTS Instance Identifier.
    pub id: gts::GtsInstanceId,
    /// Human-readable plugin name.
    pub name: String,
}

/// Upstream protocol catalog item.
///
/// GTS Type Identifier: `gts.cf.core.oagw.protocol.v1~`
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.oagw.protocol.v1~"),
    description = "OAGW upstream protocol",
    properties = "id,name",
    base = true
)]
pub struct ProtocolSchemaV1 {
    /// Full GTS Instance Identifier.
    pub id: gts::GtsInstanceId,
    /// Protocol name (`http`, `https`, `wss`, `wt`, `grpc`).
    pub name: String,
}

// ---------------------------------------------------------------------------
// Protocol instances
// ---------------------------------------------------------------------------

gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"),
    "name": "http",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.https.v1"),
    "name": "https",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.wss.v1"),
    "name": "wss",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.wt.v1"),
    "name": "wt",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"),
    "name": "grpc",
});

// ---------------------------------------------------------------------------
// Built-in auth plugin instances (ADR-0008)
// ---------------------------------------------------------------------------

gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"),
    "name": "noop",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
    "name": "apikey",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1"),
    "name": "oauth2_client_cred",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"),
    "name": "oauth2_client_cred_basic",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"),
    "name": "basic",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1"),
    "name": "bearer",
});

// ---------------------------------------------------------------------------
// Built-in guard plugin instances
// ---------------------------------------------------------------------------

gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
    "name": "required_headers",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"),
    "name": "timeout",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1"),
    "name": "cors",
});

// ---------------------------------------------------------------------------
// Built-in transform plugin instances
// ---------------------------------------------------------------------------

gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
    "name": "request_id",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1"),
    "name": "logging",
});
gts_instance_raw!({
    "id": gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1"),
    "name": "metrics",
});
