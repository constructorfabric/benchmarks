//! GTS identifier helpers for the OAGW gear.
//!
//! Every OAGW resource is identified on the wire by a GTS identifier of the form
//! `gts.cf.core.oagw.{type}.v1~{uuid}`. These helpers parse and build such
//! identifiers and define the gateway error identifiers.

use uuid::Uuid;

/// Type id of the upstream resource: `gts.cf.core.oagw.upstream.v1~`.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// Type id of the route resource: `gts.cf.core.oagw.route.v1~`.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// Type id of the plugin resource: `gts.cf.core.oagw.plugin.v1~`.
pub const PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.plugin.v1~";
/// Type id of the auth plugin catalogue entry: `gts.cf.core.oagw.auth_plugin.v1~`.
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Type id of the guard plugin catalogue entry: `gts.cf.core.oagw.guard_plugin.v1~`.
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Type id of the transform plugin catalogue entry: `gts.cf.core.oagw.transform_plugin.v1~`.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// Type id of the protocol catalogue entry: `gts.cf.core.oagw.protocol.v1~`.
pub const PROTOCOL_TYPE_ID: &str = "gts.cf.core.oagw.protocol.v1~";

/// Root of every OAGW gateway error identifier.
pub const ERROR_TYPE_ROOT: &str = "gts.cf.core.errors.err.v1~cf.oagw";

/// Builds a full error identifier from its OAGW error name,
/// e.g. `route.not_found` → `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`.
#[must_use]
pub fn error_id(name: &str) -> String {
    format!("{ERROR_TYPE_ROOT}{name}.v1")
}

/// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`
pub const ERR_VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`
pub const ERR_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1`
pub const ERR_UPSTREAM_NOT_FOUND: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`
pub const ERR_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1`
pub const ERR_ALIAS_CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.route.conflict.v1`
pub const ERR_ROUTE_CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.conflict.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`
pub const ERR_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1`
pub const ERR_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1`
pub const ERR_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1`
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`
pub const ERR_AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1`
pub const ERR_SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`
pub const ERR_RATE_LIMIT: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`
pub const ERR_PAYLOAD_TOO_LARGE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1`
pub const ERR_DOWNSTREAM: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1`
pub const ERR_PROTOCOL: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`
pub const ERR_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1`
///
/// The breaker itself is a documented future development (`DESIGN.md` §4.7);
/// the identifier exists so the error catalogue is complete.
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1`
pub const ERR_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1`
pub const ERR_CONNECT_TIMEOUT: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`
pub const ERR_IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`
pub const ERR_CORS_ORIGIN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`
pub const ERR_CORS_METHOD: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.guard.required_header_missing.v1`
pub const ERR_REQUIRED_HEADER: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.guard.required_header_missing.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1`
pub const ERR_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1`
pub const ERR_INTERNAL: &str = "gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`
pub const ERR_LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";

/// Builds `gts.cf.core.oagw.upstream.v1~{uuid}`.
#[must_use]
pub fn upstream_id(uuid: Uuid) -> String {
    format!("{UPSTREAM_TYPE_ID}{uuid}")
}

/// Builds `gts.cf.core.oagw.route.v1~{uuid}`.
#[must_use]
pub fn route_id(uuid: Uuid) -> String {
    format!("{ROUTE_TYPE_ID}{uuid}")
}

/// Builds `gts.cf.core.oagw.{family}.v1~{uuid}` for the plugin's family, so a
/// stored plugin's identifier names the family it can be bound through.
#[must_use]
pub fn plugin_id(family: &str, uuid: Uuid) -> String {
    format!("{family}{uuid}")
}

/// The instance part of a GTS identifier (the segment after `~`), when the
/// identifier matches `type_prefix~instance`.
#[must_use]
pub fn split_gts_id(id: &str) -> Option<(&str, &str)> {
    let (prefix, instance) = id.split_once('~')?;
    Some((prefix, instance))
}

/// Returns the instance segment of a GTS identifier when it parses as a UUID.
#[must_use]
pub fn uuid_of(id: &str) -> Option<Uuid> {
    uuid_of_in(id, PLUGIN_TYPE_ID)
}

/// Returns the instance segment of a GTS identifier typed as `type_prefix`.
#[must_use]
pub fn uuid_of_in(id: &str, type_prefix: &str) -> Option<Uuid> {
    let (prefix, instance) = split_gts_id(id)?;
    if prefix != type_prefix.trim_end_matches('~') {
        return None;
    }
    Uuid::parse_str(instance).ok()
}

/// True when `id` is a GTS identifier of the given type prefix.
#[must_use]
pub fn is_type(id: &str, type_prefix: &str) -> bool {
    split_gts_id(id)
        .is_some_and(|(prefix, _)| prefix == type_prefix.trim_end_matches('~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_upstream_identifier() {
        let uuid = Uuid::from_u128(0x1234);
        assert_eq!(
            upstream_id(uuid),
            "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000001234"
        );
    }

    #[test]
    fn parses_and_filters_identifiers() {
        let id = "gts.cf.core.oagw.plugin.v1~00000000-0000-4000-8000-000000000001";
        let expected = Uuid::parse_str("00000000-0000-4000-8000-000000000001").ok();
        assert_eq!(uuid_of(id), expected);
        assert_eq!(
            uuid_of_in(id, PLUGIN_TYPE_ID),
            Uuid::parse_str("00000000-0000-4000-8000-000000000001").ok()
        );
        assert_eq!(uuid_of_in(id, UPSTREAM_TYPE_ID), None);
        assert_eq!(uuid_of("nope"), None);
    }

    #[test]
    fn type_checks() {
        assert!(is_type("gts.cf.core.oagw.plugin.v1~abc", PLUGIN_TYPE_ID));
        assert!(!is_type("gts.cf.core.oagw.plugin.v1~abc", ROUTE_TYPE_ID));
        assert!(!is_type("not-a-gts-id", PLUGIN_TYPE_ID));
    }
}
