//! GTS identifier constants for the OAGW gear.
//!
//! OAGW resource instances are addressed with anonymous GTS identifiers of the
//! form `gts.cf.core.oagw.{type}.v1~{uuid}`; built-in plugins use named
//! identifiers `gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{name}.v1`.
//!
//! The identifiers below are the single source of truth for this crate: the
//! REST layer builds path parameters and the plugin registries resolve
//! bindings from them. They are plain `&'static str` constants (rather than
//! `gts_id!` expansions) because the contract documents them verbatim
//! (`docs/DESIGN.md` §3.3) and their wire spelling is part of the API.

// ---------------------------------------------------------------------------
// Resource types
// ---------------------------------------------------------------------------

/// GTS base type for upstreams.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS base type for routes.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// GTS base type for auth plugins.
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// GTS base type for guard plugins.
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// GTS base type for transform plugins.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// GTS base type for the upstream protocol enum.
pub const PROTOCOL_TYPE_ID: &str = "gts.cf.core.oagw.protocol.v1~";

// ---------------------------------------------------------------------------
// Protocol instances
// ---------------------------------------------------------------------------

/// HTTP/1.1 + HTTP/2 upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC upstream protocol (catalogued; no proxy code path — see DESIGN §4.7).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Built-in plugin instances (resolvable via the in-process registries)
// ---------------------------------------------------------------------------

/// Built-in auth plugin: no credential injection.
pub const AUTH_PLUGIN_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// Built-in auth plugin: API key injection from a CredStore reference.
pub const AUTH_PLUGIN_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Built-in auth plugin: OAuth2 client credentials, `Form` client auth.
pub const AUTH_PLUGIN_OAUTH2_CC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// Built-in auth plugin: OAuth2 client credentials, `Basic` client auth.
pub const AUTH_PLUGIN_OAUTH2_CC_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Built-in guard plugin: required header enforcement (ADR-0009).
pub const GUARD_PLUGIN_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Built-in transform plugin: `X-Request-ID` propagation.
pub const TRANSFORM_PLUGIN_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

// ---------------------------------------------------------------------------
// Catalog-only identifiers (reserved, not resolvable via any registry)
// ---------------------------------------------------------------------------

/// Reserved auth plugin identifier — no backing implementation (DESIGN §3.1).
pub const AUTH_PLUGIN_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Reserved auth plugin identifier — no backing implementation (DESIGN §3.1).
pub const AUTH_PLUGIN_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
/// Reserved guard plugin identifier — the request timeout is core DP logic.
pub const GUARD_PLUGIN_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Reserved guard plugin identifier — CORS is core DP logic (ADR-0004).
pub const GUARD_PLUGIN_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
/// Reserved transform plugin identifier — logging is core DP instrumentation.
pub const TRANSFORM_PLUGIN_LOGGING: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Reserved transform plugin identifier — metrics are core DP instrumentation.
pub const TRANSFORM_PLUGIN_METRICS: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Instance part of [`AUTH_PLUGIN_NOOP`].
pub const AUTH_PLUGIN_NOOP_INSTANCE: &str = "cf.core.oagw.noop.v1";
/// Instance part of [`AUTH_PLUGIN_APIKEY`].
pub const AUTH_PLUGIN_APIKEY_INSTANCE: &str = "cf.core.oagw.apikey.v1";
/// Instance part of [`AUTH_PLUGIN_OAUTH2_CC`].
pub const AUTH_PLUGIN_OAUTH2_CC_INSTANCE: &str = "cf.core.oagw.oauth2_client_cred.v1";
/// Instance part of [`AUTH_PLUGIN_OAUTH2_CC_BASIC`].
pub const AUTH_PLUGIN_OAUTH2_CC_BASIC_INSTANCE: &str = "cf.core.oagw.oauth2_client_cred_basic.v1";
/// Instance part of [`GUARD_PLUGIN_REQUIRED_HEADERS`].
pub const GUARD_PLUGIN_REQUIRED_HEADERS_INSTANCE: &str = "cf.core.oagw.required_headers.v1";
/// Instance part of [`TRANSFORM_PLUGIN_REQUEST_ID`].
pub const TRANSFORM_PLUGIN_REQUEST_ID_INSTANCE: &str = "cf.core.oagw.request_id.v1";

/// All built-in auth plugin identifiers accepted by `AuthPluginRegistry`.
pub const BUILTIN_AUTH_PLUGINS: [&str; 4] = [
    AUTH_PLUGIN_NOOP,
    AUTH_PLUGIN_APIKEY,
    AUTH_PLUGIN_OAUTH2_CC,
    AUTH_PLUGIN_OAUTH2_CC_BASIC,
];

