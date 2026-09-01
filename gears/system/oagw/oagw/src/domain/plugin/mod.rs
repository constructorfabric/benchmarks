//! Plugin contracts (DESIGN slices 5 and 6).
//!
//! Two plugin families are implemented in the MVP:
//!
//! - **Auth plugins** (slice 5): one `AuthPlugin` is bound per upstream via
//!   `Upstream.auth`. The plugin resolves credentials from `cred_store` by
//!   `secret_ref` at request time and injects them into the outbound request
//!   (header or query parameter).
//! - **Guard plugins** (slice 6, ADR 0009): multiple may be bound per
//!   upstream via `Upstream.plugins.items` and can reject a request or
//!   response. `required_headers.v1` is the only guard identifier with a
//!   backing implementation.
//!
//! # DESIGN-led deviations
//!
//! - The full generic plugin system (transform chains, per-route plugin
//!   bindings, Starlark custom plugins) is out of scope for the MVP; only the
//!   upstream-level **auth** and **guard** plugins are implemented here.
//! - `basic.v1` and `bearer.v1` are resolved exactly as the DESIGN states:
//!   catalog-only identifiers with no backing implementation — using either
//!   fails with `UnknownPlugin` (503 `plugin.not_found.v1`).
//! - Plugin executions are sequential and non-retrying (DESIGN "Retry Policy").

use http::HeaderMap;
use toolkit_security::SecurityContext;

/// Well-known GTS identifiers for the built-in auth plugins
/// (DESIGN "Built-in auth plugins" / "Catalog-only identifiers").
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
pub const API_KEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Catalog-only auth plugin identifiers: reserved in the types-registry but
/// *not* resolvable via `AuthPluginRegistry` (DESIGN "Catalog-only identifiers").
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// GTS identifier of the built-in required-headers guard plugin (ADR 0009) —
/// the only guard identifier resolvable via `GuardPluginRegistry`.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// Everything an auth plugin needs to inject credentials into an outbound
/// request.
///
/// - `headers` collects injected request headers (merged into the outbound
///   request with set semantics after the header-transform pipeline);
/// - `query_params` collects `(name, value)` pairs for query-mode credential
///   injection (appended to the outbound URL query).
pub struct AuthContext<'a> {
    /// Authenticated caller (tenant + subject) driving the proxy request.
    pub security_context: &'a SecurityContext,
    /// The plugin-specific config object from `Upstream.auth.config`.
    pub config: &'a serde_json::Value,
    /// Outbound request headers being accumulated.
    pub headers: &'a mut HeaderMap,
    /// Outbound query parameters being accumulated.
    pub query_params: &'a mut Vec<(String, String)>,
}

/// Error reported by an auth plugin, mapped to the DESIGN error table by the
/// data plane:
///
/// | Variant | HTTP | GTS type |
/// |---|---|---|
/// | `SecretNotFound` | 500 | `...secret.not_found.v1` |
/// | `UnknownPlugin` | 503 | `...plugin.not_found.v1` |
/// | `AuthenticationFailed` | 401 | `...auth.failed.v1` |
/// | `Internal` | 503 | `...link.unavailable.v1` |
#[derive(Debug, Clone)]
pub enum PluginError {
    /// A `cred://` reference did not resolve to a secret in `cred_store`
    /// (including not-found and non-accessible surfaces).
    SecretNotFound(String),
    /// The configured plugin id cannot be resolved by the registry (including
    /// the catalog-only `basic`/`bearer` identifiers).
    UnknownPlugin(String),
    /// Credentials could not be produced (bad plugin config, empty secret
    /// value, `IdP` exchange failure).
    AuthenticationFailed(String),
    /// The credential backend/unexpected failure (`cred_store` unreachable).
    Internal(String),
}

/// A credential-injection plugin bound to an upstream.
///
/// Implementations are stateless where possible; the `OAuth2` plugin owns its
/// internal token cache (ADR 0008).
#[async_trait::async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The canonical plugin id (matches `Upstream.auth.type`).
    fn id(&self) -> &'static str;

    /// Resolve and inject credentials for one proxied request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] the data plane maps to the gateway error
    /// table. Failed executions are never cached by the plugin.
    async fn authenticate(&self, ctx: &mut AuthContext<'_>) -> Result<(), PluginError>;
}

/// Shared helpers for plugin-config parsing (config values are free-form JSON
/// from `Upstream.auth.config`).
#[must_use]
pub fn cfg_string<'a>(config: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Strip a leading `cred://` scheme from a secret reference. `cred_store`
/// references are bare `[a-zA-Z0-9_-]+` names; the DESIGN spells them in
/// config as `cred://<name>` URLs.
#[must_use]
pub fn secret_ref_name(reference: &str) -> &str {
    reference.strip_prefix("cred://").unwrap_or(reference)
}

// ---------------------------------------------------------------------------
// Guard plugins (slice 6, ADR 0009)
// ---------------------------------------------------------------------------

/// Which phase of the request lifecycle a guard check applies to
/// (ADR 0009's symmetric request/response enforcement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardPhase {
    /// The inbound request, checked before proxying to the upstream.
    Request,
    /// The upstream's response, checked before returning to the caller.
    Response,
}

/// Everything a guard plugin needs to inspect one phase.
pub struct GuardContext<'a> {
    /// Authenticated caller driving the proxy request (absent in router-less
    /// tests and unauthenticated paths).
    pub security_context: Option<&'a SecurityContext>,
    /// The plugin-specific config object (from the plugin binding).
    pub config: &'a serde_json::Value,
    /// The headers being guarded (inbound request or upstream response).
    pub headers: &'a HeaderMap,
}

/// Error reported by a guard plugin, mapped to the DESIGN error table by the
/// data plane.
#[derive(Debug, Clone)]
pub enum GuardError {
    /// A configured required header is missing (ADR 0009). The phase selects
    /// the status: request → 400, response → 502 (both
    /// `...required_header.missing.v1`).
    RequiredHeaderMissing {
        /// The phase in which the header was missing.
        phase: GuardPhase,
        /// The first missing header name (lowercased).
        header: String,
    },
}

/// A validation/policy-enforcement plugin (DESIGN "Plugin System"). Bound via
/// `Upstream.plugins.items`; multiple may be bound per upstream and they can
/// reject a request or response (execution order: Auth → Guards → Transform).
pub trait GuardPlugin: Send + Sync {
    /// The canonical plugin id (matches the entry in `plugins.items`).
    fn id(&self) -> &'static str;

    /// Guard the inbound request (before proxying). Default: allow.
    ///
    /// # Errors
    ///
    /// Returns a [`GuardError`] the data plane maps to the gateway error table.
    fn guard_request(&self, _ctx: &GuardContext<'_>) -> Result<(), GuardError> {
        Ok(())
    }

    /// Guard the upstream's response (before returning it to the caller).
    /// Default: allow.
    ///
    /// # Errors
    ///
    /// Returns a [`GuardError`] the data plane maps to the gateway error table.
    fn guard_response(&self, _ctx: &GuardContext<'_>) -> Result<(), GuardError> {
        Ok(())
    }
}
