//! Plugin contracts (ADR `0002-plugin-system`).
//!
//! Three plugin types with separate traits — `AuthPlugin` (credential
//! injection), `GuardPlugin` (validation, may reject) and `TransformPlugin`
//! (request/response/error mutation) — plus the request/response contexts they
//! operate on and the in-process registries named bindings resolve through.
//!
//! Built-in implementations live in [`crate::infra::plugin::builtin`]; external
//! gears implement the same traits and register into the same registries.
//!
//! Execution order (ADR 0002): auth → guards → transforms → upstream call →
//! response transforms. Upstream plugins run before route plugins.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Auth plugin base type.
pub const AUTH_PLUGIN_TYPE_PREFIX: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Guard plugin base type.
pub const GUARD_PLUGIN_TYPE_PREFIX: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Transform plugin base type.
pub const TRANSFORM_PLUGIN_TYPE_PREFIX: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Built-in no-op auth plugin identifier.
pub const AUTH_PLUGIN_NOOP: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// Built-in API-key auth plugin identifier.
pub const AUTH_PLUGIN_API_KEY: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Built-in OAuth2 client-credentials auth plugin (`client_secret_post`).
pub const AUTH_PLUGIN_OAUTH2_CLIENT_CRED: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// Built-in OAuth2 client-credentials auth plugin (`client_secret_basic`).
pub const AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Built-in required-headers guard plugin identifier.
pub const GUARD_PLUGIN_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Built-in request-id transform plugin identifier.
pub const TRANSFORM_PLUGIN_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// `true` when `plugin_ref` is the full GTS identifier of a named auth plugin.
#[must_use]
pub fn is_auth_plugin_ref(plugin_ref: &str) -> bool {
    plugin_ref.starts_with(AUTH_PLUGIN_TYPE_PREFIX)
}

/// `true` when `plugin_ref` is the full GTS identifier of a named guard plugin.
#[must_use]
pub fn is_guard_plugin_ref(plugin_ref: &str) -> bool {
    plugin_ref.starts_with(GUARD_PLUGIN_TYPE_PREFIX)
}

/// `true` when `plugin_ref` is the full GTS identifier of a transform plugin.
#[must_use]
pub fn is_transform_plugin_ref(plugin_ref: &str) -> bool {
    plugin_ref.starts_with(TRANSFORM_PLUGIN_TYPE_PREFIX)
}

/// Extracts the UUID of a custom (UUID-backed) plugin reference.
#[must_use]
pub fn custom_plugin_uuid(plugin_ref: &str) -> Option<Uuid> {
    let tail = plugin_ref
        .rsplit_once('~')
        .map(|(_, tail)| tail)
        .unwrap_or(plugin_ref);
    Uuid::parse_str(tail).ok()
}

/// Short registry key of a plugin reference (`…~cf.core.oagw.noop.v1` →
/// `cf.core.oagw.noop.v1`), or `None` for UUID-backed (custom) plugins.
#[must_use]
pub fn registry_key(plugin_ref: &str) -> Option<&str> {
    let tail = plugin_ref.rsplit_once('~')?.1;
    if tail.is_empty() || Uuid::parse_str(tail).is_ok() {
        None
    } else {
        Some(tail)
    }
}

