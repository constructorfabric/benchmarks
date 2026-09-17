//! GTS identifiers used by the `oagw` gear.
//!
//! Every resource is addressed on the wire by an anonymous GTS identifier of
//! the form `gts.cf.core.oagw.{type}.v1~{instance}` where `{instance}` is a
//! UUID for stored resources and a dotted name for built-in plugin
//! identifiers (see `DESIGN.md` §3.1 "Plugin Identification Model").

use uuid::Uuid;

/// Base type of an upstream resource: `gts.cf.core.oagw.upstream.v1~`.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Base type of a route resource: `gts.cf.core.oagw.route.v1~`.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Base type of a stored (custom) plugin resource: `gts.cf.core.oagw.plugin.v1~`.
pub const PLUGIN_TYPE: &str = "gts.cf.core.oagw.plugin.v1~";
/// Base type of an auth plugin: `gts.cf.core.oagw.auth_plugin.v1~`.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Base type of a guard plugin: `gts.cf.core.oagw.guard_plugin.v1~`.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Base type of a transform plugin: `gts.cf.core.oagw.transform_plugin.v1~`.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// HTTP protocol identifier.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC protocol identifier.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Built-in auth plugin: no credential injection.
pub const AUTH_NOOP: &str = "cf.core.oagw.noop.v1";
/// Built-in auth plugin: static API key injected into a header.
pub const AUTH_APIKEY: &str = "cf.core.oagw.apikey.v1";
/// Built-in auth plugin: `OAuth2` client credentials, form-encoded.
pub const AUTH_OAUTH2_CLIENT_CRED: &str = "cf.core.oagw.oauth2_client_cred.v1";
/// Built-in auth plugin: `OAuth2` client credentials, HTTP Basic.
pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str = "cf.core.oagw.oauth2_client_cred_basic.v1";
/// Built-in guard plugin: required-header enforcement.
pub const GUARD_REQUIRED_HEADERS: &str = "cf.core.oagw.required_headers.v1";
/// Built-in transform plugin: `X-Request-ID` injection/propagation.
pub const TRANSFORM_REQUEST_ID: &str = "cf.core.oagw.request_id.v1";

/// Catalog-only auth plugin identifiers — reserved in the types-registry with
/// no backing implementation, therefore never resolvable.
pub const CATALOG_ONLY_AUTH: [&str; 2] = ["cf.core.oagw.basic.v1", "cf.core.oagw.bearer.v1"];
/// Catalog-only guard plugin identifiers.
pub const CATALOG_ONLY_GUARD: [&str; 2] = ["cf.core.oagw.timeout.v1", "cf.core.oagw.cors.v1"];
/// Catalog-only transform plugin identifiers.
pub const CATALOG_ONLY_TRANSFORM: [&str; 2] =
    ["cf.core.oagw.logging.v1", "cf.core.oagw.metrics.v1"];

/// Render a full GTS identifier from a base type and an instance UUID.
#[must_use]
pub fn format_id(base: &str, id: Uuid) -> String {
    format!("{base}{id}")
}

/// Return the instance part of a GTS identifier (the text after the `~`).
///
/// An identifier without a `~` separator is returned unchanged so callers can
/// accept both the full GTS form and the bare instance form.
#[must_use]
pub fn instance_part(id: &str) -> &str {
    id.split('~').next_back().unwrap_or(id)
}

/// Parse the instance part of a GTS identifier as a UUID.
#[must_use]
pub fn uuid_instance(id: &str) -> Option<Uuid> {
    Uuid::parse_str(instance_part(id)).ok()
}

/// Strip a GTS base-type prefix, returning the bare instance part.
///
/// Returns `None` when `id` does not start with `base`, so callers can
/// distinguish "wrong type" from "bare instance".
#[must_use]
pub fn strip_base<'a>(base: &str, id: &'a str) -> Option<&'a str> {
    id.strip_prefix(base)
        .or_else(|| if id.contains('~') { None } else { Some(id) })
}

#[cfg(test)]
#[path = "ids_tests.rs"]
mod ids_tests;
