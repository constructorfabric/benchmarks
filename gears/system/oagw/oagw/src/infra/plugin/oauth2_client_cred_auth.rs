//! Built-in OAuth2 client-credentials auth plugins (ADR-0008).
//!
//! Two variants are registered — `oauth2_client_cred` (form client authentication) and
//! `oauth2_client_cred_basic` (`Authorization` header). Tokens are fetched with
//! [`toolkit_auth::oauth2::fetch_token`] and cached in a `pingora-memory-cache` keyed by
//! `(tenant, subject, auth method, config hash)` with a TTL of
//! `min(configured_ttl, expires_in − 30s)`.

use std::sync::Arc;
use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};
use url::Url;

use crate::domain::gts_helpers;
use crate::domain::plugin::{AuthPlugin, PluginError, ProxyRequest};
use crate::infra::credentials::{OAuth2PluginConfig, SecretValue, hash_config};

/// Safety margin subtracted from the IdP-reported `expires_in`.
const TOKEN_TTL_MARGIN_SECS: u64 = 30;

/// A cached access token, carrying its key so hash collisions are detected (ADR-0008).
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretValue,
}

/// One of the two client-credential plugins.
#[derive(Clone)]
pub struct OAuth2ClientCredAuthPlugin {
    method: ClientAuthMethod,
    method_tag: &'static str,
    ttl: Duration,
    cache: Arc<MemoryCache<String, CachedToken>>,
    resolver: crate::infra::credentials::SecretResolver,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("method_tag", &self.method_tag)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// The form-authenticated variant.
    #[must_use]
    pub fn form() -> Self {
        Self::new(ClientAuthMethod::Form, None, None)
    }

    /// The basic-authenticated variant.
    #[must_use]
    pub fn basic() -> Self {
        Self::new(ClientAuthMethod::Basic, None, None)
    }

    /// Plugin with explicit cache settings and credential resolver.
    #[must_use]
    pub fn new(
        method: ClientAuthMethod,
        ttl: Option<Duration>,
        capacity: Option<usize>,
    ) -> Self {
        Self {
            method,
            method_tag: match method {
                ClientAuthMethod::Form => "form",
                ClientAuthMethod::Basic => "basic",
            },
            ttl: ttl.unwrap_or_else(|| Duration::from_secs(300)),
            cache: Arc::new(MemoryCache::new(capacity.unwrap_or(10_000))),
            resolver: std::sync::Arc::new(crate::infra::credentials::MissingResolver),
        }
    }

    /// Plugin with the given credential resolver.
    #[must_use]
    pub fn with_resolver(mut self, resolver: crate::infra::credentials::SecretResolver) -> Self {
        self.resolver = resolver;
        self
    }

    async fn resolve(
        &self,
        reference: &str,
        ctx: Option<Arc<toolkit_security::SecurityContext>>,
    ) -> Result<SecretValue, PluginError> {
        crate::infra::credentials::resolve_secret(&self.resolver, ctx, reference).await
    }
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        match self.method {
            ClientAuthMethod::Form => gts_helpers::AUTH_OAUTH2_CC,
            ClientAuthMethod::Basic => gts_helpers::AUTH_OAUTH2_CC_BASIC,
        }
    }

    fn plugin_type(&self) -> &'static str {
        "oauth2_client_cred"
    }

    async fn authenticate(
        &self,
        request: &mut ProxyRequest,
        config: &Value,
    ) -> Result<(), PluginError> {
        let parsed = OAuth2PluginConfig::parse(config)?;
        let tenant = request
            .security
            .as_ref()
            .map(|s| s.subject_tenant_id().to_string())
            .unwrap_or_else(|| request.tenant_id.to_string());
        let subject = request
            .security
            .as_ref()
            .map(|s| s.subject_id().to_string())
            .unwrap_or_else(|| request.tenant_id.to_string());
        let key = format!(
            "{tenant}:{subject}:{}:{}",
            self.method_tag,
            hash_config(config)
        );

        if let (Some(hit), _) = self.cache.get(&key)
            && hit.key == key {
                request.set_header("authorization", &format!("Bearer {}", hit.token.expose()));
                return Ok(());
            }

        let client_id = self
            .resolve(&parsed.client_id_ref, request.security.clone())
            .await?;
        let client_secret = self
            .resolve(&parsed.client_secret_ref, request.security.clone())
            .await?;

        let config = OAuthClientConfig {
            token_endpoint: parsed
                .token_endpoint
                .as_deref()
                .map(Url::parse)
                .transpose()
                .map_err(|e| PluginError::Config(format!("token_endpoint is not a URL: {e}")))?,
            issuer_url: parsed
                .issuer_url
                .as_deref()
                .map(Url::parse)
                .transpose()
                .map_err(|e| PluginError::Config(format!("issuer_url is not a URL: {e}")))?,
            client_id: client_id.expose().to_string(),
            client_secret: toolkit_auth::SecretString::new(client_secret.expose()),
            scopes: parsed
                .scopes
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            auth_method: self.method,
            ..OAuthClientConfig::default()
        };
        config.validate().map_err(|e| PluginError::AuthFailed(e.to_string()))?;

        let fetched = fetch_token(config).await.map_err(|e| {
            PluginError::AuthFailed(format!("token endpoint rejected the request: {e}"))
        })?;

        let ttl = self
            .ttl
            .min(fetched
                .expires_in
                .saturating_sub(Duration::from_secs(TOKEN_TTL_MARGIN_SECS)));
        let cache_key = key.clone();
        self.cache.put(
            &cache_key,
            CachedToken {
                key,
                token: SecretValue::new(fetched.bearer.expose().to_string()),
            },
            Some(ttl),
        );
        request.set_header("authorization", &format!("Bearer {}", fetched.bearer.expose()));
        Ok(())
    }
}

#[cfg(test)]
#[path = "oauth2_client_cred_auth_tests.rs"]
mod tests;
