//! Built-in OAuth2 client-credentials auth plugin — `oauth2_client_cred` and
//! `oauth2_client_cred_basic` (ADR 0008, DoD
//! `cpt-cf-oagw-dod-plugin-system-builtins` and
//! `cpt-cf-oagw-dod-plugin-system-oauth2-cache`, algorithm
//! `cpt-cf-oagw-algo-plugin-system-oauth2-cache`).
//!
//! Both variants share one implementation parameterized by
//! [`ClientAuthMethod`] (`Form` vs `Basic` client authentication to the token
//! endpoint, RFC 6749 §2.3.1); they differ only in their registered GTS
//! identifier.  Tokens are cached in a `pingora-memory-cache` keyed by
//! `subject_tenant_id:subject_id:auth_method:config_hash` with key
//! re-verification on every hit (steps `inst-ps-oauth-key` .. `inst-ps-oauth-return`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{
    ClientAuthMethod, FetchedToken, OAuthClientConfig, SecretString, TokenError,
};
use toolkit_security::SecurityContext;
use url::Url;

use crate::domain::DomainError;
use crate::domain::plugin::{AuthPlugin, RequestContext};

/// Safety margin subtracted from the IdP-reported `expires_in` when computing
/// the cache TTL (step `inst-ps-oauth-ttl`): tokens with
/// `expires_in <= 30s` are never cached.
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// The configured ceiling TTL used when no gear-level override is supplied
/// (matches the `OagwConfig::default().token_cache_ttl_secs` of 300s / 5 min).
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);

/// The configured cache capacity used when no gear-level override is supplied
/// (matches `OagwConfig::default().token_cache_capacity` of 10,000).
pub const DEFAULT_CACHE_CAPACITY: usize = 10_000;

/// Injectable one-shot token fetch, mirroring
/// `toolkit_auth::oauth2::fetch_token` so tests can serve cached tokens
/// without network access.
#[async_trait]
pub trait TokenFetcher: Send + Sync {
    /// Performs a single OAuth2 client-credentials token exchange.
    ///
    /// # Errors
    /// Returns a [`TokenError`] for configuration, transport, or protocol
    /// failures.
    async fn fetch(&self, config: OAuthClientConfig) -> Result<FetchedToken, TokenError>;
}

/// The production [`TokenFetcher`] backed by `toolkit_auth::oauth2::fetch_token`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchToken;

#[async_trait]
impl TokenFetcher for FetchToken {
    async fn fetch(&self, config: OAuthClientConfig) -> Result<FetchedToken, TokenError> {
        toolkit_auth::oauth2::fetch_token(config).await
    }
}

/// Cache entry carrying the original lookup key so a key mismatch on a hit is
/// treated as a miss (defense-in-depth against hash collisions, ADR 0008 —
/// step `inst-ps-oauth-hit`).
#[derive(Debug, Clone)]
struct CachedToken {
    /// The exact cache key the entry was stored under.
    key: String,
    /// The bearer token, zeroized on eviction/drop.
    token: SecretString,
}

/// The `ClientAuthMethod` tag for the oauth2-client-credentials `Form` variant.
#[must_use]
pub const fn client_auth_form() -> ClientAuthMethod {
    ClientAuthMethod::Form
}

/// The `ClientAuthMethod` tag for the `Basic` variant.
#[must_use]
pub const fn client_auth_basic() -> ClientAuthMethod {
    ClientAuthMethod::Basic
}

