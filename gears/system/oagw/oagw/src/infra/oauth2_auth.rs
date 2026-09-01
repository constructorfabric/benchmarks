//! `OAuth2` client-credentials auth plugin (ADR-0008).
//!
//! Fetches a token from the `IdP` on cache miss for a `(tenant, subject,
//! auth_method, config)` tuple, caches it with
//! `min(config_ttl, expires_in - 30s)`, and injects
//! `Authorization: Bearer <token>` on every proxied request. Failed fetches
//! are never cached (the next request retries the `IdP`).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{
    ClientAuthMethod, FetchedToken, OAuthClientConfig, SecretString, fetch_token,
};
use toolkit_http::HttpClientConfig;
use url::Url;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestCtx};
use crate::infra::secrets::lookup_secret;

/// Cached token entry keyed by the full isolation tuple (ADR-0008
/// "Hash-Collision Safety"): `TinyUfo` hashes keys to `u64` without `Eq`
/// resolution, so the original key is verified on every hit.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// `OAuth2` client-credentials auth plugin (one instance per auth method).
pub struct OAuth2ClientCredAuthPlugin {
    id: &'static str,
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    token_http_config: Option<HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
    /// Single-flight guard: concurrent cache misses wait on one fetch instead
    /// of stampeding the identity provider (a std-gated futures mutex; tokio
    /// sync primitives are not in the lib dependency set).
    fetch_lock: Arc<futures_util::lock::Mutex<()>>,
    /// When `false` (default), `token_endpoint`/`issuer_url` must be HTTPS.
    allow_insecure_token_endpoint: bool,
}

impl OAuth2ClientCredAuthPlugin {
    /// Construct the plugin with the given `auth_method`.
    #[must_use]
    pub fn new(
        id: &'static str,
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache_config: TokenCacheConfig,
        token_http_config: Option<HttpClientConfig>,
        allow_insecure_token_endpoint: bool,
    ) -> Self {
        Self {
            id,
            credstore,
            auth_method,
            token_http_config,
            cache: MemoryCache::new(cache_config.capacity),
            cache_ttl: Duration::from_secs(cache_config.ttl_secs),
            fetch_lock: Arc::new(futures_util::lock::Mutex::new(())),
            allow_insecure_token_endpoint,
        }
    }
}

/// Parsed plugin configuration (config keys per ADR-0008).
struct OAuth2PluginConfig {
    token_endpoint: Option<Url>,
    issuer_url: Option<Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
    cache_ttl: Duration,
}

fn parse_config(
    config: &std::collections::BTreeMap<String, serde_json::Value>,
    fallback_ttl: Duration,
    allow_insecure: bool,
) -> Result<OAuth2PluginConfig, PluginError> {
    let token_endpoint = match config.get("token_endpoint") {
        Some(v) => Some(
            v.as_str()
                .and_then(|s| Url::parse(s).ok())
                .ok_or_else(|| PluginError::Config {
                    detail: "token_endpoint must be a valid URL".to_owned(),
                })
                .and_then(|url| secure_endpoint("token_endpoint", url, allow_insecure))?,
        ),
        None => None,
    };
    let issuer_url = match config.get("issuer_url") {
        Some(v) => Some(
            v.as_str()
                .and_then(|s| Url::parse(s).ok())
                .ok_or_else(|| PluginError::Config {
                    detail: "issuer_url must be a valid URL".to_owned(),
                })
                .and_then(|url| secure_endpoint("issuer_url", url, allow_insecure))?,
        ),
        None => None,
    };
    if token_endpoint.is_some() && issuer_url.is_some() {
        return Err(PluginError::Config {
            detail: "token_endpoint and issuer_url are mutually exclusive".to_owned(),
        });
    }
    if token_endpoint.is_none() && issuer_url.is_none() {
        return Err(PluginError::Config {
            detail: "one of token_endpoint|issuer_url is required".to_owned(),
        });
    }
    let client_id_ref = config
        .get("client_id_ref")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| PluginError::Config {
            detail: "client_id_ref (cred://...) is required".to_owned(),
        })?;
    let client_secret_ref = config
        .get("client_secret_ref")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| PluginError::Config {
            detail: "client_secret_ref (cred://...) is required".to_owned(),
        })?;
    let scopes = config
        .get("scopes")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let cache_ttl = config
        .get("token_cache_ttl_secs")
        .and_then(serde_json::Value::as_u64)
        .map_or(fallback_ttl, Duration::from_secs);
    Ok(OAuth2PluginConfig {
        token_endpoint,
        issuer_url,
        client_id_ref,
        client_secret_ref,
        scopes,
        cache_ttl,
    })
}

