//! OAuth2 client-credentials auth plugin with an internal token cache
//! (ADR-0008).
//!
//! One plugin type, registered twice — once for each RFC 6749 §2.3.1 client
//! authentication method. A cache hit is verified against the full key
//! before use, so a `TinyUfo` hash collision degrades to a miss instead of
//! handing one tenant another tenant's token.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use url::Url;

use crate::domain::error::PluginError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{AuthPlugin, RequestContext};
use crate::infra::plugin::credentials::resolve_secret;

/// Margin subtracted from the IdP's `expires_in` so a token is never served
/// within this window of its expiry.
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Cached token together with the key it was stored under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// Gear-level cache sizing, threaded in from [`crate::config::OagwConfig`].
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    /// Ceiling on a cached token's TTL.
    pub ttl: Duration,
    /// Maximum number of cached tokens.
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

/// OAuth2 client-credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build a plugin for one client authentication method.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache: TokenCacheConfig,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            cache: MemoryCache::new(cache.capacity),
            cache_ttl: cache.ttl,
        }
    }

    fn method_tag(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "basic",
            ClientAuthMethod::Form => "form",
        }
    }

    /// Cache key: tenant and subject give cross-tenant and cross-subject
    /// isolation, the method tag separates the two registrations, and the
    /// config hash separates different scopes or endpoints.
    fn cache_key(&self, ctx: &RequestContext) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            self.method_tag(),
            hash_config(&ctx.config),
        )
    }
}

/// Deterministic hash over the plugin config: sorted keys, canonical JSON.
fn hash_config(config: &serde_json::Map<String, serde_json::Value>) -> String {
    use std::hash::{Hash, Hasher};
    let mut entries: Vec<(&String, String)> =
        config.iter().map(|(k, v)| (k, v.to_string())).collect();
    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in entries {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
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
            ClientAuthMethod::Basic => gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            ClientAuthMethod::Form => gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let key = self.cache_key(ctx);
        if let (Some(entry), _) = self.cache.get(&key)
            && entry.key == key
        {
            let value = format!("Bearer {}", entry.token.expose());
            return ctx.set_header("Authorization", &value);
        }

        let token_endpoint = ctx
            .config_str(&["token_endpoint"])
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let issuer_url = ctx
            .config_str(&["issuer_url", "issuer"])
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::Config(
                "oauth2 client credentials plugin: exactly one of 'token_endpoint' or \
                 'issuer_url' must be set"
                    .to_owned(),
            ));
        }
        let client_id_ref = ctx.config_str(&["client_id_ref"]).ok_or_else(|| {
            PluginError::Config(
                "oauth2 client credentials plugin: 'client_id_ref' is required".to_owned(),
            )
        })?;
        let client_secret_ref = ctx.config_str(&["client_secret_ref"]).ok_or_else(|| {
            PluginError::Config(
                "oauth2 client credentials plugin: 'client_secret_ref' is required".to_owned(),
            )
        })?;
        let scopes: Vec<String> = ctx
            .config_str(&["scopes", "scope"])
            .map(|raw| {
                raw.split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let client_id = resolve_secret(
            self.credstore.as_ref(),
            &ctx.security_context,
            client_id_ref,
        )
        .await?;
        let client_secret = resolve_secret(
            self.credstore.as_ref(),
            &ctx.security_context,
            client_secret_ref,
        )
        .await?;

        let mut config = OAuthClientConfig {
            client_id: client_id.expose().to_owned(),
            client_secret,
            scopes,
            auth_method: self.auth_method,
            ..OAuthClientConfig::default()
        };
        if let Some(endpoint) = token_endpoint {
            config.token_endpoint = Some(parse_url("token_endpoint", endpoint)?);
        }
        if let Some(issuer) = issuer_url {
            config.issuer_url = Some(parse_url("issuer_url", issuer)?);
        }

        let fetched = fetch_token(config).await.map_err(|err| {
            // The error text is the IdP's, never the credential's.
            PluginError::Internal(format!("oauth2 token exchange failed: {err}"))
        })?;

        // `min(config_ttl, expires_in - margin)`; a token that would expire
        // inside the margin is used once and not cached.
        let ttl = fetched
            .expires_in
            .checked_sub(EXPIRY_SAFETY_MARGIN)
            .map(|remaining| remaining.min(self.cache_ttl));
        if let Some(ttl) = ttl.filter(|t| !t.is_zero()) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }

        let value = format!("Bearer {}", fetched.bearer.expose());
        ctx.set_header("Authorization", &value)
    }
}

