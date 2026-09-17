//! Plugin traits and execution contexts (`ADR 0002`).
//!
//! Three plugin types with separate traits, executed in a deterministic
//! order: Auth → Guards → Transform(request) → upstream →
//! Transform(response|error). Upstream bindings always run before route
//! bindings.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, Method, StatusCode};
use uuid::Uuid;

use crate::domain::error::OagwError;

/// Security identity of the caller, threaded through the plugin chain.
pub type SharedSecurityContext = Arc<toolkit_security::SecurityContext>;

/// Per-request state handed to auth, guard, and request-phase transform
/// plugins.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Owning tenant of the resolved upstream configuration.
    pub tenant_id: Uuid,
    /// Tenant of the CALLER's own identity, i.e.
    /// `SecurityContext::subject_tenant_id()`.
    ///
    /// Deliberately separate from [`RequestContext::tenant_id`]: that one names
    /// the tenant that owns the upstream being called — the resource owner —
    /// while this one names who is asking. Plugins that key per-caller state
    /// (the `OAuth2` token cache, `ADR 0008` "Cache Key Design") must use this
    /// one, or a token minted for tenant A's credentials would be replayed for
    /// tenant B's request to the same upstream.
    pub caller_tenant_id: Uuid,
    /// Authenticated subject of the caller (empty when anonymous).
    pub subject_id: String,
    /// Outbound request method.
    pub method: Method,
    /// Path forwarded upstream (already suffix-resolved).
    pub path: String,
    /// Raw query string forwarded upstream.
    pub query: Option<String>,
    /// Outbound headers; plugins may add, set, or remove entries.
    pub headers: HeaderMap,
    /// Headers exactly as the client sent them.
    ///
    /// Validation runs against this set, not against `headers`: what a caller
    /// actually sent must not depend on which headers the upstream's
    /// passthrough filter happens to forward (`ADR 0009`).
    pub inbound_headers: HeaderMap,
    /// Best-effort client address for scoped rate limits.
    pub client_ip: Option<IpAddr>,
    /// Authenticated caller identity, used for credential resolution.
    pub security: Option<SharedSecurityContext>,
    /// Configuration of the plugin binding currently being executed.
    pub config: serde_json::Value,
    /// Scratch space shared between plugins of one request.
    pub attributes: BTreeMap<String, serde_json::Value>,
}

impl RequestContext {
    /// Insert (replacing any previous value) an outbound header.
    pub fn set_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            self.headers.insert(name, value);
        }
    }

    /// Append an outbound header, keeping existing values.
    pub fn add_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            self.headers.append(name, value);
        }
    }

    /// Remove every value of an outbound header.
    pub fn remove_header(&mut self, name: &str) {
        if let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) {
            self.headers.remove(name);
        }
    }

    /// Read the first value of an outbound header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// Per-response state handed to guard and response-phase transform plugins.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Owning tenant of the resolved upstream configuration.
    pub tenant_id: Uuid,
    /// Upstream response status.
    pub status: StatusCode,
    /// Upstream response headers; plugins may mutate them.
    pub headers: HeaderMap,
    /// Configuration of the plugin binding currently being executed.
    pub config: serde_json::Value,
    /// Scratch space shared with the request phase.
    pub attributes: BTreeMap<String, serde_json::Value>,
}

impl ResponseContext {
    /// Insert (replacing any previous value) a response header.
    pub fn set_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            self.headers.insert(name, value);
        }
    }

    /// Read the first value of a response header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// State handed to error-phase transform plugins.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// The gateway error about to be rendered.
    pub error: OagwError,
    /// Configuration of the plugin binding currently being executed.
    pub config: serde_json::Value,
    /// Scratch space shared with the request phase.
    pub attributes: BTreeMap<String, serde_json::Value>,
}

/// Attribute map seeded before the plugin chain runs.
///
/// Plugins communicate through these keys; `x-request-id` is pre-seeded from
/// the inbound request so an error hook can echo the caller's identifier back
/// instead of minting a new one.
#[must_use]
pub fn request_attributes(headers: &http::HeaderMap) -> BTreeMap<String, serde_json::Value> {
    let mut attributes = BTreeMap::new();
    if let Some(value) = headers
        .get(crate::infra::plugin::transform::REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        attributes.insert(
            crate::infra::plugin::transform::REQUEST_ID_HEADER.to_owned(),
            serde_json::Value::String(value.to_owned()),
        );
    }
    attributes
}

/// A guard plugin's verdict.
#[derive(Debug, Clone)]
pub enum GuardDecision {
    /// Continue processing.
    Allow,
    /// Reject the request with a specific status and machine-readable code.
    Reject(Rejection),
}

