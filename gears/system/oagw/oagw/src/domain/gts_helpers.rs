//! GTS identifiers, plugin identifiers, permission identifiers and error-type
//! constants for the `oagw` gear (ADR 0008 and ADR 0009).
//!
//! Every string here is a contract: the resource base types registered by
//! [`crate::infra::type_provisioning`], the protocol identifiers an upstream
//! `protocol` field may carry, the plugin identifiers the plugin chain
//! resolves, the hierarchy permissions `authz_resolver` evaluates, and the
//! GTS error types `api/rest/error.rs` renders. They are collected in one
//! module so no later entry re-derives them.

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Base (resource) types registered at initialization.
// ---------------------------------------------------------------------------

/// `gts.cf.core.oagw.upstream.v1~`
pub const UPSTREAM_BASE_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// `gts.cf.core.oagw.route.v1~`
pub const ROUTE_BASE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// `gts.cf.core.oagw.auth_plugin.v1~`
pub const AUTH_PLUGIN_BASE_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// `gts.cf.core.oagw.guard_plugin.v1~`
pub const GUARD_PLUGIN_BASE_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// `gts.cf.core.oagw.transform_plugin.v1~`
pub const TRANSFORM_PLUGIN_BASE_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// `gts.cf.core.oagw.proxy.v1~`
pub const PROXY_BASE_TYPE: &str = "gts.cf.core.oagw.proxy.v1~";

/// The six base GTS types `infra/type_provisioning.rs` registers, in the
/// order registration is attempted (the batch is re-sorted by the registry).
pub const BASE_TYPES: [&str; 6] = [
    UPSTREAM_BASE_TYPE,
    ROUTE_BASE_TYPE,
    AUTH_PLUGIN_BASE_TYPE,
    GUARD_PLUGIN_BASE_TYPE,
    TRANSFORM_PLUGIN_BASE_TYPE,
    PROXY_BASE_TYPE,
];

/// The three plugin base types a `plugin_type` must resolve to.
pub const PLUGIN_BASE_TYPES: [&str; 3] = [
    AUTH_PLUGIN_BASE_TYPE,
    GUARD_PLUGIN_BASE_TYPE,
    TRANSFORM_PLUGIN_BASE_TYPE,
];

// ---------------------------------------------------------------------------
// Protocol identifiers — the only two legal `Upstream.protocol` values.
// ---------------------------------------------------------------------------

/// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// The two legal protocol identifiers (gRPC is configuration surface only,
/// graded deviation 7).
pub const PROTOCOLS: [&str; 2] = [PROTOCOL_HTTP, PROTOCOL_GRPC];

// ---------------------------------------------------------------------------
// Built-in plugin identifiers — implemented and bindable (PRD §5.3).
// ---------------------------------------------------------------------------

/// No authentication.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// API key injection (header/query).
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// OAuth2 client credentials flow.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// OAuth2 client credentials with Basic auth at the token endpoint.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Required header enforcement; the only guard identifier bindable through
/// `plugins.items[].plugin_ref`.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// `X-Request-ID` propagation.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Plugin identifiers that have a backing implementation and are bindable.
pub const BUILTIN_PLUGIN_IDS: [&str; 6] = [
    NOOP_AUTH_PLUGIN_ID,
    APIKEY_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
    REQUEST_ID_TRANSFORM_PLUGIN_ID,
];

// ---------------------------------------------------------------------------
// Catalog-only plugin identifiers (PRD §5.3, graded deviation 10).
// ---------------------------------------------------------------------------

