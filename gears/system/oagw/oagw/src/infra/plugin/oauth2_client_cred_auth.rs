//! `OAuth2ClientCredAuthPlugin` — client-credentials flow with an internal
//! token cache (ADR-0008).
//!
//! Configuration (`ctx.config` keys), per ADR-0008:
//!
//! | Key | Required | Description |
//! |---|---|---|
//! | `token_endpoint` | xor `issuer_url` | Direct token endpoint URL |
//! | `issuer_url` | xor `token_endpoint` | OIDC issuer for discovery |
//! | `client_id_ref` | yes | `cred://` reference for the client id |
//! | `client_secret_ref` | yes | `cred://` reference for the client secret |
//! | `scopes` | no | Space-separated scope list |
//!
//! Cache key = `(tenant, subject, auth_method, config-hash)` with a
//! `CachedToken` wrapper that re-verifies the key on hit, so a `TinyUfo` hash
//! collision can never leak another tenant's token.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_utils::SecretString;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID, OAUTH2_CLIENT_CRED_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, RequestContext};

/// Which client-auth variant this plugin instance implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Credentials in the request body.
    Form,
    /// Credentials in the `Authorization: Basic` header.
    Basic,
}

impl Variant {
    fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }

    fn plugin_id(self) -> &'static str {
        match self {
            Self::Form => OAUTH2_CLIENT_CRED_PLUGIN_ID,
            Self::Basic => OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID,
        }
    }
}

/// Cached token plus the cache key it belongs to (ADR-0008 collision safety).
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// OAuth2 client-credentials plugin with a TTL token cache.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    variant: Variant,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("variant", &self.variant)
            .field("has_credstore", &self.credstore.is_some())
            .field("cache_ttl", &self.cache_ttl)
            .finish()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the plugin with an in-process token cache.
    #[must_use]
    pub fn new(
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
        variant: Variant,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            credstore,
            variant,
            cache: MemoryCache::new(cache_capacity.max(1)),
            cache_ttl,
        }
    }

    /// Deterministic, tenant/subject-scoped cache key (ADR-0008).
    fn build_cache_key(ctx: &RequestContext, variant: Variant) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.tenant_id,
            ctx.security_context.subject_id(),
            variant.tag(),
            Self::config_signature(&ctx.config),
        )
    }

    fn config_signature(config: &serde_json::Value) -> String {
        use std::collections::BTreeMap;
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        if let Some(obj) = config.as_object() {
            for (key, value) in obj {
                let value = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                map.insert(key.clone(), value);
            }
        }
        let mut signature = String::new();
        for (key, value) in &map {
            signature.push_str(key);
            signature.push('=');
            signature.push_str(&value);
            signature.push('&');
        }
        signature
    }

    async fn resolve_secret(
        &self,
        ctx: &RequestContext,
        reference: &str,
    ) -> OagwResult<SecretString> {
        let credstore = self.credstore.as_ref().ok_or_else(|| {
            OagwError::SecretNotFound(format!(
                "credential '{reference}' cannot be resolved: credstore unavailable"
            ))
        })?;
        let secret_ref = credstore_sdk::SecretRef::new(reference.to_owned())
            .map_err(|err| OagwError::Validation(format!("invalid credential reference: {err}")))?;
        match credstore.get(&ctx.security_context, &secret_ref).await {
            Ok(Some(response)) => {
                let value = String::from_utf8_lossy(response.value.as_bytes()).to_string();
                Ok(SecretString::new(value))
            }
            Ok(None) => Err(OagwError::SecretNotFound(format!(
                "credential '{reference}' not found"
            ))),
            Err(err) => Err(OagwError::SecretNotFound(format!(
                "credential '{reference}' lookup failed: {err}"
            ))),
        }
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.variant.plugin_id()
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> OagwResult<()> {
        let key = Self::build_cache_key(ctx, self.variant);
        let (cached, _status) = self.cache.get(&key);
        if let Some(cached) = cached {
            if cached.key == key {
                inject_bearer(&mut ctx.headers, cached.token.expose());
                return Ok(());
            }
        }

        let config = parse_config(&ctx.config)?;
        let client_id = self.resolve_secret(ctx, &config.client_id_ref).await?;
        let client_secret = self.resolve_secret(ctx, &config.client_secret_ref).await?;

        let scopes: Vec<String> = config
            .scopes
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        let oauth_config = toolkit_auth::oauth2::OAuthClientConfig {
            token_endpoint: config
                .token_endpoint
                .as_deref()
                .map(url::Url::parse)
                .transpose()
                .map_err(|err| {
                    OagwError::Validation(format!("token_endpoint is not a valid URL: {err}"))
                })?,
            issuer_url: config
                .issuer_url
                .as_deref()
                .map(url::Url::parse)
                .transpose()
                .map_err(|err| {
                    OagwError::Validation(format!("issuer_url is not a valid URL: {err}"))
                })?,
            client_id: client_id.expose().to_owned(),
            client_secret: client_secret.clone(),
            scopes,
            auth_method: match self.variant {
                Variant::Form => toolkit_auth::oauth2::ClientAuthMethod::Form,
                Variant::Basic => toolkit_auth::oauth2::ClientAuthMethod::Basic,
            },
            ..toolkit_auth::oauth2::OAuthClientConfig::default()
        };

        let fetched = toolkit_auth::oauth2::fetch_token(oauth_config)
            .await
            .map_err(|err| OagwError::AuthenticationFailed(format!("token fetch failed: {err}")))?;

        // Cache TTL = min(configured ceiling, expires_in - 30s safety margin).
        let ttl = self
            .cache_ttl
            .min(fetched.expires_in.saturating_sub(Duration::from_secs(30)))
            .max(Duration::from_secs(1));
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: fetched.bearer.clone(),
            },
            Some(ttl),
        );
        inject_bearer(&mut ctx.headers, fetched.bearer.expose());
        Ok(())
    }
}

