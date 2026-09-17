// Created: 2026-09-04 by Constructor Tech
//! Plugin chain of the data plane (`docs/ADR/0002-plugin-system.md`).
//!
//! Three plugin types with separate traits — [`AuthPlugin`] (credential
//! injection), [`GuardPlugin`] (validation, may reject) and
//! [`TransformPlugin`] (request/response mutation) — plus the registry that
//! resolves a [`crate::domain::PluginRef`] into an implementation and the
//! built-in plugins of `docs/ADR/0002-plugin-system.md` and
//! `docs/ADR/0009-required-headers-guard-plugin.md`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use uuid::Uuid;

use crate::dataplane::headers::REQUEST_ID_HEADER;
use crate::domain::{HttpMethod, PluginRef, SecretRef};
use crate::error::OagwError;

/// Registry key of a built-in plugin: its GTS instance id.
pub(crate) type PluginId = String;

/// Inputs of the request-side phases
/// (`docs/ADR/0002-plugin-system.md` "Plugin Traits").
///
/// `headers` is what the client sent: a guard validates the request as it
/// arrived and the transforms read the caller's headers, while the proxy
/// forwards a header the plugin chain adds or rewrites on top of the outbound
/// set built from the upstream `headers` rules.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject of the request.
    pub subject_id: Uuid,
    /// Resolved upstream.
    pub upstream_id: Uuid,
    /// Matched route, when one matched.
    pub route_id: Option<Uuid>,
    /// Method forwarded to the upstream.
    pub method: HttpMethod,
    /// Path forwarded to the upstream.
    pub path: String,
    /// Query string forwarded to the upstream (already allowlist-filtered).
    pub query: Option<String>,
    /// Outbound headers.
    pub headers: HeaderMap,
    /// Buffered request body.
    pub body: Option<bytes::Bytes>,
    /// Request identifier chosen by the `request_id` transform plugin.
    pub request_id: Option<String>,
    /// Plugin configuration slot: the only free-form object of the Phase-1
    /// domain model is the upstream `auth.config`, so guard and transform
    /// plugins read their keys from it as well.
    pub config: Value,
}

/// Inputs of the response-side phases.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Status returned by the upstream.
    pub status: http::StatusCode,
    /// Headers returned to the caller (before the `headers.response` rules).
    pub headers: HeaderMap,
    /// `X-Request-ID` chosen during the request phase, when any.
    pub request_id: Option<String>,
    /// Plugin configuration slot; see [`RequestContext::config`].
    pub config: Value,
}

/// Inputs of the error phase.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Gateway failure being rendered.
    pub error: OagwError,
    /// Headers added to the problem response.
    pub headers: HeaderMap,
    /// Request identifier chosen during the request phase, when any.
    pub request_id: Option<String>,
    /// Plugin configuration slot; see [`RequestContext::config`].
    pub config: Value,
}

/// Decision of a guard phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Stop the chain and return the error to the caller.
    Reject(OagwError),
}

impl GuardDecision {
    /// `true` when the chain continues.
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Secret material lookup of the credential store
/// (`docs/DESIGN.md` §3.2 "Secret Access Control").
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// `true` when the resolver can reach a credential store.
    fn resolves(&self) -> bool {
        true
    }

    /// Resolves a `cred://` reference into its secret material.
    ///
    /// # Errors
    ///
    /// Returns the [`OagwError`] the auth plugin surfaces: an inaccessible or
    /// missing secret yields `401 AuthenticationFailed`
    /// (`docs/DESIGN.md` §3.2 "Secret Access Control").
    async fn resolve(&self, request: &CredentialRequest<'_>) -> Result<String, OagwError>;
}

/// Lookup request handed to a [`CredentialResolver`].
#[derive(Debug, Clone, Copy)]
pub struct CredentialRequest<'a> {
    /// Tenant the secret is resolved for.
    pub tenant_id: Uuid,
    /// Authenticated subject of the request.
    pub subject_id: Uuid,
    /// `cred://` reference.
    pub secret: &'a SecretRef,
}

/// Credential injection phase (`docs/ADR/0002-plugin-system.md`).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// GTS instance id of the plugin.
    fn id(&self) -> &str;

    /// GTS type of the plugin kind.
    fn plugin_type(&self) -> &str;

    /// Injects the upstream credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::AuthenticationFailed`] when the caller may not be
    /// authenticated (`401`).
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
}

/// Validation phase (`docs/ADR/0002-plugin-system.md`).
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// GTS instance id of the plugin.
    fn id(&self) -> &str;

    /// GTS type of the plugin kind.
    fn plugin_type(&self) -> &str;

    /// Validates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns the error a failing guard reports (a rejection carries it in
    /// [`GuardDecision::Reject`]).
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError>;

    /// Validates the upstream response.
    ///
    /// # Errors
    ///
    /// Returns the error a rejection carries in [`GuardDecision::Reject`].
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError>;
}

