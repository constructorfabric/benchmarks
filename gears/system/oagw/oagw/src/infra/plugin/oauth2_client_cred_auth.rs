//! `oauth2_client_cred.v1` / `oauth2_client_cred_basic.v1` — `OAuth2` Client
//! Credentials flow with an internal token cache (ADR 0008).
//!
//! The plugin exchanges `client_id` + `client_secret` (resolved from
//! `cred_store`) for a bearer token at a token endpoint (or via OIDC
//! discovery), injects `Authorization: Bearer <token>`, and caches the token
//! keyed on `(tenant, subject, auth method, config)` so a multi-tenant data
//! plane issues one `IdP` call per distinct identity per TTL window.

use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use dashmap::DashMap;
use futures_util::FutureExt;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use url::Url;

use crate::domain::plugin::{
    AuthContext, AuthPlugin, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, PluginError, cfg_string, secret_ref_name,
};

/// Safety margin subtracted from the IdP-reported `expires_in` (ADR 0008).
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// A cached access token tagged with the exact cache key it was stored under,
/// so a (2^-45) `TinyUfo` hash collision resolves to a miss instead of another
/// tenant's token (ADR 0008 "Hash-Collision Safety").
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Arc<SecretString>,
}

/// A shareable snapshot of a fetched token: the `IdP`'s `FetchedToken` is not
/// `Clone`, so the single-flight future's `Shared` output carries just the two
/// fields the cache + injection need. `Arc<SecretString>` keeps a single
/// zeroized copy shared by every waiter.
#[derive(Clone)]
struct FetchedTokenState {
    bearer: Arc<SecretString>,
    expires_in: Duration,
}

/// The pinned, boxed token-exchange future stored in the single-flight map.
type TokenFetch = Pin<Box<dyn Future<Output = Result<FetchedTokenState, PluginError>> + Send>>;

/// The shared, single-flight token-exchange future registered per cache key.
type SharedTokenFetch = futures_util::future::Shared<TokenFetch>;

