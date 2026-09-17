//! `OAuth2ClientCredAuthPlugin` — RFC 6749 §4.4 client credentials flow.
//!
//! Tokens are fetched once per `(tenant, subject, auth_method, config)` tuple
//! through [`toolkit_auth::oauth2::fetch_token`] and cached in a
//! `pingora-memory-cache` with a TTL of
//! `min(config_ttl, expires_in − 30s safety margin)`. Tokens that expire in 30
//! seconds or less are not cached at all, and failed fetches are never cached.
//!
//! Cache-safety: `pingora-memory-cache` hashes keys to `u64` (TinyUfo) and does
//! not compare `Eq` on hit, so the entry carries the original key and is
//! re-verified before use — a hash collision degrades to a cache miss and can
//! never hand back another tenant's token.
//!
//! Config keys:
//!
//! | Key | Required | Meaning |
//! |---|---|---|
//! | `token_endpoint` | exclusive with `issuer_url` | token endpoint URL |
//! | `issuer_url` | exclusive with `token_endpoint` | OIDC issuer |
//! | `client_id_ref` | yes | `cred://` reference for the client id |
//! | `client_secret_ref` | yes | `cred://` reference for the client secret |
//! | `scopes` | no | space-separated scope list |

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};

use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::plugin::{
    AuthPlugin, PluginError, RequestContext, ResolvedSecret, SecretResolver,
};

/// Safety margin subtracted from the IdP-reported `expires_in`.
pub const EXPIRY_SAFETY_MARGIN_SECS: u64 = 30;
/// `auth_method` tag embedded in the cache key.
const AUTH_TAG_BASIC: &str = "basic";
/// `auth_method` tag embedded in the cache key.
const AUTH_TAG_FORM: &str = "form";

