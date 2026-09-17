//! Plugin registries and built-in plugin implementations.
//!
//! * Auth: `noop`, `apikey`, `oauth2_client_cred` (+ `_basic`).
//! * Guard: `required_headers` (ADR-0009).
//! * Transform: `request_id`.
//!
//! Custom (UUID-backed, Starlark) plugins resolve to
//! [`OagwError::PluginNotFound`] on the data plane — the management API can
//! register, list and reference them, but this milestone has no Starlark
//! runtime.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue};
use pingora_memory_cache::MemoryCache;
use url::Url;
use uuid::Uuid;

use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_http::HttpClientConfig;
use toolkit_security::{SecurityContext, context::SecurityContextBuilder};
use credstore_sdk::{CredStoreClientV1, SecretRef};

use crate::domain::plugins as domain_plugins;
pub use domain_plugins::{
    ArcAuthPlugin, ArcGuardPlugin, ArcTransformPlugin, AuthPlugin, GuardDecision,
    GuardPlugin, PluginError, RequestContext, ResponseContext, TransformPlugin,
};
use crate::domain::model::PluginKind;
use crate::error::OagwError;
use crate::gts_helpers;

// ---------------------------------------------------------------------------
// Token cache (ADR-0008)
// ---------------------------------------------------------------------------

/// A cached OAuth2 bearer token with its key for collision verification
/// (MemoryCache hashes keys with a 64-bit TinyUfo, so on hit we verify).
#[derive(Debug, Clone)]
pub struct CachedToken {
    pub key: String,
    pub token: SecretString,
}

/// ADR-0008 token cache configuration.
#[derive(Debug, Clone)]
pub struct TokenCacheConfig {
    pub ttl: Duration,
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

// ---------------------------------------------------------------------------
// Shared secret resolution
// ---------------------------------------------------------------------------

pub(crate) fn secret_ref(value: &str) -> Result<String, OagwError> {
    let stripped = value.strip_prefix("cred://").unwrap_or(value);
    if stripped.trim().is_empty() {
        return Err(OagwError::validation("secret_ref must not be empty"));
    }
    Ok(stripped.to_owned())
}

/// Resolve a secret via credstore, building a security context from the
/// request context. Errors map to `AuthFailed` (401) for missing/inaccessible
/// secrets and `SecretNotFound` (500) for credstore failures.
pub(crate) async fn resolve_secret(
    credstore: &dyn CredStoreClientV1,
    ctx: &RequestContext,
    ref_value: &str,
) -> Result<Vec<u8>, OagwError> {
    let key = match SecretRef::new(secret_ref(ref_value)?) {
        Ok(k) => k,
        Err(e) => {
            return Err(OagwError::AuthFailed {
                detail: format!("invalid secret reference `{ref_value}`: {e}"),
            });
        }
    };
    let sec = security_context(ctx);
    match credstore.get(&sec, &key).await {
        Ok(Some(resp)) => Ok(resp.value.as_bytes().to_vec()),
        Ok(None) => Err(OagwError::AuthFailed {
            detail: format!("secret `{ref_value}` not found or not accessible"),
        }),
        Err(_) => Err(OagwError::SecretNotFound {
            detail: "credential store failure while resolving secret".to_owned(),
        }),
    }
}

pub(crate) fn security_context(ctx: &RequestContext) -> SecurityContext {
    SecurityContextBuilder::default()
        .subject_id(ctx.subject_id)
        .subject_tenant_id(ctx.tenant_id)
        .token_scopes(ctx.scopes.clone())
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

// ---------------------------------------------------------------------------
// Auth plugins
// ---------------------------------------------------------------------------

/// Does nothing — the default when no auth is configured.
struct NoopAuth;

#[async_trait]
impl AuthPlugin for NoopAuth {
    fn id(&self) -> &'static str {
        gts_helpers::NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}

/// API key injection. Config:
/// * `api_key_ref` — `cred://...` reference to the key material (required)
/// * `header_name` — request header carrying the key (default `X-Api-Key`)
struct ApiKeyAuth {
    credstore: Arc<dyn CredStoreClientV1>,
}

#[async_trait]
impl AuthPlugin for ApiKeyAuth {
    fn id(&self) -> &'static str {
        gts_helpers::APIKEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let cfg = ctx.plugin_config.clone().unwrap_or_default();
        let ref_value = cfg
            .get("api_key_ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PluginError::Config("apikey plugin requires `api_key_ref`".into()))?;
        let key = resolve_secret(self.credstore.as_ref(), ctx, ref_value)
            .await
            .map_err(|e| PluginError::AuthFailed(e.detail().to_owned()))?;
        let name = cfg
            .get("header_name")
            .and_then(|v| v.as_str())
            .unwrap_or("X-Api-Key")
            .to_owned();
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| PluginError::AuthFailed("api_key header name is invalid".into()))?;
        let value = std::str::from_utf8(&key)
            .map_err(|_| PluginError::AuthFailed("API key material is not valid UTF-8".into()))?;
        ctx.headers.insert(
            name,
            HeaderValue::from_str(value)
                .map_err(|_| PluginError::AuthFailed("API key is not a valid header value".into()))?,
        );
        Ok(())
    }
}

/// OAuth2 client-credentials auth (ADR-0008). Config:
/// * `token_endpoint` | `issuer_url` — exactly one (mutually exclusive)
/// * `client_id_ref` / `client_secret_ref` — `cred://` secret references
/// * `scopes` — space-separated scope list (optional)
struct OAuth2ClientCredAuth {
    credstore: Arc<dyn CredStoreClientV1>,
    token_http_config: HttpClientConfig,
    cache: Arc<MemoryCache<String, CachedToken>>,
    cache_ttl: Duration,
    /// `true` for the `..._basic.v1` variant (credentials via Basic auth
    /// header), `false` for the form-encoded variant.
    basic: bool,
}

impl OAuth2ClientCredAuth {
    fn id_prefix(&self) -> &'static str {
        if self.basic {
            gts_helpers::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID
        } else {
            gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID
        }
    }