/// Reject endpoints that would send credentials in clear text, unless the
/// operator explicitly opted into insecure token endpoints.
fn secure_endpoint(field: &str, url: Url, allow_insecure: bool) -> Result<Url, PluginError> {
    if url.scheme() != "https" && !allow_insecure {
        return Err(PluginError::Config {
            detail: format!("{field} must use https (got scheme {:?})", url.scheme()),
        });
    }
    Ok(url)
}

fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Form => "form",
        ClientAuthMethod::Basic => "basic",
    }
}

/// Deterministic, sorted hash of all config entries (isolation component).
fn hash_config(config: &std::collections::BTreeMap<String, serde_json::Value>) -> u64 {
    // BTreeMap iteration is already key-sorted, giving a deterministic hash.
    let mut hasher = DefaultHasher::new();
    for (k, v) in config {
        k.hash(&mut hasher);
        hasher.write_u8(0);
        v.to_string().hash(&mut hasher);
        hasher.write_u8(1);
    }
    hasher.finish()
}

fn build_cache_key(
    tenant_id: uuid::Uuid,
    subject_id: uuid::Uuid,
    method: ClientAuthMethod,
    config: &std::collections::BTreeMap<String, serde_json::Value>,
) -> String {
    format!(
        "{tenant_id}:{subject_id}:{}:{:x}",
        auth_method_tag(method),
        hash_config(config)
    )
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.id
    }

    async fn authenticate(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let parsed = parse_config(
            ctx.config,
            self.cache_ttl,
            self.allow_insecure_token_endpoint,
        )?;
        let lookup_key = build_cache_key(
            ctx.tenant_id,
            ctx.security.subject_id(),
            self.auth_method,
            ctx.config,
        );

        // Cache hit path (key verified — TinyUfo collision defense).
        let (cached, cache_status) = self.cache.get(&lookup_key);
        if let Some(entry) = cached
            && entry.key == lookup_key
        {
            set_authorization(ctx, &entry.token);
            return Ok(());
        }
        let _ = cache_status;

        // Single-flight: serialize the credential resolution + token fetch so
        // a burst of concurrent cache misses performs one IdP round-trip and
        // the winner's token serves every waiter.
        let _guard = self.fetch_lock.lock().await;
        // Re-check the cache under the lock: the winner may have populated it
        // while we waited.
        let (cached, _) = self.cache.get(&lookup_key);
        if let Some(entry) = cached
            && entry.key == lookup_key
        {
            set_authorization(ctx, &entry.token);
            return Ok(());
        }

        Self::fetch_and_cache(self, ctx, &parsed, &lookup_key).await
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Resolve credentials, fetch a token and populate the cache (callers
    /// hold the single-flight lock).
    async fn fetch_and_cache(
        &self,
        ctx: &mut RequestCtx<'_>,
        parsed: &OAuth2PluginConfig,
        lookup_key: &str,
    ) -> Result<(), PluginError> {
        let client_id = lookup_secret(&self.credstore, ctx.security, &parsed.client_id_ref).await?;
        let client_secret =
            lookup_secret(&self.credstore, ctx.security, &parsed.client_secret_ref).await?;

        let oauth_config = OAuthClientConfig {
            token_endpoint: parsed.token_endpoint.clone(),
            issuer_url: parsed.issuer_url.clone(),
            client_id: client_id.expose().to_owned(),
            client_secret: client_secret.clone(),
            scopes: parsed.scopes.clone(),
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            ..Default::default()
        };
        let oauth_config = if let Some(http) = &self.token_http_config {
            OAuthClientConfig {
                http_config: Some(http.clone()),
                ..oauth_config
            }
        } else {
            oauth_config
        };

        let fetched: FetchedToken =
            fetch_token(oauth_config)
                .await
                .map_err(|e| PluginError::AuthFailed {
                    detail: format!("token fetch failed: {e}"),
                })?;

        let ttl = effective_ttl(parsed.cache_ttl, fetched.expires_in);
        let token = fetched.bearer;
        self.cache.put(
            &lookup_key.to_owned(),
            CachedToken {
                key: lookup_key.to_owned(),
                token: token.clone(),
            },
            Some(ttl),
        );
        set_authorization(ctx, &token);
        Ok(())
    }
}

fn set_authorization(ctx: &mut RequestCtx<'_>, token: &SecretString) {
    let header = format!("Bearer {}", token.expose());
    if let Ok(value) = http::HeaderValue::from_str(&header) {
        ctx.headers.insert(http::header::AUTHORIZATION, value);
    }
}

/// `min(config_ttl, expires_in - 30s safety margin)`.
fn effective_ttl(config_ttl: Duration, expires_in: Duration) -> Duration {
    let safety = expires_in.saturating_sub(Duration::from_secs(30));
    safety.min(config_ttl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg(kv: &[(&str, &str)]) -> BTreeMap<String, serde_json::Value> {
        kv.iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String((*v).to_owned())))
            .collect()
    }

    /// Run one plugin authentication against an httpmock token endpoint and
    /// return the headers the plugin injected (used by the single-flight test).
    async fn authenticate_for(
        plugin: Arc<OAuth2ClientCredAuthPlugin>,
        security: &toolkit_security::SecurityContext,
        token_url: &str,
    ) -> Result<http::HeaderMap, PluginError> {
        let config = BTreeMap::from([
            (
                "token_endpoint".to_owned(),
                serde_json::Value::String(token_url.to_owned()),
            ),
            (
                "client_id_ref".to_owned(),
                serde_json::Value::String("cred://cid".to_owned()),
            ),
            (
                "client_secret_ref".to_owned(),
                serde_json::Value::String("cred://secret".to_owned()),
            ),
        ]);
        let mut headers = http::HeaderMap::new();
        let mut ctx = RequestCtx {
            security,
            config: &config,
            headers: &mut headers,
            tenant_id: security.subject_tenant_id(),
        };
        plugin.authenticate(&mut ctx).await?;
        Ok(headers)
    }

    #[test]
    fn config_hash_is_deterministic_and_order_independent() {
        let a = cfg(&[("scopes", "a b"), ("client_id_ref", "cred://x")]);
        let b = cfg(&[("client_id_ref", "cred://x"), ("scopes", "a b")]);
        assert_eq!(hash_config(&a), hash_config(&b));
    }

    #[test]
    fn cache_key_includes_tenant_subject_method() {
        let tenant = uuid::Uuid::nil();
        let subject = uuid::Uuid::nil();
        let config = cfg(&[("scopes", "a")]);
        let k1 = build_cache_key(tenant, subject, ClientAuthMethod::Form, &config);
        let k2 = build_cache_key(tenant, subject, ClientAuthMethod::Basic, &config);
        assert_ne!(k1, k2);
        let other_tenant = uuid::Uuid::new_v4();
        let k3 = build_cache_key(other_tenant, subject, ClientAuthMethod::Form, &config);
        assert_ne!(k1, k3);
    }

    #[test]
    fn ttl_caps_at_config_ceiling() {
        let ceiling = Duration::from_mins(5);
        assert_eq!(
            effective_ttl(ceiling, Duration::from_mins(10)),
            Duration::from_mins(5)
        );
        assert_eq!(
            effective_ttl(ceiling, Duration::from_mins(1)),
            Duration::from_secs(30)
        );
        assert_eq!(
            effective_ttl(ceiling, Duration::from_secs(10)),
            Duration::from_secs(0)
        );
    }

    #[test]
    fn parse_config_requires_endpoint_and_credentials() {
        let fallback = Duration::from_mins(5);
        let creds = &[
            ("token_endpoint", "https://idp.example.com/token"),
            ("client_id_ref", "cred://cid"),
            ("client_secret_ref", "cred://secret"),
        ];
        assert!(parse_config(&cfg(creds), fallback, false).is_ok());
        let missing_endpoint = parse_config(
            &cfg(&[
                ("client_id_ref", "cred://cid"),
                ("client_secret_ref", "cred://secret"),
            ]),
            fallback,
            false,
        );
        assert!(matches!(missing_endpoint, Err(PluginError::Config { .. })));
        let both_endpoints = parse_config(
            &cfg(&[
                ("token_endpoint", "https://idp.example.com/token"),
                ("issuer_url", "https://idp.example.com"),
                ("client_id_ref", "cred://cid"),
                ("client_secret_ref", "cred://secret"),
            ]),
            fallback,
            false,
        );
        assert!(matches!(both_endpoints, Err(PluginError::Config { .. })));
    }

    #[test]
    fn insecure_token_endpoint_rejected_unless_opted_in() {
        let fallback = Duration::from_mins(5);
        let insecure = cfg(&[
            ("token_endpoint", "http://idp.example.com/token"),
            ("client_id_ref", "cred://cid"),
            ("client_secret_ref", "cred://secret"),
        ]);
        assert!(matches!(
            parse_config(&insecure, fallback, false),
            Err(PluginError::Config { .. })
        ));
        assert!(parse_config(&insecure, fallback, true).is_ok());
    }

    #[tokio::test]
    async fn concurrent_cache_misses_perform_one_token_fetch() {
        // The identity provider is slow (250ms) and slow enough that two
        // concurrent requests both miss the cache before either wins the
        // single-flight lock: the loser must re-check the cache under the
        // lock instead of stampeding the IdP.
        let server = httpmock::MockServer::start();
        let token_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-shared","expires_in":3600,"token_type":"Bearer"}"#)
                .delay(std::time::Duration::from_millis(250));
        });

        let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
                ("cid".to_owned(), "client-id".to_owned()),
                ("secret".to_owned(), "client-secret".to_owned()),
            ]),
        );
        let plugin = Arc::new(OAuth2ClientCredAuthPlugin::new(
            crate::gts::AUTH_OAUTH2_ID,
            credstore,
            ClientAuthMethod::Form,
            TokenCacheConfig {
                ttl_secs: 300,
                capacity: 16,
            },
            Some(toolkit_http::HttpClientConfig::for_testing()),
            // The httpmock endpoint is plain HTTP; the plugin's own
            // allow-insecure flag opts the test into it (not the transport).
            true,
        ));
        let token_url = format!("http://localhost:{}/token", server.port());
        let security = toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::new_v4())
            .build()
            .expect("valid security context");

        let (left, right) = tokio::join!(
            authenticate_for(Arc::clone(&plugin), &security, &token_url),
            authenticate_for(Arc::clone(&plugin), &security, &token_url),
        );
        let left = left.expect("left authenticate");
        let right = right.expect("right authenticate");
        assert_eq!(
            left.get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tok-shared"),
        );
        assert_eq!(
            right
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tok-shared"),
        );
        // Single-flight: one winner fetched, the loser served from the cache.
        assert_eq!(token_mock.calls(), 1, "exactly one IdP round trip");
    }
}