/// HTTP Basic auth; no backing `AuthPlugin`.
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Bearer token injection; no backing `AuthPlugin`.
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
/// Request timeout is core Data Plane config; not `plugins`-bindable.
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// CORS is core Data Plane config on `Upstream.cors` / `Route.cors`; not
/// bindable.
// @cpt-begin:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-1
// `inst-cors-cat-1`/`-4`: the `cors` guard identifier is a cataloged type with
// no backing guard implementation and no registry resolution, so CORS is
// reachable only through the `cors` field of an upstream or a route and never
// through a plugin binding.
// @cpt-begin:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-3
// @cpt-begin:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-4
pub const CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
//
// @cpt-end:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-4
// @cpt-end:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-3
//
// @cpt-end:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-1
/// Core Data Plane instrumentation; not `TransformPluginRegistry`-resolvable.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Core Data Plane instrumentation; not `TransformPluginRegistry`-resolvable.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Plugin identifiers registered in the types-registry catalog only; they are
/// rejected at binding time as unresolvable (graded deviations 6 and 10).
// @cpt-begin:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-2
// `inst-cors-cat-2`/`-3`: a `plugins.items[].plugin_ref` naming the `cors`
// guard identifier is rejected at binding time instead of being resolved, and
// no plugin binding is stored for it.
pub const CATALOG_ONLY_PLUGIN_IDS: [&str; 6] = [
    BASIC_AUTH_PLUGIN_ID,
    BEARER_AUTH_PLUGIN_ID,
    TIMEOUT_GUARD_PLUGIN_ID,
    CORS_GUARD_PLUGIN_ID,
    LOGGING_TRANSFORM_PLUGIN_ID,
    METRICS_TRANSFORM_PLUGIN_ID,
];
// @cpt-end:cpt-cf-oagw-flow-cors-catalog-identifier:p1:inst-cors-cat-2

// ---------------------------------------------------------------------------
// Hierarchy permissions (non-GTS, resolved through `authz_resolver`).
// ---------------------------------------------------------------------------

/// Bind an own plugin to an inherited chain.
pub const PERM_BIND: &str = "oagw:upstream:bind";
/// Override auth config (if `sharing: inherit`).
pub const PERM_OVERRIDE_AUTH: &str = "oagw:upstream:override_auth";
/// Specify own rate limits (subject to `min()`).
pub const PERM_OVERRIDE_RATE: &str = "oagw:upstream:override_rate";
/// Append own plugins to the inherited chain.
pub const PERM_ADD_PLUGINS: &str = "oagw:upstream:add_plugins";

/// Every hierarchy permission the merge engine can be gated on.
pub const HIERARCHY_PERMISSIONS: [&str; 4] = [
    PERM_BIND,
    PERM_OVERRIDE_AUTH,
    PERM_OVERRIDE_RATE,
    PERM_ADD_PLUGINS,
];

// ---------------------------------------------------------------------------
// Per-operation management permissions (DESIGN §3.2).
// ---------------------------------------------------------------------------

/// `gts.cf.core.oagw.upstream.v1~:create` — `POST /oagw/v1/upstreams`.
pub const PERM_UPSTREAM_CREATE: &str = "gts.cf.core.oagw.upstream.v1~:create";
/// `gts.cf.core.oagw.upstream.v1~:read` — the read and the list.
pub const PERM_UPSTREAM_READ: &str = "gts.cf.core.oagw.upstream.v1~:read";
/// `gts.cf.core.oagw.upstream.v1~:override` — the full replacement, and
/// therefore setting `enabled`.
pub const PERM_UPSTREAM_OVERRIDE: &str = "gts.cf.core.oagw.upstream.v1~:override";
/// `gts.cf.core.oagw.upstream.v1~:delete` — `DELETE /oagw/v1/upstreams/{id}`.
pub const PERM_UPSTREAM_DELETE: &str = "gts.cf.core.oagw.upstream.v1~:delete";

/// `gts.cf.core.oagw.route.v1~:create` — `POST /oagw/v1/routes`.
pub const PERM_ROUTE_CREATE: &str = "gts.cf.core.oagw.route.v1~:create";
/// `gts.cf.core.oagw.route.v1~:read` — the read and the list.
pub const PERM_ROUTE_READ: &str = "gts.cf.core.oagw.route.v1~:read";
/// `gts.cf.core.oagw.route.v1~:override` — the full replacement, and
/// therefore setting `enabled`.
pub const PERM_ROUTE_OVERRIDE: &str = "gts.cf.core.oagw.route.v1~:override";
/// `gts.cf.core.oagw.route.v1~:delete` — `DELETE /oagw/v1/routes/{id}`.
pub const PERM_ROUTE_DELETE: &str = "gts.cf.core.oagw.route.v1~:delete";

