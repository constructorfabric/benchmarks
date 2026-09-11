//! The built-in OAuth2 client-credentials auth plugins (ADR 0008,
//! `cpt-cf-oagw-dod-plugin-system-token-cache`).
//!
//! `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` (the
//! Form variant) and `gts.cf.core.oagw.auth_plugin.v1~
//! cf.core.oagw.oauth2_client_cred_basic.v1` (the Basic variant) are two
//! registered plugin identifiers that differ only in `auth_method` and share
//! one token cache.
//!
//! The cache is the in-process `pingora_memory_cache::MemoryCache` of
//! constraint 3 — no Redis, no second tier. Each entry carries the key it was
//! stored under, so a hit whose stored key does not match the lookup key is
//! treated as a miss and a hash collision can never serve another tenant's
//! token.

use std::sync::Arc;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{fetch_token, ClientAuthMethod, OAuthClientConfig};
use toolkit_http::HttpClientConfig;
use toolkit_security::SecurityContext;
use toolkit_auth::oauth2::SecretString;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError};
use crate::infra::plugin::credentials::{security_context_for, CredentialResolver};

/// The safety margin ADR 0008 subtracts from the IdP-reported expiry.
pub const EXPIRY_SAFETY_MARGIN_SECS: u64 = 30;

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1`
pub const OAUTH2_FORM_PLUGIN_TYPE: &str =
    crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID;
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1`
pub const OAUTH2_BASIC_PLUGIN_TYPE: &str =
    crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID;

/// One cached token entry: the token alongside the key it was stored under.
///
/// `inst-ps-key-6`: a hit whose stored key does not equal the lookup key is a
/// miss, so a hash collision can never serve another tenant's token.
///
/// The token is a `SecretString`, which zeroizes its buffer on drop
/// (`inst-ps-key-10`); `Debug` is derived from `SecretString`'s redacted
/// rendering, so no cached entry can be printed with its material.
#[derive(Clone)]
pub struct CachedToken {
    pub key: String,
    pub token: SecretString,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken").field("key", &self.key).field("token", &self.token).finish()
    }
}

/// The OAuth2 client-credentials auth plugin (ADR 0008).
///
/// One instance per registered identifier; the Form and the Basic instances
/// handed out by [`AuthPluginRegistry::with_builtins`] are built over the same
/// cache, so the two variants share one token cache by construction.
#[derive(Clone)]
pub struct OAuth2ClientCredAuthPlugin {
    plugin_type: &'static str,
    auth_method: ClientAuthMethod,
    resolver: CredentialResolver,
    cache: Arc<MemoryCache<String, CachedToken>>,
    cache_ttl: std::time::Duration,
    http_config: Option<HttpClientConfig>,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("plugin_type", &self.plugin_type)
            .field("auth_method", &self.auth_method)
            .field("cache_ttl", &self.cache_ttl)
            .finish()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// The in-process token cache the built-in registry builds once and hands
    /// to both OAuth2 variants (ADR 0008).
    #[must_use]
    pub fn new_token_cache(cache_config: TokenCacheConfig) -> Arc<MemoryCache<String, CachedToken>> {
        Arc::new(MemoryCache::new(cache_config.capacity.max(1)))
    }

    /// Build one variant over its own cache.
    #[must_use]
    pub fn new(
        plugin_type: &'static str,
        auth_method: ClientAuthMethod,
        resolver: CredentialResolver,
        cache_config: TokenCacheConfig,
    ) -> Self {
        Self::with_cache(
            plugin_type,
            auth_method,
            resolver,
            Self::new_token_cache(cache_config),
            cache_config,
            None,
        )
    }

    /// Build one variant over an existing cache, which is how the Form and the
    /// Basic variant share one token cache (ADR 0008).
    #[must_use]
    pub fn with_cache(
        plugin_type: &'static str,
        auth_method: ClientAuthMethod,
        resolver: CredentialResolver,
        cache: Arc<MemoryCache<String, CachedToken>>,
        cache_config: TokenCacheConfig,
        http_config: Option<HttpClientConfig>,
    ) -> Self {
        Self {
            plugin_type,
            auth_method,
            resolver,
            cache,
            cache_ttl: std::time::Duration::from_secs(cache_config.ttl_secs),
            http_config,
        }
    }

    /// The cache the plugin holds, so a test can observe the entries.
    #[must_use]
    pub fn cache(&self) -> &Arc<MemoryCache<String, CachedToken>> {
        &self.cache
    }

