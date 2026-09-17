//! Plugin traits, registry and built-in implementations.
//!
//! Execution order is deterministic: **Auth → Guards → Transform(request)**,
//! then the upstream call, then **Transform(response)** or
//! **Transform(error)**. Upstream (resource-level) plugins run before route
//! plugins (`DESIGN.md §3.2`, `ADR/0002-plugin-model.md`).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use uuid::Uuid;

use crate::domain::model::{
    PluginBinding, AUTH_APIKEY, AUTH_NOOP, AUTH_OAUTH2_CC, AUTH_OAUTH2_CC_BASIC,
    GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID,
};
use crate::error::{ErrorKind, OagwError};

/// In-flight proxy request the plugin chain operates on.
#[derive(Debug, Clone, Default)]
pub struct PluginRequest {
    /// Outbound request headers.
    pub headers: HeaderMap,
    /// Request metadata exposed to plugins.
    pub meta: BTreeMap<String, String>,
    /// Secrets resolved for this request, keyed by `secret_ref`.
    pub secrets: BTreeMap<String, String>,
}

/// In-flight upstream response the plugin chain operates on.
#[derive(Debug, Clone, Default)]
pub struct PluginResponse {
    /// Response headers.
    pub headers: HeaderMap,
    /// Response metadata exposed to plugins.
    pub meta: BTreeMap<String, String>,
}

/// Stage at which a plugin runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Before the upstream call.
    Request,
    /// After a successful upstream response.
    Response,
    /// After the upstream call failed.
    Error,
}

/// Outbound authentication plugin.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Plugin identifier this implementation resolves.
    fn id(&self) -> &'static str;

    /// Inject credentials into the outbound request.
    ///
    /// # Errors
    /// Returns `AuthenticationFailed` or `SecretNotFound` when credentials
    /// cannot be prepared. Credential material MUST NOT appear in the error
    /// detail, logs or API responses.
    async fn authenticate(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        request: &mut PluginRequest,
    ) -> Result<(), OagwError>;
}

/// Request / response guard plugin.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Plugin identifier this implementation resolves.
    fn id(&self) -> &'static str;

    /// Validate the outbound request; returning `Err` rejects the request.
    ///
    /// # Errors
    /// Returns a validation error carrying the offending header names.
    async fn guard_request(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        request: &PluginRequest,
    ) -> Result<(), OagwError> {
        let _ = (config, request);
        Ok(())
    }

    /// Validate the upstream response before it reaches the client.
    ///
    /// # Errors
    /// Returns a `DownstreamError` carrying the offending header names.
    async fn guard_response(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        response: &PluginResponse,
    ) -> Result<(), OagwError> {
        let _ = (config, response);
        Ok(())
    }
}

/// Header / body transformation plugin.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Plugin identifier this implementation resolves.
    fn id(&self) -> &'static str;

    /// Mutate the outbound request.
    ///
    /// # Errors
    /// Returns an error when the transformation cannot be applied.
    async fn transform_request(
        &self,
        request: &mut PluginRequest,
    ) -> Result<(), OagwError> {
        let _ = request;
        Ok(())
    }

    /// Mutate the upstream response.
    ///
    /// # Errors
    /// Returns an error when the transformation cannot be applied.
    async fn transform_response(&self, response: &mut PluginResponse) -> Result<(), OagwError> {
        let _ = response;
        Ok(())
    }

    /// Mutate the error path (typically to add correlation headers).
    ///
    /// # Errors
    /// Returns an error when the transformation cannot be applied.
    async fn transform_error(&self, request: &mut PluginRequest) -> Result<(), OagwError> {
        let _ = request;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Built-in auth plugins
// ---------------------------------------------------------------------------

/// `noop.v1` — no credential injection.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_NOOP
    }

    async fn authenticate(
        &self,
        _config: &BTreeMap<String, serde_json::Value>,
        _request: &mut PluginRequest,
    ) -> Result<(), OagwError> {
        Ok(())
    }
}

/// `apikey.v1` — injects an API key header from the credential store.
pub struct ApiKeyAuthPlugin;

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_APIKEY
    }

    async fn authenticate(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        request: &mut PluginRequest,
    ) -> Result<(), OagwError> {
        let secret_ref = config_str(config, "secret_ref").ok_or_else(|| {
            OagwError::new(
                ErrorKind::ValidationError,
                "auth plugin 'apikey' requires config.secret_ref",
            )
        })?;
        let header_name = config_str(config, "header").unwrap_or("x-api-key");
        let key = request.secrets.get(secret_ref).ok_or_else(|| {
            OagwError::new(
                ErrorKind::SecretNotFound,
                "credential referenced by auth.secret_ref is not available to this tenant",
            )
        })?;
        let (name, value) = (
            HeaderName::from_bytes(header_name.as_bytes())
                .map_err(|_| OagwError::new(ErrorKind::ValidationError, "auth plugin header name is invalid"))?,
            HeaderValue::from_str(key).map_err(|_| {
                OagwError::new(
                    ErrorKind::AuthenticationFailed,
                    "credential material cannot be carried in an HTTP header",
                )
            })?,
        );
        request.headers.insert(name, value);
        Ok(())
    }
}

