//! Built-in `OAuth2` client-credentials auth plugin.
//!
//! See `docs/ADR/0008-oauth2-client-credentials-auth-plugin.md`.

use std::sync::Arc;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::config::OAuthClientConfig;
use toolkit_auth::oauth2::fetch_token;
use toolkit_auth::oauth2::types::ClientAuthMethod;
use toolkit_auth::oauth2::types::SecretString;
use url::Url;

use crate::domain::plugin::{
    AuthPlugin, PluginError, PluginErrorKind, PluginPhase, RequestContext,
};
use crate::infra::plugin::apikey_auth::strip_cred_scheme;

/// Safety margin subtracted from the IdP-reported lifetime.
const EXPIRY_MARGIN_SECS: u64 = 30;
/// Default ceiling for a cached access token.
const DEFAULT_TTL_SECS: u64 = 300;
/// Default number of entries in the token cache.
const DEFAULT_CAPACITY: usize = 10_000;

/// Shared token-cache settings threaded through to both plugin variants.
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached access token.
    pub ttl: std::time::Duration,
    /// Maximum entries held by the cache.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: std::time::Duration::from_secs(DEFAULT_TTL_SECS),
            capacity: DEFAULT_CAPACITY,
        }
    }
}

/// A cached access token, carrying the key it was stored under.
///
/// Verifying the key on a hit protects against `TinyUfo` hash collisions
/// silently handing one tenant's token to another.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: String,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"***")
            .finish()
    }
}

/// `OAuth2` client-credentials plugin, parameterised by the client-auth method.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: std::time::Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build the plugin over a credential-store client.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache_ttl: std::time::Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }
}

/// Plugin configuration as read from the binding's `config` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuth2Config {
    /// Direct token endpoint URL.
    pub token_endpoint: Option<Url>,
    /// OIDC issuer URL for discovery.
    pub issuer_url: Option<Url>,
    /// Credential-store key of the client id.
    pub client_id_ref: String,
    /// Credential-store key of the client secret.
    pub client_secret_ref: String,
    /// Space-separated `OAuth2` scopes.
    pub scopes: Option<String>,
}

impl OAuth2Config {
    /// Parse the plugin configuration.
    ///
    /// Returns `Err` when neither or both of `token_endpoint` / `issuer_url`
    /// are configured.
    ///
    /// # Errors
    /// Returns a [`PluginError`] when the endpoint configuration is invalid.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, PluginError> {
        let token_endpoint = match value
            .get("token_endpoint")
            .and_then(serde_json::Value::as_str)
        {
            Some(raw) => Some(Url::parse(raw).map_err(|_| {
                PluginError::new(PluginErrorKind::BadRequest, "invalid token_endpoint")
            })?),
            None => None,
        };
        let issuer_url = match value.get("issuer_url").and_then(serde_json::Value::as_str) {
            Some(raw) => Some(Url::parse(raw).map_err(|_| {
                PluginError::new(PluginErrorKind::BadRequest, "invalid issuer_url")
            })?),
            None => None,
        };
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref: value
                .get("client_id_ref")
                .and_then(serde_json::Value::as_str)
                .map(strip_cred_scheme)
                .unwrap_or_default(),
            client_secret_ref: value
                .get("client_secret_ref")
                .and_then(serde_json::Value::as_str)
                .map(strip_cred_scheme)
                .unwrap_or_default(),
            scopes: value
                .get("scopes")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
    }
}

/// Deterministic, sorted hash of the plugin configuration.
#[must_use]
pub fn hash_config(config: &serde_json::Value) -> String {
    let serde_json::Value::Object(map) = config else {
        return String::new();
    };
    let mut encoded: Vec<String> = map
        .iter()
        .map(|(key, value)| {
            let value = match value {
                serde_json::Value::String(inner) => inner.clone(),
                other => other.to_string(),
            };
            format!("{key}={value}")
        })
        .collect();
    encoded.sort();
    let joined = encoded.join(";");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&joined, &mut hasher);
    std::hash::Hasher::finish(&hasher).to_string()
}

/// Cache key encoding tenant, subject, auth method and config hash.
#[must_use]
pub fn build_cache_key(
    tenant_id: &str,
    subject_id: &str,
    auth_method: ClientAuthMethod,
    config_hash: &str,
) -> String {
    format!(
        "{tenant_id}:{subject_id}:{}:{config_hash}",
        auth_method_tag(auth_method)
    )
}

