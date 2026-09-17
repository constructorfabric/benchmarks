//! Runtime services the built-in plugins are constructed with
//! (ADR 0008 "Token caching", ADR 0006 "state management").
//!
//! A [`PluginRuntime`] is the *only* thing a credential-injection plugin needs
//! beyond its configuration: a [`SecretSource`] for credential material, the
//! shared upstream transport for the OAuth2 token endpoint, and the process
//! token cache.
//!
//! Credentials live in [`SecretValue`]-typed memory whose `Debug`/`Display`
//! output is `[REDACTED]`, so nothing here can leak them through formatting.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use http::{Method, Request, Uri};
use pingora_memory_cache::MemoryCache;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID;
use crate::domain::plugin::{PluginError, PluginResult};
use crate::infra::plugin::oauth2_client_cred_auth::{
    CachedToken, ClientAuthMethod, OAuth2ClientCredAuthConfig,
};
use crate::infra::proxy::base64::base64_encode;
use crate::infra::proxy::secrets::SecretSource;
use crate::infra::proxy::transport::{ProxyTransport, TransportError};

/// A token as the issuer returned it.
#[derive(Debug, Clone)]
pub struct FetchedToken {
    /// `access_token`.
    pub access_token: String,
    /// `token_type`, normalised to `Bearer` when absent.
    pub token_type: String,
    /// `expires_in`, in seconds.
    pub expires_in: u64,
}