/// `oauth2_client_cred.v1` — injects a bearer token via a form POST token
/// endpoint.
pub struct OAuth2ClientCredentialsPlugin {
    /// Token fetcher used to obtain access tokens.
    pub fetcher: Arc<dyn oauth2::TokenFetcher>,
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredentialsPlugin {
    fn id(&self) -> &'static str {
        AUTH_OAUTH2_CC
    }

    async fn authenticate(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        request: &mut PluginRequest,
    ) -> Result<(), OagwError> {
        let token = crate::infra::plugins::oauth2::obtain(self.fetcher.as_ref(), config, request, false).await?;
        request.headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                OagwError::new(
                    ErrorKind::AuthenticationFailed,
                    "obtained token cannot be carried in an HTTP header",
                )
            })?,
        );
        Ok(())
    }
}

/// `oauth2_client_cred_basic.v1` — client credentials with HTTP Basic
/// client authentication.
pub struct OAuth2ClientCredentialsBasicPlugin {
    /// Token fetcher used to obtain access tokens.
    pub fetcher: Arc<dyn oauth2::TokenFetcher>,
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredentialsBasicPlugin {
    fn id(&self) -> &'static str {
        AUTH_OAUTH2_CC_BASIC
    }

    async fn authenticate(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        request: &mut PluginRequest,
    ) -> Result<(), OagwError> {
        let token = crate::infra::plugins::oauth2::obtain(self.fetcher.as_ref(), config, request, true).await?;
        request.headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                OagwError::new(
                    ErrorKind::AuthenticationFailed,
                    "obtained token cannot be carried in an HTTP header",
                )
            })?,
        );
        Ok(())
    }
}

/// Shared client-credentials plumbing for both OAuth2 plugin variants.
pub(crate) mod oauth2 {
    use super::*;

    /// Token source abstraction (kept injectable for tests).
    #[async_trait]
    pub trait TokenFetcher: Send + Sync {
        /// Fetch an access token for the given client configuration.
        ///
        /// # Errors
        /// Returns `AuthenticationFailed` when the token endpoint rejects the
        /// client or is unreachable.
        async fn fetch(
            &self,
            token_url: &str,
            client_id: &str,
            client_secret: &str,
            scope: Option<&str>,
            use_basic: bool,
        ) -> Result<String, OagwError>;
    }

    /// Fetch a cached-or-fresh access token.
    pub async fn obtain(
        fetcher: &dyn TokenFetcher,
        config: &BTreeMap<String, serde_json::Value>,
        request: &PluginRequest,
        use_basic: bool,
    ) -> Result<String, OagwError> {
        let token_url = config_str(config, "token_url").ok_or_else(|| {
            OagwError::new(
                ErrorKind::ValidationError,
                "auth plugin 'oauth2_client_cred' requires config.token_url",
            )
        })?;
        let client_id = config_str(config, "client_id").ok_or_else(|| {
            OagwError::new(
                ErrorKind::ValidationError,
                "auth plugin 'oauth2_client_cred' requires config.client_id",
            )
        })?;
        let secret_ref = config_str(config, "secret_ref").ok_or_else(|| {
            OagwError::new(
                ErrorKind::ValidationError,
                "auth plugin 'oauth2_client_cred' requires config.secret_ref",
            )
        })?;
        let client_secret = request.secrets.get(secret_ref).ok_or_else(|| {
            OagwError::new(
                ErrorKind::SecretNotFound,
                "credential referenced by auth.secret_ref is not available to this tenant",
            )
        })?;
        fetcher
            .fetch(token_url, client_id, client_secret, config_str(config, "scope"), use_basic)
            .await
    }
}

/// Read a string from a plugin config map.
fn config_str<'a>(
    config: &'a BTreeMap<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    config.get(key).and_then(serde_json::Value::as_str).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Built-in guard plugins
// ---------------------------------------------------------------------------