/// Outbound request context handed to auth and transform plugins.
///
/// Credential headers and resolved secret material are excluded from the
/// [`Debug`] output so no credential or PII can reach a log line.
#[derive(Clone)]
pub struct RequestContext {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Alias of the upstream this request is routed to.
    pub upstream_alias: String,
    /// Matched route, when the request went through route resolution.
    pub route_id: Option<Uuid>,
    /// HTTP method.
    pub method: http::Method,
    /// Upstream request path (suffix appended when the route allows it).
    pub path: String,
    /// Raw query string, already validated against the route allowlist.
    pub query: Option<String>,
    /// Headers forwarded upstream.
    pub headers: http::HeaderMap,
    /// Upstream request body.
    pub body: Bytes,
    /// Authenticated subject of the caller, when the transport carried one.
    pub subject_id: Option<Uuid>,
    /// Caller identity, for `cred://` resolution scoped to the caller.
    pub security: Option<Arc<toolkit_security::SecurityContext>>,
    /// Configuration of the plugin being executed
    /// (`plugins.configs[<plugin_ref>]`), reset before each plugin runs.
    pub plugin_config: Option<serde_json::Value>,
    /// Headers injected by auth plugins (credentials).
    injected: Vec<(http::HeaderName, http::HeaderValue)>,
    /// Credential material resolved from `cred://` references (never logged).
    secrets: BTreeMap<String, String>,
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("tenant_id", &self.tenant_id)
            .field("upstream_alias", &self.upstream_alias)
            .field("route_id", &self.route_id)
            .field("method", &self.method)
            .field("path", &self.path)
            .field("query", &self.query)
            .field("header_count", &self.headers.len())
            .field("body_len", &self.body.len())
            .field("injected_header_count", &self.injected.len())
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

impl RequestContext {
    /// Builds a request context.
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        upstream_alias: impl Into<String>,
        method: http::Method,
        path: impl Into<String>,
    ) -> Self {
        Self {
            tenant_id,
            upstream_alias: upstream_alias.into(),
            route_id: None,
            method,
            path: path.into(),
            query: None,
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
            subject_id: None,
            security: None,
            plugin_config: None,
            injected: Vec::new(),
            secrets: BTreeMap::new(),
        }
    }

    /// Injects a credential header (auth-plugin seam).
    pub fn inject_header(&mut self, name: http::HeaderName, value: http::HeaderValue) {
        let mut value = value;
        value.set_sensitive(true);
        self.injected.push((name, value));
    }

    /// Drains the credential headers injected so far.
    pub fn take_injected_headers(&mut self) -> Vec<(http::HeaderName, http::HeaderValue)> {
        std::mem::take(&mut self.injected)
    }

    /// Records resolved credential material under a logical key.
    pub fn set_secret(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.secrets.insert(key.into(), value.into());
    }

    /// Reads previously resolved credential material.
    #[must_use]
    pub fn secret(&self, key: &str) -> Option<&str> {
        self.secrets.get(key).map(String::as_str)
    }

    /// The configuration entry bound to `plugin_ref`, if any.
    ///
    /// Bindings are configured on the upstream or route that names the plugin
    /// (`plugins.configs`); a plugin that needs no configuration reads `None`.
    #[must_use]
    pub fn config_for(&self, plugin_ref: &str, configs: &BTreeMap<String, serde_json::Value>) -> Option<serde_json::Value> {
        configs.get(plugin_ref).cloned()
    }
}

/// Upstream response context handed to guard and transform plugins.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Alias of the upstream that answered.
    pub upstream_alias: String,
    /// Upstream status code.
    pub status: http::StatusCode,
    /// Response headers.
    pub headers: http::HeaderMap,
    /// Response body (buffered; the 100 MB limit applies).
    pub body: Bytes,
    /// Configuration of the plugin being executed
    /// (`plugins.configs[<plugin_ref>]`), reset before each plugin runs.
    pub plugin_config: Option<serde_json::Value>,
    /// Request id injected upstream, for plugins that echo it to the caller.
    pub request_id: Option<String>,
}

impl ResponseContext {
    /// Builds a response context.
    #[must_use]
    pub fn new(tenant_id: Uuid, upstream_alias: impl Into<String>, status: http::StatusCode) -> Self {
        Self {
            tenant_id,
            upstream_alias: upstream_alias.into(),
            status,
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
            plugin_config: None,
            request_id: None,
        }
    }
}

/// Error context handed to transform plugins on the error path.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Alias of the upstream the request was routed to.
    pub upstream_alias: String,
    /// The domain error to render.
    pub error: DomainError,
}

