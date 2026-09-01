// Created: 2026-08-31 by Constructor Tech
//! `OAuth2ClientCredAuthPlugin` (ADR-0008).
//!
//! Two registrations of one plugin, differing only in the client-auth method:
//!
//! | GTS id | Client auth |
//! |---|---|
//! | `…auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` | `Form` |
//! | `…auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` | `Basic` |
//!
//! The token is fetched with [`toolkit_auth::oauth2::fetch_token`] — a single
//! HTTP exchange, no background watcher — and cached per
//! `(tenant, subject, auth method, config)` in a
//! [`pingora_memory_cache::MemoryCache`]. `TinyUfo` hashes its keys to `u64`
//! and does **not** compare them for equality, so every entry stores its own
//! key and a hit is only used after the key matches: a hash collision degrades
//! to a miss and can never hand one tenant the token of another.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::HeaderValue;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{
    ClientAuthMethod, FetchedToken, OAuthClientConfig, SecretString, fetch_token,
};
use toolkit_http::HttpClientConfig;
use url::Url;

use crate::error::{OagwError, OagwErrorKind};
use crate::infra::plugin::secrets::{CredStore, resolve_secret};
use crate::infra::plugin::traits::{AuthPlugin, PluginConfig, RequestContext};

/// GTS id of the `Form` variant.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

/// GTS id of the `Basic` variant.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Safety margin subtracted from the `expires_in` the `IdP` reports (ADR-0008).
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// Configuration members of the plugin.
const TOKEN_ENDPOINT: &str = "token_endpoint";
const ISSUER_URL: &str = "issuer_url";
const CLIENT_ID_REF: &str = "client_id_ref";
const CLIENT_SECRET_REF: &str = "client_secret_ref";
const SCOPES: &str = "scopes";

/// A cached access token plus the key it was filed under.
///
/// `TinyUfo` does not compare keys on a hit, so the entry carries its own key
/// and the lookup verifies it — the multi-tenant defence of ADR-0008 ("Hash-
/// Collision Safety via `CachedToken` Wrapper").
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// One in-flight token exchange, keyed by cache key (ADR-0008 "Stampede
/// Protection").
type ExchangeGate = std::sync::Arc<tokio::sync::Mutex<()>>;

/// Gates kept for exchanges that are no longer in flight.
///
/// Only reached when concurrent requests for the same key keep arriving faster
/// than they finish; a gate nobody waits on is dropped, so the map stays as
/// small as the set of live exchanges.
const MAX_GATES: usize = 4096;

/// `OAuth2` client-credentials credential injection with an internal token cache.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: CredStore,
    auth_method: ClientAuthMethod,
    http_config: Option<HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
    /// One gate per cold key, so N concurrent misses exchange once.
    gates: std::sync::Mutex<std::collections::HashMap<String, ExchangeGate>>,
}

impl OAuth2ClientCredAuthPlugin {
    /// Plugin over `credstore`, caching at most `cache_capacity` tokens for at
    /// most `cache_ttl`.
    #[must_use]
    pub fn new(
        credstore: CredStore,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            http_config: None,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
            gates: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// HTTP client configuration the token exchange uses.
    ///
    /// Defaults to the configuration the data plane itself uses, so an `IdP`
    /// behind the same egress policy is reached the same way the upstream is.
    #[must_use]
    pub fn with_http_config(mut self, http_config: Option<HttpClientConfig>) -> Self {
        self.http_config = http_config;
        self
    }

    /// Registry id of this variant.
    #[must_use]
    pub fn plugin_id(auth_method: ClientAuthMethod) -> &'static str {
        match auth_method {
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        }
    }