    /// The client-auth method the variant transmits its credentials with.
    #[must_use]
    pub const fn auth_method(&self) -> ClientAuthMethod {
        self.auth_method
    }

    /// `inst-ps-key-1` .. `-5`: the cache key is the subject tenant
    /// identifier, the subject identifier, the client-auth method tag, and a
    /// deterministic hash of every plugin configuration key and value taken in
    /// sorted order.
    #[must_use]
    pub fn cache_key(
        auth_method: ClientAuthMethod,
        tenant_id: Option<uuid::Uuid>,
        subject_id: Option<uuid::Uuid>,
        config: Option<&serde_json::Value>,
    ) -> String {
        let config_hash = crate::infra::plugin::oauth2_client_cred_auth::config_hash(config);
        format!(
            "{tenant}:{subject}:{method}:{config_hash}",
            tenant = tenant_id.map(|tenant| tenant.to_string()).unwrap_or_default(),
            subject = subject_id.map(|subject| subject.to_string()).unwrap_or_default(),
            method = auth_method_tag(auth_method),
        )
    }

    /// `inst-ps-key-7`: the entry TTL is the lesser of the configured
    /// `token_cache_ttl_secs` and the IdP-reported expiry reduced by the
    /// 30-second safety margin; an expiry at or under the margin leaves no
    /// usable lifetime and therefore no entry.
    #[must_use]
    pub fn cache_ttl_for(configured: std::time::Duration, expires_in: std::time::Duration) -> Option<std::time::Duration> {
        let margin = std::time::Duration::from_secs(EXPIRY_SAFETY_MARGIN_SECS);
        let usable = expires_in.checked_sub(margin)?;
        Some(configured.min(usable)).filter(|ttl| !ttl.is_zero())
    }

    /// Read the OAuth2 configuration out of the effective plugin
    /// configuration, requiring the two `cred://` references.
    ///
    /// # Errors
    ///
    /// [`PluginError::Internal`] when a required key is absent; the values are
    /// never echoed.
    pub fn required_references(
        config: Option<&serde_json::Value>,
    ) -> Result<(String, String), PluginError> {
        let read = |key: &str| {
            config
                .and_then(|config| config.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    PluginError::Internal(format!("the oauth2 client credentials configuration carries no `{key}`"))
                })
        };
        Ok((read("client_id_ref")?, read("client_secret_ref")?))
    }

    /// Assemble the `OAuthClientConfig` the exchange runs with.
    ///
    /// # Errors
    ///
    /// [`PluginError::Internal`] when an endpoint URL is malformed.
    pub fn token_endpoint_config(
        config: Option<&serde_json::Value>,
        client_id: String,
        client_secret: SecretString,
        auth_method: ClientAuthMethod,
    ) -> Result<OAuthClientConfig, PluginError> {
        let read_url = |key: &str| {
            config
                .and_then(|config| config.get(key))
                .and_then(serde_json::Value::as_str)
                .map(|value| url::Url::parse(value))
        };
        let token_endpoint = match read_url("token_endpoint") {
            Some(Ok(endpoint)) => Some(endpoint),
            Some(Err(_)) | None => None,
        };
        let issuer_url = match read_url("issuer_url") {
            Some(Ok(endpoint)) => Some(endpoint),
            Some(Err(_)) | None => None,
        };
        let scopes = config
            .and_then(|config| config.get("scopes"))
            .and_then(serde_json::Value::as_array)
            .map(|scopes| {
                scopes.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect()
            })
            .unwrap_or_default();
        Ok(OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id,
            client_secret,
            scopes,
            auth_method,
            extra_headers: Vec::new(),
            refresh_offset: std::time::Duration::from_secs(0),
            jitter_max: std::time::Duration::from_secs(0),
            min_refresh_period: std::time::Duration::from_secs(0),
            default_ttl: std::time::Duration::from_secs(300),
            http_config: None,
        })
    }
}

/// The cache-key method component: the Form and Basic variants never collide
/// even when their configuration is otherwise identical (`inst-ps-key-4`).
#[must_use]
pub fn auth_method_tag(auth_method: ClientAuthMethod) -> &'static str {
    match auth_method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