/// Mutation phase (`docs/ADR/0002-plugin-system.md`).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// GTS instance id of the plugin.
    fn id(&self) -> &str;

    /// GTS type of the plugin kind.
    fn plugin_type(&self) -> &str;

    /// Mutates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for a malformed plugin
    /// configuration.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;

    /// Mutates the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Internal`] when the mutation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError>;

    /// Mutates the problem response of a gateway failure.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Internal`] when the mutation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError>;
}

/// Resolves a [`PluginRef`] into a data-plane implementation.
///
/// Unresolvable references — a custom (Starlark) plugin has no native
/// implementation in this crate — surface as
/// [`OagwError::PluginNotFound`] (`503`).
#[derive(Default)]
pub struct PluginRegistry {
    auth: HashMap<PluginId, Arc<dyn AuthPlugin>>,
    guard: HashMap<PluginId, Arc<dyn GuardPlugin>>,
    transform: HashMap<PluginId, Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginRegistry")
            .field("auth", &self.auth.keys().collect::<Vec<_>>())
            .field("guard", &self.guard.keys().collect::<Vec<_>>())
            .field("transform", &self.transform.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PluginRegistry {
    /// The built-in catalog: `noop` and `apikey` auth, the `required_headers`
    /// guard and the `request_id` transform
    /// (`docs/ADR/0002-plugin-system.md`, `docs/ADR/0009-required-headers-guard-plugin.md`).
    #[must_use]
    pub fn with_builtins(resolver: Option<Arc<dyn CredentialResolver>>) -> Self {
        let mut registry = Self::default();
        registry.register_auth(Arc::new(NoopAuthPlugin));
        registry.register_auth(Arc::new(ApiKeyAuthPlugin::new(resolver)));
        registry.register_guard(Arc::new(RequiredHeadersGuardPlugin));
        registry.register_transform(Arc::new(RequestIdTransformPlugin));
        registry
    }

    /// Registers an auth plugin under its own id.
    pub fn register_auth(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.auth.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a guard plugin under its own id.
    pub fn register_guard(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.guard.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a transform plugin under its own id.
    pub fn register_transform(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.transform.insert(plugin.id().to_owned(), plugin);
    }

    /// Auth plugin bound to `reference`.
    #[must_use]
    pub fn auth(&self, reference: &PluginRef) -> Option<Arc<dyn AuthPlugin>> {
        let id = reference.as_ref_str();
        self.auth.get(id.as_ref()).cloned()
    }

    /// Guard plugin bound to `reference`.
    #[must_use]
    pub fn guard(&self, reference: &PluginRef) -> Option<Arc<dyn GuardPlugin>> {
        let id = reference.as_ref_str();
        self.guard.get(id.as_ref()).cloned()
    }

    /// Transform plugin bound to `reference`.
    #[must_use]
    pub fn transform(&self, reference: &PluginRef) -> Option<Arc<dyn TransformPlugin>> {
        let id = reference.as_ref_str();
        self.transform.get(id.as_ref()).cloned()
    }
}

// ---------------------------------------------------------------- built-ins

/// `cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1` — no authentication.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        crate::domain::plugin::AUTH_NOOP
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        Ok(())
    }
}

/// `cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` — API key injection
/// into a header (default `x-api-key`) or a query parameter.
///
/// Configuration keys (`ctx.config`, the upstream `auth.config` object):
///
/// | Key | Meaning |
/// |---|---|
/// | `header` | Header name carrying the key (default `x-api-key`) |
/// | `query` | Query parameter name carrying the key (overrides `header`) |
/// | `key` | Literal key, or a `cred://` reference |
/// | `key_secret` / `secret_ref` | `cred://` reference to the key |
///
/// An upstream without a key source fails closed with `401`.
pub struct ApiKeyAuthPlugin {
    resolver: Option<Arc<dyn CredentialResolver>>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiKeyAuthPlugin")
            .field(
                "resolver",
                &self
                    .resolver
                    .as_ref()
                    .is_some_and(|resolver| resolver.resolves()),
            )
            .finish()
    }
}

impl ApiKeyAuthPlugin {
    /// A plugin resolving `cred://` references through `resolver`.
    #[must_use]
    pub fn new(resolver: Option<Arc<dyn CredentialResolver>>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        crate::domain::plugin::AUTH_APIKEY
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let key = self.key_material(ctx).await?;
        let query_name = config_str(&ctx.config, "query");
        match query_name {
            Some(parameter) => {
                ctx.query = Some(append_query_parameter(
                    ctx.query.as_deref(),
                    parameter,
                    &key,
                ));
            }
            None => {
                let header = config_str(&ctx.config, "header").unwrap_or("x-api-key");
                let name = HeaderName::from_bytes(header.to_ascii_lowercase().as_bytes()).map_err(
                    |_| OagwError::Validation {
                        detail: format!("'{header}' is not a valid HTTP header name"),
                    },
                )?;
                let value = HeaderValue::from_str(&key).map_err(|_| OagwError::Validation {
                    detail: String::from("the configured api key is not a valid header value"),
                })?;
                ctx.headers.insert(name, value);
            }
        }
        Ok(())
    }
}

impl ApiKeyAuthPlugin {
    /// Key material of the request, from the literal config or the credential
    /// store.
    async fn key_material(&self, ctx: &RequestContext) -> Result<String, OagwError> {
        match config_str(&ctx.config, "key") {
            Some(raw) if raw.starts_with(SecretRef::SCHEME) => {
                let secret = SecretRef::parse(raw)?;
                self.resolve(ctx, &secret).await
            }
            Some(raw) => Ok(raw.to_owned()),
            None => {
                let reference = ["key_secret", "secret_ref"].iter().find_map(|key| {
                    config_str(&ctx.config, key).filter(|raw| raw.starts_with(SecretRef::SCHEME))
                });
                let Some(raw) = reference else {
                    return Err(OagwError::AuthenticationFailed {
                        detail: String::from("the upstream auth configuration carries no api key"),
                    });
                };
                let secret = SecretRef::parse(raw)?;
                self.resolve(ctx, &secret).await
            }
        }
    }

