//! OAuth2 Client Credentials auth plugin with an internal token cache
//! (ADR-0008).
//!
//! Registered twice — once per `ClientAuthMethod` — under
//! `…cf.core.oagw.oauth2_client_cred.v1` (Form) and
//! `…cf.core.oagw.oauth2_client_cred_basic.v1` (Basic).

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use http::header::{AUTHORIZATION, HeaderValue};
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};

use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, PluginError, PluginResult, RequestContext};
use crate::infra::plugin::secret::resolve_secret;

/// Tokens are never served within this margin of their expiry.
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Gear-level token-cache knobs (`OagwConfig`).
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    pub ttl: Duration,
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            capacity: 10_000,
        }
    }
}

/// Cache entry carrying its own key.
///
/// `TinyUfo` hashes keys to `u64` and does not compare them on hit, so the key
/// is re-verified here: a collision degrades to a miss instead of handing a
/// different tenant's token to the caller.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Arc<SecretString>,
}

pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    #[must_use]
    pub fn new(
        credstore: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            cache: MemoryCache::new(cache_capacity.max(1)),
            cache_ttl,
        }
    }

    fn plugin_id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        }
    }
}

/// Stable tag for a client-auth method, used in the cache key.
#[must_use]
pub fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Form => "form",
        ClientAuthMethod::Basic => "basic",
    }
}

/// Deterministic hash over the sorted plugin config, so two upstreams with
/// different scopes never share a cache entry.
#[must_use]
pub fn hash_config(config: &std::collections::BTreeMap<String, serde_json::Value>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in config {
        key.hash(&mut hasher);
        value.to_string().hash(&mut hasher);
    }
    hasher.finish()
}

/// `{tenant}:{subject}:{auth_method}:{config_hash}` — see ADR-0008
/// "Cache Key Design".
#[must_use]
pub fn build_cache_key(ctx: &RequestContext, auth_method: ClientAuthMethod) -> String {
    format!(
        "{}:{}:{}:{}",
        ctx.security_context.subject_tenant_id(),
        ctx.security_context.subject_id(),
        auth_method_tag(auth_method),
        hash_config(&ctx.config),
    )
}

/// `min(config_ttl, expires_in - 30s)`; `None` when the token expires too soon
/// to be worth caching.
#[must_use]
pub fn effective_ttl(config_ttl: Duration, expires_in: Duration) -> Option<Duration> {
    let usable = expires_in.checked_sub(EXPIRY_SAFETY_MARGIN)?;
    if usable.is_zero() {
        return None;
    }
    Some(config_ttl.min(usable))
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => "oauth2_client_cred",
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
        }
    }

    fn plugin_type(&self) -> &str {
        self.plugin_id()
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let key = build_cache_key(ctx, self.auth_method);

        if let (Some(entry), _) = self.cache.get(&key)
            && entry.key == key
        {
            inject_bearer(ctx, entry.token.expose())?;
            return Ok(());
        }

        let token_endpoint = ctx.config_str("token_endpoint").map(str::to_owned);
        let issuer_url = ctx.config_str("issuer_url").map(str::to_owned);
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::Config(
                "oauth2 client credentials plugin: exactly one of 'token_endpoint' or \
                 'issuer_url' must be configured"
                    .to_owned(),
            ));
        }

        let client_id_ref = ctx.require_config_str("client_id_ref")?.to_owned();
        let client_secret_ref = ctx.require_config_str("client_secret_ref")?.to_owned();
        let scopes: Vec<String> = ctx
            .config_str("scopes")
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        let client_id =
            resolve_secret(&self.credstore, &ctx.security_context, &client_id_ref).await?;
        let client_secret =
            resolve_secret(&self.credstore, &ctx.security_context, &client_secret_ref).await?;

        let mut config = OAuthClientConfig {
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: self.auth_method,
            ..OAuthClientConfig::default()
        };
        if let Some(raw) = token_endpoint {
            config.token_endpoint = Some(parse_url(&raw, "token_endpoint")?);
        }
        if let Some(raw) = issuer_url {
            config.issuer_url = Some(parse_url(&raw, "issuer_url")?);
        }

        let fetched = fetch_token(config).await.map_err(|err| {
            // `TokenError`'s Display never contains the client secret.
            PluginError::Unauthenticated(format!("oauth2 token exchange failed: {err}"))
        })?;

        inject_bearer(ctx, fetched.bearer.expose())?;

        // A failed fetch is never cached; a short-lived token is not cached either.
        if let Some(ttl) = effective_ttl(self.cache_ttl, fetched.expires_in) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: Arc::new(fetched.bearer),
                },
                Some(ttl),
            );
        }
        Ok(())
    }
}

fn parse_url(raw: &str, field: &str) -> PluginResult<url::Url> {
    url::Url::parse(raw)
        .map_err(|e| PluginError::Config(format!("oauth2 plugin: invalid {field}: {e}")))
}

fn inject_bearer(ctx: &mut RequestContext, token: &str) -> PluginResult<()> {
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        PluginError::Internal(
            "oauth2 plugin: access token is not a valid header value".to_owned(),
        )
    })?;
    ctx.headers.insert(AUTHORIZATION, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn ttl_respects_the_safety_margin() {
        let cfg = Duration::from_secs(300);
        // Server says 3600s -> capped by the 300s config ceiling.
        assert_eq!(
            effective_ttl(cfg, Duration::from_secs(3600)),
            Some(Duration::from_secs(300))
        );
        // Server says 100s -> 100 - 30 = 70s wins over the 300s ceiling.
        assert_eq!(
            effective_ttl(cfg, Duration::from_secs(100)),
            Some(Duration::from_secs(70))
        );
        // Tokens expiring within the margin are not cached at all.
        assert_eq!(effective_ttl(cfg, Duration::from_secs(30)), None);
        assert_eq!(effective_ttl(cfg, Duration::from_secs(5)), None);
    }

    #[test]
    fn config_hash_is_order_independent_and_content_sensitive() {
        let mut a = BTreeMap::new();
        a.insert("scopes".to_owned(), serde_json::json!("a b"));
        a.insert("issuer_url".to_owned(), serde_json::json!("https://idp"));
        let mut b = BTreeMap::new();
        b.insert("issuer_url".to_owned(), serde_json::json!("https://idp"));
        b.insert("scopes".to_owned(), serde_json::json!("a b"));
        assert_eq!(hash_config(&a), hash_config(&b));

        let mut c = b.clone();
        c.insert("scopes".to_owned(), serde_json::json!("a"));
        assert_ne!(hash_config(&b), hash_config(&c));
    }

    #[test]
    fn method_tags_separate_the_two_variants() {
        assert_eq!(auth_method_tag(ClientAuthMethod::Form), "form");
        assert_eq!(auth_method_tag(ClientAuthMethod::Basic), "basic");
    }
}