/// `gts.cf.core.oagw.auth_plugin.v1~:create` — `POST /oagw/v1/plugins` for an
/// auth plugin (DESIGN §3.2, the per-base-type plugin permission set).
pub const PERM_AUTH_PLUGIN_CREATE: &str = "gts.cf.core.oagw.auth_plugin.v1~:create";
/// `gts.cf.core.oagw.auth_plugin.v1~:read` — the plugin catalog and the source.
pub const PERM_AUTH_PLUGIN_READ: &str = "gts.cf.core.oagw.auth_plugin.v1~:read";
/// `gts.cf.core.oagw.auth_plugin.v1~:delete` — `DELETE /oagw/v1/plugins/{id}`.
pub const PERM_AUTH_PLUGIN_DELETE: &str = "gts.cf.core.oagw.auth_plugin.v1~:delete";

/// `gts.cf.core.oagw.guard_plugin.v1~:create`.
pub const PERM_GUARD_PLUGIN_CREATE: &str = "gts.cf.core.oagw.guard_plugin.v1~:create";
/// `gts.cf.core.oagw.guard_plugin.v1~:read`.
pub const PERM_GUARD_PLUGIN_READ: &str = "gts.cf.core.oagw.guard_plugin.v1~:read";
/// `gts.cf.core.oagw.guard_plugin.v1~:delete`.
pub const PERM_GUARD_PLUGIN_DELETE: &str = "gts.cf.core.oagw.guard_plugin.v1~:delete";

/// `gts.cf.core.oagw.transform_plugin.v1~:create`.
pub const PERM_TRANSFORM_PLUGIN_CREATE: &str = "gts.cf.core.oagw.transform_plugin.v1~:create";
/// `gts.cf.core.oagw.transform_plugin.v1~:read`.
pub const PERM_TRANSFORM_PLUGIN_READ: &str = "gts.cf.core.oagw.transform_plugin.v1~:read";
/// `gts.cf.core.oagw.transform_plugin.v1~:delete`.
pub const PERM_TRANSFORM_PLUGIN_DELETE: &str = "gts.cf.core.oagw.transform_plugin.v1~:delete";

/// `gts.cf.core.oagw.proxy.v1~:invoke` — every proxied request
/// (`cpt-cf-oagw-dod-request-proxy-proxy-handler`).
pub const PERM_PROXY_INVOKE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

// ---------------------------------------------------------------------------
// GTS error types (DESIGN §3.3). Rendering is owned by entry 2.5.
// ---------------------------------------------------------------------------

pub const ERR_VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
pub const ERR_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
pub const ERR_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
pub const ERR_AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
pub const ERR_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
pub const ERR_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
pub const ERR_PAYLOAD_TOO_LARGE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
pub const ERR_RATE_LIMIT_EXCEEDED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
pub const ERR_SECRET_NOT_FOUND: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
pub const ERR_PROTOCOL: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
pub const ERR_DOWNSTREAM: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
pub const ERR_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
pub const ERR_LINK_UNAVAILABLE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
pub const ERR_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
pub const ERR_CONNECTION_TIMEOUT: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
pub const ERR_REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
pub const ERR_IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
/// CORS (owned by entry 2.8, rendered through the same mapping layer).
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

/// The platform canonical *internal* category, which renders the two variants
/// that carry no OAGW type (`DomainError::Internal` and
/// `DomainError::PluginInternal`): an invariant violation is not attributable
/// to the caller, so it is not an OAGW error type.
///
/// The value is the id `toolkit_canonical_errors::CanonicalError::internal`
/// reports; `error_tests` asserts the two spellings agree, so the constant
/// cannot drift from the platform's.
pub const CANONICAL_INTERNAL_TYPE: &str = "gts.cf.core.errors.err.v1~cf.core.err.internal.v1~";

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Render a resource instance identifier `gts.cf.core.oagw.{type}.v1~{uuid}`.
#[must_use]
pub fn resource_id(base_type: &str, id: Uuid) -> String {
    format!("{base_type}{id}")
}

