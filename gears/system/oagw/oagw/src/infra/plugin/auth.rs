//! Built-in auth plugins: `noop`, `apikey`, and the two `OAuth2`
//! client-credentials variants (`ADR 0008`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};

use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::model::auth_plugin_ids;
use crate::domain::plugin::{RequestContext, TokenCacheConfig};
use crate::infra::secrets::resolve_secret_string;

/// Short, value-free classification of a token-exchange failure.
///
/// The `TokenError` variants do not separate "the `IdP` refused our credentials"
/// from "the endpoint is down" — both surface as `Http` — so every failure keeps
/// the same `401` class and only the log line distinguishes the cause.
fn token_error_class(error: &toolkit_auth::oauth2::TokenError) -> &'static str {
    use toolkit_auth::oauth2::TokenError;
    match error {
        TokenError::Http(_) => "token_endpoint_http",
        TokenError::InvalidResponse(_) => "token_response_invalid",
        TokenError::UnsupportedTokenType(_) => "token_type_unsupported",
        TokenError::ConfigError(_) => "auth_config_invalid",
        TokenError::Unavailable(_) => "token_endpoint_unavailable",
        TokenError::InvalidTokenLifetime(_) => "token_lifetime_invalid",
        // Non-exhaustive upstream enum: an unknown variant is still an exchange
        // failure, and never a reason to render a cause the crate cannot name.
        _ => "token_endpoint_error",
    }
}

/// Credentials for an API-key upstream, resolved at request time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyConfig {
    /// Credential-store reference for the key value.
    pub key_ref: String,
    /// Header or query-parameter name carrying the key.
    pub name: String,
    /// Whether the key travels in a header (`true`) or the query (`false`).
    pub in_header: bool,
}

/// Extract an [`ApiKeyConfig`] from a plugin binding's config object.
///
/// # Errors
///
/// Returns a validation error when no credential reference is configured.
pub fn parse_api_key_config(config: &serde_json::Value) -> Result<ApiKeyConfig, OagwError> {
    let object = config.as_object();
    let read_str = |keys: &[&str]| -> Option<String> {
        object.and_then(|map| {
            keys.iter()
                .find_map(|key| map.get(*key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
    };
    let key_ref = read_str(&["api_key_ref", "secret_ref", "credential_ref", "key_ref"])
        .filter(|value| !value.trim().is_empty());
    let Some(key_ref) = key_ref else {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "auth.config.api_key_ref is required for the apikey auth plugin",
        ));
    };
    let name = read_str(&["key_name", "header", "param_name", "name"])
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "x-api-key".to_owned());
    let location = read_str(&["location", "in"]).unwrap_or_default();
    let in_header = !matches!(location.as_str(), "query" | "param");
    Ok(ApiKeyConfig {
        key_ref,
        name,
        in_header,
    })
}

/// Injects an API key from the credential store into a header or query
/// parameter.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Create the plugin with its credential-store client.
    #[must_use]
    pub fn new(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

#[async_trait]
impl crate::domain::plugin::AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        auth_plugin_ids::APIKEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let config = parse_api_key_config(&ctx.config)?;
        let Some(security) = ctx.security.clone() else {
            return Err(OagwError::new(
                ErrorKind::AuthenticationFailed,
                "no security context available for credential resolution",
            ));
        };
        let value =
            resolve_secret_string(&self.credstore, security.as_ref(), &config.key_ref).await?;
        if config.in_header {
            ctx.set_header(&config.name, &value);
        } else {
            append_query_param(&mut ctx.query, &config.name, &value);
        }
        Ok(())
    }
}

/// Append `name=value` to a query string, URL-encoding both parts.
pub fn append_query_param(query: &mut Option<String>, name: &str, value: &str) {
    let mut pairs: Vec<(String, String)> = query
        .as_deref()
        .map(|raw| {
            form_urlencoded::parse(raw.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    pairs.retain(|(key, _)| key != name);
    pairs.push((name.to_owned(), value.to_owned()));
    let encoded = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    *query = Some(encoded);
}

/// Auth plugin that injects nothing.
pub struct NoopAuthPlugin;

#[async_trait]
impl crate::domain::plugin::AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        auth_plugin_ids::NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        Ok(())
    }
}

/// A cached token plus the cache key it was stored under, so a hash collision
/// can never leak another tenant's credential (`ADR 0008`).
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: toolkit_auth::oauth2::SecretString,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// `OAuth2` client-credentials auth plugin with an internal token cache.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_config: TokenCacheConfig,
}

/// Safety margin subtracted from the IdP-reported `expires_in`.
const TTL_SAFETY_MARGIN: u64 = 30;

impl OAuth2ClientCredAuthPlugin {
    /// Create a plugin variant for `auth_method`.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache_config: TokenCacheConfig,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            cache: MemoryCache::new(cache_config.capacity),
            cache_config,
        }
    }

    /// Stable tag distinguishing the two registered variants.
    #[must_use]
    pub fn auth_method_tag(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "basic",
            ClientAuthMethod::Form => "form",
        }
    }
}

/// Deterministic, order-independent digest of a config object.
#[must_use]
pub fn hash_config(config: &serde_json::Value) -> u64 {
    let mut canonical = String::new();
    if let Some(map) = config.as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            canonical.push_str(key);
            canonical.push('=');
            canonical.push_str(&map[key].to_string());
            canonical.push('\u{1f}');
        }
    }
    // FNV-1a: stable across builds and dependency-free.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Parse the `OAuth2` client-credentials config keys (`ADR 0008`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuth2PluginConfig {
    /// Direct token endpoint, mutually exclusive with `issuer_url`.
    pub token_endpoint: Option<String>,
    /// OIDC issuer URL, mutually exclusive with `token_endpoint`.
    pub issuer_url: Option<String>,
    /// `cred://` reference for the client id.
    pub client_id_ref: String,
    /// `cred://` reference for the client secret.
    pub client_secret_ref: String,
    /// Space-separated scopes.
    pub scopes: Option<String>,
}

