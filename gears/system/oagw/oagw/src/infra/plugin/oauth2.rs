// Created: 2026-09-01 by Constructor Tech
//! OAuth2 client-credentials auth plugins.
//!
//! `docs/ADR/0008-oauth2-client-credentials-auth-plugin.md`. Two variants
//! are registered, differing only in how the client authenticates to the
//! token endpoint: `oauth2_client_cred` (`Form`) and
//! `oauth2_client_cred_basic` (`Basic`). Tokens are cached with
//! `pingora-memory-cache` under a key that encodes every identity
//! component, and each cached entry carries its key back so a hash
//! collision cannot leak another tenant's token.

use std::sync::Arc;
use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};
use toolkit_http::HttpClientConfig;

use crate::domain::errors::OagwError;
use crate::domain::model::builtin_plugins;
use crate::infra::context::PluginRequest;
use crate::infra::credstore::SecretResolver;
use crate::infra::plugin::traits::AuthPlugin;

/// A cached access token, carrying its key for collision verification.
#[derive(Clone)]
struct CachedToken {
    key: String,
    bearer: toolkit_auth::oauth2::SecretString,
}

/// Config keys read from the upstream `auth.config` block.
pub mod keys {
    /// Direct token endpoint URL.
    pub const TOKEN_ENDPOINT: &str = "token_endpoint";
    /// OIDC issuer URL, resolved via discovery.
    pub const ISSUER_URL: &str = "issuer_url";
    /// `cred://` reference for the client id.
    pub const CLIENT_ID_REF: &str = "client_id_ref";
    /// `cred://` reference for the client secret.
    pub const CLIENT_SECRET_REF: &str = "client_secret_ref";
    /// Space-separated OAuth2 scopes.
    pub const SCOPES: &str = "scopes";
}

/// Cache settings threaded from `OagwConfig`.
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached token's lifetime.
    pub ttl: Duration,
    /// Maximum entries in the cache.
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

/// Injects a bearer token obtained through the client-credentials flow.
pub struct OAuth2ClientCredAuthPlugin {
    resolver: SecretResolver,
    auth_method: ClientAuthMethod,
    http_config: Option<HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .finish()
    }
}

/// Encode the identity components of a token lookup.
///
/// The `config` hash is derived from the sorted key/value pairs, so two
/// upstreams that differ only in scopes never share an entry.
#[must_use]
pub fn build_cache_key(
    tenant_id: &str,
    subject: &str,
    auth_method: ClientAuthMethod,
    config: &std::collections::BTreeMap<String, serde_json::Value>,
) -> String {
    let method_tag = match auth_method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    };
    let mut config_key = String::new();
    for (key, value) in config {
        use std::fmt::Write;
        let _ = write!(config_key, "{key}={value};");
    }
    format!("{tenant_id}:{subject}:{method_tag}:{config_key}")
}