/// Everything a plugin may need at request time.
pub struct PluginRuntime {
    secrets: Arc<dyn SecretSource>,
    tokens: MemoryCache<String, CachedToken>,
    default_ttl: Duration,
    transport: Option<Arc<ProxyTransport>>,
}

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRuntime")
            .field("default_ttl_secs", &self.default_ttl.as_secs())
            .field("has_transport", &self.transport.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginRuntime {
    /// A runtime over `secrets`, with the token cache configured by
    /// `ttl`/`capacity` and `transport` for OAuth2 token endpoints.
    #[must_use]
    pub fn new(
        secrets: Arc<dyn SecretSource>,
        ttl: Duration,
        capacity: usize,
        transport: Option<Arc<ProxyTransport>>,
    ) -> Self {
        Self {
            secrets,
            tokens: MemoryCache::new(capacity.max(1)),
            default_ttl: ttl.max(Duration::from_secs(1)),
            transport,
        }
    }

    /// A runtime with no secret source and no transport.
    ///
    /// Used when the gear is initialised without a credstore; every
    /// credential-injection plugin then reports a secret error instead of
    /// forwarding an unauthenticated request.
    #[must_use]
    pub fn null() -> Arc<Self> {
        Arc::new(Self::new(
            Arc::new(crate::infra::proxy::secrets::InMemorySecretSource::new()),
            Duration::from_secs(300),
            10_000,
            None,
        ))
    }

    /// The secret source backing credential resolution.
    #[must_use]
    pub const fn secrets(&self) -> &Arc<dyn SecretSource> {
        &self.secrets
    }

    /// The transport used for OAuth2 token endpoints.
    #[must_use]
    pub const fn transport(&self) -> Option<&Arc<ProxyTransport>> {
        self.transport.as_ref()
    }

    /// Resolve `reference` for `tenant_id`.
    ///
    /// # Errors
    /// [`PluginError::SecretUnavailable`] when the reference does not resolve.
    pub async fn resolve_secret(
        &self,
        tenant_id: Uuid,
        security: &SecurityContext,
        reference: &str,
    ) -> Result<crate::infra::proxy::secrets::ResolvedSecret, PluginError> {
        self.secrets
            .resolve(tenant_id, security, reference)
            .await
            .map_err(|err| PluginError::SecretUnavailable {
                plugin_id: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
                // `DomainError`'s own display already carries its error class;
                // the plugin error adds its own prefix, so only the bare detail
                // is carried over.
                detail: match &err {
                    DomainError::SecretNotFound { detail } => detail.clone(),
                    other => other.to_string(),
                },
            })
    }

    /// Cache key for a token (ADR 0008: `tenant:subject:auth_method:config`).
    #[must_use]
    pub fn token_cache_key(
        &self,
        config: &OAuth2ClientCredAuthConfig,
        tenant_id: Uuid,
        subject_id: Uuid,
    ) -> String {
        format!(
            "{tenant_id}:{subject_id}:{}:{:016x}",
            config.client_auth_method.as_str(),
            oauth2_fingerprint(config)
        )
    }

    /// Return a cached token when it is still fresh.
    #[must_use]
    pub fn cached_token(&self, key: &str) -> Option<CachedToken> {
        let (hit, _status) = self.tokens.get(key);
        hit.filter(|token| !token.is_expired(Instant::now()))
    }

    /// Store a token under `key` for its effective TTL.
    pub fn store_token(&self, key: &str, token: CachedToken, ttl: Duration) {
        self.tokens.put(key, token, Some(ttl));
    }

    /// The configured default cache TTL.
    #[must_use]
    pub const fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    /// Fetch (and cache) an access token for a client-credentials flow.
    ///
    /// The cache key covers the tenant, the subject, the client-auth method and
    /// the whole plugin configuration, so two upstreams with different scopes
    /// never share a token.
    ///
    /// # Errors
    /// [`PluginError::SecretUnavailable`] when the client secret cannot be
    /// resolved, [`PluginError::Internal`] when the token endpoint cannot be
    /// reached or returns a non-2xx response.
    pub async fn token_for(
        &self,
        config: &OAuth2ClientCredAuthConfig,
        tenant_id: Uuid,
        security: &SecurityContext,
    ) -> PluginResult<CachedToken> {
        let key = self.token_cache_key(config, tenant_id, security.subject_id());
        if let Some(token) = self.cached_token(&key) {
            return Ok(token);
        }
        let secret = self
            .resolve_secret(tenant_id, security, &config.client_secret_ref)
            .await?;
        let secret_value = secret
            .as_str()
            .ok_or_else(|| PluginError::SecretUnavailable {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: "client secret is not valid UTF-8".to_owned(),
            })?
            .to_owned();
        let fetched = self.fetch_token(config, &secret_value).await?;
        let ttl = config.effective_ttl(fetched.expires_in, self.default_ttl.as_secs());
        let cached = CachedToken {
            access_token: fetched.access_token,
            token_type: if fetched.token_type.is_empty() {
                "Bearer".to_owned()
            } else {
                fetched.token_type
            },
            expires_at: Instant::now() + ttl,
        };
        self.tokens.put(&key, cached.clone(), Some(ttl));
        Ok(cached)
    }

    /// Perform the `client_credentials` exchange against the token endpoint.
    ///
    /// # Errors
    /// [`PluginError::Internal`] on transport failure, a non-2xx response, or a
    /// response that is not a well-formed token document.
    pub async fn fetch_token(
        &self,
        config: &OAuth2ClientCredAuthConfig,
        client_secret: &str,
    ) -> PluginResult<FetchedToken> {
        let transport = self
            .transport
            .as_ref()
            .ok_or_else(|| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: "no upstream transport is configured for the OAuth2 token endpoint"
                    .to_owned(),
            })?;
        let uri: Uri = config
            .token_url
            .parse()
            .map_err(|err| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: format!("auth.config.token_url is not a valid URI: {err}"),
            })?;
        transport
            .check_uri(&uri)
            .map_err(|err| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: plugin_error_detail(&err),
            })?;

        let mut form: Vec<(String, String)> = vec![
            ("grant_type".to_owned(), "client_credentials".to_owned()),
            ("client_id".to_owned(), config.client_id.clone()),
        ];
        if !config.scopes.is_empty() {
            form.push(("scope".to_owned(), config.scopes.join(" ")));
        }
        if config.client_auth_method == ClientAuthMethod::Form {
            // client_secret_post: the secret belongs in the request body.
            form.push(("client_secret".to_owned(), client_secret.to_owned()));
        }
        // Serialised in its own scope: `Serializer` holds a non-`Sync` closure,
        // so it must not be live across the `await` below.
        let body = {
            let mut serializer = form_urlencoded::Serializer::new(String::new());
            for (name, value) in &form {
                serializer.append_pair(name, value);
            }
            serializer.finish()
        };

        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(uri.clone())
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(http::header::ACCEPT, "application/json");
        if config.client_auth_method == ClientAuthMethod::Basic {
            let credentials =
                base64_encode(format!("{}:{client_secret}", config.client_id).as_bytes());
            builder = builder.header(http::header::AUTHORIZATION, format!("Basic {credentials}"));
        }
        let request = builder
            .body(Body::from(body))
            .map_err(|err| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: format!("unable to build the token request: {err}"),
            })?;

        let response = transport
            .send(request)
            .await
            .map_err(|err| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: format!("token endpoint unreachable: {}", plugin_error_detail(&err)),
            })?;
        let status = response.status();
        let bytes = read_body(Body::new(response.into_body()), transport.timeout())
            .await
            .map_err(|err| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: format!("token endpoint response could not be read: {err}"),
            })?;
        if !status.is_success() {
            // Never echo the response body: it may contain an error document
            // quoting the credentials.
            return Err(PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: format!("token endpoint returned status {status}"),
            });
        }
        let document: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|err| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: format!("token endpoint response is not JSON: {err}"),
            })?;
        let access_token = document
            .get("access_token")
            .and_then(|value| value.as_str())
            .ok_or_else(|| PluginError::Internal {
                plugin_id: config.client_auth_method.gts_id().to_owned(),
                detail: "token endpoint response has no `access_token`".to_owned(),
            })?
            .to_owned();
        let token_type = document
            .get("token_type")
            .and_then(|value| value.as_str())
            .unwrap_or("Bearer")
            .to_owned();
        let expires_in = document
            .get("expires_in")
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
            })
            .unwrap_or(self.default_ttl.as_secs());
        Ok(FetchedToken {
            access_token,
            token_type,
            expires_in,
        })
    }
}