/// `required_headers.v1` guard (ADR-0009).
pub struct RequiredHeadersGuard;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuard {
    fn id(&self) -> &'static str {
        GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        request: &PluginRequest,
    ) -> Result<(), OagwError> {
        let Some(spec) = config_str(config, "required_request_headers") else {
            return Ok(());
        };
        let missing: Vec<String> = spec
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .filter(|name| {
                HeaderName::from_bytes(name.as_bytes())
                    .map(|n| request.headers.get(&n).is_none())
                    .unwrap_or(false)
            })
            .map(str::to_owned)
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(OagwError::new(
                ErrorKind::ValidationError,
                format!("required request headers missing: {}", missing.join(", ")),
            )
            .with_ext("fields", serde_json::Value::from(missing)))
        }
    }

    async fn guard_response(
        &self,
        config: &BTreeMap<String, serde_json::Value>,
        response: &PluginResponse,
    ) -> Result<(), OagwError> {
        let Some(spec) = config_str(config, "required_response_headers") else {
            return Ok(());
        };
        let missing: Vec<String> = spec
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .filter(|name| {
                HeaderName::from_bytes(name.as_bytes())
                    .map(|n| response.headers.get(&n).is_none())
                    .unwrap_or(false)
            })
            .map(str::to_owned)
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(OagwError::new(
                ErrorKind::DownstreamError,
                format!(
                    "upstream response is missing required headers: {}",
                    missing.join(", ")
                ),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in transform plugins
// ---------------------------------------------------------------------------

/// `request_id.v1` transform (ADR-0002): inject / propagate `X-Request-ID`.
pub struct RequestIdTransform;

#[async_trait]
impl TransformPlugin for RequestIdTransform {
    fn id(&self) -> &'static str {
        TRANSFORM_REQUEST_ID
    }

    async fn transform_request(&self, request: &mut PluginRequest) -> Result<(), OagwError> {
        let existing = request
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let id = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
        request
            .meta
            .insert("request_id".to_owned(), id.clone());
        if let Ok(value) = HeaderValue::from_str(&id) {
            request.headers.insert("x-request-id", value);
        }
        Ok(())
    }

    async fn transform_response(&self, response: &mut PluginResponse) -> Result<(), OagwError> {
        if let Some(Ok(value)) = response.meta.get("request_id").map(|id| HeaderValue::from_str(id)) {
            response.headers.insert("x-request-id", value);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Registry of built-in plugin implementations plus custom plugin lookup.
pub struct PluginRegistry {
    auth: Vec<Arc<dyn AuthPlugin>>,
    guards: Vec<Arc<dyn GuardPlugin>>,
    transforms: Vec<Arc<dyn TransformPlugin>>,
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl PluginRegistry {
    /// Registry containing only the built-in plugins.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            auth: vec![
                Arc::new(NoopAuthPlugin),
                Arc::new(ApiKeyAuthPlugin),
            ],
            guards: vec![Arc::new(RequiredHeadersGuard)],
            transforms: vec![Arc::new(RequestIdTransform)],
        }
    }

    /// Build a registry with explicit OAuth2 token fetcher wiring.
    #[must_use]
    pub fn with_oauth2(fetcher: Arc<dyn oauth2::TokenFetcher>) -> Self {
        Self {
            auth: vec![
                Arc::new(NoopAuthPlugin),
                Arc::new(ApiKeyAuthPlugin),
                Arc::new(OAuth2ClientCredentialsPlugin { fetcher: fetcher.clone() }),
                Arc::new(OAuth2ClientCredentialsBasicPlugin { fetcher }),
            ],
            guards: vec![Arc::new(RequiredHeadersGuard)],
            transforms: vec![Arc::new(RequestIdTransform)],
        }
    }

    /// Resolve an auth plugin by identifier.
    #[must_use]
    pub fn auth(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.auth.iter().find(|p| p.id() == id).cloned()
    }

    /// Resolve a guard plugin by identifier.
    #[must_use]
    pub fn guard(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.guards.iter().find(|p| p.id() == id).cloned()
    }

    /// Resolve a transform plugin by identifier.
    #[must_use]
    pub fn transform(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.transforms.iter().find(|p| p.id() == id).cloned()
    }

    /// `true` when `id` is a known built-in plugin identifier of any kind.
    #[must_use]
    pub fn is_builtin(&self, id: &str) -> bool {
        self.auth(id).is_some() || self.guard(id).is_some() || self.transform(id).is_some()
    }
}

/// A single resolved plugin in an execution chain.
#[derive(Debug, Clone)]
pub enum ChainEntry {
    /// Outbound authentication.
    Auth {
        /// Plugin identifier.
        id: String,
        /// Plugin configuration.
        config: BTreeMap<String, serde_json::Value>,
    },
    /// Guard.
    Guard {
        /// Plugin identifier.
        id: String,
        /// Plugin configuration.
        config: BTreeMap<String, serde_json::Value>,
    },
    /// Transformation.
    Transform {
        /// Plugin identifier.
        id: String,
        /// Plugin configuration.
        config: BTreeMap<String, serde_json::Value>,
    },
}

impl ChainEntry {
    /// Resolve a [`PluginBinding`] into a typed chain entry.
    ///
    /// Returns `Err(PluginNotFound)` for identifiers the registry cannot
    /// resolve (catalog-only identifiers such as `basic`, `bearer`,
    /// `timeout`, `cors`, `logging`, `metrics`).
    pub fn resolve(
        registry: &PluginRegistry,
        binding: &PluginBinding,
    ) -> Result<Self, OagwError> {
        let id = binding.plugin_ref.clone();
        if registry.auth(&id).is_some() {
            return Ok(Self::Auth { id, config: binding.config.clone() });
        }
        if registry.guard(&id).is_some() {
            return Ok(Self::Guard { id, config: binding.config.clone() });
        }
        if registry.transform(&id).is_some() {
            return Ok(Self::Transform { id, config: binding.config.clone() });
        }
        Err(OagwError::new(
            ErrorKind::PluginNotFound,
            format!("plugin '{id}' is not available in the plugin registry"),
        ))
    }
}

/// Ordered plugin chain for one request.
#[derive(Debug, Clone, Default)]
pub struct PluginChain {
    /// Auth plugins, in order.
    pub auth: Vec<ChainEntry>,
    /// Guard plugins, in order.
    pub guards: Vec<ChainEntry>,
    /// Transform plugins, in order.
    pub transforms: Vec<ChainEntry>,
}

impl PluginChain {
    /// Build the execution chain from upstream + route bindings.
    ///
    /// Upstream plugins run before route plugins.
    #[must_use]
    pub fn build(
        registry: &PluginRegistry,
        upstream: &[PluginBinding],
        route: &[PluginBinding],
    ) -> Self {
        let mut chain = Self::default();
        for binding in upstream.iter().chain(route.iter()) {
            match ChainEntry::resolve(registry, binding) {
                Ok(entry) => match entry {
                    ChainEntry::Auth { .. } => chain.auth.push(entry),
                    ChainEntry::Guard { .. } => chain.guards.push(entry),
                    ChainEntry::Transform { .. } => chain.transforms.push(entry),
                },
                Err(err) => {
                    // Unresolvable bindings fail the request explicitly rather
                    // than silently dropping a plugin the operator configured.
                    tracing::warn!(plugin = %binding.plugin_ref, error = %err, "plugin unresolvable");
                }
            }
        }
        chain
    }

    /// Flatten to `(kind, id, config)` triples in execution order.
    #[must_use]
    pub fn entries(&self) -> Vec<&ChainEntry> {
        self.auth
            .iter()
            .chain(self.guards.iter())
            .chain(self.transforms.iter())
            .collect()
    }

    /// `true` when no plugin is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.auth.is_empty() && self.guards.is_empty() && self.transforms.is_empty()
    }
}

/// Run the auth + guard + request-transform phases.
///
/// Auth plugins only run when explicitly configured through
/// `Upstream::auth`; guard and transform plugins come from the bound chain.
pub(crate) async fn execute_request(
    registry: &PluginRegistry,
    chain: &PluginChain,
    request: &mut PluginRequest,
    auth: Option<&crate::domain::model::AuthConfig>,
) -> Result<(), OagwError> {
    if let Some(auth_cfg) = auth {
        authenticate_request(registry, auth_cfg, request).await?;
    }
    for entry in &chain.guards {
        let ChainEntry::Guard { id, config } = entry else {
            continue;
        };
        if let Some(plugin) = registry.guard(id) {
            plugin.guard_request(config, request).await?;
        }
    }
    for entry in &chain.transforms {
        let ChainEntry::Transform { id, .. } = entry else {
            continue;
        };
        if let Some(plugin) = registry.transform(id) {
            plugin.transform_request(request).await?;
        }
    }
    Ok(())
}

/// Run the configured outbound auth plugin for one request.
///
/// A no-op when the auth block names no plugin.
async fn authenticate_request(
    registry: &PluginRegistry,
    auth_cfg: &crate::domain::model::AuthConfig,
    request: &mut PluginRequest,
) -> Result<(), OagwError> {
    let Some(id) = auth_cfg.plugin_id() else {
        return Ok(());
    };
    let plugin = registry.auth(id).ok_or_else(|| {
        OagwError::new(
            ErrorKind::PluginNotFound,
            format!("auth plugin '{id}' is not available in the plugin registry"),
        )
    })?;
    plugin.authenticate(&auth_cfg.config, request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_resolves_named_plugins() {
        let r = PluginRegistry::builtin();
        assert!(r.auth(AUTH_NOOP).is_some());
        assert!(r.auth(AUTH_APIKEY).is_some());
        assert!(r.guard(GUARD_REQUIRED_HEADERS).is_some());
        assert!(r.transform(TRANSFORM_REQUEST_ID).is_some());
    }

    #[test]
    fn catalog_only_identifiers_do_not_resolve() {
        let r = PluginRegistry::builtin();
        assert!(r.auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1").is_none());
        assert!(r.auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1").is_none());
        assert!(r.guard("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1").is_none());
        assert!(r.guard("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1").is_none());
        assert!(r.transform("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1").is_none());
        assert!(r.transform("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1").is_none());
    }

    #[test]
    fn chain_orders_upstream_before_route() {
        let r = PluginRegistry::builtin();
        let upstream = [PluginBinding {
            plugin_ref: TRANSFORM_REQUEST_ID.to_owned(),
            plugin_uuid: None,
            config: Default::default(),
        }];
        let route = [PluginBinding {
            plugin_ref: GUARD_REQUIRED_HEADERS.to_owned(),
            plugin_uuid: None,
            config: Default::default(),
        }];
        let chain = PluginChain::build(&r, &upstream, &route);
        assert_eq!(chain.transforms.len(), 1);
        assert_eq!(chain.guards.len(), 1);
    }

    #[test]
    fn apikey_requires_secret_ref() {
        let plugin = ApiKeyAuthPlugin;
        let mut req = PluginRequest::default();
        let err = tokio_block(plugin.authenticate(&Default::default(), &mut req)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ValidationError);
    }

    #[test]
    fn apikey_missing_secret_is_secret_not_found() {
        let plugin = ApiKeyAuthPlugin;
        let mut config = BTreeMap::new();
        config.insert("secret_ref".to_owned(), serde_json::Value::from("cred://k"));
        let mut req = PluginRequest::default();
        let err = tokio_block(plugin.authenticate(&config, &mut req)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::SecretNotFound);
    }

    #[test]
    fn apikey_injects_header_without_leaking() {
        let plugin = ApiKeyAuthPlugin;
        let mut config = BTreeMap::new();
        config.insert("secret_ref".to_owned(), serde_json::Value::from("cred://k"));
        let mut req = PluginRequest::default();
        req.secrets.insert("cred://k".to_owned(), "super-secret".to_owned());
        tokio_block(plugin.authenticate(&config, &mut req)).unwrap();
        assert_eq!(req.headers.get("x-api-key").unwrap(), "super-secret");
    }

    #[test]
    fn required_headers_guard_400_on_missing() {
        let plugin = RequiredHeadersGuard;
        let mut config = BTreeMap::new();
        config.insert(
            "required_request_headers".to_owned(),
            serde_json::Value::from("x-required"),
        );
        let req = PluginRequest::default();
        let err = tokio_block(plugin.guard_request(&config, &req)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ValidationError);
        assert_eq!(err.kind.status(), http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn required_headers_guard_502_on_missing_response() {
        let plugin = RequiredHeadersGuard;
        let mut config = BTreeMap::new();
        config.insert(
            "required_response_headers".to_owned(),
            serde_json::Value::from("x-must-exist"),
        );
        let resp = PluginResponse::default();
        let err = tokio_block(plugin.guard_response(&config, &resp)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::DownstreamError);
        assert_eq!(err.kind.status(), http::StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn request_id_transform_injects_when_absent() {
        let t = RequestIdTransform;
        let mut req = PluginRequest::default();
        tokio_block(t.transform_request(&mut req)).unwrap();
        assert!(req.meta.contains_key("request_id"));
        assert!(req.headers.get("x-request-id").is_some());
    }

    #[test]
    fn request_id_transform_propagates_when_present() {
        let t = RequestIdTransform;
        let mut req = PluginRequest::default();
        req.headers
            .insert("x-request-id", HeaderValue::from_static("client-1"));
        tokio_block(t.transform_request(&mut req)).unwrap();
        assert_eq!(req.meta.get("request_id").unwrap(), "client-1");
    }

    #[test]
    fn noop_auth_is_a_no_op() {
        let mut req = PluginRequest::default();
        tokio_block(NoopAuthPlugin.authenticate(&Default::default(), &mut req)).unwrap();
        assert!(req.headers.is_empty());
    }

    fn tokio_block<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fut)
    }
}