/// A deterministic, order-independent hash of the plugin configuration
/// (`inst-ps-key-5`): the keys and values are taken in sorted order, so
/// distinct scope sets occupy distinct entries and a re-ordered object does
/// not.
#[must_use]
pub fn config_hash(config: Option<&serde_json::Value>) -> String {
    format!("{:016x}", fnv1a(canonical_form(config).as_bytes()))
}

/// A 64-bit FNV-1a digest: deterministic and dependency-free. It separates
/// distinct configurations inside one tenant's cache and protects nothing.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Flatten a configuration into a sorted `key=value` canon, so the hash is
/// independent of JSON key order.
fn canonical_form(config: Option<&serde_json::Value>) -> String {
    let Some(config) = config else {
        return String::new();
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    collect_pairs("", config, &mut pairs);
    pairs.sort();
    pairs.into_iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join("&")
}

fn collect_pairs(prefix: &str, value: &serde_json::Value, pairs: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, nested) in object {
                let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                collect_pairs(&path, nested, pairs);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_pairs(&format!("{prefix}[{index}]"), item, pairs);
            }
        }
        serde_json::Value::Null => pairs.push((prefix.to_owned(), "null".to_owned())),
        serde_json::Value::Bool(flag) => pairs.push((prefix.to_owned(), flag.to_string())),
        serde_json::Value::Number(number) => pairs.push((prefix.to_owned(), number.to_string())),
        serde_json::Value::String(text) => {
            // A `cred://` reference contributes to the key: it identifies the
            // credential, never the material behind it.
            pairs.push((prefix.to_owned(), text.clone()));
        }
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.plugin_type {
            OAUTH2_BASIC_PLUGIN_TYPE => "oauth2_client_cred_basic",
            _ => "oauth2_client_cred",
        }
    }

    fn plugin_type(&self) -> &str {
        self.plugin_type
    }

    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-2
    // `inst-ps-tok-2` .. `-8`: the key is built, looked up, and on a miss the
    // two references are resolved through `cred_store` and exactly one
    // exchange is performed; a failed exchange caches nothing.
    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        let (client_id_ref, client_secret_ref) = Self::required_references(ctx.config.as_ref())?;
        let key = Self::cache_key(
            self.auth_method,
            ctx.principal.tenant_id,
            ctx.principal.subject_id,
            ctx.config.as_ref(),
        );

        // `inst-ps-tok-4`: a hit whose stored key matches serves the cached
        // token and skips the exchange.
        if let (Some(cached), _) = self.cache.get(&key) {
            if cached.key == key {
                ctx.outbound_headers
                    .push(("authorization".to_owned(), format!("Bearer {}", cached.token.expose())));
                return Ok(());
            }
        }

        let security: SecurityContext = security_context_for(
            ctx.principal.subject_id,
            ctx.principal.tenant_id,
            &ctx.principal.scopes,
        );
        let client_id = self.resolver.resolve(&security, &client_id_ref).await?;
        let client_secret = self.resolver.resolve(&security, &client_secret_ref).await?;
        let mut config = Self::token_endpoint_config(
            ctx.config.as_ref(),
            client_id.as_str()?.to_owned(),
            SecretString::new(client_secret.as_str()?.to_owned()),
            self.auth_method,
        )?;
        config.http_config = self.http_config.clone();

        // Exactly one exchange; the credential material is dropped when the
        // exchange returns (`inst-ps-tok-7`).
        let fetched = fetch_token(config).await.map_err(|_| {
            PluginError::rejected("the OAuth2 token exchange failed")
        })?;
        drop(client_id);
        drop(client_secret);

        // `inst-ps-tok-8`: the entry is stored under the lookup key only when
        // the exchange succeeded and the remaining lifetime is usable.
        if let Some(ttl) = Self::cache_ttl_for(self.cache_ttl, fetched.expires_in) {
            self.cache.put(
                &key,
                CachedToken { key: key.clone(), token: fetched.bearer.clone() },
                Some(ttl),
            );
        }
        ctx.outbound_headers
            .push(("authorization".to_owned(), format!("Bearer {}", fetched.bearer.expose())));
        Ok(())
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-2
}

// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-1
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-10
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-11
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-12
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-13
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-3
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-5
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-7
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-8
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-9
#[cfg(test)]
#[path = "oauth2_client_cred_auth_tests.rs"]
mod oauth2_client_cred_auth_tests;
//
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-9
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-7
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-5
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-4
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-3
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-13
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-12
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-11
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-10
// @cpt-end:cpt-cf-oagw-flow-plugin-system-token-cache:p1:inst-ps-tok-1
//
