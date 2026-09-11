//! The OAuth2 client-credentials auth plugin (ADR 0008).
//!
//! Two variants share this implementation and differ only in how the client
//! authenticates at the token endpoint: `oauth2_client_cred.v1` posts the
//! credentials in the request body, `oauth2_client_cred_basic.v1` sends them
//! as HTTP Basic. Tokens are cached per `(tenant, subject, method, config)`
//! with a `CachedToken` wrapper that verifies the key on a hit so a hash
//! collision can never leak another tenant's token.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::SecretRef;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{
    ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token,
};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{BUILTIN_AUTH_OAUTH2_CC, BUILTIN_AUTH_OAUTH2_CC_BASIC};
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// Seconds shaved off the IdP-reported lifetime before caching.
const SAFETY_MARGIN_SECS: u64 = 30;

/// A cached access token carrying the key it was stored under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// Which of the two plugin variants a call came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Credentials in the request body.
    Form,
    /// Credentials as HTTP Basic.
    Basic,
}

impl Variant {
    /// The GTS plugin id of the variant.
    pub fn plugin_id(&self) -> &'static str {
        match self {
            Variant::Form => BUILTIN_AUTH_OAUTH2_CC,
            Variant::Basic => BUILTIN_AUTH_OAUTH2_CC_BASIC,
        }
    }

    /// The client-auth method handed to the token fetcher.
    pub fn auth_method(&self) -> ClientAuthMethod {
        match self {
            Variant::Form => ClientAuthMethod::Form,
            Variant::Basic => ClientAuthMethod::Basic,
        }
    }

    /// The short tag mixed into the cache key.
    fn tag(&self) -> &'static str {
        match self {
            Variant::Form => "form",
            Variant::Basic => "basic",
        }
    }
}

/// The OAuth2 client-credentials plugin.
#[derive(Clone)]
pub struct ClientCredentialsAuth {
    variant: Variant,
    credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    cache: Arc<MemoryCache<String, CachedToken>>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for ClientCredentialsAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentialsAuth")
            .field("variant", &self.variant)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl Default for ClientCredentialsAuth {
    fn default() -> Self {
        Self::new(
            Variant::Form,
            None,
            crate::config::TokenCacheConfig::default().ttl_secs,
            crate::config::TokenCacheConfig::default().capacity,
        )
    }

}

/// The plugin's declared configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCredentialsConfig {
    /// Direct token endpoint URL.
    pub token_endpoint: Option<String>,
    /// OIDC issuer URL, resolved by discovery.
    pub issuer_url: Option<String>,
    /// `cred://` reference of the client id.
    pub client_id_ref: String,
    /// `cred://` reference of the client secret.
    pub client_secret_ref: String,
    /// Space-separated scopes.
    pub scopes: Vec<String>,
}

impl ClientCredentialsConfig {
    /// Reads the plugin configuration.
    pub fn from_value(value: Option<&serde_json::Value>) -> Result<Self, PluginError> {
        let value = value.cloned().unwrap_or(serde_json::Value::Null);
        let token_endpoint = string_field(&value, "token_endpoint");
        let issuer_url = string_field(&value, "issuer_url");
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::failure(
                BUILTIN_AUTH_OAUTH2_CC,
                "config requires exactly one of `token_endpoint` or `issuer_url`",
            ));
        }
        let client_id_ref = required_string(&value, "client_id_ref")?;
        let client_secret_ref = required_string(&value, "client_secret_ref")?;
        let scopes = value
            .get("scopes")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes,
        })
    }

    /// A stable hash of the configuration, part of the cache key.
    pub fn config_hash(&self) -> String {
        let mut parts = vec![
            self.token_endpoint.clone().unwrap_or_default(),
            self.issuer_url.clone().unwrap_or_default(),
            self.client_id_ref.clone(),
            self.client_secret_ref.clone(),
        ];
        parts.push(self.scopes.join(" "));
        let joined = parts.join("|");
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in joined.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        format!("{hash:016x}")
    }
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn required_string(value: &serde_json::Value, key: &str) -> Result<String, PluginError> {
    string_field(value, key).ok_or_else(|| {
        PluginError::failure(BUILTIN_AUTH_OAUTH2_CC, format!("config requires `{key}`"))
    })
}