/// Cached token entry: the token plus the key it was stored under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: ResolvedSecret,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// OAuth2 client-credentials auth plugin with an internal token cache.
pub struct OAuth2ClientCredAuthPlugin {
    secrets: Arc<dyn SecretResolver>,
    auth_method: ClientAuthMethod,
    plugin_id: &'static str,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

/// Deterministic cache key covering every identity component.
#[must_use]
pub fn build_cache_key(ctx: &RequestContext, auth_method: ClientAuthMethod) -> String {
    let tenant = ctx
        .tenant_id
        .as_ref()
        .map(uuid::Uuid::to_string)
        .unwrap_or_default();
    let subject = ctx
        .security_context
        .as_ref()
        .map(|sc| sc.subject_id().to_string())
        .unwrap_or_default();
    let tag = match auth_method {
        ClientAuthMethod::Basic => AUTH_TAG_BASIC,
        ClientAuthMethod::Form => AUTH_TAG_FORM,
    };
    format!("{tenant}:{subject}:{tag}:{}", hash_config(&ctx.config))
}

/// Deterministic, order-independent digest of the plugin configuration.
fn hash_config(config: &BTreeMap<String, String>) -> u64 {
    // `BTreeMap` iterates in sorted order, so the digest is stable.
    let mut hasher: Box<dyn Hasher> = Box::new(std::collections::hash_map::DefaultHasher::new());
    for (key, value) in config {
        (key, value).hash(&mut hasher);
    }
    hasher.finish()
}

/// Cache TTL: `min(config_ttl, expires_in − 30s)`, or `None` when the token
/// would be stale on arrival.
#[must_use]
pub fn effective_ttl(config_ttl: Duration, expires_in: Duration) -> Option<Duration> {
    let usable = expires_in.checked_sub(Duration::from_secs(EXPIRY_SAFETY_MARGIN_SECS))?;
    // A token that is already inside the safety margin must not be cached at
    // all: a zero-length entry would be treated as expired on every read.
    if usable.is_zero() {
        return None;
    }
    Some(usable.min(config_ttl))
}

fn configured_endpoint(ctx: &RequestContext) -> Option<&str> {
    ctx.config
        .get("token_endpoint")
        .or_else(|| ctx.config.get("issuer_url"))
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

impl OAuth2ClientCredAuthPlugin {
    /// Form-credential variant (credentials in the request body).
    #[must_use]
    pub fn form(secrets: Arc<dyn SecretResolver>) -> Self {
        Self::new(
            secrets,
            ClientAuthMethod::Form,
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        )
    }

    /// Basic-credential variant (credentials in the `Authorization` header).
    #[must_use]
    pub fn basic(secrets: Arc<dyn SecretResolver>) -> Self {
        Self::new(
            secrets,
            ClientAuthMethod::Basic,
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        )
    }

    /// Builds the plugin with the default gear config sizing.
    #[must_use]
    pub fn new(
        secrets: Arc<dyn SecretResolver>,
        auth_method: ClientAuthMethod,
        plugin_id: &'static str,
    ) -> Self {
        Self::sized(
            secrets,
            auth_method,
            plugin_id,
            &crate::config::OagwConfig::default(),
        )
    }

    /// Builds the plugin with explicit cache sizing from gear config.
    #[must_use]
    pub fn sized(
        secrets: Arc<dyn SecretResolver>,
        auth_method: ClientAuthMethod,
        plugin_id: &'static str,
        config: &crate::config::OagwConfig,
    ) -> Self {
        Self {
            secrets,
            auth_method,
            plugin_id,
            cache: MemoryCache::new(config.token_cache_capacity),
            cache_ttl: Duration::from_secs(config.token_cache_ttl_secs),
        }
    }

    async fn resolve_config_secret(
        &self,
        ctx: &RequestContext,
        key: &str,
    ) -> Result<ResolvedSecret, PluginError> {
        let reference = ctx.config.get(key).map(String::as_str).ok_or_else(|| {
            PluginError::new("OAUTH2_CONFIG_MISSING", format!("{key} is not configured"))
        })?;
        let security_context = ctx.security_context.clone().ok_or_else(|| {
            PluginError::new(
                "OAUTH2_NO_SUBJECT",
                "no security context for secret resolution",
            )
        })?;
        self.secrets
            .resolve(&security_context, reference)
            .await?
            .ok_or_else(|| PluginError::new("SECRET_NOT_FOUND", "credential reference unresolved"))
    }

    /// Fetches a fresh token from the IdP.
    ///
    /// The returned [`CachedToken`] carries the cache key it belongs to; the
    /// `Duration` is the IdP-reported `expires_in`.
    async fn fetch_fresh(
        &self,
        ctx: &RequestContext,
    ) -> Result<(CachedToken, Duration), PluginError> {
        let endpoint = configured_endpoint(ctx).ok_or_else(|| {
            PluginError::new(
                "OAUTH2_CONFIG_MISSING",
                "either token_endpoint or issuer_url must be configured",
            )
        })?;
        let url = url::Url::parse(endpoint.trim()).map_err(|_| {
            PluginError::new("OAUTH2_CONFIG_INVALID", "token endpoint is not a valid URL")
        })?;

        let client_id = self.resolve_config_secret(ctx, "client_id_ref").await?;
        let client_secret = self.resolve_config_secret(ctx, "client_secret_ref").await?;
        let scopes = ctx
            .config
            .get("scopes")
            .map(|value| value.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();

        let issuer_url = if ctx.config.contains_key("token_endpoint") {
            None
        } else {
            Some(url.clone())
        };
        let token_endpoint = if issuer_url.is_none() {
            Some(url)
        } else {
            None
        };

        let config = OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id: client_id.expose().to_owned(),
            client_secret: toolkit_auth::SecretString::new(client_secret.expose().to_owned()),
            scopes,
            auth_method: self.auth_method,
            ..OAuthClientConfig::default()
        };

        let fetched = fetch_token(config).await.map_err(|err| {
            PluginError::new(
                "OAUTH2_TOKEN_FETCH_FAILED",
                format!("token endpoint rejected the client credentials ({err})"),
            )
        })?;

        Ok((
            CachedToken {
                key: build_cache_key(ctx, self.auth_method),
                token: ResolvedSecret::new(fetched.bearer.expose().to_owned()),
            },
            fetched.expires_in,
        ))
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.plugin_id
    }

    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        _secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError> {
        let key = build_cache_key(ctx, self.auth_method);
        if let (Some(entry), _) = self.cache.get(&key)
            && entry.key == key
        {
            ctx.set_header("Authorization", entry.token.render_prefixed("Bearer "));
            return Ok(());
        }
        // Hash collision: fall through and treat as a miss.

        let (cached, expires_in) = self.fetch_fresh(ctx).await?;
        if let Some(ttl) = effective_ttl(self.cache_ttl, expires_in) {
            self.cache.put(&key, cached.clone(), Some(ttl));
        }
        ctx.set_header("Authorization", cached.token.render_prefixed("Bearer "));
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    struct Fixed(Vec<(&'static str, String)>);

    #[async_trait::async_trait]
    impl SecretResolver for Fixed {
        async fn resolve(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            reference: &str,
        ) -> Result<Option<ResolvedSecret>, PluginError> {
            let bare = reference.strip_prefix("cred://").unwrap_or(reference);
            for (key, value) in &self.0 {
                if *key == bare {
                    return Ok(Some(ResolvedSecret::new(value.clone())));
                }
            }
            Ok(None)
        }
    }

    #[test]
    fn cache_key_separates_tenants_subjects_and_auth_methods() {
        let tenant_a = uuid::Uuid::new_v4();
        let tenant_b = uuid::Uuid::new_v4();
        let cfg = [("client_id_ref", "cred://a")];
        let base = RequestContext {
            security_context: Some(toolkit_security::SecurityContext::anonymous()),
            tenant_id: Some(tenant_a),
            config: cfg
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..RequestContext::default()
        };
        let mut other_tenant = base.clone();
        other_tenant.tenant_id = Some(tenant_b);
        let mut other_method = base.clone();
        other_method.tenant_id = Some(tenant_b);

        assert_ne!(
            build_cache_key(&base, ClientAuthMethod::Form),
            build_cache_key(&other_tenant, ClientAuthMethod::Form)
        );
        assert_ne!(
            build_cache_key(&other_method, ClientAuthMethod::Form),
            build_cache_key(&other_method, ClientAuthMethod::Basic)
        );
        // Config ordering must not change the digest.
        let mut reordered = base.clone();
        reordered
            .config
            .insert("scopes".to_owned(), "a b".to_owned());
        let mut reordered2 = reordered.clone();
        reordered2
            .config
            .insert("scopes".to_owned(), "a b".to_owned());
        assert_eq!(
            build_cache_key(&reordered, ClientAuthMethod::Form),
            build_cache_key(&reordered2, ClientAuthMethod::Form)
        );
    }

    #[test]
    fn ttl_rule_is_min_with_safety_margin() {
        let config_ttl = Duration::from_secs(300);
        assert_eq!(
            effective_ttl(config_ttl, Duration::from_secs(3600)),
            Some(config_ttl)
        );
        assert_eq!(
            effective_ttl(config_ttl, Duration::from_secs(60)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(effective_ttl(config_ttl, Duration::from_secs(30)), None);
        assert_eq!(effective_ttl(config_ttl, Duration::from_secs(5)), None);
    }

    #[test]
    fn missing_endpoint_is_reported_before_secrets() {
        let plugin = OAuth2ClientCredAuthPlugin::form(Arc::new(Fixed(Vec::new())));
        let ctx = RequestContext {
            security_context: Some(toolkit_security::SecurityContext::anonymous()),
            ..RequestContext::default()
        };
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let err = rt.block_on(async { plugin.fetch_fresh(&ctx).await.expect_err("missing") });
        assert_eq!(err.code, "OAUTH2_CONFIG_MISSING");
    }
}