    /// Resolves a `cred://` reference; a failure is surfaced as `401`
    /// (`docs/DESIGN.md` §3.2 "Secret Access Control").
    async fn resolve(&self, ctx: &RequestContext, secret: &SecretRef) -> Result<String, OagwError> {
        let Some(resolver) = self.resolver.as_ref() else {
            return Err(OagwError::AuthenticationFailed {
                detail: String::from(
                    "the upstream auth configuration references a credential store secret",
                ),
            });
        };
        resolver
            .resolve(&CredentialRequest {
                tenant_id: ctx.tenant_id,
                subject_id: ctx.subject_id,
                secret,
            })
            .await
            .map_err(|error| {
                tracing::debug!(%error, "credential resolution failed for the apikey plugin");
                OagwError::AuthenticationFailed {
                    detail: String::from("the credential store rejected the upstream credential"),
                }
            })
    }
}

/// `cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1` — the
/// required-headers guard (`docs/ADR/0009-required-headers-guard-plugin.md`).
///
/// Reads `required_request_headers` / `required_response_headers`
/// (comma-separated) from the plugin configuration; an absent or blank value
/// makes the phase a no-op (fail-open), and only the first missing header is
/// reported.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        crate::domain::plugin::GUARD_REQUIRED_HEADERS
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::GUARD_PLUGIN_TYPE
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
        for name in required(&ctx.config, "required_request_headers") {
            if !ctx.headers.contains_key(name.as_str()) {
                return Ok(GuardDecision::Reject(OagwError::Validation {
                    detail: format!("required request header '{name}' is missing"),
                }));
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
        for name in required(&ctx.config, "required_response_headers") {
            if !ctx.headers.contains_key(name.as_str()) {
                return Ok(GuardDecision::Reject(OagwError::DownstreamError {
                    detail: format!("required response header '{name}' is missing"),
                }));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

/// `cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1` —
/// `X-Request-ID` propagation
/// (`docs/ADR/0002-plugin-system.md` "Built-in Plugins").
///
/// A request without `X-Request-ID` gets a fresh identifier; the identifier is
/// echoed on the response and on a problem response.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        crate::domain::plugin::TRANSFORM_REQUEST_ID
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::TRANSFORM_PLUGIN_TYPE
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let existing = ctx
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let request_id = existing.unwrap_or_else(|| Uuid::new_v4().simple().to_string());
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        ctx.request_id = Some(request_id);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        if let Some(request_id) = &ctx.request_id
            && !ctx.headers.contains_key(REQUEST_ID_HEADER)
            && let Ok(value) = HeaderValue::from_str(request_id)
        {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError> {
        if let Some(request_id) = &ctx.request_id
            && let Ok(value) = HeaderValue::from_str(request_id)
        {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------- config

/// String value of a plugin configuration key.
#[must_use]
pub fn config_str<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str)
}

/// Comma-separated header names of a plugin configuration key, trimmed,
/// lowercased and with empty entries dropped.
///
/// An absent or blank value yields an empty list (fail-open, unconfigured).
#[must_use]
pub fn required(config: &Value, key: &str) -> Vec<String> {
    match config.get(key) {
        Some(Value::String(raw)) => split_names(raw),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(split_names)
            .collect(),
        _ => Vec::new(),
    }
}

/// Splits a comma-separated header list into normalized names.
#[must_use]
fn split_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Appends `name=value` to a query string, keeping the existing parameters.
#[must_use]
fn append_query_parameter(query: Option<&str>, name: &str, value: &str) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, existing) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        serializer.append_pair(key.as_ref(), existing.as_ref());
    }
    serializer.append_pair(name, value);
    serializer.finish()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "plugins_tests.rs"]
mod plugins_tests;
