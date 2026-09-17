//! Credential resolution for the data plane.
//!
//! Implements the `cpt-cf-oagw-algo-data-plane-credential-resolution`
//! algorithm: determine the auth method from the plugin catalog, resolve
//! static secrets from the credential store, and exchange/cache OAuth2
//! client-credentials tokens (pingora-memory-cache backed, TTL-bounded).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{OAuthClientConfig, SecretString, fetch_token};

use crate::domain::error::DomainError;
use crate::domain::plugins::AuthContext;

/// A cached OAuth2 bearer token with its absolute expiry.
#[derive(Debug, Clone)]
struct CachedToken {
    bearer: SecretString,
    expires_at: Instant,
}

impl CachedToken {
    /// Whether the token is still within its TTL window.
    #[must_use]
    pub fn valid(&self, horizon: Duration) -> bool {
        // Refresh a little before the server-side expiry by comparing against
        // `expires_at - refresh_offset`, approximated by a fixed horizon.
        Instant::now() < self.expires_at - horizon
    }
}

/// Resolves secrets and OAuth2 tokens for auth plugins.
pub struct CredentialResolver {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    /// OAuth token cache (pingora-memory-cache), keyed by plugin alias.
    token_cache: Arc<MemoryCache<String, CachedToken>>,
    /// TTL for cached tokens (never stored beyond it).
    token_cache_ttl: Duration,
    /// Refresh horizon: treat tokens within this distance of expiry as stale.
    refresh_horizon: Duration,
}

