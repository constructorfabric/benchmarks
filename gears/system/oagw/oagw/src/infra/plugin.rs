//! Built-in plugins and the plugin type catalogue.

use async_trait::async_trait;
use toolkit_auth::oauth2::SecretString;

use crate::domain::ids;
use crate::domain::plugin::{
    AuthPlugin, GuardDecision, GuardPlugin, PluginError, PluginRegistries, RequestContext,
    TransformPlugin,
};

/// Every catalogued OAGW plugin identifier.
///
/// `basic` and `bearer` (auth), `timeout` and `cors` (guard) and `logging` and
/// `metrics` (transform) are catalogued in the types registry but have no
/// backing implementation: binding any of them fails with `400`.
pub const CATALOG_ONLY_PLUGINS: &[&str] = &[
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
];

/// Builds a plugin GTS identifier for a built-in name.
#[must_use]
pub fn builtin_auth_id(name: &str) -> String {
    format!("{}cf.core.oagw.{name}.v1", ids::AUTH_PLUGIN_TYPE_ID)
}

/// Builds a built-in guard plugin identifier.
#[must_use]
pub fn builtin_guard_id(name: &str) -> String {
    format!("{}cf.core.oagw.{name}.v1", ids::GUARD_PLUGIN_TYPE_ID)
}

/// Builds a built-in transform plugin identifier.
#[must_use]
pub fn builtin_transform_id(name: &str) -> String {
    format!("{}cf.core.oagw.{name}.v1", ids::TRANSFORM_PLUGIN_TYPE_ID)
}

/// True when `plugin_ref` is catalogued but not implemented.
#[must_use]
pub fn is_catalog_only(plugin_ref: &str) -> bool {
    CATALOG_ONLY_PLUGINS.contains(&plugin_ref)
}

/// Reads a string member out of a plugin config object.
#[must_use]
pub fn config_string(config: &serde_json::Value, key: &str) -> Option<String> {
    config.get(key).and_then(serde_json::Value::as_str).map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Auth plugins
// ---------------------------------------------------------------------------

/// The `noop` auth plugin: injects nothing.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        "cf.core.oagw.noop.v1"
    }

    fn name(&self) -> &'static str {
        "noop"
    }

    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        _config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        ctx.record("auth:noop");
        Ok(())
    }
}

/// The `apikey` auth plugin: injects a resolved secret into a header or query
/// parameter.
pub struct ApiKeyAuthPlugin {
    resolver: crate::infra::secret::SecretResolver,
}

impl ApiKeyAuthPlugin {
    /// Creates the plugin.
    #[must_use]
    pub fn new(resolver: crate::infra::secret::SecretResolver) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        "cf.core.oagw.apikey.v1"
    }

    fn name(&self) -> &'static str {
        "apikey"
    }

    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        let Some(secret_ref) = config_string(config, "secret_ref") else {
            return Err(PluginError::AuthFailed(
                "auth.config.secret_ref is required for the apikey plugin".to_owned(),
            ));
        };
        let secret = self
            .resolver
            .resolve(ctx, &secret_ref)
            .await
            .map_err(|error| PluginError::SecretNotFound(error.0))?;

        if let Some(header) = config_string(config, "header") {
            ctx.credential = Some(crate::domain::plugin::Credential::Header(header, secret));
            ctx.record("auth:apikey:header");
            return Ok(());
        }
        if let Some(query) = config_string(config, "query") {
            ctx.credential = Some(crate::domain::plugin::Credential::Query(query, secret));
            ctx.record("auth:apikey:query");
            return Ok(());
        }
        Err(PluginError::AuthFailed(
            "auth.config requires either 'header' or 'query' for the apikey plugin".to_owned(),
        ))
    }
}

/// How the `OAuth2` plugin transmits the client credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// Credentials in the request body.
    Form,
    /// Credentials in an `Authorization: Basic` header.
    Basic,
}

impl ClientAuthMethod {
    fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }

    fn to_toolkit(self) -> toolkit_auth::oauth2::ClientAuthMethod {
        match self {
            Self::Form => toolkit_auth::oauth2::ClientAuthMethod::Form,
            Self::Basic => toolkit_auth::oauth2::ClientAuthMethod::Basic,
        }
    }
}

/// A cached `OAuth2` access token, carrying its cache key so a hash collision
/// cannot hand one tenant another tenant's token.
#[derive(Clone)]
pub struct CachedToken {
    /// The cache key the token was stored under.
    pub key: String,
    /// The access token.
    pub token: SecretString,
}

