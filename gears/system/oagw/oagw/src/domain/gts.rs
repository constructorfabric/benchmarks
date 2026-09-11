//! GTS identifiers owned by OAGW, and the helpers that parse them.
//!
//! Resource ids travel as *anonymous* GTS identifiers —
//! `gts.cf.core.oagw.upstream.v1~{uuid}` — while named plugins carry a
//! symbolic instance part (`…auth_plugin.v1~cf.core.oagw.apikey.v1`). Parsing
//! the instance part is therefore the single decision point between "resolve
//! from the in-process registry" and "resolve from the plugin store".

use toolkit_gts::gts_id;
use uuid::Uuid;

// --- Resource base types ---------------------------------------------------

pub const UPSTREAM_BASE: &str = gts_id!("cf.core.oagw.upstream.v1~");
pub const ROUTE_BASE: &str = gts_id!("cf.core.oagw.route.v1~");
pub const PROXY_BASE: &str = gts_id!("cf.core.oagw.proxy.v1~");

pub const AUTH_PLUGIN_BASE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
pub const GUARD_PLUGIN_BASE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
pub const TRANSFORM_PLUGIN_BASE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");

pub const PROTOCOL_BASE: &str = gts_id!("cf.core.oagw.protocol.v1~");
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

// --- Built-in auth plugins -------------------------------------------------

pub const NOOP_AUTH_PLUGIN_ID: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
pub const APIKEY_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");
/// Catalog identifier only — no backing `AuthPlugin` implementation.
pub const BASIC_AUTH_PLUGIN_ID: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// Catalog identifier only — no backing `AuthPlugin` implementation.
pub const BEARER_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

// --- Built-in guard plugins ------------------------------------------------

pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
/// Catalog identifier only — timeout is core Data Plane configuration.
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// Catalog identifier only — CORS is configured via `Upstream.cors`.
pub const CORS_GUARD_PLUGIN_ID: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");

// --- Built-in transform plugins --------------------------------------------

pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");
/// Catalog identifier only — logging is core Data Plane instrumentation.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Catalog identifier only — metrics are core Data Plane instrumentation.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

// --- Parsing helpers -------------------------------------------------------

/// Split a GTS identifier into `(base_type_including_tilde, instance_part)`.
///
/// Returns `None` when the identifier carries no instance part.
#[must_use]
pub fn split_instance(gts_id: &str) -> Option<(&str, &str)> {
    let idx = gts_id.rfind('~')?;
    let (base, instance) = gts_id.split_at(idx + 1);
    if instance.is_empty() {
        None
    } else {
        Some((base, instance))
    }
}

/// The UUID an identifier resolves to, if its instance part is one.
///
/// A bare UUID (no `~`) also resolves, so `plugins.items` may carry either the
/// full GTS form or a raw UUID as `schemas/upstream.v1.schema.json` allows.
#[must_use]
pub fn instance_uuid(gts_id: &str) -> Option<Uuid> {
    match split_instance(gts_id) {
        Some((_, instance)) => Uuid::parse_str(instance).ok(),
        None => Uuid::parse_str(gts_id).ok(),
    }
}

/// Resolve a path parameter that may be a bare UUID or an anonymous GTS id.
///
/// When `expected_base` is given, a GTS-shaped input whose base does not match
/// is rejected — `gts.cf.core.oagw.route.v1~{uuid}` must not address an
/// upstream.
#[must_use]
pub fn parse_resource_id(raw: &str, expected_base: Option<&str>) -> Option<Uuid> {
    let raw = raw.trim();
    if let Some((base, instance)) = split_instance(raw) {
        if let Some(expected) = expected_base
            && !base.eq_ignore_ascii_case(expected)
        {
            return None;
        }
        return Uuid::parse_str(instance).ok();
    }
    Uuid::parse_str(raw).ok()
}

/// Resolve a plugin path parameter to `(kind_base, uuid)`.
#[must_use]
pub fn parse_plugin_id(raw: &str) -> Option<(Option<&'static str>, Uuid)> {
    let raw = raw.trim();
    if let Some((base, instance)) = split_instance(raw) {
        let known = [AUTH_PLUGIN_BASE, GUARD_PLUGIN_BASE, TRANSFORM_PLUGIN_BASE]
            .into_iter()
            .find(|k| k.eq_ignore_ascii_case(base))?;
        return Uuid::parse_str(instance).ok().map(|id| (Some(known), id));
    }
    Uuid::parse_str(raw).ok().map(|id| (None, id))
}

/// Whether `gts_id` names a plugin of `base`, either as a full GTS id or as a
/// bare UUID (which is kind-agnostic and therefore always a candidate).
#[must_use]
pub fn matches_plugin_base(gts_id: &str, base: &str) -> bool {
    match split_instance(gts_id) {
        Some((found, _)) => found.eq_ignore_ascii_case(base),
        None => Uuid::parse_str(gts_id).is_ok(),
    }
}

#[cfg(test)]
#[path = "gts_tests.rs"]
mod tests;
