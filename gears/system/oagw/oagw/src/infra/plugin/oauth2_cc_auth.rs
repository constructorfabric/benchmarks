//! `oauth2_client_cred` / `oauth2_client_cred_basic` auth plugin (ADR-0008).
//!
//! One-shot `fetch_token` exchange, a per-`(tenant, subject, auth-method,
//! config)` in-process cache and `min(config_ttl, expires_in - 30s)` entry
//! lifetime. Failed fetches are never cached.

use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};

use crate::domain::plugin::{AuthPlugin, Caller, PluginError, RequestContext};
use crate::infra::credentials::SecretResolver;
use crate::infra::plugin::token_cache::TokenCacheConfig;

/// Safety margin subtracted from the IdP-reported `expires_in`.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// Tag of the form-encoded client-auth variant in cache keys.
const FORM_TAG: &str = "form";
/// Tag of the basic client-auth variant in cache keys.
const BASIC_TAG: &str = "basic";

/// A cached access token, carrying its cache key for collision detection.
#[derive(Clone)]
struct CachedToken {
    key: String,
    bearer: SecretString,
}

/// Auth plugin implementing the `OAuth2` client-credentials flow.
pub struct OAuth2ClientCredAuthPlugin {
    secrets: SecretResolver,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build the plugin with its own token cache.
    #[must_use]
    pub fn new(
        secrets: SecretResolver,
        auth_method: ClientAuthMethod,
        cache: TokenCacheConfig,
    ) -> Self {
        Self {
            secrets,
            auth_method,
            cache: MemoryCache::new(cache.capacity.max(1)),
            ttl: cache.ttl,
        }
    }
}

/// Parsed plugin configuration.
struct PluginConfig {
    token_endpoint: Option<url::Url>,
    issuer_url: Option<url::Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl PluginConfig {
    fn parse(value: &serde_json::Value) -> Result<Self, PluginError> {
        let endpoint = value
            .get("token_endpoint")
            .and_then(serde_json::Value::as_str);
        let issuer = value.get("issuer_url").and_then(serde_json::Value::as_str);
        if endpoint.is_some() && issuer.is_some() {
            return Err(PluginError::Config(
                "oauth2 plugin accepts either `token_endpoint` or `issuer_url`".to_owned(),
            ));
        }
        let parse_url = |key: &str, raw: &str| {
            url::Url::parse(raw)
                .map(Some)
                .map_err(|error| PluginError::Config(format!("invalid `{key}`: {error}")))
        };
        let client_id_ref = value
            .get("client_id_ref")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                PluginError::Config("oauth2 plugin requires `client_id_ref`".to_owned())
            })?;
        let client_secret_ref = value
            .get("client_secret_ref")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                PluginError::Config("oauth2 plugin requires `client_secret_ref`".to_owned())
            })?;
        let scopes = match value.get("scopes") {
            Some(serde_json::Value::String(scopes)) => {
                scopes.split_whitespace().map(str::to_owned).collect()
            }
            Some(serde_json::Value::Array(scopes)) => scopes
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let token_endpoint = match endpoint {
            Some(raw) => parse_url("token_endpoint", raw)?,
            None => None,
        };
        let issuer_url = match issuer {
            Some(raw) => parse_url("issuer_url", raw)?,
            None => None,
        };
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref: client_id_ref.to_owned(),
            client_secret_ref: client_secret_ref.to_owned(),
            scopes,
        })
    }
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => "oauth2_client_cred",
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
        }
    }

    fn plugin_type(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => crate::ids::AUTH_OAUTH2_CLIENT_CRED,
            ClientAuthMethod::Basic => crate::ids::AUTH_OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = PluginConfig::parse(&ctx.config)?;
        let key = build_cache_key(&ctx.caller, tag(self.auth_method), &ctx.config);
        if let Some(cached) = self.lookup(&key) {
            return inject(ctx, cached.bearer.expose());
        }

        let client_id = self
            .secrets
            .resolve(&ctx.caller.security_context, &config.client_id_ref)
            .await
            .map_err(|error| PluginError::Secret(error.to_string()))?;
        let client_secret = self
            .secrets
            .resolve(&ctx.caller.security_context, &config.client_secret_ref)
            .await
            .map_err(|error| PluginError::Secret(error.to_string()))?;

        let fetched = fetch_token(OAuthClientConfig {
            token_endpoint: config.token_endpoint,
            issuer_url: config.issuer_url,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: config.scopes,
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_secs(30),
            jitter_max: Duration::from_secs(0),
            min_refresh_period: Duration::from_secs(0),
            default_ttl: Duration::from_mins(5),
            http_config: None,
        })
        .await
        .map_err(|error| PluginError::Auth(format!("token fetch failed: {error}")))?;

        if let Some(ttl) = token_ttl(self.ttl, fetched.expires_in) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    bearer: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }
        inject(ctx, fetched.bearer.expose())
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Cache lookup that discards entries whose key does not match (ADR-0008).
    fn lookup(&self, key: &str) -> Option<CachedToken> {
        let (cached, _) = self.cache.get(key);
        cached.filter(|entry| entry.key == key)
    }
}

fn tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Form => FORM_TAG,
        ClientAuthMethod::Basic => BASIC_TAG,
    }
}

/// Cache key: tenant, subject, client-auth method and a config hash (ADR-0008).
#[must_use]
pub fn build_cache_key(caller: &Caller, auth_method: &str, config: &serde_json::Value) -> String {
    format!(
        "{}:{}:{}:{}",
        caller.tenant_id,
        caller.subject_id,
        auth_method,
        crate::infra::hash::stable_hash(config)
    )
}

/// Entry lifetime: `min(config_ttl, expires_in - 30s)`; `None` when the token
/// is too short-lived to cache.
#[must_use]
pub fn token_ttl(configured: Duration, expires_in: Duration) -> Option<Duration> {
    let effective = expires_in.saturating_sub(EXPIRY_MARGIN);
    if effective.is_zero() {
        None
    } else {
        Some(effective.min(configured))
    }
}

fn inject(ctx: &mut RequestContext, token: &str) -> Result<(), PluginError> {
    if let (Ok(name), Ok(value)) = (
        axum::http::HeaderName::try_from("authorization"),
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")),
    ) {
        ctx.headers.insert(name, value);
        return Ok(());
    }
    Err(PluginError::Internal(
        "oauth2 plugin produced an invalid authorization header".to_owned(),
    ))
}

#[cfg(test)]
#[path = "oauth2_cc_auth_tests.rs"]
mod oauth2_cc_auth_tests;
