//! OAuth2 client-credentials auth plugins (ADR-0008).
//!
//! Two registered variants:
//! * `...~cf.core.oagw.oauth2_client_cred.v1` — `Form` client auth
//! * `...~cf.core.oagw.oauth2_client_cred_basic.v1` — `Basic` client auth
//!
//! Tokens are fetched once per `(tenant, subject, auth_method, config)` tuple
//! and cached with TTL `min(config_ttl, expires_in - 30s)`. Failed fetches are
//! not cached. The `CachedToken` wrapper re-verifies the key on hit to close
//! the TinyUfo hash-collision hole.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_security::SecurityContext;
use url::Url;

use crate::config::TokenCacheConfig;
use crate::domain::models::plugin_gts::{AUTH_OAUTH2_CLIENT_CRED, AUTH_OAUTH2_CLIENT_CRED_BASIC};
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};
use credstore_sdk::{CredStoreClientV1, SecretRef};

/// Configuration keys for the OAuth2 client-credentials plugin (ADR-0008 table).
pub mod keys {
    pub const TOKEN_ENDPOINT: &str = "token_endpoint";
    pub const ISSUER_URL: &str = "issuer_url";
    pub const CLIENT_ID_REF: &str = "client_id_ref";
    pub const CLIENT_SECRET_REF: &str = "client_secret_ref";
    pub const SCOPES: &str = "scopes";
}

/// Which client-auth method the token request uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// Credentials in the request body.
    Form,
    /// `Authorization: Basic`.
    Basic,
}

impl ClientAuthMethod {
    /// Plugin GTS identifier for this variant.
    #[must_use]
    pub fn plugin_id(self) -> &'static str {
        match self {
            Self::Form => AUTH_OAUTH2_CLIENT_CRED,
            Self::Basic => AUTH_OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    /// Mapping used in cache keys to keep Form and Basic entries apart.
    fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }
}

/// Cached token wrapper that carries the original cache key for verification.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: toolkit_auth::SecretString,
}