/// OAuth2 client-credentials auth plugin with an internal token cache.
///
/// Config keys (`ctx.config`):
///
/// | Key | Required | Description |
/// |-----|----------|-------------|
/// | `token_endpoint` | Mutually exclusive with `issuer_url` | Direct token endpoint URL |
/// | `issuer_url` | Mutually exclusive with `token_endpoint` | OIDC issuer URL (discovery) |
/// | `client_id_ref` | Yes | `cred://` reference for the client id |
/// | `client_secret_ref` | Yes | `cred://` reference for the client secret |
/// | `scopes` | No | Space-separated OAuth2 scopes |
pub struct OAuth2ClientCredAuthPlugin {
    /// The canonical GTS identifier of this variant.
    gts_id: &'static str,
    /// CredStore client resolving `client_id_ref` / `client_secret_ref`.
    credstore: Arc<dyn CredStoreClientV1>,
    /// Client authentication method (`Form` or `Basic`).
    auth_method: ClientAuthMethod,
    /// In-process token cache (S3-FIFO + TinyLFU).
    cache: MemoryCache<String, CachedToken>,
    /// Ceiling TTL for cached tokens (`min(ttl, expires_in - 30s)`).
    cache_ttl: Duration,
    /// Injectable token fetch (production: [`FetchToken`]).
    fetcher: Arc<dyn TokenFetcher>,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `MemoryCache` and the fetcher are excluded: the cache holds
        // zeroizing tokens and the fetcher may capture secret material.
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("gts_id", &self.gts_id)
            .field("auth_method", &self.auth_method)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Creates a variant with the documented default cache sizing (5 min TTL
    /// ceiling, 10k capacity).
    #[must_use]
    pub fn new(
        credstore: Arc<dyn CredStoreClientV1>,
        gts_id: &'static str,
        auth_method: ClientAuthMethod,
        fetcher: Option<Arc<dyn TokenFetcher>>,
    ) -> Self {
        Self::new_with_cache(
            credstore,
            gts_id,
            auth_method,
            fetcher,
            DEFAULT_CACHE_TTL,
            DEFAULT_CACHE_CAPACITY,
        )
    }

    /// Creates a variant with an explicit cache TTL ceiling and capacity.
    ///
    /// Used by the gear to thread `OagwConfig::token_cache_ttl_secs` /
    /// `token_cache_capacity` when assembling the registries.
    #[must_use]
    pub fn new_with_cache(
        credstore: Arc<dyn CredStoreClientV1>,
        gts_id: &'static str,
        auth_method: ClientAuthMethod,
        fetcher: Option<Arc<dyn TokenFetcher>>,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            gts_id,
            credstore,
            auth_method,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
            fetcher: fetcher.unwrap_or_else(|| Arc::new(FetchToken)),
        }
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Parses the plugin config into a fetchable [`OAuthClientConfig`].
    ///
    /// # Errors
    /// [`DomainError::validation`] when a required key is missing or a URL /
    /// `cred://` reference is malformed; [`DomainError::SecretNotFound`] when a
    /// credential reference cannot be resolved.
    async fn parse_config(
        &self,
        config: &serde_json::Value,
        security: Option<&SecurityContext>,
    ) -> Result<OAuthClientConfig, DomainError> {
        let obj = config.as_object().ok_or_else(|| {
            DomainError::validation(None, "oauth2 auth plugin config must be an object")
        })?;

        let token_endpoint = match obj
            .get("token_endpoint")
            .and_then(serde_json::Value::as_str)
        {
            Some(raw) if !raw.trim().is_empty() => Some(Url::parse(raw.trim()).map_err(|e| {
                DomainError::validation(
                    None,
                    format!("oauth2 token_endpoint is not a valid URL: {e}"),
                )
            })?),
            _ => None,
        };
        let issuer_url = match obj.get("issuer_url").and_then(serde_json::Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => Some(Url::parse(raw.trim()).map_err(|e| {
                DomainError::validation(None, format!("oauth2 issuer_url is not a valid URL: {e}"))
            })?),
            _ => None,
        };
        // `token_endpoint` and `issuer_url` are mutually exclusive — reject
        // both-present and both-absent alike (the fetch precedence would
        // otherwise be left to the SDK).
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(DomainError::validation(
                None,
                "oauth2 auth plugin requires exactly one of token_endpoint or issuer_url",
            ));
        }
        if token_endpoint.is_some() && issuer_url.is_some() {
            return Err(DomainError::validation(
                None,
                "oauth2 auth plugin requires exactly one of token_endpoint or issuer_url \
                 (both were provided)",
            ));
        }

        let client_id_ref = obj
            .get("client_id_ref")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                DomainError::validation(None, "oauth2 auth plugin requires client_id_ref")
            })?;
        let client_secret_ref = obj
            .get("client_secret_ref")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                DomainError::validation(None, "oauth2 auth plugin requires client_secret_ref")
            })?;

        let client_id = resolve_secret(
            self.credstore.as_ref(),
            security,
            client_id_ref,
            "client_id",
        )
        .await?;
        let client_secret = resolve_secret(
            self.credstore.as_ref(),
            security,
            client_secret_ref,
            "client_secret",
        )
        .await?;

        let scopes = obj
            .get("scopes")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();

        Ok(OAuthClientConfig {
            token_endpoint,
            issuer_url,
            // The SDK builds a plain bearer header (`Basic base64(client:secret)`)
            // from these; the conversion borrows the zeroizing buffers only for
            // the duration of the fetch (acknowledged plaintext, ADR 0008).
            client_id: client_id.into_inner(),
            client_secret,
            scopes,
            auth_method: self.auth_method,
            ..OAuthClientConfig::default()
        })
    }

    /// Builds the cache key `subject_tenant_id:subject_id:auth_method:config_hash`
    /// (step `inst-ps-oauth-key`).
    fn build_cache_key(
        &self,
        security: Option<&SecurityContext>,
        config: &serde_json::Value,
    ) -> String {
        let (tenant, subject) = match security {
            Some(sec) => (sec.subject_tenant_id(), sec.subject_id()),
            None => (uuid::Uuid::nil(), uuid::Uuid::nil()),
        };
        let method = match self.auth_method {
            ClientAuthMethod::Form => "form",
            ClientAuthMethod::Basic => "basic",
        };
        // `serde_json::Value` objects preserve deterministic key order, so the
        // canonical string is stable for equal configs.
        let config_hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            serde_json::to_string(config)
                .unwrap_or_default()
                .hash(&mut hasher);
            hasher.finish()
        };
        format!("{tenant}:{subject}:{method}:{config_hash}")
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        if self.auth_method == ClientAuthMethod::Form {
            "oauth2_client_cred"
        } else {
            "oauth2_client_cred_basic"
        }
    }

    fn plugin_type(&self) -> &str {
        self.gts_id
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let config = &ctx.config;
        if config.is_null() || !config.is_object() {
            return Err(DomainError::validation(
                None,
                "oauth2 auth plugin requires a config object",
            ));
        }

        let cache_key = self.build_cache_key(ctx.security.as_ref(), config);

        // Cache hit with key re-verification (steps inst-ps-oauth-lookup /
        // inst-ps-oauth-hit): a mismatch is treated as a miss, never serving
        // another tuple's token.
        if let (Some(entry), _status) = self.cache.get(&cache_key)
            && entry.key == cache_key
        {
            inject_bearer(&mut ctx.headers, &entry.token);
            return Ok(());
        }

        // Cache miss: resolve credentials and fetch a fresh token (steps
        // inst-ps-oauth-fetch .. inst-ps-oauth-return).
        let oauth_cfg = self.parse_config(config, ctx.security.as_ref()).await?;
        let fetched =
            self.fetcher
                .fetch(oauth_cfg)
                .await
                .map_err(|e| DomainError::ProtocolError {
                    detail: format!("OAuth2 token fetch failed: {e}"),
                    cause: None,
                })?;

        let cached = CachedToken {
            key: cache_key.clone(),
            token: fetched.bearer.clone(),
        };

        // TTL rule (step inst-ps-oauth-ttl): `min(ttl, expires_in - 30s)`;
        // tokens with `expires_in <= 30s` are not cached (inst-ps-oauth-store).
        let ttl = fetched
            .expires_in
            .checked_sub(EXPIRY_SAFETY_MARGIN)
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| self.cache_ttl.min(remaining));
        if let Some(ttl) = ttl {
            self.cache.put(&cache_key, cached.clone(), Some(ttl));
        } else {
            // Evict defensively if a short-lived value was somehow present.
            self.cache.remove(&cache_key);
        }

        // Inject the token carried by the cache entry (or a failed-fetch token
        // reuse is not possible: on fetch failure we returned above).
        inject_bearer(&mut ctx.headers, &cached.token);
        Ok(())
    }
}

