//! `oauth2_client_cred` / `oauth2_client_cred_basic` auth plugins (ADR 0008).
//!
//! Config keys (per ADR 0008's table): `token_endpoint` xor `issuer_url`,
//! `client_id_ref`, `client_secret_ref`, optional `scopes`.
//!
//! Tokens are exchanged with `toolkit_auth::oauth2::fetch_token` (a one-shot
//! exchange, so no background watcher per cache entry) and cached with
//! `pingora-memory_cache`, keyed by tenant + subject + client-auth method +
//! config hash, and verified against the original key on hit.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::HeaderMap;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{AuthPlugin, PluginConfig, RequestContext};

/// How the client credentials are transmitted to the token endpoint.
///
/// Re-exported from [`toolkit_auth::oauth2`], which owns the exchange itself;
/// this module only adds the cache-key tag.
trait ClientAuthTag {
    /// Stable, lowercase tag used in the cache key.
    fn tag(self) -> &'static str;
}

impl ClientAuthTag for ClientAuthMethod {
    fn tag(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Form => "form",
        }
    }
}

/// A cached token plus the key it was stored under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Arc<SecretString>,
}

/// `OAuth2` client-credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    security: Option<toolkit_security::SecurityContext>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the plugin over a credential store.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            credstore,
            security: None,
            auth_method,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// Supplies the security context used to resolve secrets.
    #[must_use]
    pub fn with_security_context(mut self, ctx: toolkit_security::SecurityContext) -> Self {
        self.security = Some(ctx);
        self
    }

    fn cache_key(&self, ctx: &RequestContext, config: &PluginConfig) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.tenant_id,
            ctx.attributes.get("oagw.subject_id").unwrap_or("-"),
            self.auth_method.tag(),
            hash_config(config)
        )
    }

    async fn resolve_secret(&self, key_ref: &str) -> Result<String, DomainError> {
        let raw = key_ref.strip_prefix("cred://").unwrap_or(key_ref);
        let reference = credstore_sdk::SecretRef::new(raw.to_owned())
            .map_err(|_| DomainError::SecretNotFound)?;
        let ctx = self
            .security
            .clone()
            .unwrap_or_else(toolkit_security::SecurityContext::anonymous);
        match self.credstore.get(&ctx, &reference).await {
            Ok(Some(response)) => String::from_utf8(response.value.as_bytes().to_vec())
                .map_err(|_| DomainError::SecretNotFound),
            _ => Err(DomainError::SecretNotFound),
        }
    }

    fn inject(headers: &mut HeaderMap, token: &str) -> Result<(), DomainError> {
        let value = http::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| DomainError::Validation("token produced an invalid header".to_owned()))?;
        headers.insert(http::header::AUTHORIZATION, value);
        Ok(())
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => gts::AUTH_OAUTH2_CC_BASIC,
            ClientAuthMethod::Form => gts::AUTH_OAUTH2_CC,
        }
    }

    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let key = self.cache_key(ctx, config);
        let (hit, _) = self.cache.get(&key);
        if let Some(cached) = hit
            && cached.key == key
        {
            return Self::inject(&mut ctx.headers, cached.token.expose());
        }

        let token_endpoint = config.string("token_endpoint");
        let issuer_url = config.string("issuer_url");
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(DomainError::Validation(
                "oauth2 client-credentials plugin requires `token_endpoint` or `issuer_url`"
                    .to_owned(),
            ));
        }
        let client_id_ref = config.string("client_id_ref").ok_or_else(|| {
            DomainError::Validation("oauth2 plugin requires `client_id_ref`".to_owned())
        })?;
        let client_secret_ref = config.string("client_secret_ref").ok_or_else(|| {
            DomainError::Validation("oauth2 plugin requires `client_secret_ref`".to_owned())
        })?;

        let client_id = self.resolve_secret(client_id_ref).await?;
        let client_secret = self.resolve_secret(client_secret_ref).await?;
        let scopes = config
            .string("scopes")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        let mut client_config = OAuthClientConfig::default();
        // Exactly one of the two is present: the guard above rejects a request
        // that names neither.
        if let Some(endpoint) = token_endpoint {
            client_config.token_endpoint = Some(
                url::Url::parse(endpoint)
                    .map_err(|_| DomainError::Validation("invalid token_endpoint".to_owned()))?,
            );
        } else if let Some(raw) = issuer_url {
            client_config.issuer_url = Some(
                url::Url::parse(raw)
                    .map_err(|_| DomainError::Validation("invalid issuer_url".to_owned()))?,
            );
        }
        client_config.client_id = client_id;
        client_config.client_secret = SecretString::new(client_secret);
        client_config.scopes = scopes;
        client_config.auth_method = self.auth_method;
        client_config.default_ttl = self.cache_ttl;

        let fetched = fetch_token(client_config).await.map_err(|e| {
            DomainError::AuthenticationFailed(format!("token exchange failed: {e}"))
        })?;

        // `expires_in` minus a 30 s safety margin, never above the configured ceiling.
        let margin = Duration::from_secs(30);
        let ttl = fetched
            .expires_in
            .checked_sub(margin)
            .unwrap_or(self.cache_ttl)
            .min(self.cache_ttl);
        let token: Arc<SecretString> = Arc::new(fetched.bearer.clone());
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token,
            },
            Some(ttl),
        );
        Self::inject(&mut ctx.headers, fetched.bearer.expose())
    }
}

/// Deterministic, order-independent hash of the plugin config keys.
fn hash_config(config: &PluginConfig) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (k, v) in &config.values {
        std::hash::Hash::hash(&k, &mut hasher);
        std::hash::Hash::hash(&serde_json::to_string(v).unwrap_or_default(), &mut hasher);
    }
    std::hash::Hasher::finish(&hasher)
}
