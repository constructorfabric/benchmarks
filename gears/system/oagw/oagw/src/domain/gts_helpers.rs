//! GTS identifier helpers for OAGW.
//!
//! OAGW identifiers follow the anonymous GTS shape
//! `gts.cf.core.oagw.{type}.v1~{suffix}`:
//!
//! * protocol: `~cf.core.oagw.http.v1`
//! * plugin:   `~cf.core.oagw.apikey.v1`
//! * resource: `~{uuid}`

/// GTS namespace prefix for the OAGW protocol identifier.
pub const OAGW_PROTOCOL_PREFIX: &str = "gts.cf.core.oagw.protocol.v1~";

/// HTTP protocol identifier.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC protocol identifier (catalogued; no data-plane code path yet).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Plugin identifiers
// ---------------------------------------------------------------------------

/// Auth plugin namespace.
pub const AUTH_PLUGIN_PREFIX: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Guard plugin namespace.
pub const GUARD_PLUGIN_PREFIX: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Transform plugin namespace.
pub const TRANSFORM_PLUGIN_PREFIX: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// `ApiKeyAuthPlugin`.
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `NoopAuthPlugin`.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `OAuth2ClientCredAuthPlugin` (form client auth, ADR-0008).
pub const OAUTH2_CLIENT_CRED_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// `OAuth2ClientCredAuthPlugin` (basic client auth, ADR-0008).
pub const OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// `RequiredHeadersGuardPlugin` (ADR-0009).
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// `RequestIdTransformPlugin`.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

// Catalogued identifiers without a backing plugin implementation. They exist
// in the types-registry only and must NOT resolve in the plugin registries
// (ADR-0002 "Built-in Plugins").
/// Catalogued only — no `AuthPlugin` implementation.
pub const CATALOG_BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalogued only — no `AuthPlugin` implementation.
pub const CATALOG_BEARER_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
/// Catalogued only — enforced by core data-plane logic, not a `GuardPlugin`.
pub const CATALOG_TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalogued only — enforced by core data-plane logic, not a `GuardPlugin`.
pub const CATALOG_CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
/// Catalogued only — implemented by `infra::metrics`, not a `TransformPlugin`.
pub const CATALOG_LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalogued only — implemented by `infra::metrics`, not a `TransformPlugin`.
pub const CATALOG_METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Plugin identifiers that are catalogued but have no runtime implementation.
pub const CATALOG_ONLY_PLUGIN_IDS: &[&str] = &[
    CATALOG_BASIC_AUTH_PLUGIN_ID,
    CATALOG_BEARER_AUTH_PLUGIN_ID,
    CATALOG_TIMEOUT_GUARD_PLUGIN_ID,
    CATALOG_CORS_GUARD_PLUGIN_ID,
    CATALOG_LOGGING_TRANSFORM_PLUGIN_ID,
    CATALOG_METRICS_TRANSFORM_PLUGIN_ID,
];

// ---------------------------------------------------------------------------
// Error identifiers (RFC 9457 `type` members)
// ---------------------------------------------------------------------------

/// Error namespace used in the `type` member of OAGW problems.
pub const OAGW_ERROR_NS: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// Builds a `cf.oagw.<tail>` error identifier.
#[must_use]
pub fn error_id(tail: &str) -> String {
    format!("{OAGW_ERROR_NS}{tail}")
}

/// Builds `gts.cf.core.oagw.{kind}.v1~{uuid}` for a stored resource id.
#[must_use]
pub fn resource_id(kind: &str, uuid: &uuid::Uuid) -> String {
    format!("gts.cf.core.oagw.{kind}.v1~{uuid}")
}

/// Extracts the `{uuid}` tail of an anonymous GTS resource id, if any.
#[must_use]
pub fn resource_uuid(id: &str) -> Option<uuid::Uuid> {
    let tail = id.rsplit('~').next()?;
    uuid::Uuid::parse_str(tail).ok()
}

/// Classifies a plugin id into its plugin kind.
///
/// Returns `None` when the id does not belong to any OAGW plugin namespace.
#[must_use]
pub fn plugin_kind(id: &str) -> Option<&'static str> {
    if id.starts_with(AUTH_PLUGIN_PREFIX) {
        Some("auth")
    } else if id.starts_with(GUARD_PLUGIN_PREFIX) {
        Some("guard")
    } else if id.starts_with(TRANSFORM_PLUGIN_PREFIX) {
        Some("transform")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn resource_id_round_trips() {
        let id = Uuid::new_v4();
        let s = resource_id("upstream", &id);
        assert_eq!(s, format!("gts.cf.core.oagw.upstream.v1~{id}"));
        assert_eq!(resource_uuid(&s), Some(id));
    }

    #[test]
    fn plugin_kind_distinguishes_namespaces() {
        assert_eq!(plugin_kind(APIKEY_AUTH_PLUGIN_ID), Some("auth"));
        assert_eq!(plugin_kind(REQUIRED_HEADERS_GUARD_PLUGIN_ID), Some("guard"));
        assert_eq!(plugin_kind(REQUEST_ID_TRANSFORM_PLUGIN_ID), Some("transform"));
        assert_eq!(plugin_kind(PROTOCOL_HTTP), None);
    }

    #[test]
    fn error_ids_are_namespaced() {
        assert_eq!(
            error_id("route.not_found.v1"),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }
}