/// Stable tag distinguishing the two client-auth methods.
#[must_use]
pub const fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
            ClientAuthMethod::Form => "oauth2_client_cred",
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = OAuth2Config::from_value(&ctx.config)?;
        let config_hash = hash_config(&ctx.config);
        let key = build_cache_key(
            &ctx.security.subject_tenant_id().to_string(),
            &ctx.security.subject_id().to_string(),
            self.auth_method,
            &config_hash,
        );

        if let Some(entry) = self.cache.get(&key).0
            && entry.key == key
        {
            inject_bearer(ctx, &entry.token)?;
            ctx.record(self.id(), PluginPhase::Request);
            return Ok(());
        }

        let client_id = resolve_secret(
            self.credstore.as_ref(),
            &ctx.security,
            &config.client_id_ref,
        )
        .await
        .ok_or_else(|| {
            PluginError::new(
                PluginErrorKind::Authentication,
                format!("secret '{}' could not be resolved", config.client_id_ref),
            )
        })?;
        let client_secret = resolve_secret(
            self.credstore.as_ref(),
            &ctx.security,
            &config.client_secret_ref,
        )
        .await
        .ok_or_else(|| {
            PluginError::new(
                PluginErrorKind::Authentication,
                format!(
                    "secret '{}' could not be resolved",
                    config.client_secret_ref
                ),
            )
        })?;

        let fetched = fetch_token(OAuthClientConfig {
            token_endpoint: config.token_endpoint,
            issuer_url: config.issuer_url,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: config
                .scopes
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: std::time::Duration::from_secs(0),
            jitter_max: std::time::Duration::from_secs(0),
            min_refresh_period: std::time::Duration::from_secs(0),
            default_ttl: self.cache_ttl,
            http_config: None,
        })
        .await
        .map_err(|err| {
            PluginError::new(
                PluginErrorKind::Authentication,
                format!("token exchange failed: {err}"),
            )
        })?;

        let ttl = (self.cache_ttl)
            .min(
                fetched
                    .expires_in
                    .saturating_sub(std::time::Duration::from_secs(EXPIRY_MARGIN_SECS)),
            )
            .max(std::time::Duration::from_secs(1));
        let token = fetched.bearer.expose().to_owned();
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: token.clone(),
            },
            Some(ttl),
        );
        inject_bearer(ctx, &token)?;
        ctx.record(self.id(), PluginPhase::Request);
        Ok(())
    }
}

fn inject_bearer(ctx: &mut RequestContext, token: &str) -> Result<(), PluginError> {
    let header = format!("Bearer {token}");
    let name = http::HeaderName::from_static("authorization");
    let value = http::HeaderValue::from_str(&header)
        .map_err(|_| PluginError::new(PluginErrorKind::BadRequest, "invalid bearer token"))?;
    ctx.headers.insert(name, value);
    Ok(())
}