/// Cancellation-safe removal of the in-flight single-flight entry.
///
/// The registering request holds one of these across its `.await` on its own
/// shared future. If the registering future is dropped mid-await (e.g. a
/// client disconnect drops the axum handler), the continuation that would have
/// removed the entry never runs — without this guard the entry (a `Shared`
/// future) is pinned forever, so a later `IdP` failure makes every subsequent
/// request for that key fail (auth outage). RAII keeps the removal on the
/// `Drop` path too, so the entry is released no matter how the await ends.
struct InFlightGuard {
    key: String,
    in_flight: Arc<DashMap<String, SharedTokenFetch>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.remove(&self.key);
    }
}

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1`
/// (`Form`) and `...oauth2_client_cred_basic.v1` (`Basic`).
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: toolkit_auth::oauth2::ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
    /// Per-key single-flight: the shared future of the one in-progress `IdP`
    /// exchange per cache key, so a concurrent stampede of cache misses issues
    /// exactly one token request (the future is removed when it completes).
    /// Wrapped in an `Arc` so the cancellation-safe [`InFlightGuard`] can hold
    /// a handle to the map across an await and remove its entry on drop.
    in_flight: Arc<DashMap<String, SharedTokenFetch>>,
}

impl OAuth2ClientCredAuthPlugin {
    /// Create the plugin for one client-auth method with the shared token
    /// cache configuration (ADR 0008 gear-level config).
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
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
            in_flight: Arc::new(DashMap::new()),
        }
    }

    fn gts_id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        }
    }

    /// Deterministic tag of the client-auth method for cache-key separation.
    fn auth_method_tag(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => "form",
            ClientAuthMethod::Basic => "basic",
        }
    }
}

/// Parsed plugin configuration (`auth.config`).
struct Config {
    token_endpoint: Option<Url>,
    issuer_url: Option<Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

fn parse_config(config: &serde_json::Value) -> Result<Config, PluginError> {
    let token_endpoint = match cfg_string(config, "token_endpoint") {
        Some(raw) => Some(Url::parse(raw).map_err(|e| {
            PluginError::AuthenticationFailed(format!(
                "oauth2 plugin: invalid token_endpoint '{raw}': {e}"
            ))
        })?),
        None => None,
    };
    let issuer_url = match cfg_string(config, "issuer_url") {
        Some(raw) => Some(Url::parse(raw).map_err(|e| {
            PluginError::AuthenticationFailed(format!(
                "oauth2 plugin: invalid issuer_url '{raw}': {e}"
            ))
        })?),
        None => None,
    };
    if token_endpoint.is_some() && issuer_url.is_some() {
        return Err(PluginError::AuthenticationFailed(
            "oauth2 plugin: token_endpoint and issuer_url are mutually exclusive".to_owned(),
        ));
    }
    if token_endpoint.is_none() && issuer_url.is_none() {
        return Err(PluginError::AuthenticationFailed(
            "oauth2 plugin requires exactly one of 'token_endpoint' or 'issuer_url' in auth.config"
                .to_owned(),
        ));
    }
    let client_id_ref = cfg_string(config, "client_id_ref").ok_or_else(|| {
        PluginError::AuthenticationFailed(
            "oauth2 plugin requires a 'client_id_ref' (cred://...) in auth.config".to_owned(),
        )
    })?;
    let client_secret_ref = cfg_string(config, "client_secret_ref").ok_or_else(|| {
        PluginError::AuthenticationFailed(
            "oauth2 plugin requires a 'client_secret_ref' (cred://...) in auth.config".to_owned(),
        )
    })?;
    let scopes = cfg_string(config, "scopes")
        .map(|s| s.split_whitespace().map(ToOwned::to_owned).collect())
        .unwrap_or_default();

    Ok(Config {
        token_endpoint,
        issuer_url,
        client_id_ref: client_id_ref.to_owned(),
        client_secret_ref: client_secret_ref.to_owned(),
        scopes,
    })
}

/// Deterministic hash of the plugin config's string key/value pairs (keys
/// sorted) so different upstream configs get distinct cache entries
/// (ADR 0008 "Cache Key Design").
fn hash_config(config: &serde_json::Value) -> u64 {
    let mut keys: Vec<&str> = config
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    keys.sort_unstable();
    let mut hasher = DefaultHasher::new();
    for key in keys {
        key.hash(&mut hasher);
        if let Some(v) = config.get(key) {
            v.to_string().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Build the cache key from the full identity + config tuple
/// (ADR 0008 "Cache Key Design").
fn build_cache_key(
    tenant: uuid::Uuid,
    subject: uuid::Uuid,
    auth_tag: &str,
    config_hash: u64,
) -> String {
    format!("{tenant}:{subject}:{auth_tag}:{config_hash}")
}

/// TTL for one cached token: the IdP-reported lifetime minus the safety
/// margin, capped at the gear-level cache TTL. `None` when the token has no
/// usable lifetime after the margin (not cached) (ADR 0008).
fn token_cache_ttl(expires_in: Duration, config_ttl: Duration) -> Option<Duration> {
    expires_in
        .checked_sub(EXPIRY_SAFETY_MARGIN)
        .filter(|t| !t.is_zero())
        .map(|t| t.min(config_ttl))
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        self.gts_id()
    }

    async fn authenticate(&self, ctx: &mut AuthContext<'_>) -> Result<(), PluginError> {
        let cfg = parse_config(ctx.config)?;
        let config_hash = hash_config(ctx.config);
        let cache_key = build_cache_key(
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            self.auth_method_tag(),
            config_hash,
        );

        // Cache hit (defense-in-depth: verify the stored key).
        if let Some(entry) = self.cache.get(&cache_key).0
            && entry.key == cache_key
        {
            let token = entry.token.expose();
            insert_bearer(ctx, token);
            return Ok(());
        }

        // Cache miss: resolve credentials and exchange for a token. All
        // concurrent misses for the same key share one in-flight IdP exchange
        // (single-flight) instead of stampeding the token endpoint.
        let client_id =
            resolve_secret(&self.credstore, ctx, "client_id", &cfg.client_id_ref).await?;
        let client_secret = SecretString::new(
            resolve_secret(
                &self.credstore,
                ctx,
                "client_secret",
                &cfg.client_secret_ref,
            )
            .await?,
        );

        let oauth_config = OAuthClientConfig {
            token_endpoint: cfg.token_endpoint,
            issuer_url: cfg.issuer_url,
            client_id,
            client_secret,
            scopes: cfg.scopes.clone(),
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            // Field defaults mirroring `toolkit-auth`'s own defaults; only
            // `default_ttl` matters for the cache fallback when the IdP omits
            // `expires_in`.
            refresh_offset: Duration::from_mins(30),
            jitter_max: Duration::from_mins(5),
            min_refresh_period: Duration::from_secs(10),
            default_ttl: self.cache_ttl,
            http_config: None,
        };

        // The fetched token is cached by the single-flight registerer (before
        // its in-flight entry is removed), so every caller — registerer and
        // waiter alike — observes the same cached value after the flight.
        let state = self.single_flight_fetch(&cache_key, oauth_config).await?;
        insert_bearer(ctx, state.bearer.expose());
        Ok(())
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Fetch a token for `cache_key`, single-flighted: the first request to
    /// register registers the shared exchange future; concurrent waiters for
    /// the same key await that same future. On completion (success or error)
    /// only the registering request removes the entry, so all waiters observe
    /// the same outcome and the `IdP` sees exactly one call per key per window.
    async fn single_flight_fetch(
        &self,
        cache_key: &str,
        oauth_config: OAuthClientConfig,
    ) -> Result<FetchedTokenState, PluginError> {
        // Re-check the cache inside the single-flight path: a parallel miss
        // may have completed and cached since the caller's first check, in
        // which case there is nothing to register.
        if let Some(entry) = self.cache.get(cache_key).0
            && entry.key == cache_key
        {
            return Ok(FetchedTokenState {
                bearer: Arc::clone(&entry.token),
                // Unused for injection; the registerer re-puts a fresh TTL
                // with the same key once the new flight completes.
                expires_in: self.cache_ttl,
            });
        }

        let future: TokenFetch = Box::pin(async move {
            let fetched = fetch_token(oauth_config).await.map_err(|e| {
                PluginError::AuthenticationFailed(format!(
                    "oauth2 plugin: token exchange failed: {e}"
                ))
            })?;
            Ok(FetchedTokenState {
                bearer: Arc::new(fetched.bearer),
                expires_in: fetched.expires_in,
            })
        });
        let shared = future.shared();

        // Register (or reuse) the in-flight exchange under the shard lock. The
        // `vacant` registerer holds a guard across its own await: if the
        // registering future is dropped mid-await (e.g. a client disconnect
        // drops the axum handler), the guard's `Drop` still removes the entry,
        // so a later IdP failure can never pin the key forever. Waiters never
        // create a guard, keeping the existing rule that only the registerer
        // removes the entry.
        let mut guard = None;
        let waiter = match self.in_flight.entry(cache_key.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(occupied) => occupied.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(shared.clone());
                guard = Some(InFlightGuard {
                    key: cache_key.to_owned(),
                    in_flight: Arc::clone(&self.in_flight),
                });
                shared
            }
        };

        let result = waiter.await;

        // The registerer caches the fetched token BEFORE its guard drops, so
        // waiters re-checking the cache after the flight observe the token
        // and the in-flight entry stays present until the cache is warm (no
        // duplicate-flight window between completion and cache put).
        if guard.is_some()
            && let Ok(state) = &result
            && let Some(ttl) = token_cache_ttl(state.expires_in, self.cache_ttl)
        {
            let entry = CachedToken {
                key: cache_key.to_owned(),
                token: Arc::clone(&state.bearer),
            };
            self.cache.put(cache_key, entry, Some(ttl));
        }
        // `guard` drops here → the in-flight entry is removed (only the
        // registerer removes, matching the existing rule).
        result
    }
}

/// Resolve a `cred://` reference to a UTF-8 secret via `cred_store`.
async fn resolve_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &mut AuthContext<'_>,
    which: &str,
    reference: &str,
) -> Result<String, PluginError> {
    let reference = SecretRef::new(secret_ref_name(reference)).map_err(|e| {
        PluginError::AuthenticationFailed(format!(
            "oauth2 plugin: invalid {which} secret reference '{reference}': {e}"
        ))
    })?;
    let Some(secret) = credstore
        .get(ctx.security_context, &reference)
        .await
        .map_err(|e| {
            PluginError::Internal(format!(
                "oauth2 plugin: cred_store error resolving {which} '{}': {e}",
                reference.as_ref()
            ))
        })?
    else {
        return Err(PluginError::SecretNotFound(format!(
            "oauth2 plugin: {which} secret '{}' not found or not accessible",
            reference.as_ref()
        )));
    };
    let value = std::str::from_utf8(secret.value.as_bytes()).map_err(|_| {
        PluginError::AuthenticationFailed(format!(
            "oauth2 plugin: {which} secret '{}' is not valid UTF-8",
            reference.as_ref()
        ))
    })?;
    if value.is_empty() {
        return Err(PluginError::AuthenticationFailed(format!(
            "oauth2 plugin: {which} secret '{}' is empty",
            reference.as_ref()
        )));
    }
    Ok(value.to_owned())
}

