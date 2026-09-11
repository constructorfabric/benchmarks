//! GTS identifier constants and helpers for the `oagw` gear.
//!
//! All identifiers used on the wire are GTS identifiers. This module is the
//! single place where the type-level identifiers (`gts.…~` without an instance
//! part) and the built-in plugin instance identifiers are spelled out.

/// Base type of an upstream resource.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Base type of a route resource.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Base type of an auth plugin resource.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Base type of a guard plugin resource.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Base type of a transform plugin resource.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// Base type of the proxy resource (the invoke permission).
pub const PROXY_TYPE: &str = "gts.cf.core.oagw.proxy.v1~";

/// Proxyable HTTP protocol identifier.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// Proxyable gRPC protocol identifier.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Base prefix of every OAGW error identifier.
pub const ERROR_TYPE_BASE: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// Built-in auth plugin: no credential injection.
pub const BUILTIN_AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// Built-in auth plugin: API key injected from a credential reference.
pub const BUILTIN_AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Built-in auth plugin: OAuth2 client credentials, form-encoded token request.
pub const BUILTIN_AUTH_OAUTH2_CC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// Built-in auth plugin: OAuth2 client credentials, HTTP Basic token request.
pub const BUILTIN_AUTH_OAUTH2_CC_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Built-in guard plugin: required-header enforcement (ADR 0009).
pub const BUILTIN_GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only guard identifier for the request timeout.
pub const CATALOG_GUARD_TIMEOUT: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only guard identifier for CORS.
pub const CATALOG_GUARD_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// Built-in transform plugin: `X-Request-ID` propagation.
pub const BUILTIN_TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only transform identifier for request logging.
pub const CATALOG_TRANSFORM_LOGGING: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only transform identifier for metrics.
pub const CATALOG_TRANSFORM_METRICS: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Catalog-only auth identifiers that exist for types-registry cataloging only.
pub const CATALOG_AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalog-only bearer auth identifier.
pub const CATALOG_AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// Short name of a plugin instance identifier (the segment after the last `.`).
/// The readable name of a plugin identifier.
///
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` → `apikey`: the
/// instance tail with its version segment removed. A reference that carries
/// no version is returned whole.
pub fn plugin_short_name(id: &str) -> &str {
    let instance = id.split('~').next_back().unwrap_or(id);
    let last = instance.split('.').next_back().unwrap_or(instance);
    let is_version = last.len() >= 2
        && last.starts_with('v')
        && last[1..].bytes().all(|b| b.is_ascii_digit());
    if is_version {
        instance.split('.').rev().nth(1).unwrap_or(instance)
    } else {
        last
    }
}

/// Builds an anonymous GTS instance identifier for a type: `{type}{uuid}`.
pub fn anonymous_gts_id(type_id: &str, uuid: impl std::fmt::Display) -> String {
    format!("{type_id}{uuid}")
}

/// Extracts the instance part of a GTS identifier (everything after `~`).
///
/// Returns `None` when the identifier carries no instance part.
pub fn instance_part(id: &str) -> Option<&str> {
    id.split_once('~').map(|(_, tail)| tail).filter(|t| !t.is_empty())
}

/// Extracts the type part of a GTS identifier (everything before and including `~`).
///
/// The trailing `~` is kept so the result is the bare type identifier, the
/// form every `*_TYPE` constant in this module is written in. When the
/// identifier has no `~` it is returned unchanged.
pub fn type_part(id: &str) -> &str {
    match id.split_once('~') {
        Some((head, _)) => &id[..head.len() + '~'.len_utf8()],
        None => id,
    }
}

/// Parses a GTS identifier, reporting whether it is well formed.
///
/// A GTS identifier is `gts.<package>.<kind>.<name>.v<version>[~<instance>]`:
/// dot-separated, lower-case segments that may snake_case a multi-word name
/// (`auth_plugin`, `required_headers`), optionally carrying an instance tail
/// after a `~`.
pub fn is_gts_id(id: &str) -> bool {
    let Some(stripped) = id.strip_prefix("gts.") else {
        return false;
    };
    if stripped.is_empty() || id.len() > 255 {
        return false;
    }
    let (body, instance) = match stripped.split_once('~') {
        Some((head, tail)) => (head, Some(tail)),
        None => (stripped, None),
    };
    body.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }) && instance.is_none_or(|i| {
        !i.is_empty()
            && i.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    })
}

/// Returns `true` when the identifier's instance part is a UUID
/// (i.e. it addresses a persisted, tenant-owned plugin).
pub fn is_uuid_instance(id: &str) -> bool {
    instance_part(id)
        .map(|p| uuid::Uuid::parse_str(p).is_ok())
        .unwrap_or(false)
}

/// The UUID a plugin reference addresses: the instance part of a full GTS
/// identifier, or the bare UUID a client may send in its place. Both forms
/// name the same `oagw_plugin` row (DESIGN §"Plugin Identification Model"),
/// so resolution treats them alike.
pub fn plugin_uuid(id: &str) -> Option<uuid::Uuid> {
    uuid::Uuid::parse_str(instance_part(id).unwrap_or(id)).ok()
}

/// Whether `id` names a plugin the types registry catalogues but this gear has
/// no executable implementation for.
pub fn is_catalog_only_plugin(id: &str) -> bool {
    matches!(
        id,
        CATALOG_AUTH_BASIC
            | CATALOG_AUTH_BEARER
            | CATALOG_GUARD_TIMEOUT
            | CATALOG_GUARD_CORS
            | CATALOG_TRANSFORM_LOGGING
            | CATALOG_TRANSFORM_METRICS
    )
}

pub fn is_named_instance(id: &str) -> bool {
    matches!(instance_part(id), Some(p) if !p.is_empty() && !is_uuid_instance(id))
}

/// Builds the GTS error identifier for a documented error-type suffix.
pub fn error_gts_id(suffix: &str) -> String {
    format!("{ERROR_TYPE_BASE}{suffix}")
}

/// The permission set an operator needs for upstream CRUD.
pub const UPSTREAM_ACTIONS: &[&str] = &["create", "override", "read", "delete"];
/// The permission set an operator needs for route CRUD.
pub const ROUTE_ACTIONS: &[&str] = &["create", "override", "read", "delete"];
/// The permission set an operator needs for plugin CRUD.
pub const PLUGIN_ACTIONS: &[&str] = &["create", "read", "delete"];
/// The permission a caller needs to proxy a request.
pub const PROXY_ACTION: &str = "invoke";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_gts_id_appends_the_uuid() {
        let id = anonymous_gts_id(UPSTREAM_TYPE, "1b2c3d4e-0000-0000-0000-000000000001");
        assert_eq!(
            id,
            "gts.cf.core.oagw.upstream.v1~1b2c3d4e-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn instance_part_and_type_part_split_cleanly() {
        let id = "gts.cf.core.oagw.route.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7";
        assert_eq!(
            instance_part(id),
            Some("7c9e6679-7425-40de-944b-e07fc1f90ae7")
        );
        assert_eq!(type_part(id), "gts.cf.core.oagw.route.v1~");
        assert_eq!(instance_part(UPSTREAM_TYPE), None);
    }

    #[test]
    fn uuid_and_named_instances_are_distinguished() {
        assert!(is_uuid_instance(
            "gts.cf.core.oagw.auth_plugin.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7"
        ));
        assert!(is_named_instance(BUILTIN_AUTH_APIKEY));
        assert!(!is_uuid_instance(BUILTIN_AUTH_APIKEY));
        assert!(!is_named_instance(UPSTREAM_TYPE));
    }

    #[test]
    fn plugin_short_name_picks_the_last_segment() {
        assert_eq!(plugin_short_name(BUILTIN_AUTH_APIKEY), "apikey");
        assert_eq!(
            plugin_short_name("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
            "required_headers"
        );
        assert_eq!(plugin_short_name("some-plain-ref"), "some-plain-ref");
    }

    #[test]
    fn error_gts_id_builds_the_documented_prefix() {
        assert_eq!(
            error_gts_id("rate_limit.exceeded.v1"),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
    }
}