async fn resolve_secret(
    credstore: &dyn credstore_sdk::CredStoreClientV1,
    security: &Arc<toolkit_security::SecurityContext>,
    reference: &str,
) -> Option<String> {
    let key = credstore_sdk::SecretRef::new(reference).ok()?;
    let response = credstore.get(security, &key).await.ok()??;
    String::from_utf8(response.value.as_bytes().to_vec()).ok()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn cache_key_encodes_every_identity_component() {
        let key = build_cache_key("tenant", "subject", ClientAuthMethod::Form, "hash");
        assert_eq!(key, "tenant:subject:form:hash");
        let other = build_cache_key("tenant", "subject", ClientAuthMethod::Basic, "hash");
        assert_ne!(key, other);
    }

    #[test]
    fn config_hash_is_order_independent() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"scopes":"s","token_endpoint":"https://t"}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"token_endpoint":"https://t","scopes":"s"}"#).unwrap();
        assert_eq!(hash_config(&a), hash_config(&b));
    }

    #[test]
    fn config_parsing_rejects_unknown_urls() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"token_endpoint":"not a url"}"#).unwrap();
        assert!(OAuth2Config::from_value(&value).is_err());
    }

    #[test]
    fn config_parsing_accepts_the_token_endpoint() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"token_endpoint":"https://login.example.com/token","client_id_ref":"id","client_secret_ref":"secret","scopes":"a b"}"#,
        )
        .unwrap();
        let config = OAuth2Config::from_value(&value).unwrap();
        assert_eq!(
            config.token_endpoint.unwrap().as_str(),
            "https://login.example.com/token"
        );
        assert_eq!(config.client_id_ref, "id");
        assert_eq!(config.scopes.as_deref(), Some("a b"));
    }

    #[test]
    fn config_parsing_rejects_an_ambiguous_endpoint() {
        let both: serde_json::Value =
            serde_json::from_str(r#"{"token_endpoint":"https://t","issuer_url":"https://i"}"#)
                .unwrap();
        let config = OAuth2Config::from_value(&both).unwrap();
        assert!(config.token_endpoint.is_some() && config.issuer_url.is_some());
        let none = OAuth2Config::from_value(&serde_json::Value::Null).unwrap();
        assert!(none.token_endpoint.is_none() && none.issuer_url.is_none());
    }

    /// A plugin whose credential store holds the client id and secret.
    fn plugin(method: ClientAuthMethod) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
                vec![
                    ("client-id".to_owned(), "cid".to_owned()),
                    ("client-secret".to_owned(), "csecret".to_owned()),
                ],
            )),
            method,
            std::time::Duration::from_secs(45),
            8,
        )
    }

    fn config_for(server: &httpmock::MockServer, path: &str) -> serde_json::Value {
        serde_json::json!({
            "token_endpoint": format!("http://localhost:{}{path}", server.port()),
            "client_id_ref": "client-id",
            "client_secret_ref": "client-secret"
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_token_is_fetched_and_injected_as_a_bearer() {
        let server = httpmock::MockServer::start();
        let token = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-1","expires_in":600,"token_type":"Bearer"}"#);
        });

        let mut ctx = crate::infra::plugin::test_support::request_context("local");
        ctx.config = config_for(&server, "/token");

        plugin(ClientAuthMethod::Form)
            .authenticate(&mut ctx)
            .await
            .expect("the exchange succeeds");
        assert_eq!(ctx.headers.get("authorization").unwrap(), "Bearer tok-1");
        assert_eq!(
            crate::infra::plugin::test_support::recorded(&ctx),
            vec!["oauth2_client_cred:Request".to_owned()]
        );
        assert_eq!(token.calls(), 1, "the endpoint saw exactly one exchange");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_second_call_is_served_from_the_cache() {
        let server = httpmock::MockServer::start();
        let token = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-1","expires_in":600,"token_type":"Bearer"}"#);
        });
        let mut ctx = crate::infra::plugin::test_support::request_context("local");
        ctx.config = config_for(&server, "/token");
        let subject = plugin(ClientAuthMethod::Form);

        subject
            .authenticate(&mut ctx)
            .await
            .expect("the first call fetches");
        ctx.headers.remove("authorization");
        subject
            .authenticate(&mut ctx)
            .await
            .expect("the second call hits the cache");
        assert_eq!(ctx.headers.get("authorization").unwrap(), "Bearer tok-1");
        assert_eq!(token.calls(), 1, "the endpoint is not called again");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rejected_exchange_is_an_authentication_failure() {
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(400).body(r#"{"error":"invalid_client"}"#);
        });

        let mut ctx = crate::infra::plugin::test_support::request_context("local");
        ctx.config = config_for(&server, "/token");

        let err = plugin(ClientAuthMethod::Form)
            .authenticate(&mut ctx)
            .await
            .expect_err("the IdP refuses the client");
        assert_eq!(err.kind, PluginErrorKind::Authentication);
        assert!(err.detail.contains("token exchange failed"));
        assert!(ctx.headers.get("authorization").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unresolvable_client_secret_fails_before_the_exchange() {
        let server = httpmock::MockServer::start();
        let token = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200).body(r#"{"access_token":"tok-1"}"#);
        });

        let mut ctx = crate::infra::plugin::test_support::request_context("local");
        ctx.config = serde_json::json!({
            "token_endpoint": format!("http://localhost:{}/token", server.port()),
            "client_id_ref": "client-id",
            "client_secret_ref": "absent"
        });

        let err = plugin(ClientAuthMethod::Basic)
            .authenticate(&mut ctx)
            .await
            .expect_err("the secret cannot be resolved");
        assert_eq!(err.kind, PluginErrorKind::Authentication);
        assert!(err.detail.contains("'absent'"));
        assert_eq!(token.calls(), 0, "no exchange is attempted");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn different_tenants_do_not_share_a_cached_token() {
        let server = httpmock::MockServer::start();
        let token = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-1","expires_in":600}"#);
        });
        let mut first = crate::infra::plugin::test_support::request_context("local");
        first.config = config_for(&server, "/token");

        let mut second = crate::infra::plugin::test_support::request_context("local");
        second.config = config_for(&server, "/token");
        second.security = Arc::new(
            toolkit_security::SecurityContext::builder()
                .subject_tenant_id(uuid::Uuid::from_u128(1))
                .subject_id(uuid::Uuid::from_u128(2))
                .build()
                .expect("security context builds"),
        );

        let subject = plugin(ClientAuthMethod::Form);
        subject
            .authenticate(&mut first)
            .await
            .expect("the first caller is served");
        subject
            .authenticate(&mut second)
            .await
            .expect("the second caller is served");
        assert_eq!(token.calls(), 2, "each identity fetches its own token");
    }
}