impl ErrorContext {
    /// Builds an error context.
    #[must_use]
    pub fn new(tenant_id: Uuid, upstream_alias: impl Into<String>, error: DomainError) -> Self {
        Self {
            tenant_id,
            upstream_alias: upstream_alias.into(),
            error,
        }
    }
}

/// Outcome of a guard plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Let the request/response through.
    Continue,
    /// Reject with a domain error (mapped to a problem response).
    Reject(DomainError),
}

impl GuardDecision {
    /// Builds a [`GuardDecision::Reject`] carrying `error`.
    #[must_use]
    pub fn reject(error: DomainError) -> Self {
        Self::Reject(error)
    }

    /// `true` when the guard let the exchange continue.
    #[must_use]
    pub fn is_continue(&self) -> bool {
        matches!(self, Self::Continue)
    }
}

/// Injects authentication credentials into the upstream request.
///
/// Executed once per request, before guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Registry key of this plugin (`cf.core.oagw.noop.v1`).
    fn id(&self) -> &str;
    /// Full GTS type identifier of this plugin.
    fn plugin_type(&self) -> &str;
    /// Injects credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Implementations return [`DomainError::SecretNotFound`] when a
    /// `cred://` reference cannot be resolved, or
    /// [`DomainError::AuthenticationFailed`] when the credential cannot be
    /// minted.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError>;
}

/// Validates a request or response and may reject it.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Registry key of this plugin.
    fn id(&self) -> &str;
    /// Full GTS type identifier of this plugin.
    fn plugin_type(&self) -> &str;
    /// Validates the outbound request.
    ///
    /// # Errors
    ///
    /// Transport or configuration failures propagate as [`DomainError`].
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, DomainError>;
    /// Validates the upstream response.
    ///
    /// # Errors
    ///
    /// Transport or configuration failures propagate as [`DomainError`].
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, DomainError>;
}

/// Mutates the request, response or error data.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Registry key of this plugin.
    fn id(&self) -> &str;
    /// Full GTS type identifier of this plugin.
    fn plugin_type(&self) -> &str;
    /// Mutates the outbound request.
    ///
    /// # Errors
    ///
    /// Transport or configuration failures propagate as [`DomainError`].
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError>;
    /// Mutates the upstream response.
    ///
    /// # Errors
    ///
    /// Transport or configuration failures propagate as [`DomainError`].
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), DomainError>;
    /// Mutates the error rendered to the caller.
    ///
    /// # Errors
    ///
    /// Transport or configuration failures propagate as [`DomainError`].
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), DomainError>;
}