/// Read a (small) response body to completion.
///
/// Token-endpoint responses are a few hundred bytes, so buffering them is
/// safe; the read is still bounded by `timeout` so a slow issuer cannot hang
/// the request.
async fn read_body(body: axum::body::Body, timeout: Duration) -> Result<Vec<u8>, String> {
    use futures_util::TryStreamExt;
    let mut stream = body.into_data_stream();
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = tokio::time::timeout(timeout, stream.try_next())
        .await
        .map_err(|_| "timed out".to_owned())?
        .map_err(|err| err.to_string())?
    {
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Human-readable transport failure (never contains credential material).
fn plugin_error_detail(err: &TransportError) -> String {
    match err {
        TransportError::PlaintextDisabled { host } => {
            format!("`{host}` uses a cleartext scheme and allow_http_upstream is disabled")
        }
        TransportError::InvalidUri { detail }
        | TransportError::Connect { detail }
        | TransportError::Timeout { detail } => detail.clone(),
    }
}

/// A stable, process-local fingerprint of a plugin configuration, used as part
/// of the token cache key so configurations with different scopes never share
/// a token.
#[must_use]
pub fn config_fingerprint(value: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    canonical_json(value).hash(&mut hasher);
    hasher.finish()
}

/// Serialise a JSON value with its object keys in a deterministic order.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{:?}:{}", key, canonical_json(&map[key])))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// Fingerprint a whole OAuth2 plugin configuration.
#[must_use]
pub fn oauth2_fingerprint(config: &OAuth2ClientCredAuthConfig) -> u64 {
    let mut hasher = DefaultHasher::new();
    config.token_url.hash(&mut hasher);
    config.client_id.hash(&mut hasher);
    for scope in &config.scopes {
        scope.hash(&mut hasher);
    }
    config.client_auth_method.as_str().hash(&mut hasher);
    for (name, value) in &config.extra_params {
        name.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    hasher.finish()
}

/// Convert a domain error into the plugin error the trait expects.
#[must_use]
pub fn plugin_internal(plugin_id: &str, detail: impl Into<String>) -> PluginError {
    PluginError::Internal {
        plugin_id: plugin_id.to_owned(),
        detail: detail.into(),
    }
}

/// Convert a [`DomainError`] raised while resolving a secret.
#[must_use]
pub fn secret_error(plugin_id: &str, err: &DomainError) -> PluginError {
    PluginError::SecretUnavailable {
        plugin_id: plugin_id.to_owned(),
        detail: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::proxy::secrets::InMemorySecretSource;

    fn runtime() -> PluginRuntime {
        PluginRuntime::new(
            Arc::new(InMemorySecretSource::new()),
            Duration::from_secs(300),
            100,
            None,
        )
    }

    fn config(token_url: &str) -> OAuth2ClientCredAuthConfig {
        OAuth2ClientCredAuthConfig::from_config(&serde_json::json!({
            "token_url": token_url,
            "client_id": "svc",
            "client_secret_ref": "cred",
            "scopes": ["read"]
        }))
    }

    #[test]
    fn cache_keys_separate_tenants_subjects_and_configs() {
        let rt = runtime();
        let cfg = config("https://auth.example.com/token");
        let tenant = Uuid::new_v4();
        let security = SecurityContext::anonymous();
        let subject = security.subject_id();
        let a = rt.token_cache_key(&cfg, tenant, subject);
        // A different configuration (different issuer) must not share a token.
        let b = rt.token_cache_key(&config("https://other.example.com/token"), tenant, subject);
        assert_ne!(a, b);
        // The same tuple is stable.
        assert_eq!(a, rt.token_cache_key(&cfg, tenant, subject));
        // A different tenant, subject or auth method is a different cache entry.
        let other_tenant = Uuid::new_v4();
        assert_ne!(a, rt.token_cache_key(&cfg, other_tenant, subject));
        assert_ne!(a, rt.token_cache_key(&cfg, tenant, Uuid::new_v4()));
        let mut basic = cfg.clone();
        basic.client_auth_method = ClientAuthMethod::Basic;
        assert_ne!(a, rt.token_cache_key(&basic, tenant, subject));
    }

    #[test]
    fn canonical_json_is_key_order_independent() {
        let a = serde_json::json!({"a": 1, "b": {"y": 2, "x": 1}});
        let b = serde_json::json!({"b": {"x": 1, "y": 2}, "a": 1});
        assert_eq!(config_fingerprint(&a), config_fingerprint(&b));
    }

    #[tokio::test]
    async fn cached_tokens_are_reused_until_they_expire() {
        let rt = runtime();
        let key = "tenant:subject:form:1234";
        let token = CachedToken {
            access_token: "abc".to_owned(),
            token_type: "Bearer".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(60),
        };
        rt.store_token(key, token.clone(), Duration::from_secs(60));
        assert_eq!(rt.cached_token(key).unwrap().access_token, "abc");

        // A token whose own `expires_at` has passed is never served, even though
        // the cache entry itself is still live.
        let stale = CachedToken {
            access_token: "old".to_owned(),
            token_type: "Bearer".to_owned(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        rt.store_token(key, stale, Duration::from_secs(60));
        assert!(rt.cached_token(key).is_none());

        // A zero effective TTL is never inserted (ADR 0008 safety margin), so a
        // rejected exchange cannot poison the cache.
        rt.store_token("tenant:subject:form:none", token, Duration::from_secs(0));
        assert!(rt.cached_token("tenant:subject:form:none").is_none());
    }
}