/// All built-in guard plugin identifiers accepted by `GuardPluginRegistry`.
pub const BUILTIN_GUARD_PLUGINS: [&str; 1] = [GUARD_PLUGIN_REQUIRED_HEADERS];

/// All built-in transform plugin identifiers accepted by `TransformPluginRegistry`.
pub const BUILTIN_TRANSFORM_PLUGINS: [&str; 1] = [TRANSFORM_PLUGIN_REQUEST_ID];

/// Auth plugin identifiers reserved in the types-registry with no
/// implementation. Binding one fails with `unknown auth plugin`.
pub const CATALOG_ONLY_AUTH_PLUGINS: [&str; 2] = [AUTH_PLUGIN_BASIC, AUTH_PLUGIN_BEARER];

/// Guard plugin identifiers reserved in the types-registry with no
/// implementation (core Data Plane functionality instead).
pub const CATALOG_ONLY_GUARD_PLUGINS: [&str; 2] = [GUARD_PLUGIN_TIMEOUT, GUARD_PLUGIN_CORS];

/// Transform plugin identifiers reserved in the types-registry with no
/// implementation (core Data Plane instrumentation instead).
pub const CATALOG_ONLY_TRANSFORM_PLUGINS: [&str; 2] =
    [TRANSFORM_PLUGIN_LOGGING, TRANSFORM_PLUGIN_METRICS];

/// Builds the anonymous GTS instance id for an upstream.
#[must_use]
pub fn upstream_instance_id(id: uuid::Uuid) -> String {
    format!("{UPSTREAM_TYPE_ID}{id}")
}

/// Builds the anonymous GTS instance id for a route.
#[must_use]
pub fn route_instance_id(id: uuid::Uuid) -> String {
    format!("{ROUTE_TYPE_ID}{id}")
}

/// Builds the anonymous GTS instance id for a custom plugin.
#[must_use]
pub fn plugin_instance_id(plugin_type: crate::domain::model::PluginType, id: uuid::Uuid) -> String {
    format!("{}{id}", plugin_type.base_type_id())
}

/// Splits a GTS instance id into `(type_id, instance_part)`.
///
/// The type part ends at the first `~`. Returns `None` when the string is not
/// a `gts.`-prefixed identifier containing a type delimiter.
#[must_use]
pub fn split_instance_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix("gts.")?;
    let (type_part, instance) = rest.split_once('~')?;
    Some((type_part, instance))
}

/// Extracts the UUID tail of an anonymous GTS instance id.
///
/// Accepts either a bare UUID or the full `gts.cf.core.oagw.<type>.v1~<uuid>`
/// form, so management API path parameters can be given in either spelling.
#[must_use]
pub fn uuid_from_instance_id(id: &str) -> Option<uuid::Uuid> {
    if let Ok(parsed) = uuid::Uuid::parse_str(id) {
        return Some(parsed);
    }
    let (_, instance) = split_instance_id(id)?;
    uuid::Uuid::parse_str(instance).ok()
}

/// Returns the canonical instance part of a plugin reference.
///
/// A plugin reference is either a full GTS identifier
/// (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`) or a
/// bare UUID naming a tenant-defined plugin. The returned tail is `cf.core.oagw.required_headers.v1`
/// or the UUID string respectively.
#[must_use]
pub fn plugin_ref_instance(plugin_ref: &str) -> &str {
    split_instance_id(plugin_ref).map_or(plugin_ref, |(_, instance)| instance)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn builds_upstream_instance_id() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(upstream_instance_id(id), format!("{UPSTREAM_TYPE_ID}{id}"));
        assert!(upstream_instance_id(id).starts_with("gts.cf.core.oagw.upstream.v1~"));
    }

    #[test]
    fn round_trips_uuid_instance_id() {
        let id = uuid::Uuid::new_v4();
        let gts_id = upstream_instance_id(id);
        assert_eq!(uuid_from_instance_id(&gts_id), Some(id));
        assert_eq!(uuid_from_instance_id(&id.to_string()), Some(id));
        assert_eq!(
            uuid_from_instance_id("gts.cf.core.oagw.upstream.v1~not-a-uuid"),
            None
        );
    }

    #[test]
    fn splits_plugin_ref() {
        let (_, instance) = split_instance_id(GUARD_PLUGIN_REQUIRED_HEADERS).unwrap();
        assert_eq!(instance, "cf.core.oagw.required_headers.v1");
        assert_eq!(
            plugin_ref_instance(GUARD_PLUGIN_REQUIRED_HEADERS),
            "cf.core.oagw.required_headers.v1"
        );
        assert_eq!(
            plugin_ref_instance("550e8400-e29b-41d4-a716-446655440000"),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }
}