/// A guard rejection: status, machine-readable code, and detail.
#[derive(Debug, Clone)]
pub struct Rejection {
    /// HTTP status to answer with.
    pub status: StatusCode,
    /// Stable machine-readable code (emitted as `context.error_code`).
    pub error_code: String,
    /// Human-readable explanation (never includes credential material).
    pub detail: String,
}

impl GuardDecision {
    /// Whether the request may proceed.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Injects authentication credentials into the outbound request (`ADR 0002`).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short registry key (`apikey`, `noop`, ...).
    fn id(&self) -> &'static str;
    /// Canonical GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Resolve or mint credentials and inject them into `ctx.headers`.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] classified as `auth.failed` (or
    /// `secret.not_found`) when credentials cannot be resolved.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
}

/// Validates a request (or response) and may reject it (`ADR 0002`).
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short registry key.
    fn id(&self) -> &'static str;
    /// Canonical GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Validate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] only for infrastructure failures; policy
    /// violations are reported as [`GuardDecision::Reject`].
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError>;
    /// Validate the upstream response before it reaches the caller.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] only for infrastructure failures.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError>;
}

/// Modifies request, response, or error data (`ADR 0002`).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short registry key.
    fn id(&self) -> &'static str;
    /// Canonical GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Mutate the outbound request before it is sent.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
    /// Mutate the upstream response before it is returned.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError>;
    /// Enrich a gateway error before it is rendered.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError>;
}

/// Cache tuning for the `OAuth2` client-credentials plugin (`ADR 0008`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached access-token TTL.
    pub ttl: Duration,
    /// Maximum entries in the cache.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(crate::config::DEFAULT_TOKEN_CACHE_TTL_SECS),
            capacity: crate::config::DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

/// Registry of named [`AuthPlugin`] implementations.
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Build a registry holding every built-in auth plugin.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(crate::infra::plugin::auth::NoopAuthPlugin));
        registry.register(Arc::new(crate::infra::plugin::auth::ApiKeyAuthPlugin::new(
            credstore.clone(),
        )));
        registry.register(Arc::new(
            crate::infra::plugin::auth::OAuth2ClientCredAuthPlugin::new(
                credstore.clone(),
                toolkit_auth::oauth2::ClientAuthMethod::Form,
                token_cache,
            ),
        ));
        registry.register(Arc::new(
            crate::infra::plugin::auth::OAuth2ClientCredAuthPlugin::new(
                credstore,
                toolkit_auth::oauth2::ClientAuthMethod::Basic,
                token_cache,
            ),
        ));
        registry
    }

    /// Register (or replace) a plugin.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolve a plugin by its canonical GTS identifier or short key.
    #[must_use]
    pub fn resolve(&self, plugin_type: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(plugin_type).cloned().or_else(|| {
            self.plugins
                .values()
                .find(|p| p.plugin_type() == plugin_type)
                .cloned()
        })
    }

    /// Keys of every registered plugin.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }
}

/// Registry of named [`GuardPlugin`] implementations.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Registry holding the only bindable guard plugin (`ADR 0009`).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(
            crate::infra::plugin::guard::RequiredHeadersGuardPlugin,
        ));
        registry
    }

    /// Register (or replace) a plugin.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolve a plugin by canonical GTS identifier or short key.
    #[must_use]
    pub fn resolve(&self, plugin_type: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(plugin_type).cloned().or_else(|| {
            self.plugins
                .values()
                .find(|p| p.plugin_type() == plugin_type)
                .cloned()
        })
    }
}

/// Registry of named [`TransformPlugin`] implementations.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Registry holding the request-id transform plugin.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(
            crate::infra::plugin::transform::RequestIdTransformPlugin,
        ));
        registry
    }

    /// Register (or replace) a plugin.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolve a plugin by canonical GTS identifier or short key.
    #[must_use]
    pub fn resolve(&self, plugin_type: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(plugin_type).cloned().or_else(|| {
            self.plugins
                .values()
                .find(|p| p.plugin_type() == plugin_type)
                .cloned()
        })
    }
}

/// How a configured plugin reference resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPlugin {
    /// A named built-in plugin (GTS identifier, no stored definition).
    Named,
    /// A tenant-defined custom plugin backed by a stored definition.
    Custom(Uuid),
    /// No implementation exists for the identifier.
    Unknown,
}

/// Classify a `plugin_ref` value: UUID-backed custom plugin, named plugin, or
/// unknown (`DESIGN.md` "Resolution Algorithm").
#[must_use]
pub fn classify_plugin_ref(plugin_ref: &str) -> ResolvedPlugin {
    let instance = crate::domain::model::gts_instance(plugin_ref);
    match crate::domain::model::parse_gts_uuid(instance, "") {
        Some(id) => ResolvedPlugin::Custom(id),
        None => {
            if instance.is_empty() {
                ResolvedPlugin::Unknown
            } else {
                ResolvedPlugin::Named
            }
        }
    }
}
