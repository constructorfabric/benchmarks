//! `cf.core.oagw.oauth2_client_cred[_basic].v1` — OAuth2 client credentials
//! with an internal token cache (`ADR/0008-…`).
//!
//! One implementation, registered twice under different
//! [`ClientAuthMethod`]s. The cache key encodes tenant, subject, auth method
//! and a hash of the binding config so credentials can never leak across any
//! of those boundaries; the cached entry re-verifies its own key on hit,
//! which turns a `TinyUfo` hash collision into a miss rather than a
//! cross-tenant token.

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use pingora_memory_cache::MemoryCache;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, PluginError, PluginResult, RequestContext};

use super::credstore::resolve_secret;

/// Safety margin subtracted from the IdP's `expires_in` before caching.
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Gear-level cache tuning, threaded down from [`crate::config::OagwConfig`].
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached token's lifetime.
    pub ttl: Duration,
    /// Maximum entries.
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

/// Cache entry carrying its own key so a hash collision is detectable.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Arc<SecretString>,
}

/// OAuth2 client-credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build a plugin for one client-auth method.
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

    fn auth_method_tag(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "basic",
            ClientAuthMethod::Form => "form",
        }
    }

    /// Deterministic hash over the binding config: sorted key/value pairs so
    /// two logically identical configs share one cache entry.
    fn hash_config(config: &serde_json::Map<String, serde_json::Value>) -> u64 {
        use std::hash::{Hash, Hasher};
        let ordered: BTreeMap<&String, String> = config
            .iter()
            .map(|(k, v)| (k, v.to_string()))
            .collect();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (key, value) in ordered {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        hasher.finish()
    }

    fn build_cache_key(&self, ctx: &RequestContext) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            self.auth_method_tag(),
            Self::hash_config(&ctx.config),
        )
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
            ClientAuthMethod::Form => "oauth2_client_cred",
        }
    }

    fn plugin_type(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult {
        let key = self.build_cache_key(ctx);

        if let (Some(entry), _) = self.cache.get(&key)
            && entry.key == key
        {
            ctx.headers
                .set("authorization", format!("Bearer {}", entry.token.expose()));
            return Ok(());
        }

        let token_endpoint = ctx.config_str("token_endpoint").map(str::to_owned);
        let issuer_url = ctx.config_str("issuer_url").map(str::to_owned);
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::Rejected(DomainError::validation(
                "oauth2 auth plugin requires exactly one of 'token_endpoint' or 'issuer_url'",
            )));
        }
        let client_id_ref = ctx.config_str("client_id_ref").ok_or_else(|| {
            PluginError::Rejected(DomainError::authentication_failed(
                "oauth2 auth plugin requires a 'client_id_ref' config key",
            ))
        })?;
        let client_secret_ref = ctx.config_str("client_secret_ref").ok_or_else(|| {
            PluginError::Rejected(DomainError::authentication_failed(
                "oauth2 auth plugin requires a 'client_secret_ref' config key",
            ))
        })?;
        let scopes: Vec<String> = ctx
            .config_str("scopes")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        let client_id =
            resolve_secret(&self.credstore, &ctx.security_context, client_id_ref).await?;
        let client_secret =
            resolve_secret(&self.credstore, &ctx.security_context, client_secret_ref).await?;

        let parse_url = |raw: &str, field: &str| {
            url::Url::parse(raw).map_err(|err| {
                PluginError::Rejected(DomainError::validation(format!(
                    "oauth2 auth plugin '{field}' is not a valid URL: {err}"
                )))
            })
        };
        let config = OAuthClientConfig {
            token_endpoint: token_endpoint
                .as_deref()
                .map(|raw| parse_url(raw, "token_endpoint"))
                .transpose()?,
            issuer_url: issuer_url
                .as_deref()
                .map(|raw| parse_url(raw, "issuer_url"))
                .transpose()?,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: self.auth_method,
            ..OAuthClientConfig::default()
        };

        // Failed fetches are deliberately not cached: a transient IdP outage
        // must self-heal on the next request.
        let fetched = fetch_token(config).await.map_err(|err| {
            PluginError::Rejected(DomainError::authentication_failed(format!(
                "oauth2 token request failed: {err}"
            )))
        })?;

        ctx.headers
            .set("authorization", format!("Bearer {}", fetched.bearer.expose()));

        let ttl = self
            .cache_ttl
            .min(fetched.expires_in.saturating_sub(EXPIRY_SAFETY_MARGIN));
        if !ttl.is_zero() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::{mock_credstore, request_context};
    use serde_json::json;

    fn plugin(method: ClientAuthMethod) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            mock_credstore(vec![
                ("test-oauth2-client-id", "test-client-id"),
                ("test-oauth2-client-secret", "test-client-secret"),
            ]),
            method,
            Duration::from_secs(300),
            16,
        )
    }

    fn config(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn the_two_variants_advertise_distinct_gts_ids() {
        assert_eq!(
            plugin(ClientAuthMethod::Form).plugin_type(),
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID
        );
        assert_eq!(
            plugin(ClientAuthMethod::Basic).plugin_type(),
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID
        );
    }

    #[test]
    fn cache_keys_isolate_tenants_subjects_and_methods() {
        let form = plugin(ClientAuthMethod::Form);
        let basic = plugin(ClientAuthMethod::Basic);
        let cfg = config(json!({ "token_endpoint": "https://idp.example/token" }));

        let a = request_context(cfg.clone());
        let mut b = request_context(cfg.clone());
        b.security_context = toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::new_v4())
            .build()
            .expect("context");

        assert_ne!(form.build_cache_key(&a), form.build_cache_key(&b));
        assert_ne!(form.build_cache_key(&a), basic.build_cache_key(&a));
    }

    #[test]
    fn cache_keys_track_the_binding_config() {
        let form = plugin(ClientAuthMethod::Form);
        let narrow = request_context(config(json!({ "scopes": "read" })));
        let wide = request_context(config(json!({ "scopes": "read write" })));
        assert_ne!(form.build_cache_key(&narrow), form.build_cache_key(&wide));
    }

    #[test]
    fn config_hash_is_order_independent() {
        let a = config(json!({ "a": 1, "b": 2 }));
        let b = config(json!({ "b": 2, "a": 1 }));
        assert_eq!(
            OAuth2ClientCredAuthPlugin::hash_config(&a),
            OAuth2ClientCredAuthPlugin::hash_config(&b)
        );
    }

    #[tokio::test]
    async fn endpoint_and_issuer_are_mutually_exclusive() {
        let form = plugin(ClientAuthMethod::Form);
        let mut both = request_context(config(json!({
            "token_endpoint": "https://idp.example/token",
            "issuer_url": "https://idp.example",
            "client_id_ref": "cred://test-oauth2-client-id",
            "client_secret_ref": "cred://test-oauth2-client-secret"
        })));
        assert!(form.authenticate(&mut both).await.is_err());

        let mut neither = request_context(config(json!({
            "client_id_ref": "cred://test-oauth2-client-id",
            "client_secret_ref": "cred://test-oauth2-client-secret"
        })));
        assert!(form.authenticate(&mut neither).await.is_err());
    }

    #[tokio::test]
    async fn a_cached_token_is_served_without_touching_the_idp() {
        let form = plugin(ClientAuthMethod::Form);
        let cfg = config(json!({ "token_endpoint": "https://idp.invalid/token" }));
        let mut ctx = request_context(cfg);
        let key = form.build_cache_key(&ctx);
        form.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: Arc::new(SecretString::new("cached-token")),
            },
            Some(Duration::from_secs(60)),
        );
        // The endpoint is unreachable; a hit is the only way this succeeds.
        form.authenticate(&mut ctx).await.expect("cache hit");
        assert_eq!(
            ctx.headers.get("authorization"),
            Some("Bearer cached-token")
        );
    }

    #[tokio::test]
    async fn a_key_mismatch_on_hit_is_treated_as_a_miss() {
        let form = plugin(ClientAuthMethod::Form);
        let cfg = config(json!({ "token_endpoint": "https://idp.invalid/token" }));
        let mut ctx = request_context(cfg);
        let key = form.build_cache_key(&ctx);
        form.cache.put(
            &key,
            CachedToken {
                key: "someone-elses-key".to_owned(),
                token: Arc::new(SecretString::new("other-tenant-token")),
            },
            Some(Duration::from_secs(60)),
        );
        // Falls through to a real fetch, which fails against `idp.invalid`.
        assert!(form.authenticate(&mut ctx).await.is_err());
        assert_eq!(ctx.headers.get("authorization"), None);
    }
}
