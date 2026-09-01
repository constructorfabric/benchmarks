//! `oauth2_client_cred` auth plugins — `OAuth2` client credentials flow
//! (ADR-0008).
//!
//! Two registered flavors differ only in how the credentials reach the token
//! endpoint:
//! - `oauth2_client_cred.v1` — `ClientAuthMethod::Form`
//! - `oauth2_client_cred_basic.v1` — `ClientAuthMethod::Basic`
//!
//! Both shares a TTL-bounded `MemoryCache` whose key embeds the caller's
//! subject tenant, subject, auth method and a deterministic hash of the
//! plugin config, and whose values carry the original key for verification
//! on hit (hash-collision safety — a collision resolves to a miss, never to
//! another tenant's token). The subject tenant — not the upstream-owner
//! tenant — is used so a cross-tenant share never returns another tenant's
//! cached token (ADR-0008).

use std::sync::Arc;
use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use serde::Deserialize;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString};

use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, PluginError, PluginResult, RequestContext, async_trait};

use super::resolve_secret;

/// Which credential-transmission flavor this plugin instance implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuth2Flavor {
    /// Credentials in the form body (`client_id`, `client_secret`).
    Form,
    /// Credentials in the `Authorization: Basic` header.
    Basic,
}

impl OAuth2Flavor {
    fn client_auth_method(self) -> ClientAuthMethod {
        match self {
            Self::Form => ClientAuthMethod::Form,
            Self::Basic => ClientAuthMethod::Basic,
        }
    }

    fn plugin_id(self) -> &'static str {
        match self {
            Self::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            Self::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }
}

/// Parsed plugin configuration (ADR-0008).
#[derive(Debug, Deserialize, Default)]
struct OAuth2Config {
    token_endpoint: Option<String>,
    issuer_url: Option<String>,
    client_id_ref: Option<String>,
    client_secret_ref: Option<String>,
    scopes: Option<String>,
}