impl CredentialResolver {
    /// Builds the resolver with the given cache sizing.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_cache_capacity: usize,
        token_cache_ttl: Duration,
    ) -> Self {
        Self {
            credstore,
            token_cache: Arc::new(MemoryCache::new(token_cache_capacity.max(1))),
            token_cache_ttl,
            refresh_horizon: Duration::from_secs(30),
        }
    }

    /// Resolves a `cred://`-style secret reference to its plaintext value.
    ///
    /// # Errors
    ///
    /// Returns `CredentialError` when the reference is malformed, missing, or
    /// inaccessible.
    pub async fn resolve_secret(
        &self,
        reference: &str,
        ctx: &AuthContext<'_>,
    ) -> Result<String, DomainError> {
        let parsed = credstore_sdk::SecretRef::new(reference).map_err(|e| {
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-fail
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-401
            DomainError::CredentialError(format!("invalid secret reference `{reference}`: {e}"))
            // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-fail
            // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-401
        })?;
        let found = self
            .credstore
            .get(ctx.security, &parsed)
            .await
            .map_err(|e| {
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-fail
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-401
                DomainError::CredentialError(format!("credential store error: {e}"))
                // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-fail
                // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-401
            })?;
        match found {
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-ok
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-return
            Some(resp) => Ok(String::from_utf8_lossy(resp.value.as_bytes()).into_owned()),
            // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-ok
            // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-return
            None => {
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-fail
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-401
                Err(DomainError::CredentialError(format!(
                    // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-fail
                    // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-cred-401
                    "secret `{reference}` not found or inaccessible"
                )))
            }
        }
    }

    /// Performs a (cached) OAuth2 client-credentials exchange for `config`
    /// in the caller's security context and returns the bearer token string.
    ///
    /// # Errors
    ///
    /// Returns `CredentialError` when the exchange or cache fails.
    pub async fn fetch_and_cache_token(
        &self,
        config: &OAuthClientConfig,
        ctx: &AuthContext<'_>,
    ) -> Result<String, DomainError> {
        let key = self.token_cache_key(config, ctx);

        // Cache lookup (`inst-token-cache` / `inst-token-cached` / `inst-token-reuse`).
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-cache
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-cached
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-reuse
        let (cached, _status) = self.token_cache.get(&key);
        if let Some(token) = cached
            && token.valid(self.refresh_horizon)
        {
            return Ok(token.bearer.expose().to_owned());
        }
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-reuse
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-cached
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-cache

        // Exchange (`inst-token-exchange`), then store (`inst-token-store`).
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-exchange
        let fetched = fetch_token(config.clone()).await.map_err(|e| {
            DomainError::CredentialError(format!("oauth2 token exchange failed: {e}"))
        })?;
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-exchange

        // Cache TTL must never outlive the server-side expiry minus the
        // refresh horizon: tokens within `refresh_horizon` of expiry are
        // already treated as stale by `CachedToken::valid`, so caching one
        // past that point would serve an effectively dead token or drift the
        // refresh horizon.
        let ttl = self
            .token_cache_ttl
            .min(fetched.expires_in.saturating_sub(self.refresh_horizon));
        let bearer = fetched.bearer;
        let token = CachedToken {
            bearer: bearer.clone(),
            expires_at: Instant::now() + fetched.expires_in,
        };
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-store
        if !ttl.is_zero() {
            self.token_cache.put(&key, token, Some(ttl));
        }
        Ok(bearer.expose().to_owned())
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-token-store
    }

    /// Cache key covering the full caller identity + credential configuration
    /// so tokens can never be reused across tenants, subjects, auth methods
    /// or credential configs (client_id, secret, endpoint, scopes, headers).
    fn token_cache_key(&self, config: &OAuthClientConfig, ctx: &AuthContext<'_>) -> String {
        let mut hasher = DefaultHasher::new();
        // `DefaultHasher` uses fixed keys, so the digest is stable across
        // calls within a process (a deterministic identity for the cache map).
        config.client_id.hash(&mut hasher);
        config.client_secret.expose().hash(&mut hasher);
        config
            .token_endpoint
            .as_ref()
            .map_or("", url::Url::as_str)
            .hash(&mut hasher);
        config
            .issuer_url
            .as_ref()
            .map_or("", |u| u.as_str())
            .hash(&mut hasher);
        config.scopes.hash(&mut hasher);
        format!("{:?}", config.auth_method).hash(&mut hasher);
        config.extra_headers.hash(&mut hasher);
        let config_hash = hasher.finish();
        let auth_method = format!("{:?}", config.auth_method);
        format!(
            "{}|{}|{}|{:016x}",
            ctx.tenant_id,
            ctx.security.subject_id(),
            auth_method,
            config_hash,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use toolkit_security::SecurityContext;
    use toolkit_security::constants::{DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID};

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(DEFAULT_SUBJECT_ID)
            .subject_tenant_id(DEFAULT_TENANT_ID)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .expect("valid test security context")
    }

    fn resolver_with(store: credstore_sdk::test_util::MockCredStoreClient) -> CredentialResolver {
        CredentialResolver::new(Arc::new(store), 8, Duration::from_secs(300))
    }

    #[test]
    fn cached_token_validity_window() {
        let t = CachedToken {
            bearer: SecretString::new("abc"),
            expires_at: Instant::now() + Duration::from_secs(60),
        };
        assert!(t.valid(Duration::from_secs(10)));
    }

    /// `fetch_and_cache_token` round trip is exercised end to end at the gate
    /// level; here we pin the cache keying that decides
    /// hit/reuse vs exchange (`inst-token-cache` / `inst-token-cached` /
    /// `inst-token-reuse` / `inst-token-exchange` / `inst-token-store`).
    ///
    /// The key must cover caller identity (tenant, subject) plus the full
    /// credential config — otherwise tokens could be reused across
    /// credentials or scopes, and TTL accounting would drift.
    #[test]
    fn token_cache_key_is_deterministic_and_identity_scoped() {
        let resolver = resolver_with(credstore_sdk::test_util::MockCredStoreClient::empty());
        let security = ctx();
        let context = auth_ctx(&security);
        let config = OAuthClientConfig {
            client_id: "client-a".to_owned(),
            ..Default::default()
        };
        let key = resolver.token_cache_key(&config, &context);
        // Same caller + same config → same key (token reuse across requests).
        assert_eq!(key, resolver.token_cache_key(&config, &context));
        // Key covers tenant + subject + auth method.
        assert!(
            key.starts_with(&format!(
                "{}|{}|{:?}|",
                DEFAULT_TENANT_ID, DEFAULT_SUBJECT_ID, config.auth_method
            )),
            "key must embed identity: {key}"
        );

        // Different client secret (any credential material) → different key.
        let mut other_config = config.clone();
        other_config.client_secret = SecretString::new("different-secret");
        assert_ne!(key, resolver.token_cache_key(&other_config, &context));

        // Different scopes → different key.
        let mut scoped = config.clone();
        scoped.scopes = vec!["read:orders".to_owned()];
        assert_ne!(key, resolver.token_cache_key(&scoped, &context));

        // Different tenant → different key.
        let foreign = SecurityContext::builder()
            .subject_id(DEFAULT_SUBJECT_ID)
            .subject_tenant_id(
                uuid::Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap(),
            )
            .token_scopes(vec!["*".to_owned()])
            .build()
            .expect("valid foreign tenant context");
        assert_ne!(key, resolver.token_cache_key(&config, &auth_ctx(&foreign)));
    }

    /// Statically resolved secret round trip (`inst-cred-ok` / `inst-cred-return`).
    #[test]
    fn resolve_secret_returns_stored_value() {
        let resolver = resolver_with(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
            vec![("my-secret".to_owned(), "s3cr3t".to_owned())],
        ));
        let security = ctx();
        let value = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resolver.resolve_secret("my-secret", &auth_ctx(&security)));
        assert_eq!(value.expect("resolved"), "s3cr3t");
    }

    /// Missing secret → credentialed 401-typed failure (`inst-cred-401`).
    #[test]
    fn resolve_secret_missing_returns_credential_error() {
        let resolver = resolver_with(credstore_sdk::test_util::MockCredStoreClient::empty());
        let security = ctx();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resolver.resolve_secret("absent", &auth_ctx(&security)))
            .expect_err("missing secret must fail");
        assert!(
            err.to_string().contains("not found or inaccessible"),
            "unexpected error: {err}"
        );
    }

    /// Backend failure maps to a credentialed failure (`inst-cred-401`).
    #[test]
    fn resolve_secret_maps_store_failure() {
        let resolver =
            resolver_with(credstore_sdk::test_util::MockCredStoreClient::always_failing());
        let security = ctx();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resolver.resolve_secret("any", &auth_ctx(&security)))
            .expect_err("failing store must error");
        assert!(
            err.to_string().contains("credential store error"),
            "unexpected error: {err}"
        );
    }

    /// Malformed reference (a URI-style `cred://` spelling is rejected by the
    /// ref grammar) fails loudly (`inst-cred-fail`).
    #[test]
    fn resolve_secret_rejects_malformed_reference() {
        let resolver = resolver_with(credstore_sdk::test_util::MockCredStoreClient::empty());
        let security = ctx();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resolver.resolve_secret("cred://bad/ref", &auth_ctx(&security)))
            .expect_err("malformed reference must fail validation");
        assert!(
            err.to_string().contains("invalid secret reference"),
            "unexpected error: {err}"
        );
    }

    fn auth_ctx<'a>(security: &'a SecurityContext) -> crate::domain::plugins::AuthContext<'a> {
        // `headers` borrows a leaked empty map: resolve_secret never reads
        // request headers, only the security context.
        let headers: &'a axum::http::HeaderMap = Box::leak(Box::new(axum::http::HeaderMap::new()));
        crate::domain::plugins::AuthContext {
            headers,
            security,
            // Mirrors the gate which derives the tenant from the subject's
            // tenant (single-tenant MVP callers resolve to DEFAULT_TENANT_ID).
            tenant_id: security.subject_tenant_id().to_string(),
        }
    }
}
