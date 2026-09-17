// Created: 2026-09-03 by Constructor Tech
//! GTS identifier helpers for the OAGW gear.
//!
//! Every OAGW resource is addressed by an anonymous GTS identifier of the form
//! `gts.cf.core.oagw.{entity}.v1~{uuid}`. Named plugins use the
//! `gts.cf.core.oagw.{entity}.v1~cf.core.oagw.{name}.v1` form instead.

/// Base type identifier of an upstream resource (instance appended after `~`).
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Base type identifier of a route resource (instance appended after `~`).
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Base type identifier of a custom auth plugin (instance appended after `~`).
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Base type identifier of a custom guard plugin (instance appended after `~`).
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Base type identifier of a custom transform plugin (instance appended after `~`).
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Base type prefix shared by every error type emitted by this gear.
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// Namespace used by named (built-in) plugin identifiers.
const NAMED_PLUGIN_NS: &str = "cf.core.oagw.";

/// Upstream protocol selector: HTTP/1.1 and HTTP/2 request proxying.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// Upstream protocol selector: gRPC. Catalogued but not proxied in this build.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Builds the full GTS identifier of an upstream instance.
#[must_use]
pub fn upstream_id(id: uuid::Uuid) -> String {
    format!("{UPSTREAM_TYPE}{id}")
}

/// Builds the full GTS identifier of a route instance.
#[must_use]
pub fn route_id(id: uuid::Uuid) -> String {
    format!("{ROUTE_TYPE}{id}")
}

/// Builds the full GTS identifier of a custom plugin instance.
///
/// `base` is one of the `*_PLUGIN_TYPE` constants above.
#[must_use]
pub fn plugin_id(base: &str, id: uuid::Uuid) -> String {
    format!("{base}{id}")
}

/// Builds the identifier of a named (built-in) plugin instance.
///
/// For example `(AUTH_PLUGIN_TYPE, "apikey")` yields
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`.
#[must_use]
pub fn named_plugin_id(base: &str, name: &str) -> String {
    format!("{base}{NAMED_PLUGIN_NS}{name}.v1")
}

/// Splits a GTS identifier into its type prefix and instance part.
#[must_use]
pub fn split(gts_id: &str) -> Option<(&str, &str)> {
    gts_id.split_once('~')
}

/// Extracts the UUID instance of a GTS identifier, if the instance is a UUID.
#[must_use]
pub fn uuid_instance(gts_id: &str) -> Option<uuid::Uuid> {
    let (_, instance) = split(gts_id)?;
    uuid::Uuid::parse_str(instance).ok()
}

/// Extracts the short name of a named plugin instance (`cf.core.oagw.apikey.v1`
/// -> `apikey`).
#[must_use]
pub fn named_instance(gts_id: &str) -> Option<&str> {
    let (_, instance) = split(gts_id)?;
    let name = instance.strip_prefix(NAMED_PLUGIN_NS)?;
    name.strip_suffix(".v1")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use uuid::Uuid;
    #[test]
    fn named_instance_extracts_short_name() {
        let id = named_plugin_id(AUTH_PLUGIN_TYPE, "apikey");
        assert_eq!(named_instance(&id), Some("apikey"));
        assert!(uuid_instance(&id).is_none());
    }

    #[test]
    fn uuid_instance_parses_uuid_backed_identifiers() {
        let id = Uuid::new_v4();
        let gts = plugin_id(GUARD_PLUGIN_TYPE, id);
        assert_eq!(uuid_instance(&gts), Some(id));
        assert!(named_instance(&gts).is_none());
    }

    #[test]
    fn split_returns_none_without_tilde() {
        assert!(split("gts.cf.core.oagw.upstream.v1").is_none());
    }
}