/// OAuth2 client-credentials auth plugin with an internal token cache.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    http_config: toolkit_http::HttpClientConfig,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Create the plugin.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        token_cache: TokenCacheConfig,
        http_config: toolkit_http::HttpClientConfig,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            http_config,
            cache: MemoryCache::new(token_cache.capacity),
            cache_ttl: Duration::from_secs(token_cache.ttl_secs),
        }
    }

    /// Resolve a `cred://{name}` reference to a plain string.
    async fn resolve(&self, ctx: &SecurityContext, reference: &str) -> Result<String, PluginError> {
        let name = reference.trim_start_matches("cred://");
        let secret_ref = SecretRef::new(name.to_owned()).map_err(|e| PluginError::Config {
            message: format!("invalid {reference}: {e}"),
        })?;
        match self.credstore.get(ctx, &secret_ref).await {
            Ok(Some(resp)) => Ok(String::from_utf8_lossy(resp.value.as_bytes()).into_owned()),
            Ok(None) => Err(PluginError::auth(format!(
                "credential {reference} is inaccessible to the current tenant"
            ))),
            Err(e) => Err(PluginError::Internal {
                message: format!("credential store lookup failed for {reference}: {e}"),
            }),
        }
    }

    fn build_cache_key(ctx: &RequestContext<'_>, auth_method: ClientAuthMethod) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            auth_method.tag(),
            stable_config_hash(ctx.config)
        )
    }

    /// Fetch a token from the IdP and inject it (cache miss path).
    async fn fetch_and_inject(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError> {
        let config = ctx.config;
        let client_id_ref = config
            .get(keys::CLIENT_ID_REF)
            .and_then(|v| v.as_str())
            .ok_or_else(|| PluginError::Config {
                message: format!("oauth2 plugin requires `{}`", keys::CLIENT_ID_REF),
            })?;
        let client_secret_ref = config
            .get(keys::CLIENT_SECRET_REF)
            .and_then(|v| v.as_str())
            .ok_or_else(|| PluginError::Config {
                message: format!("oauth2 plugin requires `{}`", keys::CLIENT_SECRET_REF),
            })?;

        let client_id = self.resolve(ctx.security_context, client_id_ref).await?;
        let client_secret = self
            .resolve(ctx.security_context, client_secret_ref)
            .await?;

        let mut oauth_config = toolkit_auth::OAuthClientConfig {
            client_id,
            client_secret: toolkit_auth::SecretString::new(client_secret),
            auth_method: match self.auth_method {
                ClientAuthMethod::Form => toolkit_auth::ClientAuthMethod::Form,
                ClientAuthMethod::Basic => toolkit_auth::ClientAuthMethod::Basic,
            },
            scopes: config
                .get(keys::SCOPES)
                .and_then(|v| v.as_str())
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            http_config: Some(self.http_config.clone()),
            // fetch_token derives expiry from expires_in; the default_ttl is
            // only used when the IdP omits expires_in.
            default_ttl: self.cache_ttl,
            ..Default::default()
        };

        let (token_endpoint, issuer_url) = match (
            config.get(keys::TOKEN_ENDPOINT).and_then(|v| v.as_str()),
            config.get(keys::ISSUER_URL).and_then(|v| v.as_str()),
        ) {
            (Some(t), None) => (
                Some(Url::parse(t).map_err(|e| PluginError::Config {
                    message: format!("invalid {}: {e}", keys::TOKEN_ENDPOINT),
                })?),
                None,
            ),
            (None, Some(i)) => (
                None,
                Some(Url::parse(i).map_err(|e| PluginError::Config {
                    message: format!("invalid {}: {e}", keys::ISSUER_URL),
                })?),
            ),
            (Some(_), Some(_)) => {
                return Err(PluginError::Config {
                    message: format!(
                        "{} and {} are mutually exclusive",
                        keys::TOKEN_ENDPOINT,
                        keys::ISSUER_URL
                    ),
                });
            }
            (None, None) => {
                return Err(PluginError::Config {
                    message: format!(
                        "one of {} or {} is required",
                        keys::TOKEN_ENDPOINT,
                        keys::ISSUER_URL
                    ),
                });
            }
        };
        oauth_config.token_endpoint = token_endpoint;
        oauth_config.issuer_url = issuer_url;

        let fetched =
            toolkit_auth::fetch_token(oauth_config)
                .await
                .map_err(|e| PluginError::Internal {
                    message: format!("OAuth2 token fetch failed: {e}"),
                })?;

        // Cache with min(config_ttl, expires_in - 30s); tokens expiring within
        // 30s are not cached (ADR-0008).
        let ttl_secs = fetched
            .expires_in
            .as_secs()
            .saturating_sub(30)
            .min(self.cache_ttl.as_secs());
        if ttl_secs > 0 {
            let key = Self::build_cache_key(ctx, self.auth_method);
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(Duration::from_secs(ttl_secs)),
            );
        }

        Self::inject(ctx, &fetched.bearer.expose());
        Ok(())
    }

    fn inject(ctx: &mut RequestContext<'_>, bearer: &str) {
        // Plain short-lived string, scoped to the request (ADR-0008 residual).
        ctx.headers
            .push(("authorization".to_owned(), format!("Bearer {bearer}")));
    }
}

/// Deterministic, sorted hash of the plugin config: every key/value pair
/// contributes a quoted `key=value` fragment in lexicographic key order.
fn stable_config_hash(config: &serde_json::Value) -> String {
    let Some(obj) = config.as_object() else {
        return "{}".to_owned();
    };
    let mut parts: Vec<String> = obj.iter().map(|(k, v)| format!("{k}={v}")).collect();
    parts.sort();
    parts.join("&")
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        self.auth_method.plugin_id()
    }

    async fn authenticate(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError> {
        let key = Self::build_cache_key(ctx, self.auth_method);
        let (cached, _status) = self.cache.get(&key);
        if let Some(entry) = cached {
            // Verify the key on hit — a hash collision is treated as a miss.
            if entry.key == key {
                Self::inject(ctx, entry.token.expose());
                return Ok(());
            }
        }
        self.fetch_and_inject(ctx).await
    }
}