/// Inject `Authorization: Bearer <token>` into the outbound request.
fn insert_bearer(ctx: &mut AuthContext<'_>, token: &str) {
    let value = format!("Bearer {token}");
    if let Ok(v) = http::HeaderValue::from_str(&value) {
        ctx.headers.insert(http::header::AUTHORIZATION, v);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_key_is_deterministic_and_identity_scoped() {
        let tenant = uuid::Uuid::from_u128(1);
        let subject_a = uuid::Uuid::from_u128(2);
        let subject_b = uuid::Uuid::from_u128(3);
        let h = hash_config(&json!({"token_endpoint": "https://idp/token", "scopes": "a b"}));

        let k1 = build_cache_key(tenant, subject_a, "form", h);
        let k1_repeat = build_cache_key(tenant, subject_a, "form", h);
        // Deterministic.
        assert_eq!(k1, k1_repeat);
        // Cross-subject isolation (ADR 0008).
        assert_ne!(k1, build_cache_key(tenant, subject_b, "form", h));
        // Cross-auth-method isolation (Form vs Basic never collide).
        assert_ne!(k1, build_cache_key(tenant, subject_a, "basic", h));
        // Cross-config isolation.
        assert_ne!(
            k1,
            build_cache_key(
                tenant,
                subject_a,
                "form",
                hash_config(&json!({"scopes": "c"}))
            )
        );
    }

    #[test]
    fn config_hash_is_order_independent_but_value_sensitive() {
        let a = json!({"token_endpoint": "https://idp/token", "client_id_ref": "cred://x"});
        let b = json!({"client_id_ref": "cred://x", "token_endpoint": "https://idp/token"});
        assert_eq!(hash_config(&a), hash_config(&b));
        let c = json!({"token_endpoint": "https://idp/token", "client_id_ref": "cred://y"});
        assert_ne!(hash_config(&a), hash_config(&c));
    }

    #[test]
    fn in_flight_guard_drop_removes_the_map_entry_without_awaiting() {
        // Regression for the single-flight leak: if the registering future is
        // dropped mid-await (client disconnect), the continuation that removed
        // the entry never runs. The guard's `Drop` must release the key even
        // when nothing is awaited.
        let map: Arc<DashMap<String, SharedTokenFetch>> = Arc::new(DashMap::new());
        let future: TokenFetch = Box::pin(async {
            Ok(FetchedTokenState {
                bearer: Arc::new(SecretString::new("t".to_owned())),
                expires_in: Duration::from_mins(1),
            })
        });
        let shared = future.shared();
        map.insert("k".to_owned(), shared);
        assert!(map.contains_key("k"));

        let guard = InFlightGuard {
            key: "k".to_owned(),
            in_flight: Arc::clone(&map),
        };
        // Drop the guard without awaiting — this is what cancellation does to
        // the registerer whose future is dropped mid-await.
        drop(guard);
        assert!(
            !map.contains_key("k"),
            "guard Drop must remove the in-flight entry even without awaiting"
        );
    }

    #[test]
    fn parse_config_requires_exactly_one_endpoint() {
        assert!(parse_config(&json!({"token_endpoint": "https://idp/token"})).is_err());
        let ok = parse_config(&json!({
            "token_endpoint": "https://idp/token",
            "client_id_ref": "cred://cid",
            "client_secret_ref": "cred://cs"
        }));
        assert!(ok.is_ok());
        // Both endpoints are mutually exclusive (ADR 0008).
        let both = parse_config(&json!({
            "token_endpoint": "https://idp/token",
            "issuer_url": "https://issuer",
            "client_id_ref": "cred://cid",
            "client_secret_ref": "cred://cs"
        }));
        assert!(both.is_err());
    }
}