    /// GTS id of this variant, for the registry key and the diagnostics.
    fn key_tag(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => "form",
            ClientAuthMethod::Basic => "basic",
        }
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        "oauth2_client_cred"
    }

    fn plugin_type(&self) -> &'static str {
        Self::plugin_id(self.auth_method)
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let binding = Binding::of(&ctx.config)?;
        let key = self.cache_key(ctx, &binding);
        if let Some(token) = self.cached(&key) {
            return inject(ctx, token.expose());
        }
        // Single flight (ADR-0008 "Stampede Protection"): the first requester of
        // a cold key holds the gate while it exchanges, the others wait for it
        // and then read the token it filed. The phase runs under the head
        // budget, so a queued request shares the same deadline as the dial.
        let gate = self.gate(&key);
        let _in_flight = gate.lock().await;
        if let Some(token) = self.cached(&key) {
            return inject(ctx, token.expose());
        }
        let fetched = self.fetch(ctx, &binding).await?;
        self.store(&key, &fetched);
        self.release(&key, &gate);
        inject(ctx, fetched.bearer.expose())
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// `{tenant_id}:{subject_id}:{auth_method}:{config_hash}` (ADR-0008).
    ///
    /// The tenant and the subject isolate the credentials of one caller from
    /// another; the auth method keeps the two variants apart when a deployment
    /// binds both with the same configuration; the configuration hash gives
    /// every distinct binding its own entry.
    fn cache_key(&self, ctx: &RequestContext, binding: &Binding) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.tenant_id(),
            ctx.subject_id(),
            self.key_tag(),
            binding.stable_hash()
        )
    }

    /// Serve a token from the cache, treating a key mismatch as a miss.
    ///
    /// A `u64` hash collision would surface here; it degrades to a cache miss,
    /// never to another tenant's token.
    fn cached(&self, key: &str) -> Option<SecretString> {
        let (entry, _status) = self.cache.get(key);
        entry
            .filter(|cached| cached.key == key)
            .map(|cached| cached.token)
    }

    /// The gate of `key`, created when no exchange is in flight for it.
    fn gate(&self, key: &str) -> ExchangeGate {
        let Ok(mut gates) = self.gates.lock() else {
            // A poisoned map costs coalescing, never correctness: the exchange
            // still runs, and the token is still filed.
            return std::sync::Arc::new(tokio::sync::Mutex::new(()));
        };
        if gates.len() > MAX_GATES {
            gates.retain(|_key, gate| std::sync::Arc::strong_count(gate) == 1);
        }
        std::sync::Arc::clone(
            gates
                .entry(key.to_owned())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Forget the gate of `key` once nobody waits on it.
    ///
    /// The map and the caller hold the only two references when the exchange is
    /// done alone, so a stronger count means a request is still queued on it.
    fn release(&self, key: &str, gate: &ExchangeGate) {
        if std::sync::Arc::strong_count(gate) != 2 {
            return;
        }
        if let Ok(mut gates) = self.gates.lock()
            && gates
                .get(key)
                .is_some_and(|held| std::sync::Arc::ptr_eq(held, gate))
        {
            gates.remove(key);
        }
    }

    /// File a token under `key`; a failed fetch is never filed (ADR-0008).
    fn store(&self, key: &str, fetched: &FetchedToken) {
        let Some(ttl) = self.ttl_of(fetched) else {
            return;
        };
        self.cache.put(
            key,
            CachedToken {
                key: key.to_owned(),
                token: fetched.bearer.clone(),
            },
            Some(ttl),
        );
    }

    /// `min(config_ttl, expires_in − 30s)`; a token that would expire within
    /// the margin is not cached at all.
    fn ttl_of(&self, fetched: &FetchedToken) -> Option<Duration> {
        let ttl = fetched.expires_in.checked_sub(EXPIRY_MARGIN)?;
        Some(self.cache_ttl.min(ttl))
    }

    /// Resolve the credentials and exchange them for an access token.
    async fn fetch(
        &self,
        ctx: &RequestContext,
        binding: &Binding,
    ) -> Result<FetchedToken, OagwError> {
        let client_id =
            resolve_secret(&self.credstore, &ctx.security, &binding.client_id_ref).await?;
        let client_secret =
            resolve_secret(&self.credstore, &ctx.security, &binding.client_secret_ref).await?;
        let config = OAuthClientConfig {
            token_endpoint: binding.token_endpoint.clone(),
            issuer_url: binding.issuer_url.clone(),
            client_id: client_id.expose().to_owned(),
            client_secret: SecretString::new(client_secret.expose().to_owned()),
            scopes: binding.scopes.clone(),
            auth_method: self.auth_method,
            http_config: self.http_config.clone(),
            ..OAuthClientConfig::default()
        };
        let fetched = fetch_token(config)
            .await
            .map_err(|error| token_source_failed(&error))?;
        tracing::debug!(
            "oauth2 client-credentials exchange completed ({}s lifetime)",
            fetched.expires_in.as_secs()
        );
        Ok(FetchedToken {
            bearer: fetched.bearer,
            expires_in: fetched.expires_in,
        })
    }
}

/// The plugin binding of one request.
#[derive(Debug)]
struct Binding {
    token_endpoint: Option<Url>,
    issuer_url: Option<Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl Binding {
    /// Parse the binding, rejecting an incomplete one.
    ///
    /// ADR-0008 requires `client_id_ref`, `client_secret_ref` and exactly one
    /// of `token_endpoint` / `issuer_url`. `OAuthClientConfig::validate`
    /// enforces the same rule again inside `fetch_token`; the check is repeated
    /// here so a directly-seeded record fails with the validation contract of
    /// the gateway instead of an internal error.
    fn of(config: &PluginConfig) -> Result<Binding, OagwError> {
        let client_id_ref = required_reference(config, CLIENT_ID_REF)?;
        let client_secret_ref = required_reference(config, CLIENT_SECRET_REF)?;
        let token_endpoint = endpoint_url(config, TOKEN_ENDPOINT)?;
        let issuer_url = endpoint_url(config, ISSUER_URL)?;
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(OagwError::validation(format!(
                "auth binding requires exactly one of '{TOKEN_ENDPOINT}' or '{ISSUER_URL}'"
            )));
        }
        Ok(Binding {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes: scopes(config),
        })
    }

    /// Deterministic digest of the resolved configuration (ADR-0008
    /// "Cache Key Design").
    ///
    /// The members are concatenated in a fixed order into a canonical string —
    /// no separators inside a member, `|` between them — and digested with
    /// FNV-1a/64: a non-cryptographic, release-stable digest whose only job is
    /// to keep distinct configurations in distinct cache entries. The token
    /// cache is additionally keyed by tenant, subject and auth method, so a
    /// digest collision could only mix two configurations of the *same*
    /// caller's binding.
    fn stable_hash(&self) -> String {
        let canonical = format!(
            "{}|{}|{}|{}|{}",
            self.token_endpoint
                .as_ref()
                .map_or_else(String::new, Url::to_string),
            self.issuer_url
                .as_ref()
                .map_or_else(String::new, Url::to_string),
            self.client_id_ref,
            self.client_secret_ref,
            self.scopes.join(" ")
        );
        let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in canonical.as_bytes() {
            digest ^= u64::from(*byte);
            digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{digest:016x}")
    }
}

/// 400 for a `cred://` member the binding omits.
fn required_reference(config: &PluginConfig, member: &str) -> Result<String, OagwError> {
    config
        .string(member)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            OagwError::validation(format!(
                "auth binding requires the '{member}' cred:// reference"
            ))
        })
}

