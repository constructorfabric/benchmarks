//! Built-in auth plugins: `noop`, `apikey`, and the two OAuth2
//! client-credentials variants (`docs/ADR/0002`, `docs/ADR/0008`).
//!
//! `basic`, `bearer`, `timeout`, `cors`, `logging` and `metrics` are
//! catalogued GTS identifiers with no implementation here — see
//! `crate::domain::services::management::BUILT_IN_PLUGINS`, which marks them
//! `bindable: false`.
use std::sync::Arc;

use async_trait::async_trait;
use http::header::AUTHORIZATION;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};

use crate::domain::plugin::{
    AuthPlugin, INTERNAL, PROTOCOL_ERROR, PluginError, RequestContext, ResolvedSecret,
    SECRET_NOT_FOUND, SecretError, SecretResolver, problem_type,
};

/// GTS type id of the built-in `noop` auth plugin.
pub const NOOP_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// GTS type id of the built-in `apikey` auth plugin.
pub const APIKEY_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// GTS type id of the built-in OAuth2 client-credentials (`Form`) plugin.
pub const OAUTH2_FORM_PLUGIN_TYPE: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// GTS type id of the built-in OAuth2 client-credentials (`Basic`) plugin.
pub const OAUTH2_BASIC_PLUGIN_TYPE: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Default request header the `apikey` plugin writes.
pub const APIKEY_DEFAULT_HEADER: &str = "x-api-key";

/// Forwards the request unauthenticated.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        NOOP_PLUGIN_TYPE
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Injects a static API key resolved from the credential store.
///
/// Effective configuration keys:
///
/// | Key | Default | Meaning |
/// |---|---|---|
/// | `key_ref` | required | `cred://` reference holding the key |
/// | `header_name` | `x-api-key` | Header to write; blank disables the header |
/// | `query_name` | *(unset)* | Query parameter to write; blank disables it |
/// | `prefix` | *(unset)* | Prefix prepended to the value (`Bearer ` &c.) |
pub struct ApiKeyAuthPlugin {
    secrets: Arc<dyn SecretResolver>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin")
            .field("secrets", &"<secret resolver>")
            .finish()
    }
}

impl ApiKeyAuthPlugin {
    /// Build the plugin over a secret resolver.
    #[must_use]
    pub fn new(secrets: Arc<dyn SecretResolver>) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        APIKEY_PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let reference = required_config(ctx, "key_ref", self.plugin_type())?.to_owned();
        let key = self
            .secrets
            .resolve(&reference)
            .await
            .map_err(|error| secret_failure(&reference, error))?
            .ok_or_else(|| missing_config("key_ref", self.plugin_type()))?;

        let prefix = ctx.config_str_or("prefix", "").to_owned();
        let value = format!("{prefix}{}", key.expose());
        let header_name = ctx
            .config_str_or("header_name", APIKEY_DEFAULT_HEADER)
            .to_owned();
        if !header_name.trim().is_empty() {
            ctx.set_header(header_name.trim(), value);
        }
        if let Some(query_name) = ctx
            .config_str("query_name")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let query_name = query_name.to_owned();
            let mut query = ctx.query.clone().unwrap_or_default();
            let mut serializer = form_urlencoded::Serializer::new(query);
            serializer.append_pair(&query_name, key.expose());
            query = serializer.finish();
            ctx.set_query(query);
        }
        ctx.set_attribute("oagw.auth", "apikey");
        Ok(())
    }
}

/// Shared token cache of the two OAuth2 client-credentials plugins.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: String,
}

/// Configuration of the OAuth2 token cache.
///
/// `OagwConfig` is a `deny_unknown_fields` document that carries no
/// `token_cache_*` section in this revision, so the gear runs with the
/// `docs/ADR/0008` defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached token's lifetime, in seconds.
    pub ttl_seconds: u64,
    /// Maximum number of cached entries.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: 300,
            capacity: 10_000,
        }
    }
}