impl ClientCredentialsAuth {
    /// Builds a variant of the plugin.
    pub fn new(
        variant: Variant,
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
        cache_ttl_secs: u64,
        cache_capacity: usize,
    ) -> Self {
        Self {
            variant,
            credstore,
            cache: Arc::new(MemoryCache::new(cache_capacity.max(1))),
            cache_ttl: Duration::from_secs(cache_ttl_secs.max(1)),
        }
    }

    /// The `oauth2_client_cred_basic` variant, which sends its credentials as
    /// HTTP Basic at the token endpoint.
    pub fn basic() -> Self {
        Self {
            variant: Variant::Basic,
            credstore: None,
            cache: Arc::new(MemoryCache::new(
                crate::config::TokenCacheConfig::default().capacity,
            )),
            cache_ttl: Duration::from_secs(
                crate::config::TokenCacheConfig::default().ttl_secs,
            ),
        }
    }

    /// The cache key of a request, isolating tenants, subjects and configs.
    pub fn cache_key(&self, ctx: &RequestContext, config: &ClientCredentialsConfig) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.tenant_id.clone().unwrap_or_default(),
            ctx.principal_id.clone().unwrap_or_default(),
            self.variant.tag(),
            config.config_hash()
        )
    }

    pub(crate) fn ttl_for(&self, expires_in: Duration) -> Duration {
        let effective = expires_in.saturating_sub(Duration::from_secs(SAFETY_MARGIN_SECS));
        effective.min(self.cache_ttl).max(Duration::from_secs(1))
    }
}

#[async_trait]
impl AuthPlugin for ClientCredentialsAuth {
    fn id(&self) -> &str {
        self.variant.plugin_id()
    }

    fn plugin_type(&self) -> &str {
        crate::domain::gts_helpers::AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = ClientCredentialsConfig::from_value(ctx.plugin_config.as_ref())?;
        let key = self.cache_key(ctx, &config);
        if let (Some(entry), _) = self.cache.get(&key) {
            if entry.key == key {
                ctx.set_header(
                    "authorization",
                    format!("Bearer {}", entry.token.expose()),
                );
                return Ok(());
            }
        }
        let client_id = self
            .resolve(&config.client_id_ref, ctx)
            .await?;
        let client_secret = self.resolve(&config.client_secret_ref, ctx).await?;
        let mut oauth = OAuthClientConfig::default();
        oauth.auth_method = self.variant.auth_method();
        if let Some(endpoint) = config.token_endpoint.as_deref() {
            oauth.token_endpoint = Some(url::Url::parse(endpoint).map_err(|err| {
                PluginError::failure(
                    self.variant.plugin_id(),
                    format!("`token_endpoint` is not a URL: {err}"),
                )
            })?);
        }
        if let Some(issuer) = config.issuer_url.as_deref() {
            oauth.issuer_url = Some(url::Url::parse(issuer).map_err(|err| {
                PluginError::failure(
                    self.variant.plugin_id(),
                    format!("`issuer_url` is not a URL: {err}"),
                )
            })?);
        }
        oauth.client_id = client_id;
        oauth.client_secret = SecretString::new(client_secret);
        oauth.scopes = config.scopes.clone();
        let fetched = fetch_token(oauth).await.map_err(|err| {
            PluginError::Reject(DomainError::AuthenticationFailed {
                detail: format!("token fetch failed: {err}"),
                plugin_id: Some(self.variant.plugin_id().to_string()),
            })
        })?;
        let ttl = self.ttl_for(fetched.expires_in);
        self.cache.put(
            &key,
            CachedToken { key: key.clone(), token: fetched.bearer.clone() },
            Some(ttl),
        );
        ctx.set_header("authorization", format!("Bearer {}", fetched.bearer.expose()));
        Ok(())
    }
}