/// In-process registry of the built-in and deployed-gear plugins.
///
/// Named bindings (`plugin_ref` = full GTS identifier of a named plugin)
/// resolve through these maps; UUID-backed (custom) plugins resolve through
/// persistence in a later phase.
#[derive(Default)]
pub struct PluginRegistry {
    auth_plugins: BTreeMap<String, Arc<dyn AuthPlugin>>,
    guard_plugins: BTreeMap<String, Arc<dyn GuardPlugin>>,
    transform_plugins: BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("auth", &self.auth_plugins.keys().collect::<Vec<_>>())
            .field("guard", &self.guard_plugins.keys().collect::<Vec<_>>())
            .field(
                "transform",
                &self.transform_plugins.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PluginRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an auth plugin under its short id.
    pub fn register_auth(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.auth_plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a guard plugin under its short id.
    pub fn register_guard(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.guard_plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a transform plugin under its short id.
    pub fn register_transform(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.transform_plugins
            .insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a full plugin reference to an auth plugin.
    #[must_use]
    pub fn auth_plugin(&self, plugin_ref: &str) -> Option<Arc<dyn AuthPlugin>> {
        let key = registry_key(plugin_ref)?;
        self.auth_plugins.get(key).cloned()
    }

    /// Resolves a full plugin reference to a guard plugin.
    #[must_use]
    pub fn guard_plugin(&self, plugin_ref: &str) -> Option<Arc<dyn GuardPlugin>> {
        let key = registry_key(plugin_ref)?;
        self.guard_plugins.get(key).cloned()
    }

    /// Resolves a full plugin reference to a transform plugin.
    #[must_use]
    pub fn transform_plugin(&self, plugin_ref: &str) -> Option<Arc<dyn TransformPlugin>> {
        let key = registry_key(plugin_ref)?;
        self.transform_plugins.get(key).cloned()
    }

    /// `true` when `plugin_ref` resolves in this registry.
    #[must_use]
    pub fn knows(&self, plugin_ref: &str) -> bool {
        self.auth_plugin(plugin_ref).is_some()
            || self.guard_plugin(plugin_ref).is_some()
            || self.transform_plugin(plugin_ref).is_some()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn plugin_type_prefixes_match_the_adr() {
        assert_eq!(AUTH_PLUGIN_TYPE_PREFIX, "gts.cf.core.oagw.auth_plugin.v1~");
        assert_eq!(GUARD_PLUGIN_TYPE_PREFIX, "gts.cf.core.oagw.guard_plugin.v1~");
        assert_eq!(
            TRANSFORM_PLUGIN_TYPE_PREFIX,
            "gts.cf.core.oagw.transform_plugin.v1~"
        );
        assert!(is_auth_plugin_ref(AUTH_PLUGIN_NOOP));
        assert!(is_guard_plugin_ref(GUARD_PLUGIN_REQUIRED_HEADERS));
        assert!(is_transform_plugin_ref(TRANSFORM_PLUGIN_REQUEST_ID));
        assert!(!is_guard_plugin_ref(AUTH_PLUGIN_API_KEY));
    }

    #[test]
    fn custom_plugin_refs_are_uuid_backed() {
        let id = Uuid::new_v4();
        let plugin_ref = format!("{AUTH_PLUGIN_TYPE_PREFIX}{id}");
        assert_eq!(custom_plugin_uuid(&plugin_ref), Some(id));
        assert!(registry_key(&plugin_ref).is_none());
        assert_eq!(
            registry_key(AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC),
            Some("cf.core.oagw.oauth2_client_cred_basic.v1")
        );
        assert!(custom_plugin_uuid("not-a-ref").is_none());
    }

    #[test]
    fn injected_credential_headers_are_sensitive() {
        let mut ctx = RequestContext::new(Uuid::nil(), "api.openai.com", http::Method::GET, "/v1");
        ctx.inject_header(
            http::HeaderName::from_static("authorization"),
            http::HeaderValue::from_static("Bearer token"),
        );
        let injected = ctx.take_injected_headers();
        assert_eq!(injected.len(), 1);
        assert!(injected[0].1.is_sensitive());
        assert!(ctx.take_injected_headers().is_empty());
    }

    #[test]
    fn request_context_debug_does_not_leak_headers_or_secrets() {
        let mut ctx = RequestContext::new(Uuid::nil(), "api.openai.com", http::Method::GET, "/v1");
        ctx.set_secret("api_key", "super-secret");
        ctx.inject_header(
            http::HeaderName::from_static("authorization"),
            http::HeaderValue::from_static("Bearer super-secret"),
        );
        let rendered = format!("{ctx:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("secret_count: 1"));
        assert_eq!(ctx.secret("api_key"), Some("super-secret"));
        assert_eq!(ctx.secret("other"), None);
    }

    #[test]
    fn guard_decisions_are_inspectable() {
        assert!(GuardDecision::Continue.is_continue());
        let reject = GuardDecision::reject(DomainError::RateLimitExceeded("a".to_owned()));
        assert!(!reject.is_continue());
        assert!(matches!(reject, GuardDecision::Reject(_)));
    }
}