/// `gts.cf.core.oagw.upstream.v1~{uuid}`
#[must_use]
pub fn upstream_resource_id(id: Uuid) -> String {
    resource_id(UPSTREAM_BASE_TYPE, id)
}

/// `gts.cf.core.oagw.route.v1~{uuid}`
#[must_use]
pub fn route_resource_id(id: Uuid) -> String {
    resource_id(ROUTE_BASE_TYPE, id)
}

/// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`
#[must_use]
pub fn plugin_resource_id(base_type: &str, id: Uuid) -> String {
    resource_id(base_type, id)
}

/// The base type a resource instance identifier was minted from, or `None`
/// when the identifier is not an `oagw` resource instance.
#[must_use]
pub fn base_type_of(gts_id: &str) -> Option<&str> {
    BASE_TYPES
        .into_iter()
        .find(|base| gts_id.starts_with(base))
}

/// The UUID half of a resource instance identifier, when it carries one.
#[must_use]
pub fn uuid_of(gts_id: &str) -> Option<Uuid> {
    let base = base_type_of(gts_id)?;
    Uuid::parse_str(gts_id.strip_prefix(base)?).ok()
}

/// The short kind word a plugin base type names: `auth`, `guard`, or
/// `transform`, or `None` when the base type is not a plugin base type.
///
/// This is the value `GET /oagw/v1/plugins?$filter=type eq 'guard'` compares,
/// and the word the plugin management surface derives a permission set from.
#[must_use]
pub fn plugin_kind_of(base_type: &str) -> Option<&'static str> {
    match base_type {
        AUTH_PLUGIN_BASE_TYPE => Some("auth"),
        GUARD_PLUGIN_BASE_TYPE => Some("guard"),
        TRANSFORM_PLUGIN_BASE_TYPE => Some("transform"),
        _ => None,
    }
}

/// The permission set element of a plugin base type: the base type followed by
/// `:create`, `:read` or `:delete`.
#[must_use]
pub fn plugin_permission(base_type: &str, action: PluginAction) -> &'static str {
    match (base_type, action) {
        (AUTH_PLUGIN_BASE_TYPE, PluginAction::Create) => PERM_AUTH_PLUGIN_CREATE,
        (AUTH_PLUGIN_BASE_TYPE, PluginAction::Read) => PERM_AUTH_PLUGIN_READ,
        (AUTH_PLUGIN_BASE_TYPE, PluginAction::Delete) => PERM_AUTH_PLUGIN_DELETE,
        (GUARD_PLUGIN_BASE_TYPE, PluginAction::Create) => PERM_GUARD_PLUGIN_CREATE,
        (GUARD_PLUGIN_BASE_TYPE, PluginAction::Read) => PERM_GUARD_PLUGIN_READ,
        (GUARD_PLUGIN_BASE_TYPE, PluginAction::Delete) => PERM_GUARD_PLUGIN_DELETE,
        (TRANSFORM_PLUGIN_BASE_TYPE, PluginAction::Create) => PERM_TRANSFORM_PLUGIN_CREATE,
        (TRANSFORM_PLUGIN_BASE_TYPE, PluginAction::Read) => PERM_TRANSFORM_PLUGIN_READ,
        (TRANSFORM_PLUGIN_BASE_TYPE, PluginAction::Delete) => PERM_TRANSFORM_PLUGIN_DELETE,
        _ => PERM_AUTH_PLUGIN_READ,
    }
}

/// The three elements of a plugin base type's permission set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginAction {
    /// The create element.
    Create,
    /// The read element, which the catalog, the by-identifier read, the source
    /// retrieval and the binding-time resolvability check share.
    Read,
    /// The delete element.
    Delete,
}

#[cfg(test)]
#[path = "gts_helpers_tests.rs"]
mod tests;