fn parse_url(field: &str, raw: &str) -> Result<Url, PluginError> {
    Url::parse(raw).map_err(|err| {
        PluginError::Config(format!(
            "oauth2 client credentials plugin: '{field}' is not a valid URL: {err}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{OAuth2ClientCredAuthPlugin, TokenCacheConfig, hash_config};
    use crate::domain::error::PluginError;
    use crate::domain::gts_helpers as gts;
    use crate::domain::plugin::{AuthPlugin, RequestContext};
    use credstore_sdk::test_util::MockCredStoreClient;
    use http::{HeaderMap, Method};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use toolkit_auth::oauth2::ClientAuthMethod;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn plugin(method: ClientAuthMethod) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            Arc::new(MockCredStoreClient::with_secrets(vec![
                (
                    "test-oauth2-client-id".to_owned(),
                    "test-client-id".to_owned(),
                ),
                (
                    "test-oauth2-client-secret".to_owned(),
                    "test-client-secret".to_owned(),
                ),
            ])),
            method,
            TokenCacheConfig::default(),
        )
    }

    fn ctx(config: Value) -> RequestContext {
        RequestContext {
            method: Method::POST,
            path: "/v1/me".to_owned(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            config: config.as_object().cloned().unwrap_or_default(),
            security_context: SecurityContext::builder()
                .subject_id(Uuid::new_v4())
                .subject_tenant_id(Uuid::new_v4())
                .build()
                .expect("context"),
            alias: "graph.microsoft.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            route_id: None,
        }
    }

    #[test]
    fn both_variants_are_distinct_plugin_types() {
        assert_eq!(
            plugin(ClientAuthMethod::Form).plugin_type(),
            gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID
        );
        assert_eq!(
            plugin(ClientAuthMethod::Basic).plugin_type(),
            gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID
        );
    }

    #[tokio::test]
    async fn endpoint_and_issuer_are_mutually_exclusive() {
        let base = json!({
            "client_id_ref": "test-oauth2-client-id",
            "client_secret_ref": "test-oauth2-client-secret"
        });
        // Neither.
        let mut c = ctx(base.clone());
        assert!(matches!(
            plugin(ClientAuthMethod::Form)
                .authenticate(&mut c)
                .await
                .expect_err("neither"),
            PluginError::Config(_)
        ));

        // Both.
        let mut both = base.clone();
        both["token_endpoint"] = json!("https://idp.example.com/token");
        both["issuer_url"] = json!("https://idp.example.com");
        let mut c = ctx(both);
        assert!(matches!(
            plugin(ClientAuthMethod::Form)
                .authenticate(&mut c)
                .await
                .expect_err("both"),
            PluginError::Config(_)
        ));
    }

    #[tokio::test]
    async fn credential_references_are_required() {
        let mut c = ctx(json!({ "token_endpoint": "https://idp.example.com/token" }));
        assert!(matches!(
            plugin(ClientAuthMethod::Form)
                .authenticate(&mut c)
                .await
                .expect_err("client_id_ref"),
            PluginError::Config(_)
        ));
    }

    #[tokio::test]
    async fn a_malformed_endpoint_url_is_a_config_error() {
        let mut c = ctx(json!({
            "token_endpoint": "not a url",
            "client_id_ref": "test-oauth2-client-id",
            "client_secret_ref": "test-oauth2-client-secret"
        }));
        assert!(matches!(
            plugin(ClientAuthMethod::Form)
                .authenticate(&mut c)
                .await
                .expect_err("bad url"),
            PluginError::Config(_)
        ));
    }

    #[tokio::test]
    async fn a_missing_credential_is_reported_before_any_idp_call() {
        let mut c = ctx(json!({
            "token_endpoint": "https://idp.invalid/token",
            "client_id_ref": "absent",
            "client_secret_ref": "test-oauth2-client-secret"
        }));
        assert!(matches!(
            plugin(ClientAuthMethod::Form)
                .authenticate(&mut c)
                .await
                .expect_err("absent"),
            PluginError::SecretNotFound(_)
        ));
    }

    #[test]
    fn config_hash_is_order_independent_and_value_sensitive() {
        let a = json!({ "scopes": "a b", "token_endpoint": "https://x/token" });
        let b = json!({ "token_endpoint": "https://x/token", "scopes": "a b" });
        let c = json!({ "token_endpoint": "https://x/token", "scopes": "a" });
        let map = |v: &Value| v.as_object().cloned().unwrap_or_default();
        assert_eq!(hash_config(&map(&a)), hash_config(&map(&b)));
        assert_ne!(hash_config(&map(&a)), hash_config(&map(&c)));
    }

    #[test]
    fn cache_keys_isolate_tenants_subjects_and_methods() {
        let form = plugin(ClientAuthMethod::Form);
        let basic = plugin(ClientAuthMethod::Basic);
        let config = json!({ "token_endpoint": "https://x/token" });
        let one = ctx(config.clone());
        let two = ctx(config.clone());
        assert_ne!(
            form.cache_key(&one),
            form.cache_key(&two),
            "different tenants must not share a cache entry"
        );
        assert_ne!(
            form.cache_key(&one),
            basic.cache_key(&one),
            "Form and Basic must not share a cache entry"
        );
    }
}