/// Cached token entry — carries the originating cache key for verification
/// on hit.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// `OAuth2` client credentials auth plugin (ADR-0008).
pub struct OAuth2ClientCredAuthPlugin {
    flavor: OAuth2Flavor,
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    http_config: Option<toolkit_http::HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Create the plugin with a token cache of `cache_capacity` entries whose
    /// per-entry TTL is bounded by `cache_ttl`.
    #[must_use]
    pub fn new(
        flavor: OAuth2Flavor,
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            flavor,
            credstore,
            http_config: None,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// Override the HTTP client configuration used for token fetches.
    #[must_use]
    pub fn with_http_config(mut self, http_config: Option<toolkit_http::HttpClientConfig>) -> Self {
        self.http_config = http_config;
        self
    }

    /// Deterministic, sorted hash of the config for cache-key disambiguation.
    fn config_hash(config: &serde_json::Map<String, serde_json::Value>) -> String {
        use std::collections::BTreeMap;
        use std::hash::{Hash, Hasher};
        let mut flat = BTreeMap::new();
        for (k, v) in config {
            flat.insert(k.clone(), v.to_string());
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (k, v) in flat {
            k.hash(&mut hasher);
            v.hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
    }

    fn cache_key(&self, ctx: &RequestContext) -> String {
        // Scope by the caller's subject tenant (ADR-0008 cross-tenant
        // isolation), not the upstream-owner tenant from `ctx.tenant_id`.
        format!(
            "{}:{}:{}:{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            self.flavor.tag(),
            Self::config_hash(&ctx.config)
        )
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.flavor.plugin_id()
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait declares `&str`; returns a literal
    fn plugin_type(&self) -> &str {
        "oauth2_client_cred"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let cfg: OAuth2Config =
            serde_json::from_value(serde_json::Value::Object(ctx.config.clone()))
                .map_err(|e| PluginError::config(format!("invalid oauth2 config: {e}")))?;

        let Some(client_id_ref) = cfg.client_id_ref else {
            return Err(PluginError::config(
                "oauth2 config requires 'client_id_ref'",
            ));
        };
        let Some(client_secret_ref) = cfg.client_secret_ref else {
            return Err(PluginError::config(
                "oauth2 config requires 'client_secret_ref'",
            ));
        };
        if cfg.token_endpoint.is_none() && cfg.issuer_url.is_none() {
            return Err(PluginError::config(
                "oauth2 config requires exactly one of 'token_endpoint' or 'issuer_url'",
            ));
        }
        if cfg.token_endpoint.is_some() && cfg.issuer_url.is_some() {
            return Err(PluginError::config(
                "oauth2 config: 'token_endpoint' and 'issuer_url' are mutually exclusive",
            ));
        }

        let key = self.cache_key(ctx);

        // Cache hit (with verification) → inject cached token.
        if let (Some(entry), _) = self.cache.get(&key)
            && entry.key == key
        {
            ctx.headers.insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&format!("Bearer {}", entry.token.expose())).map_err(
                    |e| PluginError::Internal {
                        detail: format!("invalid bearer header: {e}"),
                    },
                )?,
            );
            return Ok(());
        }

        // Resolve credentials → fetch token.
        let client_id = resolve_secret(&self.credstore, &ctx.security_context, &client_id_ref)
            .await
            .map_err(|e| PluginError::Internal {
                detail: format!("credential store lookup failed: {e}"),
            })?
            .ok_or_else(|| PluginError::SecretNotFound {
                detail: "referenced oauth2 client_id secret not found".to_owned(),
            })?;
        let client_secret =
            resolve_secret(&self.credstore, &ctx.security_context, &client_secret_ref)
                .await
                .map_err(|e| PluginError::Internal {
                    detail: format!("credential store lookup failed: {e}"),
                })?
                .ok_or_else(|| PluginError::SecretNotFound {
                    detail: "referenced oauth2 client_secret secret not found".to_owned(),
                })?;

        let mut oauth_cfg = OAuthClientConfig {
            token_endpoint: cfg
                .token_endpoint
                .as_deref()
                .and_then(|u| url::Url::parse(u).ok()),
            issuer_url: cfg
                .issuer_url
                .as_deref()
                .and_then(|u| url::Url::parse(u).ok()),
            client_id: String::from_utf8_lossy(&client_id).into_owned(),
            client_secret: SecretString::new(String::from_utf8_lossy(&client_secret).into_owned()),
            scopes: cfg
                .scopes
                .as_deref()
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            auth_method: self.flavor.client_auth_method(),
            ..Default::default()
        };
        if self.http_config.is_some() {
            oauth_cfg.http_config = self.http_config.clone();
        }

        let token = toolkit_auth::oauth2::fetch_token(oauth_cfg)
            .await
            .map_err(|e| PluginError::AuthFailed {
                detail: format!("oauth2 token fetch failed: {e}"),
            })?;

        let ttl = token
            .expires_in
            .checked_sub(Duration::from_secs(30))
            .unwrap_or_default()
            .min(self.cache_ttl);

        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: token.bearer.clone(),
            },
            Some(ttl),
        );

        ctx.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {}", token.bearer.expose())).map_err(
                |e| PluginError::Internal {
                    detail: format!("invalid bearer header: {e}"),
                },
            )?,
        );
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn config_hash_is_deterministic_and_key_sensitive() {
        let mut a = serde_json::Map::new();
        a.insert("scopes".into(), serde_json::json!("a b c"));
        a.insert(
            "token_endpoint".into(),
            serde_json::json!("https://x/token"),
        );
        let mut b = serde_json::Map::new();
        b.insert(
            "token_endpoint".into(),
            serde_json::json!("https://x/token"),
        );
        b.insert("scopes".into(), serde_json::json!("a b c"));
        assert_eq!(
            OAuth2ClientCredAuthPlugin::config_hash(&a),
            OAuth2ClientCredAuthPlugin::config_hash(&b)
        );

        let mut c = serde_json::Map::new();
        c.insert("scopes".into(), serde_json::json!("a b"));
        c.insert(
            "token_endpoint".into(),
            serde_json::json!("https://x/token"),
        );
        assert_ne!(
            OAuth2ClientCredAuthPlugin::config_hash(&a),
            OAuth2ClientCredAuthPlugin::config_hash(&c)
        );
    }
}