impl ClientCredentialsAuth {
    async fn resolve(&self, reference: &str, ctx: &RequestContext) -> Result<String, PluginError> {
        let client = self.credstore.as_ref().ok_or_else(|| {
            PluginError::failure(self.variant.plugin_id(), "no CredStore client is wired to the gear")
        })?;
        let bare = reference
            .strip_prefix("cred://")
            .unwrap_or(reference)
            .trim_start_matches('/');
        let secret_ref = SecretRef::new(bare).map_err(|err| {
            PluginError::failure(
                self.variant.plugin_id(),
                format!("invalid credential reference `{reference}`: {err}"),
            )
        })?;
        let security = super::security_context::service_context(ctx);
        match client.get(&security, &secret_ref).await {
            Ok(Some(response)) => {
                let value = String::from_utf8_lossy(response.value.as_bytes()).to_string();
                if value.is_empty() {
                    Err(PluginError::failure(
                        self.variant.plugin_id(),
                        format!("credential `{reference}` resolved to an empty value"),
                    ))
                } else {
                    Ok(value)
                }
            }
            Ok(None) => Err(PluginError::Reject(DomainError::SecretNotFound {
                detail: format!("credential `{reference}` does not exist"),
                plugin_id: Some(self.variant.plugin_id().to_string()),
            })),
            Err(err) => Err(PluginError::Reject(DomainError::AuthenticationFailed {
                detail: format!("credential lookup failed: {err}"),
                plugin_id: Some(self.variant.plugin_id().to_string()),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_requires_exactly_one_endpoint() {
        assert!(ClientCredentialsConfig::from_value(None).is_err());
        assert!(
            ClientCredentialsConfig::from_value(Some(&serde_json::json!({
                "token_endpoint": "https://idp/token",
                "issuer_url": "https://idp",
                "client_id_ref": "cred://a",
                "client_secret_ref": "cred://b"
            })))
            .is_err()
        );
    }

    #[test]
    fn the_config_reads_every_declared_key() {
        let config = ClientCredentialsConfig::from_value(Some(&serde_json::json!({
            "token_endpoint": "https://idp/token",
            "client_id_ref": "cred://a",
            "client_secret_ref": "cred://b",
            "scopes": "a b  c"
        })))
        .unwrap();
        assert_eq!(config.token_endpoint.as_deref(), Some("https://idp/token"));
        assert_eq!(config.client_id_ref, "cred://a");
        assert_eq!(config.client_secret_ref, "cred://b");
        assert_eq!(config.scopes, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn the_cache_key_separates_tenants_subjects_and_configs() {
        let plugin = ClientCredentialsAuth::default();
        let config = ClientCredentialsConfig {
            token_endpoint: Some("https://idp/token".to_string()),
            issuer_url: None,
            client_id_ref: "cred://a".to_string(),
            client_secret_ref: "cred://b".to_string(),
            scopes: vec![],
        };
        let mut base = RequestContext::default();
        base.tenant_id = Some("11111111-1111-1111-1111-111111111111".to_string());
        base.principal_id = Some("alice".to_string());

        let mut other_tenant = base.clone();
        other_tenant.tenant_id = Some("22222222-2222-2222-2222-222222222222".to_string());
        assert_ne!(plugin.cache_key(&base, &config), plugin.cache_key(&other_tenant, &config));

        let mut other_subject = base.clone();
        other_subject.principal_id = Some("bob".to_string());
        assert_ne!(plugin.cache_key(&base, &config), plugin.cache_key(&other_subject, &config));

        let mut other_config = config.clone();
        other_config.scopes = vec!["d".to_string()];
        assert_ne!(plugin.cache_key(&base, &config), plugin.cache_key(&base, &other_config));
    }

    #[test]
    fn the_ttl_never_exceeds_the_configured_ceiling() {
        let plugin = ClientCredentialsAuth::default();
        assert_eq!(plugin.ttl_for(Duration::from_secs(3600)), plugin.cache_ttl);
        // `expires_in` below the margin still yields a positive TTL.
        assert!(plugin.ttl_for(Duration::from_secs(10)) >= Duration::from_secs(1));
    }

    #[test]
    fn the_variants_use_their_own_plugin_ids() {
        assert_eq!(Variant::Form.plugin_id(), BUILTIN_AUTH_OAUTH2_CC);
        assert_eq!(Variant::Basic.plugin_id(), BUILTIN_AUTH_OAUTH2_CC_BASIC);
    }
}