/// Cache entry lifetime is `min(configured TTL, expires_in − margin)`.
const SAFETY_MARGIN_SECS: u64 = 30;

/// Acquires OAuth2 client-credentials tokens and caches them.
///
/// `auth_method` is the only difference between the two registered variants
/// (`docs/ADR/0008` — "Plugin Variants"): `Form` puts the credentials in the
/// request body, `Basic` in the `Authorization` header. Both share one cache
/// configuration but key it by the auth method, so a `Form` and a `Basic`
/// binding with identical config never collide.
pub struct OAuth2ClientCredAuthPlugin {
    secrets: Arc<dyn SecretResolver>,
    auth_method: ClientAuthMethod,
    cache: Arc<MemoryCache<String, CachedToken>>,
    cache_ttl_seconds: u64,
    plugin_type: &'static str,
    plugin_id: &'static str,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build one variant over a secret resolver.
    #[must_use]
    pub fn new(
        secrets: Arc<dyn SecretResolver>,
        auth_method: ClientAuthMethod,
        cache: TokenCacheConfig,
    ) -> Self {
        let (plugin_type, plugin_id) = match auth_method {
            ClientAuthMethod::Form => (OAUTH2_FORM_PLUGIN_TYPE, "oauth2_client_cred"),
            ClientAuthMethod::Basic => (OAUTH2_BASIC_PLUGIN_TYPE, "oauth2_client_cred_basic"),
        };
        Self {
            secrets,
            auth_method,
            cache: Arc::new(MemoryCache::new(cache.capacity)),
            cache_ttl_seconds: cache.ttl_seconds,
            plugin_type,
            plugin_id,
        }
    }

    /// `tenant:subject:auth_method:config_hash`, as `docs/ADR/0008` pins it.
    fn cache_key(&self, ctx: &RequestContext, config: &str) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.subject_tenant_id,
            ctx.subject_id,
            self.plugin_id,
            config_hash(config)
        )
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.plugin_id
    }

    fn plugin_type(&self) -> &str {
        self.plugin_type
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = OAuth2PluginConfig::from_context(ctx, self.plugin_id)?;
        let canonical = canonical_config(&ctx.config);
        let key = self.cache_key(ctx, &canonical);

        if let (Some(cached), _) = self.cache.get(&key)
            && cached.key == key
        {
            ctx.set_header(AUTHORIZATION.as_str(), cached.token);
            ctx.set_attribute("oagw.auth", "oauth2-cache");
            return Ok(());
        }

        let client_id = self
            .resolve(&config.client_id_ref)
            .await?
            .ok_or_else(|| missing_secret(&config.client_id_ref))?;
        let client_secret = self
            .resolve(&config.client_secret_ref)
            .await?
            .ok_or_else(|| missing_secret(&config.client_secret_ref))?;

        let fetched = fetch_token(OAuthClientConfig {
            token_endpoint: config.token_endpoint.clone(),
            issuer_url: config.issuer_url.clone(),
            client_id: client_id.expose().to_owned(),
            client_secret: SecretString::new(client_secret.expose()),
            scopes: config.scopes.clone(),
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: std::time::Duration::from_secs(0),
            jitter_max: std::time::Duration::from_secs(0),
            min_refresh_period: std::time::Duration::from_secs(0),
            default_ttl: std::time::Duration::from_secs(300),
            http_config: None,
        })
        .await
        .map_err(|error| protocol_failure(format!("oauth2 token exchange failed: {error}")))?;

        let token = format!("Bearer {}", fetched.bearer.expose());
        let ttl_seconds = fetched
            .expires_in
            .as_secs()
            .saturating_sub(SAFETY_MARGIN_SECS)
            .min(self.cache_ttl_seconds);
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: token.clone(),
            },
            Some(std::time::Duration::from_secs(ttl_seconds)),
        );

        ctx.set_header(AUTHORIZATION.as_str(), token);
        ctx.set_attribute("oagw.auth", self.plugin_id);
        Ok(())
    }
}

