//! Built-in plugin catalogue (DESIGN §3.1 "Plugin Schemas", PRD §5.3).
//!
//! Every identifier below is registered in the types-registry. Six of them are
//! *catalog only*: they name core data-plane behaviour or reserved future
//! plugins, have no backing implementation, and must be refused when used as a
//! plugin binding.

use toolkit_gts::gts_id;

/// Auth plugin: no authentication.
pub const AUTH_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
/// Auth plugin: API key injection (header/query).
pub const AUTH_API_KEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
/// Auth plugin: `OAuth2` client credentials (form body).
pub const AUTH_OAUTH2_CLIENT_CRED: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
/// Auth plugin: `OAuth2` client credentials (Basic client auth).
pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");
/// Auth plugin: HTTP Basic — catalog identifier only, not implemented.
pub const AUTH_BASIC: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// Auth plugin: bearer token — catalog identifier only, not implemented.
pub const AUTH_BEARER: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

/// Guard plugin: required header enforcement (the only bindable guard).
pub const GUARD_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
/// Guard plugin: request timeout — core data-plane config, catalog only.
pub const GUARD_TIMEOUT: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// Guard plugin: CORS preflight — core data-plane config, catalog only.
pub const GUARD_CORS: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");

/// Transform plugin: `X-Request-ID` propagation.
pub const TRANSFORM_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");
/// Transform plugin: request/response logging — core instrumentation.
pub const TRANSFORM_LOGGING: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Transform plugin: Prometheus metrics — core instrumentation.
pub const TRANSFORM_METRICS: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

/// A built-in plugin catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinPlugin {
    /// Full GTS identifier.
    pub plugin_type: &'static str,
    /// Owning plugin family.
    pub kind: crate::domain::models::PluginKind,
    /// Short registry key (`noop`, `apikey`, …).
    pub name: &'static str,
    /// Whether the identifier can be bound through `auth.plugin_type` or
    /// `plugins.items`.
    pub bindable: bool,
}

/// The complete built-in plugin catalogue, in PRD §5.3 order.
#[must_use]
pub fn builtin_plugin_catalog() -> Vec<BuiltinPlugin> {
    use crate::domain::models::PluginKind;
    vec![
        BuiltinPlugin {
            plugin_type: AUTH_NOOP,
            kind: PluginKind::Auth,
            name: "noop",
            bindable: true,
        },
        BuiltinPlugin {
            plugin_type: AUTH_API_KEY,
            kind: PluginKind::Auth,
            name: "apikey",
            bindable: true,
        },
        BuiltinPlugin {
            plugin_type: AUTH_OAUTH2_CLIENT_CRED,
            kind: PluginKind::Auth,
            name: "oauth2_client_cred",
            bindable: true,
        },
        BuiltinPlugin {
            plugin_type: AUTH_OAUTH2_CLIENT_CRED_BASIC,
            kind: PluginKind::Auth,
            name: "oauth2_client_cred_basic",
            bindable: true,
        },
        BuiltinPlugin {
            plugin_type: AUTH_BASIC,
            kind: PluginKind::Auth,
            name: "basic",
            bindable: false,
        },
        BuiltinPlugin {
            plugin_type: AUTH_BEARER,
            kind: PluginKind::Auth,
            name: "bearer",
            bindable: false,
        },
        BuiltinPlugin {
            plugin_type: GUARD_REQUIRED_HEADERS,
            kind: PluginKind::Guard,
            name: "required_headers",
            bindable: true,
        },
        BuiltinPlugin {
            plugin_type: GUARD_TIMEOUT,
            kind: PluginKind::Guard,
            name: "timeout",
            bindable: false,
        },
        BuiltinPlugin {
            plugin_type: GUARD_CORS,
            kind: PluginKind::Guard,
            name: "cors",
            bindable: false,
        },
        BuiltinPlugin {
            plugin_type: TRANSFORM_REQUEST_ID,
            kind: PluginKind::Transform,
            name: "request_id",
            bindable: true,
        },
        BuiltinPlugin {
            plugin_type: TRANSFORM_LOGGING,
            kind: PluginKind::Transform,
            name: "logging",
            bindable: false,
        },
        BuiltinPlugin {
            plugin_type: TRANSFORM_METRICS,
            kind: PluginKind::Transform,
            name: "metrics",
            bindable: false,
        },
    ]
}

/// Looks a built-in plugin up by full GTS identifier or short name.
#[must_use]
pub fn builtin_plugin(reference: &str) -> Option<BuiltinPlugin> {
    builtin_plugin_catalog().into_iter().find(|entry| {
        entry.plugin_type == reference || entry.name == reference
    })
}

/// Whether a built-in identifier is bindable (`basic`, `bearer`, `timeout`,
/// `cors`, `logging` and `metrics` are not).
#[must_use]
pub fn builtin_plugin_is_bindable(reference: &str) -> bool {
    builtin_plugin(reference).is_none_or(|entry| entry.bindable)
}

/// Whether `reference` names a built-in plugin of the given family.
#[must_use]
pub fn builtin_plugin_is_kind(reference: &str, kind: crate::domain::models::PluginKind) -> bool {
    builtin_plugin(reference).is_some_and(|entry| entry.kind == kind)
}