/// Parse an endpoint member, rejecting a value that is not a URL.
fn endpoint_url(config: &PluginConfig, member: &str) -> Result<Option<Url>, OagwError> {
    let Some(raw) = config
        .string(member)
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    Url::parse(raw)
        .map(Some)
        .map_err(|_| OagwError::validation(format!("auth binding member '{member}' is not a URL")))
}

/// Space-separated scope list of the binding.
fn scopes(config: &PluginConfig) -> Vec<String> {
    config
        .string(SCOPES)
        .map(|raw| raw.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// 401 `auth.failed.v1` for a failed token exchange (ADR-0008).
///
/// The failure is **not** cached: the next request retries the `IdP`.
fn token_source_failed(error: &impl std::fmt::Display) -> OagwError {
    tracing::warn!(error = %error, "oauth2 token exchange failed");
    OagwError::new(
        OagwErrorKind::AuthenticationFailed,
        format!("oauth2 client-credentials exchange failed: {error}"),
    )
}

/// Write `Authorization: Bearer <token>` into the outbound request.
fn inject(ctx: &mut RequestContext, token: &str) -> Result<(), OagwError> {
    let value = format!("Bearer {token}");
    let header = HeaderValue::from_str(&value).map_err(|_| {
        OagwError::new(
            OagwErrorKind::Internal,
            "the resolved access token cannot be sent as a header value",
        )
    })?;
    ctx.headers.insert(http::header::AUTHORIZATION, header);
    Ok(())
}

/// The two registered variants.
#[must_use]
pub fn with_builtins(
    credstore: CredStore,
    http_config: Option<HttpClientConfig>,
    cache_ttl: Duration,
    cache_capacity: usize,
) -> Vec<(String, Arc<OAuth2ClientCredAuthPlugin>)> {
    let form = OAuth2ClientCredAuthPlugin::new(
        Arc::clone(&credstore),
        ClientAuthMethod::Form,
        cache_ttl,
        cache_capacity,
    )
    .with_http_config(http_config.clone());
    let basic = OAuth2ClientCredAuthPlugin::new(
        credstore,
        ClientAuthMethod::Basic,
        cache_ttl,
        cache_capacity,
    )
    .with_http_config(http_config);
    vec![
        (OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(), Arc::new(form)),
        (
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(basic),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::secrets::stub::{Behaviour, StubCredStore};
    use serde_json::json;

    fn store(behaviour: Behaviour) -> CredStore {
        std::sync::Arc::new(StubCredStore(behaviour))
    }

    fn config(raw: &serde_json::Value) -> PluginConfig {
        PluginConfig::new(
            raw.as_object()
                .cloned()
                .unwrap_or_else(serde_json::Map::new),
        )
    }

    #[test]
    fn the_variants_use_the_documented_gts_ids() {
        assert_eq!(
            OAuth2ClientCredAuthPlugin::plugin_id(ClientAuthMethod::Form),
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID
        );
        assert_eq!(
            OAuth2ClientCredAuthPlugin::plugin_id(ClientAuthMethod::Basic),
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID
        );
        assert_eq!(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            crate::domain::plugin::PluginKind::Auth.built_in_id("oauth2_client_cred_basic")
        );
    }

    #[test]
    fn a_binding_needs_both_references_and_one_endpoint() {
        let missing_id = Binding::of(&config(&json!({
            "client_secret_ref": "cred://secret",
            "token_endpoint": "https://idp/token"
        })));
        assert!(missing_id.is_err());

        let missing_endpoint = Binding::of(&config(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret"
        })));
        assert!(missing_endpoint.is_err());

        let both = Binding::of(&config(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "token_endpoint": "https://idp/token",
            "issuer_url": "https://idp/"
        })));
        assert!(both.is_err());
    }

    #[test]
    fn a_valid_binding_is_accepted() {
        let binding = Binding::of(&config(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "issuer_url": "https://idp/",
            "scopes": "openid profile"
        })))
        .unwrap();
        assert_eq!(binding.client_id_ref, "cred://id");
        assert_eq!(binding.scopes, Vec::from(["openid", "profile"]));
        assert!(binding.token_endpoint.is_none());
    }

    #[test]
    fn an_endpoint_member_must_be_a_url() {
        let error = Binding::of(&config(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "token_endpoint": "not a url"
        })))
        .unwrap_err();
        assert_eq!(*error.kind(), OagwErrorKind::Validation);
    }

    #[test]
    fn a_blank_reference_is_rejected() {
        let error = Binding::of(&config(&json!({
            "client_id_ref": " ",
            "client_secret_ref": "cred://secret",
            "token_endpoint": "https://idp/token"
        })))
        .unwrap_err();
        assert_eq!(*error.kind(), OagwErrorKind::Validation);
    }

    #[test]
    fn the_configuration_hash_separates_distinct_bindings() {
        let one = Binding::of(&config(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "token_endpoint": "https://idp/token",
            "scopes": "openid"
        })))
        .unwrap();
        let mut other = Binding::of(&config(&json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "token_endpoint": "https://idp/token",
            "scopes": "openid profile"
        })))
        .unwrap();
        other.client_id_ref = "cred://other-id".to_owned();
        assert_ne!(one.stable_hash(), other.stable_hash());
        assert_eq!(one.stable_hash(), one.stable_hash());
    }

    #[test]
    fn the_ttl_is_the_config_ceiling_when_the_token_outlives_it() {
        let plugin = plugin_cache(Duration::from_mins(5));
        let token = FetchedToken {
            bearer: SecretString::new("t"),
            expires_in: Duration::from_hours(1),
        };
        assert_eq!(plugin.ttl_of(&token), Some(Duration::from_mins(5)));
    }

    #[test]
    fn a_short_lived_token_is_cached_only_for_its_lifetime() {
        let plugin = plugin_cache(Duration::from_mins(5));
        let token = FetchedToken {
            bearer: SecretString::new("t"),
            expires_in: Duration::from_mins(2),
        };
        assert_eq!(plugin.ttl_of(&token), Some(Duration::from_secs(90)));
    }

    #[test]
    fn a_token_within_the_expiry_margin_is_not_cached() {
        let plugin = plugin_cache(Duration::from_mins(5));
        let token = FetchedToken {
            bearer: SecretString::new("t"),
            expires_in: Duration::from_secs(10),
        };
        assert_eq!(plugin.ttl_of(&token), None);
    }

    #[test]
    fn a_cache_hit_must_carry_its_own_key() {
        let plugin = plugin_cache(Duration::from_mins(5));
        let token = FetchedToken {
            bearer: SecretString::new("first"),
            expires_in: Duration::from_hours(1),
        };
        plugin.store("tenant:subject:form:aaa", &token);
        let (entry, _status) = plugin.cache.get("tenant:subject:form:aaa");
        let entry = entry.expect("the filed token must be retrievable");
        assert_eq!(entry.key, "tenant:subject:form:aaa");
        assert_eq!(entry.token.expose(), "first");
        // A key the entry was not filed under is treated as a miss, which is
        // what a hash collision degrades to.
        assert!(plugin.cached("tenant:subject:form:bbb").is_none());
    }

    #[test]
    fn a_token_is_not_filed_without_a_usable_ttl() {
        let plugin = plugin_cache(Duration::from_mins(5));
        let token = FetchedToken {
            bearer: SecretString::new("never-cached"),
            expires_in: Duration::from_secs(10),
        };
        plugin.store("key", &token);
        assert!(plugin.cached("key").is_none());
    }

    fn plugin_cache(ttl: Duration) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(store(Behaviour::Empty), ClientAuthMethod::Form, ttl, 10)
    }
}