    fn config_hash(cfg: &serde_json::Value) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        format!(
            "{:?}|{:?}|{:?}",
            cfg.get("token_endpoint").and_then(|v| v.as_str()),
            cfg.get("issuer_url").and_then(|v| v.as_str()),
            cfg.get("scopes").and_then(|v| v.as_str()),
        )
        .hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuth {
    fn id(&self) -> &'static str {
        self.id_prefix()
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let cfg = ctx.plugin_config.clone().unwrap_or_default();
        let token_endpoint = cfg.get("token_endpoint").and_then(|v| v.as_str());
        let issuer_url = cfg.get("issuer_url").and_then(|v| v.as_str());
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(PluginError::Config(
                "oauth2 plugin requires `token_endpoint` or `issuer_url`".into(),
            ));
        }
        let client_id_ref = cfg
            .get("client_id_ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PluginError::Config("oauth2 plugin requires `client_id_ref`".into()))?;
        let client_secret_ref = cfg
            .get("client_secret_ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PluginError::Config("oauth2 plugin requires `client_secret_ref`".into()))?;
        let client_id = resolve_secret(self.credstore.as_ref(), ctx, client_id_ref)
            .await
            .map_err(|e| PluginError::AuthFailed(e.detail().to_owned()))?;
        let client_secret = resolve_secret(self.credstore.as_ref(), ctx, client_secret_ref)
            .await
            .map_err(|e| PluginError::AuthFailed(e.detail().to_owned()))?;
        let client_id = String::from_utf8(client_id)
            .map_err(|_| PluginError::Config("client_id material is not valid UTF-8".into()))?;
        let client_secret_bytes = client_secret;

        let scopes: Vec<String> = cfg
            .get("scopes")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();

        let method_tag = if self.basic { "basic" } else { "form" };
        let cache_key = format!(
            "{}:{}:{}:{}",
            ctx.tenant_id,
            ctx.subject_id,
            method_tag,
            Self::config_hash(&cfg)
        );

        // Cache hit path (avoid a token fetch per request).
        let cached = self.cache.get(&cache_key).0;
        if let Some(cached) = cached {
            if cached.key == cache_key {
                ctx.headers.insert(
                    "authorization",
                    HeaderValue::from_str(&format!("Bearer {}", cached.token.expose()))
                        .map_err(|_| PluginError::AuthFailed("cached token is invalid".into()))?,
                );
                return Ok(());
            }
        }

        let endpoint: Option<Url> = match (token_endpoint, issuer_url) {
            (Some(te), _) => Url::parse(te).ok(),
            (None, Some(iss)) => Url::parse(iss).ok(),
            _ => None,
        };

        let oauth_config = OAuthClientConfig {
            token_endpoint: if token_endpoint.is_some() { endpoint.clone() } else { None },
            issuer_url: if issuer_url.is_some() { endpoint } else { None },
            client_id,
            client_secret: SecretString::new(String::from_utf8_lossy(&client_secret_bytes).into_owned()),
            scopes,
            auth_method: if self.basic {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            },
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_secs(30),
            jitter_max: Duration::from_secs(5),
            min_refresh_period: Duration::from_secs(10),
            default_ttl: Duration::from_secs(300),
            http_config: Some(self.token_http_config.clone()),
        };

        let token = fetch_token(oauth_config)
            .await
            .map_err(|e| PluginError::Upstream(format!("OAuth2 token exchange failed: {e}")))?;

        // ADR-0008: TTL = min(config_ttl, expires_in - 30s); do not cache
        // tokens that expire within the safety margin.
        let expires_in = token.expires_in;
        let ttl = expires_in.saturating_sub(Duration::from_secs(30)).min(self.cache_ttl);
        if ttl > Duration::ZERO {
            self.cache.put(
                &cache_key,
                CachedToken {
                    key: cache_key.clone(),
                    token: token.bearer.clone(),
                },
                Some(ttl),
            );
        }

        ctx.headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", token.bearer.expose()))
                .map_err(|_| PluginError::AuthFailed("received token is invalid".into()))?,
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Guard plugins
// ---------------------------------------------------------------------------

/// Required-headers guard (ADR-0009). Config:
/// * `required_request_headers` — comma-separated, presence-only, case-insensitive
/// * `required_response_headers` — same, evaluated on the response
struct RequiredHeadersGuard;

fn parse_header_list(config: &serde_json::Value, key: &str) -> Vec<String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl GuardPlugin for RequiredHeadersGuard {
    fn id(&self) -> &'static str {
        gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let cfg = ctx.plugin_config.clone().unwrap_or_default();
        let required = parse_header_list(&cfg, "required_request_headers");
        for name in required {
            if !ctx
                .headers
                .keys()
                .any(|h| h.as_str().eq_ignore_ascii_case(&name))
            {
                return Ok(GuardDecision::Reject(crate::domain::plugins::GuardRejectInfo {
                    status: 400,
                    problem_type: gts_helpers::ERR_REQUIRED_HEADER_MISSING,
                    detail: format!(
                        "required request header `{name}` is missing (error_code=REQUIRED_HEADER_MISSING)"
                    ),
                }));
            }
        }
        Ok(GuardDecision::Allow)
    }

    fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let cfg = ctx.plugin_config.clone().unwrap_or_default();
        let required = parse_header_list(&cfg, "required_response_headers");
        for name in required {
            if !ctx
                .headers
                .keys()
                .any(|h| h.as_str().eq_ignore_ascii_case(&name))
            {
                return Ok(GuardDecision::Reject(crate::domain::plugins::GuardRejectInfo {
                    status: 502,
                    problem_type: gts_helpers::ERR_REQUIRED_HEADER_MISSING,
                    detail: format!(
                        "required response header `{name}` is missing (error_code=REQUIRED_HEADER_MISSING)"
                    ),
                }));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

// ---------------------------------------------------------------------------
// Transform plugins
// ---------------------------------------------------------------------------

/// Request-ID transform: injects `X-Request-ID` on outbound requests when
/// absent, and ensures it is propagated on responses.
struct RequestIdTransform;

impl TransformPlugin for RequestIdTransform {
    fn id(&self) -> &'static str {
        gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        if !ctx.headers.contains_key("x-request-id") {
            let id = Uuid::new_v4().to_string();
            ctx.headers.insert(
                "x-request-id",
                HeaderValue::from_str(&id)
                    .map_err(|_| PluginError::Config("invalid request id".into()))?,
            );
            // Stash for the response phase, where the id is propagated back
            // to the client on `X-Request-Id`.
            ctx.metadata
                .insert("x-request-id".to_owned(), serde_json::Value::String(id));
        }
        Ok(())
    }

    fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        if let Some(v) = ctx
            .metadata
            .get("x-request-id")
            .and_then(|v| v.as_str())
        {
            ctx.headers.insert(
                "x-request-id",
                HeaderValue::from_str(v).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Registries
// ---------------------------------------------------------------------------

/// Registry of resolvable auth plugins. Builtin instances are constructed
/// once and share the OAuth2 token cache (ADR-0008).
pub struct AuthPluginRegistry {
    plugins: std::collections::HashMap<&'static str, ArcAuthPlugin>,
}

impl AuthPluginRegistry {
    /// Build the built-in registry (noop, apikey, oauth2 form + basic).
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_http_config: HttpClientConfig,
        cache_cfg: TokenCacheConfig,
    ) -> Self {
        let cache: Arc<MemoryCache<String, CachedToken>> =
            Arc::new(MemoryCache::new(cache_cfg.capacity.max(1)));
        let cache_ttl = cache_cfg.ttl;
        let mut plugins: std::collections::HashMap<&'static str, ArcAuthPlugin> =
            std::collections::HashMap::new();
        plugins.insert(
            gts_helpers::NOOP_AUTH_PLUGIN_ID,
            Arc::new(NoopAuth) as ArcAuthPlugin,
        );
        plugins.insert(
            gts_helpers::APIKEY_AUTH_PLUGIN_ID,
            Arc::new(ApiKeyAuth {
                credstore: credstore.clone(),
            }) as ArcAuthPlugin,
        );
        plugins.insert(
            gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            Arc::new(OAuth2ClientCredAuth {
                credstore: credstore.clone(),
                token_http_config: token_http_config.clone(),
                cache: cache.clone(),
                cache_ttl,
                basic: false,
            }) as ArcAuthPlugin,
        );
        plugins.insert(
            gts_helpers::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            Arc::new(OAuth2ClientCredAuth {
                credstore,
                token_http_config,
                cache,
                cache_ttl,
                basic: true,
            }) as ArcAuthPlugin,
        );
        Self { plugins }
    }

    /// Resolve a GTS identifier to a runnable auth plugin, or
    /// `PluginNotFound`.
    pub fn resolve(&self, id: &str) -> Result<ArcAuthPlugin, OagwError> {
        self.plugins.get(id).cloned().ok_or_else(|| {
            OagwError::PluginNotFound {
                detail: format!("unknown auth plugin `{id}`"),
            }
        })
    }
}

/// Registry of resolvable guard plugins.
pub struct GuardPluginRegistry;

impl GuardPluginRegistry {
    #[must_use]
    pub fn with_builtins() -> Vec<ArcGuardPlugin> {
        vec![Arc::new(RequiredHeadersGuard)]
    }

    pub fn resolve(id: &str) -> Result<ArcGuardPlugin, OagwError> {
        match id {
            gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID => Ok(Arc::new(RequiredHeadersGuard)),
            _ => Err(OagwError::PluginNotFound {
                detail: format!("unknown guard plugin `{id}`"),
            }),
        }
    }
}

/// Registry of resolvable transform plugins.
pub struct TransformPluginRegistry;

impl TransformPluginRegistry {
    #[must_use]
    pub fn with_builtins() -> Vec<ArcTransformPlugin> {
        vec![Arc::new(RequestIdTransform)]
    }

    pub fn resolve(id: &str) -> Result<ArcTransformPlugin, OagwError> {
        match id {
            gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID => Ok(Arc::new(RequestIdTransform)),
            _ => Err(OagwError::PluginNotFound {
                detail: format!("unknown transform plugin `{id}`"),
            }),
        }
    }
}

/// Classify a plugin binding by its GTS type prefix.
#[must_use]
pub fn classify_binding(plugin_ref: &str) -> Option<PluginKind> {
    if plugin_ref.starts_with("gts.cf.core.oagw.auth_plugin.v1~") {
        Some(PluginKind::Auth)
    } else if plugin_ref.starts_with("gts.cf.core.oagw.guard_plugin.v1~") {
        Some(PluginKind::Guard)
    } else if plugin_ref.starts_with("gts.cf.core.oagw.transform_plugin.v1~") {
        Some(PluginKind::Transform)
    } else {
        None
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::OagwConfig;

    fn svc_plugins() -> AuthPluginRegistry {
        AuthPluginRegistry::with_builtins(
            Arc::new(super::super::storage::NoopCredStore),
            toolkit_http::HttpClientConfig::proxy(),
            TokenCacheConfig::default(),
        )
    }

    #[test]
    fn builtin_auth_registry_resolves() {
        let reg = svc_plugins();
        for id in [
            gts_helpers::NOOP_AUTH_PLUGIN_ID,
            gts_helpers::APIKEY_AUTH_PLUGIN_ID,
            gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            gts_helpers::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        ] {
            let plugin = reg.resolve(id).expect("builtin resolves");
            assert!(plugin.id() == id);
        }
        match reg.resolve("gts.cf.core.oagw.auth_plugin.v1~does.not.exist.v1") {
            Err(OagwError::PluginNotFound { .. }) => {}
            other => panic!("expected PluginNotFound, got {:?}", other.as_ref().err()),
        }
        let _ = OagwConfig::default();
    }

    #[test]
    fn guard_and_transform_registries_resolve_builtins() {
        let guard = GuardPluginRegistry::resolve(gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID)
            .expect("guard builtin");
        assert_eq!(guard.id(), gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID);
        assert!(GuardPluginRegistry::resolve("unknown.guard.v1").is_err());

        let tr = TransformPluginRegistry::resolve(gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID)
            .expect("transform builtin");
        assert_eq!(tr.id(), gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID);
        assert!(TransformPluginRegistry::resolve("unknown.transform.v1").is_err());
    }

    #[test]
    fn binding_classification() {
        assert_eq!(
            classify_binding("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"),
            Some(crate::domain::model::PluginKind::Auth)
        );
        assert_eq!(
            classify_binding("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
            Some(crate::domain::model::PluginKind::Guard)
        );
        assert_eq!(
            classify_binding("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
            Some(crate::domain::model::PluginKind::Transform)
        );
        assert_eq!(classify_binding("gts.cf.other.thing.v1~x"), None);
    }
}