impl OAuth2ClientCredAuthPlugin {
    async fn resolve(&self, reference: &str) -> Result<Option<ResolvedSecret>, PluginError> {
        self.secrets
            .resolve(reference)
            .await
            .map_err(|error| secret_failure(reference, error))
    }
}

/// The plugin-side view of an OAuth2 plugin configuration.
struct OAuth2PluginConfig {
    token_endpoint: Option<url::Url>,
    issuer_url: Option<url::Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl OAuth2PluginConfig {
    fn from_context(ctx: &RequestContext, plugin_id: &str) -> Result<Self, PluginError> {
        let client_id_ref = ctx
            .config_str("client_id_ref")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing_config("client_id_ref", plugin_id))?;
        let client_secret_ref = ctx
            .config_str("client_secret_ref")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing_config("client_secret_ref", plugin_id))?;

        let token_endpoint = parse_endpoint(ctx, "token_endpoint", plugin_id)?;
        let issuer_url = parse_endpoint(ctx, "issuer_url", plugin_id)?;
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::new(
                500,
                problem_type(INTERNAL),
                format!(
                    "{plugin_id} plugin is misconfigured: exactly one of 'token_endpoint' or \
                     'issuer_url' is required"
                ),
            ));
        }
        let scopes = ctx
            .config_str("scopes")
            .map(|scopes| scopes.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();

        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref: client_id_ref.to_owned(),
            client_secret_ref: client_secret_ref.to_owned(),
            scopes,
        })
    }
}

fn parse_endpoint(
    ctx: &RequestContext,
    key: &str,
    plugin_id: &str,
) -> Result<Option<url::Url>, PluginError> {
    let Some(raw) = ctx.config_str(key).map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    url::Url::parse(raw).map(Some).map_err(|error| {
        protocol_failure(format!("{plugin_id} plugin: '{key}' is not a URL: {error}"))
    })
}

fn missing_config(key: &str, plugin_id: &str) -> PluginError {
    PluginError::new(
        500,
        problem_type(INTERNAL),
        format!("{plugin_id} plugin is misconfigured: '{key}' is required"),
    )
}

fn missing_secret(reference: &str) -> PluginError {
    PluginError::new(
        500,
        problem_type(SECRET_NOT_FOUND),
        format!("credential reference '{reference}' does not exist"),
    )
}

/// Map a credential-store failure onto a plugin error.
fn secret_failure(reference: &str, error: SecretError) -> PluginError {
    match error {
        SecretError::NotFound { .. } => missing_secret(reference),
        SecretError::Invalid { reason, .. } => PluginError::new(
            500,
            problem_type(INTERNAL),
            format!("credential reference '{reference}' is invalid: {reason}"),
        ),
        SecretError::Unavailable(detail) => PluginError::new(500, problem_type(INTERNAL), detail),
    }
}

/// A 502 for an upstream (here: identity-provider) protocol failure.
fn protocol_failure(detail: impl Into<String>) -> PluginError {
    PluginError::new(502, problem_type(PROTOCOL_ERROR), detail)
}

/// A non-blank configuration string, or a misconfiguration error.
fn required_config<'a>(
    ctx: &'a RequestContext,
    key: &'static str,
    plugin_id: &str,
) -> Result<&'a str, PluginError> {
    ctx.config_str(key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_config(key, plugin_id))
}

/// Canonical (sorted-key) JSON text of a configuration document, so that two
/// logically equal documents hash identically.
fn canonical_config(config: &serde_json::Value) -> String {
    match config {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            keys.into_iter()
                .map(|key| {
                    format!(
                        "{key}={}",
                        canonical_config(map.get(key).unwrap_or(&serde_json::Value::Null))
                    )
                })
                .collect::<Vec<_>>()
                .join("|")
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(canonical_config)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

/// Stable, dependency-free FNV-1a hash of the plugin configuration text.
fn config_hash(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