impl OAuth2ClientCredAuthPlugin {
    /// A plugin with its own token cache.
    #[must_use]
    pub fn new(
        resolver: SecretResolver,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            resolver,
            auth_method,
            http_config: None,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// Override the HTTP client used for the token exchange.
    #[must_use]
    pub fn with_http_config(mut self, config: HttpClientConfig) -> Self {
        self.http_config = Some(config);
        self
    }

    fn cached(&self, key: &str) -> Option<toolkit_auth::oauth2::SecretString> {
        let (hit, _status) = self.cache.get(&key.to_owned());
        match hit {
            Some(entry) if entry.key == key => Some(entry.bearer),
            _ => None,
        }
    }

    async fn resolve_config(
        &self,
        request: &PluginRequest,
    ) -> Result<OAuthClientConfig, OagwError> {
        let config = &request.auth_config;
        let value_of = |key: &str| -> Option<String> {
            config.get(key).and_then(|v| v.as_str()).map(str::to_owned)
        };
        let client_id_ref = value_of(keys::CLIENT_ID_REF).ok_or_else(|| {
            OagwError::authentication_failed(
                "oauth2 client credentials plugin requires a 'client_id_ref'",
            )
        })?;
        let client_secret_ref = value_of(keys::CLIENT_SECRET_REF).ok_or_else(|| {
            OagwError::authentication_failed(
                "oauth2 client credentials plugin requires a 'client_secret_ref'",
            )
        })?;
        let client_id = self
            .resolver
            .resolve(&request.security, &client_id_ref)
            .await?;
        let client_secret = self
            .resolver
            .resolve(&request.security, &client_secret_ref)
            .await?;
        let token_endpoint = value_of(keys::TOKEN_ENDPOINT)
            .map(|u| url::Url::parse(&u))
            .transpose()
            .map_err(|err| {
                OagwError::validation_error(format!("token_endpoint is not a valid URL: {err}"))
            })?;
        let issuer_url = value_of(keys::ISSUER_URL)
            .map(|u| url::Url::parse(&u))
            .transpose()
            .map_err(|err| {
                OagwError::validation_error(format!("issuer_url is not a valid URL: {err}"))
            })?;
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(OagwError::validation_error(
                "oauth2 plugin requires either 'token_endpoint' or 'issuer_url'",
            ));
        }
        let scopes = value_of(keys::SCOPES)
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id,
            client_secret: toolkit_auth::oauth2::SecretString::new(client_secret),
            scopes,
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_secs(1800),
            jitter_max: Duration::from_secs(300),
            min_refresh_period: Duration::from_secs(10),
            default_ttl: Duration::from_secs(300),
            http_config: self.http_config.clone(),
        })
    }
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => builtin_plugins::AUTH_OAUTH2_FORM,
            ClientAuthMethod::Basic => builtin_plugins::AUTH_OAUTH2_BASIC,
        }
    }

    async fn authenticate(&self, request: &mut PluginRequest) -> Result<(), OagwError> {
        let key = build_cache_key(
            &request.tenant_id,
            request.subject.as_deref().unwrap_or(""),
            self.auth_method,
            &request.auth_config,
        );
        if let Some(bearer) = self.cached(&key) {
            request.set_header("authorization", format!("Bearer {}", bearer.expose()));
            return Ok(());
        }
        let config = self.resolve_config(request).await?;
        let fetched = fetch_token(config).await.map_err(|err| {
            OagwError::authentication_failed(format!("token exchange failed: {err}"))
        })?;
        let ttl = self
            .cache_ttl
            .min(fetched.expires_in.saturating_sub(Duration::from_secs(30)));
        self.cache.put(
            &key.clone(),
            CachedToken {
                key: key.clone(),
                bearer: fetched.bearer.clone(),
            },
            Some(ttl.max(Duration::from_secs(1))),
        );
        request.set_header(
            "authorization",
            format!("Bearer {}", fetched.bearer.expose()),
        );
        Ok(())
    }
}

/// `true` when two cache keys would be treated as the same entry.
#[must_use]
pub fn same_entry(a: &str, b: &str) -> bool {
    a == b
}

/// Build the two plugins `AuthPluginRegistry::with_builtins` registers.
#[must_use]
pub fn pair(
    resolver: SecretResolver,
    cache: TokenCacheConfig,
) -> [Arc<OAuth2ClientCredAuthPlugin>; 2] {
    [
        Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver.clone(),
            ClientAuthMethod::Form,
            cache.ttl,
            cache.capacity,
        )),
        Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver,
            ClientAuthMethod::Basic,
            cache.ttl,
            cache.capacity,
        )),
    ]
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config(entries: &[(&str, &str)]) -> BTreeMap<String, serde_json::Value> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::json!(v)))
            .collect()
    }

    #[test]
    fn cache_keys_separate_tenants_subjects_methods_and_configs() {
        let base = config(&[("scopes", "a b"), ("client_id_ref", "cred://x")]);
        let a = build_cache_key("t1", "alice", ClientAuthMethod::Form, &base);
        assert_eq!(
            a,
            build_cache_key("t1", "alice", ClientAuthMethod::Form, &base),
            "identical inputs produce identical keys"
        );
        assert_ne!(
            a,
            build_cache_key("t2", "alice", ClientAuthMethod::Form, &base),
            "tenant isolation"
        );
        assert_ne!(
            a,
            build_cache_key("t1", "bob", ClientAuthMethod::Form, &base),
            "subject isolation"
        );
        assert_ne!(
            a,
            build_cache_key("t1", "alice", ClientAuthMethod::Basic, &base),
            "method isolation"
        );
        let mut other = base.clone();
        other.insert("scopes".to_owned(), serde_json::json!("c d"));
        assert_ne!(
            a,
            build_cache_key("t1", "alice", ClientAuthMethod::Form, &other),
            "config isolation"
        );
    }

    #[test]
    fn cache_keys_are_stable_across_config_order() {
        let a = config(&[("a", "1"), ("b", "2")]);
        let b = config(&[("b", "2"), ("a", "1")]);
        assert_eq!(
            build_cache_key("t", "s", ClientAuthMethod::Form, &a),
            build_cache_key("t", "s", ClientAuthMethod::Form, &b)
        );
    }
}