/// Extract [`OAuth2PluginConfig`] from a plugin binding's config object.
///
/// # Errors
///
/// Returns a validation error when the required keys are absent.
pub fn parse_oauth2_config(config: &serde_json::Value) -> Result<OAuth2PluginConfig, OagwError> {
    let Some(map) = config.as_object() else {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "auth.config must be an object",
        ));
    };
    let read = |key: &str| -> Option<String> {
        map.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty())
    };
    let missing = |key: &str| {
        OagwError::new(
            ErrorKind::Validation,
            format!("auth.config.{key} is required"),
        )
    };
    let token_endpoint = read("token_endpoint");
    let issuer_url = read("issuer_url");
    if token_endpoint.is_some() == issuer_url.is_some() {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "exactly one of auth.config.token_endpoint or auth.config.issuer_url is required",
        ));
    }
    Ok(OAuth2PluginConfig {
        token_endpoint,
        issuer_url,
        client_id_ref: read("client_id_ref").ok_or_else(|| missing("client_id_ref"))?,
        client_secret_ref: read("client_secret_ref").ok_or_else(|| missing("client_secret_ref"))?,
        scopes: read("scopes"),
    })
}

/// Build a `toolkit-auth` client config from the plugin config and secrets.
///
/// # Errors
///
/// Returns a validation error when the endpoint URL cannot be parsed.
pub fn build_client_config(
    config: &OAuth2PluginConfig,
    client_id: &str,
    client_secret: &str,
    auth_method: ClientAuthMethod,
) -> Result<OAuthClientConfig, OagwError> {
    let token_endpoint = config
        .token_endpoint
        .as_deref()
        .map(url::Url::parse)
        .transpose()
        .map_err(|_| {
            OagwError::new(
                ErrorKind::Validation,
                "auth.config.token_endpoint is not a valid URL",
            )
        })?;
    let issuer_url = config
        .issuer_url
        .as_deref()
        .map(url::Url::parse)
        .transpose()
        .map_err(|_| {
            OagwError::new(
                ErrorKind::Validation,
                "auth.config.issuer_url is not a valid URL",
            )
        })?;
    Ok(OAuthClientConfig {
        token_endpoint,
        issuer_url,
        client_id: client_id.to_owned(),
        client_secret: toolkit_auth::oauth2::SecretString::new(client_secret.to_owned()),
        scopes: config
            .scopes
            .as_deref()
            .map(|raw| raw.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        auth_method,
        ..OAuthClientConfig::default()
    })
}

#[async_trait]
impl crate::domain::plugin::AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
            ClientAuthMethod::Form => "oauth2_client_cred",
        }
    }

    fn plugin_type(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => auth_plugin_ids::OAUTH2_CC_BASIC,
            ClientAuthMethod::Form => auth_plugin_ids::OAUTH2_CC,
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let config = parse_oauth2_config(&ctx.config)?;
        // The caller's tenant, not the upstream owner's (`ctx.tenant_id`): two
        // tenants may call one upstream, and a token minted from tenant A's
        // client credentials must never be replayed for tenant B (`ADR 0008`
        // "Cache Key Design": `subject_tenant_id` exists for cross-tenant
        // isolation).
        let key = format!(
            "{}:{}:{}:{}",
            ctx.caller_tenant_id,
            ctx.subject_id,
            self.auth_method_tag(),
            hash_config(&ctx.config)
        );
        if let (Some(cached), _) = self.cache.get(&key)
            && cached.key == key
        {
            let bearer = format!("Bearer {}", cached.token.expose());
            ctx.set_header("authorization", &bearer);
            return Ok(());
        }

        let Some(security) = ctx.security.clone() else {
            return Err(OagwError::new(
                ErrorKind::AuthenticationFailed,
                "no security context available for credential resolution",
            ));
        };

        let client_id =
            resolve_secret_string(&self.credstore, security.as_ref(), &config.client_id_ref)
                .await?;
        let client_secret = resolve_secret_string(
            &self.credstore,
            security.as_ref(),
            &config.client_secret_ref,
        )
        .await?;
        let client_config =
            build_client_config(&config, &client_id, &client_secret, self.auth_method)?;
        let fetched = fetch_token(client_config).await.map_err(|error| {
            // Logged by credential REFERENCE name and a short cause class only:
            // the token, the client secret, and the endpoint's own response body
            // never reach a log line or a problem body.
            tracing::warn!(
                credential_reference = %config.client_id_ref,
                cause = %token_error_class(&error),
                "identity provider token exchange failed"
            );
            OagwError::new(
                ErrorKind::AuthenticationFailed,
                "upstream token exchange failed",
            )
        })?;

        let ttl = fetched
            .expires_in
            .checked_sub(Duration::from_secs(TTL_SAFETY_MARGIN))
            .unwrap_or(Duration::ZERO);
        let ttl = ttl.min(self.cache_config.ttl);
        if !ttl.is_zero() {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }

        let bearer = format!("Bearer {}", fetched.bearer.expose());
        ctx.set_header("authorization", &bearer);
        Ok(())
    }
}