/// The `OAuth2` client-credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    resolver: crate::infra::secret::SecretResolver,
    auth_method: ClientAuthMethod,
    cache: std::sync::Arc<pingora_memory_cache::MemoryCache<String, CachedToken>>,
    cache_ttl: std::time::Duration,
}

/// Safety margin subtracted from the `IdP`-reported token lifetime.
const TOKEN_TTL_MARGIN_SECS: u64 = 30;

impl OAuth2ClientCredAuthPlugin {
    /// Creates the plugin.
    #[must_use]
    pub fn new(
        resolver: crate::infra::secret::SecretResolver,
        auth_method: ClientAuthMethod,
        cache_ttl: std::time::Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            resolver,
            auth_method,
            cache: std::sync::Arc::new(pingora_memory_cache::MemoryCache::new(
                cache_capacity.max(1),
            )),
            cache_ttl,
        }
    }

    fn cache_key(&self, ctx: &RequestContext, config: &serde_json::Value) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.subject_tenant_id,
            ctx.subject_id,
            self.auth_method.tag(),
            crate::infra::secret::hash_config(config)
        )
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => "cf.core.oagw.oauth2_client_cred.v1",
            ClientAuthMethod::Basic => "cf.core.oagw.oauth2_client_cred_basic.v1",
        }
    }

    fn name(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => "oauth2_client_cred",
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
        }
    }

    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        let key = self.cache_key(ctx, config);
        if let (Some(cached), status) = self.cache.get(&key)
            && cached.key == key
        {
            tracing::debug!(status = status.as_str(), "oauth2 token cache hit");
            ctx.credential = Some(crate::domain::plugin::Credential::Header(
                "authorization".to_owned(),
                format!("Bearer {}", cached.token.expose()),
            ));
            ctx.record("auth:oauth2:cache-hit");
            return Ok(());
        }

        let client_id_ref = config_string(config, "client_id_ref").ok_or_else(|| {
            PluginError::AuthFailed("auth.config.client_id_ref is required".to_owned())
        })?;
        let client_secret_ref = config_string(config, "client_secret_ref").ok_or_else(|| {
            PluginError::AuthFailed("auth.config.client_secret_ref is required".to_owned())
        })?;
        let has_token_endpoint = config.get("token_endpoint").is_some();
        let token_endpoint = config_string(config, "token_endpoint");
        let issuer_url = config_string(config, "issuer_url");

        let client_id = self
            .resolver
            .resolve(ctx, &client_id_ref)
            .await
            .map_err(|error| PluginError::SecretNotFound(error.0))?;
        let client_secret = self
            .resolver
            .resolve(ctx, &client_secret_ref)
            .await
            .map_err(|error| PluginError::SecretNotFound(error.0))?;

        let url = if let Some(endpoint) = token_endpoint {
            endpoint
        } else if let Some(issuer) = issuer_url {
            format!("{issuer}/.well-known/openid-configuration")
        } else {
            return Err(PluginError::AuthFailed(
                "auth.config requires token_endpoint or issuer_url".to_owned(),
            ));
        };

        let parsed = url::Url::parse(&url)
            .map_err(|error| PluginError::AuthFailed(format!("invalid token endpoint: {error}")))?;

        let oauth_config = toolkit_auth::oauth2::OAuthClientConfig {
            token_endpoint: has_token_endpoint.then_some(parsed.clone()),
            issuer_url: (!has_token_endpoint).then_some(parsed),
            client_id: client_id.clone(),
            client_secret: SecretString::new(client_secret),
            scopes: config
                .get("scopes")
                .and_then(serde_json::Value::as_str)
                .map(|scopes| scopes.split(' ').map(str::to_owned).collect())
                .unwrap_or_default(),
            auth_method: self.auth_method.to_toolkit(),
            extra_headers: Vec::new(),
            refresh_offset: std::time::Duration::from_mins(30),
            jitter_max: std::time::Duration::from_mins(5),
            min_refresh_period: std::time::Duration::from_secs(10),
            default_ttl: std::time::Duration::from_mins(5),
            http_config: Some(toolkit_http::HttpClientConfig::token_endpoint()),
        };

        let fetched = toolkit_auth::oauth2::fetch_token(oauth_config)
            .await
            .map_err(|error| {
                tracing::debug!(error = %error, "oauth2 token exchange failed");
                PluginError::AuthFailed("oauth2 token exchange failed".to_owned())
            })?;

        let ttl = token_ttl(self.cache_ttl, fetched.expires_in);
        let token = fetched.bearer;
        self.cache.put(&key, CachedToken { key: key.clone(), token: token.clone() }, Some(ttl));

        ctx.credential = Some(crate::domain::plugin::Credential::Header(
            "authorization".to_owned(),
            format!("Bearer {}", token.expose()),
        ));
        ctx.record("auth:oauth2:fetched");
        Ok(())
    }
}