/// Injects `Authorization: Bearer <token>` into the request headers.  The
/// bearer is copied into a plain header value that lives only for the request
/// (acknowledged short-lived plaintext, ADR 0008).
fn inject_bearer(headers: &mut crate::domain::plugin::Headers, token: &SecretString) {
    headers.insert("Authorization", format!("Bearer {}", token.expose()));
}

/// Resolves a `cred://` reference through the CredStore SDK (algorithm
/// `cpt-cf-oagw-algo-plugin-system-resolve-secret`).
///
/// # Errors
/// [`DomainError::SecretNotFound`] (500) when the reference is malformed,
/// resolves to nothing, is access-denied, or is not valid UTF-8.
async fn resolve_secret(
    credstore: &dyn CredStoreClientV1,
    security: Option<&SecurityContext>,
    value_ref: &str,
    role: &str,
) -> Result<SecretString, DomainError> {
    let bare = value_ref
        .strip_prefix("cred://")
        .map(str::trim)
        .unwrap_or(value_ref.trim());
    let key = SecretRef::new(bare).map_err(|e| DomainError::SecretNotFound {
        detail: format!("invalid cred:// reference for {role}: {e}"),
    })?;

    let ctx = security.cloned().unwrap_or_else(SecurityContext::anonymous);
    let response = credstore
        .get(&ctx, &key)
        .await
        .map_err(|e| DomainError::SecretNotFound {
            detail: format!("credstore lookup failed for {role}: {e}"),
        })?;
    let secret = response.ok_or_else(|| DomainError::SecretNotFound {
        detail: format!("secret for {role} not found or access denied"),
    })?;

    let value = String::from_utf8(secret.value.as_bytes().to_vec()).map_err(|_| {
        DomainError::SecretNotFound {
            detail: format!("secret for {role} is not valid UTF-8"),
        }
    })?;
    Ok(SecretString::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::Headers;
    use crate::domain::plugin::ids::{OAUTH2_CLIENT_CRED, OAUTH2_CLIENT_CRED_BASIC};

    /// Deterministic fake IdP returning a fixed token/lifetime without network.
    #[derive(Debug, Clone)]
    struct FakeIdp {
        token: String,
        expires_in: Duration,
        fail: bool,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl TokenFetcher for FakeIdp {
        async fn fetch(&self, _cfg: OAuthClientConfig) -> Result<FetchedToken, TokenError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(TokenError::ConfigError("idp down".into()));
            }
            Ok(FetchedToken {
                bearer: SecretString::new(self.token.clone()),
                expires_in: self.expires_in,
            })
        }
    }

    fn credstore_with(creds: Vec<(String, String)>) -> Arc<dyn CredStoreClientV1> {
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
            creds,
        ))
    }

    const OID_ID: &str = "cred://oid-instance";
    const OID_SECRET: &str = "cred://oid-secret";

    fn oauth_config() -> serde_json::Value {
        serde_json::json!({
            "token_endpoint": "https://auth.example.com/token",
            "client_id_ref": OID_ID,
            "client_secret_ref": OID_SECRET,
            "scopes": "https://graph.microsoft.com/.default",
        })
    }

    fn plugin(
        idp: FakeIdp,
        method: ClientAuthMethod,
        gts_id: &'static str,
    ) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            credstore_with(vec![
                ("oid-instance".to_owned(), "client-abc".to_owned()),
                ("oid-secret".to_owned(), "hunter2-secret".to_owned()),
            ]),
            gts_id,
            method,
            Some(Arc::new(idp)),
        )
    }

    fn req() -> RequestContext {
        RequestContext {
            method: "POST".to_owned(),
            path: "/v1/models".to_owned(),
            query: Vec::new(),
            headers: Headers::new(),
            config: oauth_config(),
            security: None,
        }
    }

    #[tokio::test]
    async fn form_variant_fetches_and_injects_bearer() {
        let idp = FakeIdp {
            token: "tok-form".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        };
        let p = plugin(idp, client_auth_form(), OAUTH2_CLIENT_CRED);
        let mut ctx = req();
        p.authenticate(&mut ctx).await.expect("fetch succeeds");
        assert_eq!(ctx.headers.get("authorization"), Some("Bearer tok-form"));
        assert_eq!(p.id(), "oauth2_client_cred");
        assert_eq!(p.plugin_type(), OAUTH2_CLIENT_CRED);
    }

    #[tokio::test]
    async fn basic_variant_registers_its_own_id() {
        let idp = FakeIdp {
            token: "tok-basic".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        };
        let p = plugin(idp, client_auth_basic(), OAUTH2_CLIENT_CRED_BASIC);
        assert_eq!(p.id(), "oauth2_client_cred_basic");
        assert_eq!(p.plugin_type(), OAUTH2_CLIENT_CRED_BASIC);
    }

    #[tokio::test]
    async fn cache_hit_serves_token_without_second_fetch() {
        let idp = Arc::new(FakeIdp {
            token: "tok-cached".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        });
        let p = OAuth2ClientCredAuthPlugin::new(
            credstore_with(vec![
                ("oid-instance".to_owned(), "client-abc".to_owned()),
                ("oid-secret".to_owned(), "hunter2-secret".to_owned()),
            ]),
            OAUTH2_CLIENT_CRED,
            client_auth_form(),
            Some(Arc::clone(&idp) as Arc<dyn TokenFetcher>),
        );
        let mut first = req();
        p.authenticate(&mut first).await.expect("first fetch");
        assert_eq!(idp.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second call for the same tuple must hit the cache (no new IdP call).
        let mut second = req();
        p.authenticate(&mut second).await.expect("cached");
        assert_eq!(idp.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            second.headers.get("authorization"),
            Some("Bearer tok-cached")
        );
    }

    #[tokio::test]
    async fn different_config_isolates_cache_entries() {
        let idp = Arc::new(FakeIdp {
            token: "tok-scoped-a".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        });
        let p = OAuth2ClientCredAuthPlugin::new(
            credstore_with(vec![
                ("oid-instance".to_owned(), "client-abc".to_owned()),
                ("oid-secret".to_owned(), "hunter2-secret".to_owned()),
            ]),
            OAUTH2_CLIENT_CRED,
            client_auth_form(),
            Some(Arc::clone(&idp) as Arc<dyn TokenFetcher>),
        );
        let mut a = req();
        p.authenticate(&mut a).await.expect("fetch");
        // Same tuple → cached.
        let mut b = req();
        p.authenticate(&mut b).await.expect("cached");
        assert_eq!(idp.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A different scope → different config hash → a second fetch.
        let mut c = req();
        c.config["scopes"] = serde_json::json!("https://other.example/.default");
        p.authenticate(&mut c).await.expect("second fetch");
        assert_eq!(idp.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn short_lived_tokens_are_not_cached() {
        let idp = Arc::new(FakeIdp {
            token: "tok-short".to_owned(),
            expires_in: Duration::from_secs(20), // <= 30s safety margin
            fail: false,
            calls: Arc::new(0.into()),
        });
        let p = OAuth2ClientCredAuthPlugin::new(
            credstore_with(vec![
                ("oid-instance".to_owned(), "client-abc".to_owned()),
                ("oid-secret".to_owned(), "hunter2-secret".to_owned()),
            ]),
            OAUTH2_CLIENT_CRED,
            client_auth_form(),
            Some(Arc::clone(&idp) as Arc<dyn TokenFetcher>),
        );
        let mut a = req();
        p.authenticate(&mut a).await.expect("fetch");
        let mut b = req();
        p.authenticate(&mut b)
            .await
            .expect("fetch again — must not be cached");
        assert_eq!(idp.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_fetches_are_not_cached() {
        let idp = Arc::new(FakeIdp {
            token: "unused".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: true,
            calls: Arc::new(0.into()),
        });
        let p = OAuth2ClientCredAuthPlugin::new(
            credstore_with(vec![
                ("oid-instance".to_owned(), "client-abc".to_owned()),
                ("oid-secret".to_owned(), "hunter2-secret".to_owned()),
            ]),
            OAUTH2_CLIENT_CRED,
            client_auth_form(),
            Some(Arc::clone(&idp) as Arc<dyn TokenFetcher>),
        );
        let mut a = req();
        let err = p.authenticate(&mut a).await.expect_err("idp down");
        assert_eq!(err.status(), 502);
        assert_eq!(
            err.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1"
        );
        // Next request retries (not served from a cached failure).
        let mut b = req();
        p.authenticate(&mut b).await.expect_err("again");
        assert_eq!(idp.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn missing_secret_yields_secret_not_found_500() {
        let idp = Arc::new(FakeIdp {
            token: "unused".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        });
        // No secrets seeded → resolution fails before any fetch.
        let p = OAuth2ClientCredAuthPlugin::new(
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
            OAUTH2_CLIENT_CRED,
            client_auth_form(),
            Some(Arc::clone(&idp) as Arc<dyn TokenFetcher>),
        );
        let mut ctx = req();
        let err = p.authenticate(&mut ctx).await.expect_err("no secrets");
        assert_eq!(err.status(), 500);
        assert_eq!(
            err.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[tokio::test]
    async fn debug_output_never_reveals_tokens() {
        let idp = FakeIdp {
            token: "super-tok-secret".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        };
        let p = plugin(idp, client_auth_form(), OAUTH2_CLIENT_CRED);
        let mut ctx = req();
        p.authenticate(&mut ctx).await.expect("fetch");
        // The plugin itself stores no plaintext token; only its Debug (which
        // may be traced) is asserted — the injected `Authorization` header is
        // necessarily a short-lived plaintext (ADR 0008).
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("super-tok-secret"), "Debug leaked a token");
    }

    #[tokio::test]
    async fn both_token_endpoint_and_issuer_url_are_rejected() {
        let idp = FakeIdp {
            token: "tok".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        };
        let p = plugin(idp, client_auth_form(), OAUTH2_CLIENT_CRED);
        let config = serde_json::json!({
            "token_endpoint": "https://auth.example.com/token",
            "issuer_url": "https://issuer.example.com",
            "client_id_ref": OID_ID,
            "client_secret_ref": OID_SECRET,
        });
        let err = p
            .parse_config(&config, None)
            .await
            .expect_err("mutually exclusive sources must fail validation");
        assert!(
            matches!(err, DomainError::Validation { .. }),
            "expected a Validation error, got {err:?}"
        );
        assert!(
            err.to_string().contains("exactly one"),
            "diagnostic should state the exactly-one contract: {err:?}"
        );
    }

    #[tokio::test]
    async fn neither_token_endpoint_nor_issuer_url_is_rejected() {
        let idp = FakeIdp {
            token: "tok".to_owned(),
            expires_in: Duration::from_secs(3600),
            fail: false,
            calls: Arc::new(0.into()),
        };
        let p = plugin(idp, client_auth_form(), OAUTH2_CLIENT_CRED);
        let config = serde_json::json!({
            "client_id_ref": OID_ID,
            "client_secret_ref": OID_SECRET,
        });
        let err = p
            .parse_config(&config, None)
            .await
            .expect_err("absent sources must fail validation");
        assert!(
            matches!(err, DomainError::Validation { .. }),
            "expected a Validation error, got {err:?}"
        );
    }
}