fn inject_bearer(headers: &mut http::HeaderMap, token: &str) {
    if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(http::header::AUTHORIZATION, value);
    }
}

#[derive(Debug)]
struct OAuth2PluginConfig {
    token_endpoint: Option<String>,
    issuer_url: Option<String>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Option<String>,
}

fn parse_config(config: &serde_json::Value) -> OagwResult<OAuth2PluginConfig> {
    let obj = config.as_object().ok_or_else(|| {
        OagwError::Validation("oauth2 plugin requires an object configuration".to_owned())
    })?;
    let get_str = |key: &str| obj.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
    let token_endpoint = get_str("token_endpoint");
    let issuer_url = get_str("issuer_url");
    if token_endpoint.is_some() && issuer_url.is_some() {
        return Err(OagwError::Validation(
            "oauth2 plugin requires exactly one of {token_endpoint, issuer_url}".to_owned(),
        ));
    }
    if token_endpoint.is_none() && issuer_url.is_none() {
        return Err(OagwError::Validation(
            "oauth2 plugin requires token_endpoint or issuer_url".to_owned(),
        ));
    }
    let client_id_ref = get_str("client_id_ref").ok_or_else(|| {
        OagwError::Validation("oauth2 plugin requires client_id_ref".to_owned())
    })?;
    let client_secret_ref = get_str("client_secret_ref").ok_or_else(|| {
        OagwError::Validation("oauth2 plugin requires client_secret_ref".to_owned())
    })?;
    Ok(OAuth2PluginConfig {
        token_endpoint,
        issuer_url,
        client_id_ref,
        client_secret_ref,
        scopes: get_str("scopes"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::request_context;

    #[test]
    fn config_signature_is_order_independent() {
        let a = serde_json::json!({"token_endpoint": "https://idp/token", "scopes": "read"});
        let b = serde_json::json!({"scopes": "read", "token_endpoint": "https://idp/token"});
        assert_eq!(
            OAuth2ClientCredAuthPlugin::config_signature(&a),
            OAuth2ClientCredAuthPlugin::config_signature(&b)
        );
    }

    #[test]
    fn cache_key_is_tenant_scoped() {
        let mut ctx = request_context();
        ctx.config = serde_json::json!({"token_endpoint": "https://idp/token"});
        let key_a = OAuth2ClientCredAuthPlugin::build_cache_key(&ctx, Variant::Form);
        let mut other = request_context();
        other.config = ctx.config.clone();
        let key_b = OAuth2ClientCredAuthPlugin::build_cache_key(&other, Variant::Form);
        assert_ne!(key_a, key_b, "different tenants must not share cache entries");
    }

    #[test]
    fn rejects_configs_without_endpoint() {
        let err = parse_config(&serde_json::json!({})).unwrap_err();
        assert!(matches!(err, OagwError::Validation(_)));
    }

    #[test]
    fn rejects_both_endpoints() {
        let err = parse_config(&serde_json::json!({
            "token_endpoint": "https://idp/token",
            "issuer_url": "https://idp",
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret"
        }))
        .unwrap_err();
        assert!(matches!(err, OagwError::Validation(_)));
    }

    #[tokio::test]
    async fn missing_credentials_fail_without_calling_idp() {
        let mut ctx = request_context();
        ctx.config = serde_json::json!({
            "token_endpoint": "https://idp.invalid/token",
            "client_id_ref": "cred://missing-id",
            "client_secret_ref": "cred://missing-secret"
        });
        let plugin = OAuth2ClientCredAuthPlugin::new(None, Variant::Form, Duration::from_secs(60), 16);
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, OagwError::SecretNotFound(_)));
        assert!(ctx.headers.get(http::header::AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn variant_selects_plugin_id() {
        assert_eq!(Variant::Form.plugin_id(), OAUTH2_CLIENT_CRED_PLUGIN_ID);
        assert_eq!(
            Variant::Basic.plugin_id(),
            OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID
        );
    }
}