/// Cache lifetime for a fetched token: the configured floor, never past the
/// `IdP`-reported expiry minus a safety margin.
#[must_use]
fn token_ttl(config_ttl: std::time::Duration, expires_in: std::time::Duration) -> std::time::Duration {
    config_ttl.min(expires_in.saturating_sub(std::time::Duration::from_secs(TOKEN_TTL_MARGIN_SECS)))
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

/// The `required_headers` guard: enforces header presence in the request and
/// response phases independently.
pub struct RequiredHeadersGuard;

/// Parses a comma-separated header list, dropping blank entries.
#[must_use]
pub fn parse_header_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuard {
    fn id(&self) -> &'static str {
        "cf.core.oagw.required_headers.v1"
    }

    fn name(&self) -> &'static str {
        "required_headers"
    }

    async fn guard_request(
        &self,
        ctx: &RequestContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, PluginError> {
        let required = parse_header_list(config_string(config, "required_request_headers").as_deref());
        for name in required {
            let missing = ctx
                .header(&name)
                .is_none_or(|value| value.trim().is_empty());
            if missing {
                return Ok(GuardDecision::Reject(PluginError::guard(
                    400,
                    ids::ERR_REQUIRED_HEADER,
                    "REQUIRED_HEADER_MISSING",
                    format!("required request header '{name}' is missing"),
                )));
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(
        &self,
        ctx: &RequestContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, PluginError> {
        let required = parse_header_list(config_string(config, "required_response_headers").as_deref());
        for name in required {
            let lowered = name.to_ascii_lowercase();
            let present = ctx
                .response_headers
                .iter()
                .any(|(key, value)| *key == lowered && !value.trim().is_empty());
            if !present {
                return Ok(GuardDecision::Reject(PluginError::guard(
                    502,
                    ids::ERR_REQUIRED_HEADER,
                    "REQUIRED_HEADER_MISSING",
                    format!("required response header '{name}' is missing"),
                )));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

// ---------------------------------------------------------------------------
// Transforms
// ---------------------------------------------------------------------------

/// The `request_id` transform plugin: propagates or generates `X-Request-ID`.
pub struct RequestIdTransformPlugin;

/// Header carrying the correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        "cf.core.oagw.request_id.v1"
    }

    fn name(&self) -> &'static str {
        "request_id"
    }

    async fn transform_request(
        &self,
        ctx: &mut RequestContext,
        _config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        let existing = ctx.header(REQUEST_ID_HEADER).map(str::to_owned);
        let request_id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        ctx.set_header(REQUEST_ID_HEADER, &request_id);
        ctx.request_id = Some(request_id);
        ctx.record("transform:request_id:request");
        Ok(())
    }

    async fn transform_response(
        &self,
        ctx: &mut RequestContext,
        _config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        if let Some(request_id) = ctx.request_id.clone() {
            ctx.response_headers.push((REQUEST_ID_HEADER.to_owned(), request_id));
        }
        ctx.record("transform:request_id:response");
        Ok(())
    }
}

/// Registers every built-in plugin into the registries.
///
/// The two `OAuth2` variants share one token cache; `resolver` is shared by
/// every credential-resolving plugin.
#[must_use]
pub fn builtin_registries(
    resolver: crate::infra::secret::SecretResolver,
    cache_ttl: std::time::Duration,
    cache_capacity: usize,
) -> PluginRegistries {
    let mut registries = PluginRegistries::default();
    registries
        .auth
        .register(std::sync::Arc::new(NoopAuthPlugin));
    registries
        .auth
        .register(std::sync::Arc::new(ApiKeyAuthPlugin::new(resolver.clone())));
    registries
        .auth
        .register(std::sync::Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver.clone(),
            ClientAuthMethod::Form,
            cache_ttl,
            cache_capacity,
        )));
    registries
        .auth
        .register(std::sync::Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver,
            ClientAuthMethod::Basic,
            cache_ttl,
            cache_capacity,
        )));
    registries
        .guard
        .register(std::sync::Arc::new(RequiredHeadersGuard));
    registries
        .transform
        .register(std::sync::Arc::new(RequestIdTransformPlugin));
    registries
}
