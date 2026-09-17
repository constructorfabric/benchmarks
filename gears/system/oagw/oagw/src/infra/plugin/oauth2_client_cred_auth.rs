//! The OAuth2 Client Credentials auth plugin (ADR-0008).
//!
//! An upstream that authenticates its callers with an OAuth2 client-credentials
//! grant (RFC 6749 §4.4) declares one of the two identifiers below and names the
//! identity provider and the credentials in its `auth.config`:
//!
//! ```json
//! { "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
//!   "config": { "token_endpoint": "https://idp.example.com/token",
//!               "client_id_ref": "cred://vendor-client-id",
//!               "client_secret_ref": "cred://vendor-client-secret",
//!               "scopes": "read write" } }
//! ```
//!
//! `token_endpoint` (fetch directly) and `issuer_url` (resolve the endpoint
//! through OIDC discovery) are mutually exclusive and exactly one of them is
//! required. `client_id_ref` and `client_secret_ref` are both required:
//! `client_id_ref` may be a literal (a client identifier is not a secret) or a
//! `cred://` reference, while `client_secret_ref` may only be a reference —
//! credential material never lives in configuration (ADR-0008's configuration
//! table, DESIGN §2.1 "no credentials in configuration"). A reference is
//! resolved through the host's credential store with the *caller's* security
//! context (DESIGN §2.1 "Credential Isolation").
//!
//! # Why `fetch_token` and an internal cache
//!
//! The plugin is multi-tenant: every cache miss is a different (tenant,
//! subject, config) tuple, so a long-lived `toolkit_auth::oauth2::Token` would
//! spawn one background watcher per tuple and hold each tuple's client secret in
//! its closure. `fetch_token` performs a single exchange and spawns nothing; the
//! cache is this plugin's own `pingora_memory_cache::MemoryCache`, sized and
//! timed by the gear-level configuration (ADR-0008 "Gear-Level Configuration").
//!
//! # Cache key, and why the tenant is in it
//!
//! The key is
//! `{subject_tenant_id}:{subject_id}:{auth_method_tag}:{hash_config}`: a
//! different tenant, a different subject (a CredStore `private` sharing mode is
//! per subject), a different client-auth method or a different configuration
//! (different scopes, a different IdP) can never share an entry.
//! `pingora-memory-cache` hashes keys to `u64` and resolves a hit by hash alone,
//! so the entry also carries the key it was stored under and the hit is only
//! used when the two match ([`CachedToken`]) — a hash collision is a cache miss,
//! never another tenant's token.
//!
//! # What fails closed
//!
//! A binding without a usable configuration, a malformed endpoint URL, a
//! credential that cannot be resolved or resolves to nothing, an IdP that
//! refuses the exchange and a token that cannot become a header value all fail
//! the request. The token is injected or the request does not go out: there is
//! no silent unauthenticated forward, and a failed fetch — including one whose
//! token turned out to be unusable — is never cached (the next request retries
//! the IdP).
//!
//! # Where the secret material may appear
//!
//! Only in the outbound `Authorization` header, and in the token request the IdP
//! sees. It is never in a `Debug` output, never in an error detail and never
//! logged: the audit record of
//! [`crate::domain::services::data_plane`] is field-structured and carries no
//! header, and this module emits no log record at all.
//!
//! # Documented deviation
//!
//! ADR-0008 sketches names and a wiring this crate does not have; the slice
//! adapts to the code instead of renaming it:
//!
//! * The registries are
//!   [`PluginRegistries`](super::registry::PluginRegistries) in
//!   `infra/plugin/registry.rs` (not `AuthPluginRegistry` in
//!   `domain/plugin/mod.rs`), and their built-in constructor is
//!   `with_builtins_and(credstore)`. The token-cache settings reach the plugins
//!   through the added `with_builtins_and_config(credstore, TokenCacheConfig)`;
//!   `with_builtins_and` stays as a default-cached wrapper so the existing
//!   callers and tests are untouched, and the no-credstore `with_builtins()`
//!   keeps its signature.
//! * The ADR threads `TokenCacheConfig` through `DataPlaneServiceImpl::new()`.
//!   This crate's data-plane service (`DataPlaneService`) is deliberately
//!   cache-agnostic and its hooks are built in `crate::gear`, so the operator's
//!   `token_cache_ttl_secs` / `token_cache_capacity` are applied there.
//! * The ADR's constructor takes `credstore: Arc<dyn CredStoreClientV1>`; here
//!   it is an `Option`, exactly as
//!   [`ApiKeyAuthPlugin::with_client`](super::api_key_auth::ApiKeyAuthPlugin)
//!   takes one. A host that publishes no credential store cannot be detected at
//!   construction (the gear is built before the client hub is read), so the
//!   failure moves to the request that needs the secret — which is *stricter*,
//!   not looser: that request fails closed with
//!   `cf.oagw.secret.not_found.v1`.
//! * The ADR's plugin struct carries `http_config: Option<HttpClientConfig>` for
//!   the token request. This slice has no operator surface for it, so the
//!   constructor does not take one: the settings are built in code
//!   ([`token_request_http_config`]), from the toolkit's token-endpoint preset
//!   with its retry policy dropped.
//! * The ADR sketches `authenticate(&self, ctx: &mut AuthContext)` returning
//!   `PluginError::Internal`. The trait of this crate
//!   ([`AuthPlugin`](super::traits)) is header-only and returns
//!   [`OagwError`](crate::error::OagwError), so an IdP failure is
//!   `cf.oagw.downstream.error.v1` (the identity provider *is* a downstream
//!   service) and the configuration failures are `cf.oagw.validation.error.v1`.
//! * There is **no retry on an upstream 401** (ADR-0008 "Retry on Upstream 401
//!   (Deferred)"): a cached token the upstream rejects keeps being injected
//!   until its TTL expires.
//! * `pingora-memory-cache` is already a direct dependency of this crate
//!   (pinned at `0.8`), so it is not re-declared through the workspace table the
//!   way `sha2` and `hex` are.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::api::CredStoreClientV1;
use http::HeaderMap;
use http::header::AUTHORIZATION;
use http::header::HeaderValue;
use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use toolkit_auth::oauth2::ClientAuthMethod;
use toolkit_auth::oauth2::OAuthClientConfig;
use toolkit_auth::oauth2::SecretString;
use toolkit_auth::oauth2::TokenError;
use toolkit_auth::oauth2::fetch_token;
use toolkit_security::SecurityContext;
use url::Url;

use super::registry::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF;
use super::registry::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF;
use super::secret::SecretResolver;
use super::traits::AuthPlugin;
use super::traits::PluginContext;
use crate::domain::services::data_plane::ProxyContext;
// The four keys below are the ones write-time validation enforces on a binding
// (`UpstreamSpec::validate`), so they are declared once, in `domain::types`, and
// imported here: a rename in either file cannot leave one side enforcing a key
// the other never reads.
use crate::domain::types::CLIENT_ID_REF_KEY;
use crate::domain::types::CLIENT_SECRET_REF_KEY;
use crate::domain::types::CREDENTIAL_REFERENCE_SCHEME;
use crate::domain::types::ISSUER_URL_KEY;
use crate::domain::types::TOKEN_ENDPOINT_KEY;
use crate::error::OagwError;

/// The configuration key naming the space-separated scopes (ADR-0008).
///
/// Deliberately *not* in `domain::types`: write-time validation does not
/// constrain the scopes a binding asks for, so nothing but this plugin reads it.
const SCOPES: &str = "scopes";

/// How far before the IdP's `expires_in` a cached token is dropped, so a token
/// is never served inside the window where the IdP may already have expired it
/// (ADR-0008 "Gear-Level Configuration").
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// The HTTP settings of a token request, and of the OIDC discovery request that
/// may precede it.
///
/// `toolkit-http`'s own token-endpoint preset retries a failed exchange three
/// times, which is right for a service that fetches one token for its whole
/// lifetime and wrong here: a cache miss is on the caller's request path, and a
/// gateway request is retried *as a request*, never as its four underlying
/// attempts (DESIGN §2.1 `cpt-cf-oagw-principle-no-retry`, ADR-0008's
/// per-request retry model). The preset is otherwise a good fit — conservative
/// timeouts, a small connection pool — so it is cloned and its retry policy
/// dropped rather than rebuilt from scratch. There is no operator surface for
/// these settings, which is why they are built in code instead of configuration.
#[must_use]
fn token_request_http_config() -> toolkit_http::HttpClientConfig {
    let mut config = toolkit_http::HttpClientConfig::token_endpoint();
    config.retry = None;
    config
}

/// The token-cache settings the gear-level configuration carries (ADR-0008
/// "Gear-Level Configuration").
///
/// [`PluginRegistries::with_builtins_and_config`](super::registry::PluginRegistries)
/// hands one to every OAuth2 plugin it builds; the data plane never sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached access token's TTL. The effective TTL of an entry is
    /// `min(ttl, expires_in - 30s)`.
    pub ttl: Duration,
    /// Maximum number of entries the cache keeps.
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

/// A cached access token, and the cache key it was stored under.
///
/// `pingora-memory-cache` hashes its keys to `u64` and resolves a hit by hash
/// alone, so the key is stored *with* the token and verified on every hit: a
/// collision returns a miss, never another tenant's token (ADR-0008
/// "Hash-Collision Safety via CachedToken Wrapper").
///
/// `Debug` is deliberately not derived: a cache entry is exactly the value that
/// must never reach a log line, and an accidentally derivable `Debug` would put
/// it one `{entry:?}` away from one.
struct CachedToken {
    /// The key the entry was stored under.
    key: String,
    /// The access token, zeroized when the entry is evicted.
    token: SecretString,
}

impl Clone for CachedToken {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            token: self.token.clone(),
        }
    }
}

/// Injects a fetched (or cached) OAuth2 access token into the outbound request.
pub struct OAuth2ClientCredAuthPlugin {
    /// Resolves the `cred://` references the binding names.
    resolver: SecretResolver,
    /// How the client credentials travel to the token endpoint.
    auth_method: ClientAuthMethod,
    /// The token cache, one entry per (tenant, subject, method, config).
    cache: MemoryCache<String, CachedToken>,
    /// Ceiling for a cached entry's TTL.
    cache_ttl: Duration,
}

impl fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuth2ClientCredAuthPlugin")
            .field("plugin_ref", &self.plugin_ref())
            .field("auth_method", &self.auth_method)
            .field("cache_ttl", &self.cache_ttl)
            .field("resolver", &self.resolver)
            // The cache is not printable on purpose: its entries are bearer
            // tokens, and there is no `Debug` for `CachedToken` to leak.
            .field("cache", &"MemoryCache<String, CachedToken>")
            .finish()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// A plugin that resolves `cred://` references through `credstore`, when the
    /// host published one, and caches tokens for `cache_ttl` at most
    /// `cache_capacity` entries.
    ///
    /// `None` leaves every reference unresolved: the request that names one
    /// fails closed, which is the contract the API-key plugin ships
    /// ([`ApiKeyAuthPlugin::with_client`](super::api_key_auth::ApiKeyAuthPlugin::with_client)).
    #[must_use]
    pub fn new(
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            resolver: SecretResolver::new(credstore),
            auth_method,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// The cached token for `key`, when the cache holds one *for this key*.
    ///
    /// A stored entry whose key does not match the lookup key is a hash
    /// collision and is reported as a miss.
    fn cached_token(&self, key: &str) -> Option<SecretString> {
        match self.cache.get(key) {
            (Some(entry), _) if entry.key == key => Some(entry.token.clone()),
            _ => None,
        }
    }

    /// The token request the binding describes.
    fn client_config(
        &self,
        binding: &Binding,
        client_id: String,
        client_secret: String,
    ) -> OAuthClientConfig {
        let (token_endpoint, issuer_url) = binding.endpoint.parts();

        OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: binding.scopes.clone(),
            auth_method: self.auth_method,
            http_config: Some(token_request_http_config()),
            ..OAuthClientConfig::default()
        }
    }
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn plugin_ref(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF,
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF,
        }
    }

    async fn authenticate(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        let Some(config) = context.config else {
            return Err(missing_config());
        };
        let binding = Binding::parse(config)?;

        // The key covers the whole configuration value, so two bindings that
        // differ in any key fetch and cache independently.
        let key = build_cache_key(&context.request.security, self.auth_method, config);

        if let Some(token) = self.cached_token(&key) {
            inject_bearer(headers, &token)?;
            return Ok(());
        }

        let security = &context.request.security;
        let client_id = self
            .resolver
            .resolve(security, &binding.client_id_ref)
            .await?;
        let client_secret = self
            .resolver
            .resolve(security, &binding.client_secret_ref)
            .await?;

        let fetched = fetch_token(self.client_config(&binding, client_id, client_secret))
            .await
            .map_err(|error| token_fetch_error(context.request, error))?;

        // The token is proven injectable *before* it is cached: a token the IdP
        // issued that cannot become a header value fails this request, and an
        // entry that was already cached would replay that failure for the whole
        // TTL instead of letting the next request re-fetch (ADR-0008 "Failed
        // token fetches are not cached").
        inject_bearer(headers, &fetched.bearer)?;

        if let Some(ttl) = cache_ttl(self.cache_ttl, fetched.expires_in) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }

        Ok(())
    }
}

/// The binding's own view of the endpoint a token is fetched from.
#[derive(Debug, Clone)]
enum TokenEndpoint {
    /// `token_endpoint`: the token request goes straight to this URL.
    Direct(Url),
    /// `issuer_url`: the endpoint is resolved through OIDC discovery first.
    Issuer(Url),
}

impl TokenEndpoint {
    /// The two endpoint fields of an [`OAuthClientConfig`]; exactly one is set.
    fn parts(&self) -> (Option<Url>, Option<Url>) {
        match self {
            Self::Direct(url) => (Some(url.clone()), None),
            Self::Issuer(url) => (None, Some(url.clone())),
        }
    }
}

/// The parsed configuration of one plugin binding.
#[derive(Debug, Clone)]
struct Binding {
    /// Where the token request goes.
    endpoint: TokenEndpoint,
    /// The `cred://` reference or literal naming the OAuth2 client.
    client_id_ref: String,
    /// The `cred://` reference naming the client secret.
    client_secret_ref: String,
    /// The scopes to request, one word per entry.
    scopes: Vec<String>,
}

impl Binding {
    /// Parse the binding's configuration.
    ///
    /// # Errors
    /// A key of the wrong type, a malformed endpoint URL, both or neither of
    /// `token_endpoint`/`issuer_url`, a missing or empty required key, and a
    /// `client_secret_ref` that is not a `cred://` reference are
    /// [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind)s. The
    /// detail names the offending key and never the value behind it: the value
    /// of `client_secret_ref` is credential material, and a URL is operator
    /// configuration that may carry credential material in its query.
    fn parse(config: &Value) -> Result<Self, OagwError> {
        let endpoint = parse_endpoint(config)?;
        let client_id_ref = required_string(config, CLIENT_ID_REF_KEY)?;
        let client_secret_ref = required_string(config, CLIENT_SECRET_REF_KEY)?;

        // The same rule write-time validation applies
        // (`validate_oauth2_config`), mirrored here because a binding can also
        // reach the request path through a stored `plugins.items[]` entry. A
        // client identifier stays free-form — it is not a secret — but the
        // secret is credential material and never lives in configuration
        // (ADR-0008's configuration table, DESIGN §2.1).
        if !client_secret_ref.starts_with(CREDENTIAL_REFERENCE_SCHEME) {
            return Err(OagwError::validation(format!(
                "the oauth2 client-credentials auth plugin is bound with a \
                 '{CLIENT_SECRET_REF_KEY}' that is not a '{CREDENTIAL_REFERENCE_SCHEME}' reference"
            ))
            .with_extension("field", binding_field(CLIENT_SECRET_REF_KEY)));
        }

        let scopes = optional_string(config, SCOPES)?.map_or_else(Vec::new, |raw| {
            raw.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });

        Ok(Self {
            endpoint,
            client_id_ref,
            client_secret_ref,
            scopes,
        })
    }
}

/// Read the endpoint keys of `config` and enforce their mutual exclusion.
///
/// # Errors
/// Both or neither of `token_endpoint`/`issuer_url`, a non-string value, or a
/// URL that does not parse.
fn parse_endpoint(config: &Value) -> Result<TokenEndpoint, OagwError> {
    match (
        optional_string(config, TOKEN_ENDPOINT_KEY)?,
        optional_string(config, ISSUER_URL_KEY)?,
    ) {
        (Some(_), Some(_)) => Err(OagwError::validation(format!(
            "the oauth2 client-credentials auth plugin is bound with both '{TOKEN_ENDPOINT_KEY}' and \
             '{ISSUER_URL_KEY}'; the two are mutually exclusive, exactly one of them may name the \
             token endpoint"
        ))
        .with_extension("field", Value::from("auth.config"))),
        (None, None) => Err(OagwError::validation(format!(
            "the oauth2 client-credentials auth plugin is bound with neither '{TOKEN_ENDPOINT_KEY}' \
             nor '{ISSUER_URL_KEY}', so no token can be fetched"
        ))
        .with_extension("field", Value::from("auth.config"))),
        (Some(raw), None) => parse_url(TOKEN_ENDPOINT_KEY, raw).map(TokenEndpoint::Direct),
        (None, Some(raw)) => parse_url(ISSUER_URL_KEY, raw).map(TokenEndpoint::Issuer),
    }
}

/// The problem+json `field` extension for a binding's configuration key.
fn binding_field(key: &str) -> Value {
    Value::from(format!("auth.config.{key}"))
}

/// The error for a binding that carries no configuration object at all.
fn missing_config() -> OagwError {
    OagwError::validation(
        "the oauth2 client-credentials auth plugin is bound without a configuration object, so \
         no token can be fetched",
    )
    .with_extension("field", Value::from("auth.config"))
}

/// Parse one endpoint key into a `Url`.
///
/// # Errors
/// A value that is not a URL. The parse error names the *reason*, never the
/// value: a token endpoint is operator configuration and may carry credential
/// material in its query string.
fn parse_url(key: &str, raw: &str) -> Result<Url, OagwError> {
    Url::parse(raw).map_err(|error| {
        OagwError::validation(format!(
            "the oauth2 client-credentials auth plugin is bound with an unusable '{key}' in its \
             configuration: {error}"
        ))
        .with_extension("field", binding_field(key))
    })
}

/// One string entry of the binding's configuration.
///
/// # Errors
/// A key that is present but not a string. An absent key (or an explicit
/// `null`) is not an error: the caller decides what a missing key means.
fn optional_string<'a>(config: &'a Value, key: &str) -> Result<Option<&'a str>, OagwError> {
    match config.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => Ok(Some(raw)),
        Some(_) => Err(OagwError::validation(format!(
            "the oauth2 client-credentials auth plugin is bound with a non-string '{key}' in its \
             configuration"
        ))
        .with_extension("field", binding_field(key))),
    }
}

/// One required, non-empty string entry of the binding's configuration.
///
/// # Errors
/// A key that is absent, not a string, or empty. The detail names the key and
/// never the value: for `client_secret_ref` the value is a secret reference, and
/// echoing it would put credential material into the problem document.
fn required_string(config: &Value, key: &str) -> Result<String, OagwError> {
    match optional_string(config, key)? {
        Some(raw) if !raw.trim().is_empty() => Ok(raw.to_owned()),
        _ => Err(OagwError::validation(format!(
            "the oauth2 client-credentials auth plugin is bound without a usable '{key}' in its \
             configuration"
        ))
        .with_extension("field", binding_field(key))),
    }
}

/// The cache key of one (tenant, subject, auth method, config) tuple (ADR-0008
/// "Cache Key Design").
///
/// `hash_config` is a SHA-256 over a *canonically ordered* serialization of the
/// whole configuration value, so two bindings that differ in any key — or only
/// in the order their JSON happened to be written in — never share an entry.
#[must_use]
fn build_cache_key(
    security: &SecurityContext,
    auth_method: ClientAuthMethod,
    config: &Value,
) -> String {
    format!(
        "{}:{}:{}:{}",
        security.subject_tenant_id(),
        security.subject_id(),
        auth_method_tag(auth_method),
        hash_config(config)
    )
}

/// The cache-key segment that keeps the two plugin variants apart.
#[must_use]
fn auth_method_tag(auth_method: ClientAuthMethod) -> &'static str {
    match auth_method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

/// The hex-encoded SHA-256 of the configuration's canonical serialization.
#[must_use]
fn hash_config(config: &Value) -> String {
    let mut canonical = String::new();
    canonical_form(config, &mut canonical);

    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Serialize `value` with object keys in sorted order, into `out`.
///
/// `serde_json`'s map is ordered, but which order that is depends on its feature
/// flags; the cache key must not depend on either of them, so the order is
/// produced here.
fn canonical_form(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(string) => push_escaped(string, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical_form(item, out);
            }
            out.push(']');
        }
        Value::Object(entries) => {
            let mut ordered: Vec<(&str, &Value)> = entries
                .iter()
                .map(|(key, item)| (key.as_str(), item))
                .collect();
            ordered.sort_unstable_by_key(|(key, _)| *key);

            out.push('{');
            for (index, (key, item)) in ordered.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_escaped(key, out);
                out.push(':');
                canonical_form(item, out);
            }
            out.push('}');
        }
    }
}

/// Append `value` as a JSON string literal.
fn push_escaped(value: &str, out: &mut String) {
    match serde_json::to_string(value) {
        Ok(encoded) => out.push_str(&encoded),
        // Unreachable for a `&str`, but a cache key must never panic.
        Err(_) => out.push_str(value),
    }
}

/// The TTL a fetched token is cached for.
///
/// The IdP's own lifetime wins when it is shorter than the configured ceiling,
/// minus the safety margin; a token the IdP issues for less than the margin is
/// not cached at all, because there is no window in which it is still valid.
#[must_use]
fn cache_ttl(configured: Duration, expires_in: Duration) -> Option<Duration> {
    let usable = expires_in.checked_sub(EXPIRY_SAFETY_MARGIN)?;
    Some(configured.min(usable)).filter(|ttl| !ttl.is_zero())
}

/// The `Authorization` header the upstream reads.
///
/// # Errors
/// A token the identity provider issued that is not a valid header value cannot
/// be injected, and forwarding without it would be an unauthenticated request.
fn inject_bearer(headers: &mut HeaderMap, token: &SecretString) -> Result<(), OagwError> {
    let value = HeaderValue::try_from(format!("Bearer {}", token.expose())).map_err(|error| {
        OagwError::secret_not_found(format!(
            "the access token the identity provider issued is not a usable header value: {error}"
        ))
    })?;

    // Overwrite: a credential the caller sent under the same name is the
    // caller's, not the upstream's, and the gateway's credential is the one the
    // upstream must see.
    headers.insert(AUTHORIZATION, value);

    Ok(())
}

/// The error a failed token exchange becomes.
///
/// The detail names the phase and the *kind* of the IdP failure, and nothing
/// else: the token, the client secret and the response body never reach it, and
/// neither does the token endpoint URL, which is operator configuration that may
/// carry credential material in its query string.
///
/// `TokenError::ConfigError` is the exception: the only configuration left to
/// reject at that point is an empty resolved client credential, which is
/// detected locally, before any identity provider was contacted — a gateway-side
/// credential problem, reported the way the api-key plugin reports a credential
/// it cannot use ([`OagwError::secret_not_found`]).
fn token_fetch_error(request: &ProxyContext, error: TokenError) -> OagwError {
    if matches!(error, TokenError::ConfigError(_)) {
        // The variant's own payload ("client_secret must not be empty") is
        // dropped with the rest: it names a credential, and the detail must not.
        return OagwError::secret_not_found(format!(
            "the client credential resolved for upstream '{}' is empty, so no token can be fetched",
            request.alias
        ))
        .with_extension("field", Value::from("auth.config"))
        .with_extension("phase", Value::from("auth_plugin_token_fetch"));
    }

    let kind = token_error_kind(&error);

    OagwError::downstream_error(format!(
        "the OAuth2 client-credentials token fetch for upstream '{}' failed in the \
         identity-provider phase (kind: {kind}); the failure is not cached",
        request.alias
    ))
    .with_extension("field", Value::from("auth.config"))
    .with_extension("phase", Value::from("auth_plugin_token_fetch"))
    .with_extension("idp_error_kind", Value::from(kind))
}

/// The kind of an IdP failure, as the error detail reports it.
///
/// The variant payloads are dropped on purpose: they may quote a response body,
/// a URL or a token type, and none of that belongs in a problem document.
fn token_error_kind(error: &TokenError) -> &'static str {
    match error {
        TokenError::Http(_) => "http",
        TokenError::InvalidResponse(_) => "invalid_response",
        TokenError::UnsupportedTokenType(_) => "unsupported_token_type",
        TokenError::ConfigError(_) => "config",
        TokenError::Unavailable(_) => "unavailable",
        TokenError::InvalidTokenLifetime(_) => "invalid_token_lifetime",
        // `TokenError` is `#[non_exhaustive]`: a future variant is reported as
        // an identity-provider failure of an unknown kind, which is what it is.
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use httpmock::MockServer;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::domain::services::data_plane::ProxyContext;
    use crate::infra::plugin::test_context;

    /// A `SecurityContext` for an explicit tenant and subject.
    fn security(tenant: Uuid, subject: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(subject)
            .subject_tenant_id(tenant)
            .build()
            .expect("the security context builds")
    }

    /// A `PluginContext` over `request` and the binding `config`.
    fn context<'a>(request: &'a ProxyContext, config: &'a Value) -> PluginContext<'a> {
        PluginContext {
            config: Some(config),
            request,
        }
    }

    /// A request whose caller is `security`.
    fn request_for(security: &SecurityContext) -> ProxyContext {
        let mut request = test_context();
        request.security = security.clone();
        request
    }

    /// A binding configuration pointing at `server`.
    fn config(server: &MockServer) -> Value {
        json!({
            "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
        })
    }

    /// A credstore answering the two references the test bindings name.
    fn credentials() -> Option<Arc<dyn CredStoreClientV1>> {
        Some(Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
                (
                    "cred://vendor-client-id".to_owned(),
                    "the-client-id".to_owned(),
                ),
                (
                    "cred://vendor-client-secret".to_owned(),
                    "the-client-secret".to_owned(),
                ),
            ]),
        ))
    }

    /// A token request mock that answers `token` with `expires_in` seconds.
    fn token_mock<'a>(server: &'a MockServer, token: &str, expires_in: u64) -> httpmock::Mock<'a> {
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#
                ));
        })
    }

    /// A `Basic`-auth plugin with the test credentials.
    fn basic_plugin() -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            credentials(),
            ClientAuthMethod::Basic,
            Duration::from_secs(300),
            16,
        )
    }

    /// A `Form`-auth plugin with the test credentials.
    ///
    /// Its only caller needs a live plaintext IdP, so it is compiled out under
    /// fips with that test (see the note above it).
    #[cfg(not(feature = "fips"))]
    fn form_plugin() -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            credentials(),
            ClientAuthMethod::Form,
            Duration::from_secs(300),
            16,
        )
    }

    #[test]
    fn the_two_variants_are_registered_under_their_own_identifiers() {
        let ttl = Duration::from_secs(300);
        let form = OAuth2ClientCredAuthPlugin::new(None, ClientAuthMethod::Form, ttl, 16);
        let basic = OAuth2ClientCredAuthPlugin::new(None, ClientAuthMethod::Basic, ttl, 16);

        assert_eq!(form.plugin_ref(), OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF);
        assert_eq!(basic.plugin_ref(), OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF);
        assert_ne!(form.plugin_ref(), basic.plugin_ref());
    }

    #[test]
    fn a_binding_with_no_endpoint_at_all_is_rejected() {
        let error = Binding::parse(&json!({})).expect_err("no endpoint set");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::ValidationError);
        assert_eq!(error.status().as_u16(), 400);
        assert!(
            error
                .detail()
                .contains("neither 'token_endpoint' nor 'issuer_url'")
        );
    }

    #[tokio::test]
    async fn a_binding_with_no_config_at_all_fails_closed() {
        let request = test_context();
        let plugin_context = PluginContext {
            config: None,
            request: &request,
        };
        let mut headers = HeaderMap::new();

        let error = OAuth2ClientCredAuthPlugin::new(
            None,
            ClientAuthMethod::Basic,
            Duration::from_secs(300),
            16,
        )
        .authenticate(&plugin_context, &mut headers)
        .await
        .expect_err("no configuration object");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::ValidationError);
        assert!(headers.is_empty(), "nothing is forwarded unauthenticated");
    }

    #[test]
    fn both_endpoints_are_rejected() {
        let config = json!({
            "token_endpoint": "https://idp.example.com/token",
            "issuer_url": "https://idp.example.com",
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
        });

        let error = Binding::parse(&config).expect_err("both endpoints set");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::ValidationError);
        assert!(error.detail().contains("mutually exclusive"));
        assert_eq!(error.extensions()["field"], "auth.config");
    }

    #[test]
    fn a_required_key_that_is_missing_or_empty_is_rejected_by_name() {
        for config in [
            json!({"token_endpoint": "https://idp.example.com/token"}),
            json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "cred://vendor-client-id",
            }),
            json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "   ",
                "client_secret_ref": "cred://vendor-client-secret",
            }),
            json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": 7,
                "client_secret_ref": "cred://vendor-client-secret",
            }),
        ] {
            let error = Binding::parse(&config).expect_err("a required key is unusable");

            assert_eq!(error.kind(), crate::error::OagwErrorKind::ValidationError);
            assert!(
                error.detail().contains("client_id_ref")
                    || error.detail().contains("client_secret_ref"),
                "the detail names the offending key: {}",
                error.detail()
            );
            assert!(
                !error.detail().contains("cred://vendor"),
                "the detail does not quote the configured value: {}",
                error.detail()
            );
        }
    }

    #[test]
    fn a_client_secret_that_is_not_a_reference_is_rejected_by_name() {
        // The write-time gate (`validate_oauth2_config`) and the request-time
        // parse agree: the client identifier stays free-form, the client secret
        // may only name a store entry.
        for secret in ["hunter2", "cred:/not-quite-a-reference", "  "] {
            let config = json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "oagw-gateway",
                "client_secret_ref": secret,
            });

            let error = Binding::parse(&config).expect_err("the secret is not a reference");

            assert_eq!(error.kind(), crate::error::OagwErrorKind::ValidationError);
            assert_eq!(error.status().as_u16(), 400);
            assert_eq!(
                error.extensions()["field"],
                "auth.config.client_secret_ref",
                "the field names the offending key"
            );
            assert!(
                !error.detail().contains("hunter2"),
                "the detail never quotes the configured value: {}",
                error.detail()
            );
        }
    }

    #[test]
    fn a_literal_client_id_is_a_legitimate_configuration() {
        let binding = Binding::parse(&json!({
            "issuer_url": "https://idp.example.com",
            "client_id_ref": "oagw-gateway",
            "client_secret_ref": "cred://vendor-client-secret",
        }))
        .expect("a literal client id is not a secret");

        assert_eq!(binding.client_id_ref, "oagw-gateway");
        assert_eq!(binding.client_secret_ref, "cred://vendor-client-secret");
        assert!(matches!(binding.endpoint, TokenEndpoint::Issuer(_)));
    }

    #[test]
    fn a_malformed_url_is_a_validation_error_that_does_not_quote_the_url() {
        let config = json!({
            "token_endpoint": "not a url at all",
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
        });

        let error = Binding::parse(&config).expect_err("not a url");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::ValidationError);
        assert!(error.detail().contains("'token_endpoint'"));
        assert!(
            !error.detail().contains("not a url at all"),
            "the detail does not quote the configured URL: {}",
            error.detail()
        );
    }

    #[test]
    fn the_basic_variant_hands_the_issuer_form_to_the_token_source() {
        // The client-auth method is the only thing that differs between the two
        // variants, so the discovery form has to survive the configuration
        // plumbing of both. This is exactly what `TokenEndpoint::parts` feeds:
        // a swapped field pair there would turn every `issuer_url` binding into
        // a 502, which the unit level can catch without an identity provider.
        let plugin = basic_plugin();
        let binding = Binding::parse(&json!({
            "issuer_url": "https://idp.example.com",
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
        }))
        .expect("the binding parses");

        let config = plugin.client_config(
            &binding,
            "the-client-id".to_owned(),
            "the-client-secret".to_owned(),
        );

        assert!(
            config.token_endpoint.is_none(),
            "the endpoint is not known yet: the token source resolves it"
        );
        assert_eq!(
            config.issuer_url.as_ref().map(Url::as_str),
            Some("https://idp.example.com/"),
            "the issuer reaches the token source, which runs OIDC discovery"
        );
        assert_eq!(config.auth_method, ClientAuthMethod::Basic);
    }

    #[test]
    fn the_token_request_http_settings_carry_no_retry_policy() {
        let plugin = basic_plugin();
        let binding = Binding::parse(&json!({
            "token_endpoint": "https://idp.example.com/token",
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
        }))
        .expect("the binding parses");

        let http_config = plugin
            .client_config(
                &binding,
                "the-client-id".to_owned(),
                "the-client-secret".to_owned(),
            )
            .http_config
            .expect("the plugin always sets the token request's HTTP settings");
        let preset = toolkit_http::HttpClientConfig::token_endpoint();

        assert!(
            http_config.retry.is_none(),
            "one request is one attempt, never the preset's four retries: {:?}",
            http_config.retry
        );
        assert!(token_request_http_config().retry.is_none());
        assert_eq!(
            http_config.request_timeout, preset.request_timeout,
            "the rest of the token-endpoint preset is kept"
        );
    }

    /// Under `--features fips` the token-endpoint preset is `TlsOnly`, so a
    /// token endpoint must be reached over TLS. This is the fips-side half of
    /// the contract the plaintext-IdP tests above cannot exercise (they are
    /// compiled out under fips for exactly this reason); the preset's own
    /// posture is asserted by `toolkit-http/tests/fips_default_transport.rs`.
    #[cfg(feature = "fips")]
    #[test]
    fn the_token_request_stays_tls_only_under_fips() {
        assert_eq!(
            token_request_http_config().transport,
            toolkit_http::TransportSecurity::TlsOnly,
            "FIPS mode forbids a plaintext token endpoint"
        );
    }

    #[test]
    fn scopes_are_normalized_to_one_word_per_entry() {
        let binding = Binding::parse(&json!({
            "token_endpoint": "https://idp.example.com/token",
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
            "scopes": "  read   write ",
        }))
        .expect("the binding parses");

        assert_eq!(binding.scopes, vec!["read".to_owned(), "write".to_owned()]);
    }

    #[test]
    fn a_binding_without_scopes_requests_none() {
        let binding = Binding::parse(&json!({
            "token_endpoint": "https://idp.example.com/token",
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
        }))
        .expect("the binding parses");

        assert!(binding.scopes.is_empty());
    }

    #[test]
    fn a_cache_key_separates_tenants_subjects_methods_and_configs() {
        let tenant = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let config = json!({"token_endpoint": "https://idp.example.com/token", "scopes": "read"});

        let base = build_cache_key(&security(tenant, subject), ClientAuthMethod::Form, &config);

        assert_ne!(
            base,
            build_cache_key(
                &security(Uuid::new_v4(), subject),
                ClientAuthMethod::Form,
                &config
            ),
            "another tenant never shares an entry"
        );
        assert_ne!(
            base,
            build_cache_key(
                &security(tenant, Uuid::new_v4()),
                ClientAuthMethod::Form,
                &config
            ),
            "another subject never shares an entry"
        );
        assert_ne!(
            base,
            build_cache_key(&security(tenant, subject), ClientAuthMethod::Basic, &config),
            "the other client-auth method never shares an entry"
        );
        assert_ne!(
            base,
            build_cache_key(
                &security(tenant, subject),
                ClientAuthMethod::Form,
                &json!({"token_endpoint": "https://idp.example.com/token", "scopes": "write"}),
            ),
            "another configuration never shares an entry"
        );
    }

    #[test]
    fn a_cache_key_is_stable_across_the_order_the_config_was_written_in() {
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let method = ClientAuthMethod::Form;

        let written_one_way = json!({
            "token_endpoint": "https://idp.example.com/token",
            "scopes": "read",
            "client_id_ref": "cred://vendor-client-id",
        });
        let written_another_way = json!({
            "client_id_ref": "cred://vendor-client-id",
            "scopes": "read",
            "token_endpoint": "https://idp.example.com/token",
        });

        assert_eq!(
            build_cache_key(&security, method, &written_one_way),
            build_cache_key(&security, method, &written_another_way),
            "the same configuration is one entry however its JSON was written"
        );
        assert_ne!(
            build_cache_key(&security, method, &written_one_way),
            build_cache_key(
                &security,
                method,
                &json!({
                    "token_endpoint": "https://idp.example.com/token",
                    "scopes": "read",
                    "client_id_ref": "cred://other-client-id",
                })
            ),
            "a different value is a different entry, not a different order"
        );
    }

    #[test]
    fn the_config_hash_is_the_hex_of_the_canonical_form() {
        assert_eq!(
            hash_config(&json!({"a": 1})).len(),
            64,
            "a SHA-256 is 64 hex digits"
        );
        assert_eq!(
            hash_config(&json!({"b": true, "a": [1, "x"]})),
            hash_config(&json!({"a": [1, "x"], "b": true})),
        );
        assert_ne!(
            hash_config(&json!({"a": 1})),
            hash_config(&json!({"a": "1"})),
            "a number and its spelling as a string are not the same configuration"
        );
    }

    #[tokio::test]
    async fn an_entry_stored_under_another_key_is_a_miss_not_another_tenants_token() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            None,
            ClientAuthMethod::Form,
            Duration::from_secs(300),
            16,
        );
        let lookup_key = "tenant:subject:form:hash";

        plugin.cache.put(
            lookup_key,
            CachedToken {
                key: "another-tenant:another-subject:form:hash".to_owned(),
                token: SecretString::new("someone-elses-token"),
            },
            Some(Duration::from_secs(300)),
        );

        assert!(
            plugin.cached_token(lookup_key).is_none(),
            "a key mismatch is a cache miss, never another tenant's token"
        );
    }

    #[tokio::test]
    async fn an_entry_stored_under_the_lookup_key_is_served() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            None,
            ClientAuthMethod::Form,
            Duration::from_secs(300),
            16,
        );
        let lookup_key = "tenant:subject:form:hash";

        plugin.cache.put(
            lookup_key,
            CachedToken {
                key: lookup_key.to_owned(),
                token: SecretString::new("cached-token"),
            },
            Some(Duration::from_secs(300)),
        );

        assert_eq!(
            plugin
                .cached_token(lookup_key)
                .as_ref()
                .map(SecretString::expose),
            Some("cached-token")
        );
    }

    #[test]
    fn the_cached_ttl_is_the_shorter_of_the_two_minus_the_safety_margin() {
        let configured = Duration::from_secs(300);

        assert_eq!(
            cache_ttl(configured, Duration::from_secs(600)),
            Some(Duration::from_secs(300)),
            "the configured ceiling wins for a long-lived token"
        );
        assert_eq!(
            cache_ttl(configured, Duration::from_secs(120)),
            Some(Duration::from_secs(90)),
            "120s minus the 30s margin, under the 300s ceiling"
        );
        assert_eq!(
            cache_ttl(Duration::from_secs(60), Duration::from_secs(600)),
            Some(Duration::from_secs(60)),
            "a short ceiling wins for a long-lived token"
        );
    }

    #[test]
    fn a_token_the_idp_expires_within_the_margin_is_never_cached() {
        let configured = Duration::from_secs(300);

        assert_eq!(cache_ttl(configured, Duration::from_secs(30)), None);
        assert_eq!(cache_ttl(configured, Duration::from_secs(0)), None);
        assert_eq!(
            cache_ttl(Duration::from_secs(30), Duration::from_secs(600)),
            Some(Duration::from_secs(30)),
            "a short ceiling is honoured, and the token outlives the entry by far"
        );
    }

    // The four tests below drive a real IdP over a plaintext loopback socket.
    // Under `--features fips` the `token_endpoint` preset is `TlsOnly` and
    // `HttpClientBuilder::build` rejects `AllowInsecureHttp` outright, so the
    // fetch cannot be made against a plaintext mock at all — the same reason
    // `toolkit-http` compiles its own plaintext-HTTP test module out under
    // fips. The FIPS posture itself is asserted on the fips side by
    // `the_token_request_stays_tls_only_under_fips` below and by
    // `toolkit-http/tests/fips_default_transport.rs`.
    #[cfg(not(feature = "fips"))]
    #[tokio::test]
    async fn the_form_variant_puts_the_credentials_in_the_request_body() {
        let idp = MockServer::start();
        // Matching on the form fields is what proves the placement: a `Form`
        // client must not carry its credentials in the `Authorization` header.
        let mock = idp.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .header("content-type", "application/x-www-form-urlencoded")
                .body_includes("grant_type=client_credentials")
                .body_includes("scope=read+write")
                .body_includes("client_id=the-client-id")
                .body_includes("client_secret=the-client-secret");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-form","expires_in":3600,"token_type":"Bearer"}"#);
        });
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let binding = json!({
            "token_endpoint": format!("http://127.0.0.1:{}/token", idp.port()),
            "client_id_ref": "cred://vendor-client-id",
            "client_secret_ref": "cred://vendor-client-secret",
            "scopes": "read write",
        });
        let request = request_for(&security);
        let mut headers = HeaderMap::new();

        form_plugin()
            .authenticate(&context(&request, &binding), &mut headers)
            .await
            .expect("the IdP answers");

        mock.assert();
        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer tok-form")
        );
    }

    #[cfg(not(feature = "fips"))]
    #[tokio::test]
    async fn the_basic_variant_puts_the_credentials_in_the_authorization_header() {
        let idp = MockServer::start();
        // `base64("the-client-id:the-client-secret")` (RFC 6749 §2.3.1).
        let mock = idp.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .body_includes("grant_type=client_credentials")
                .header(
                    "authorization",
                    "Basic dGhlLWNsaWVudC1pZDp0aGUtY2xpZW50LXNlY3JldA==",
                );
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-basic","expires_in":3600,"token_type":"Bearer"}"#);
        });
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let binding = config(&idp);
        let request = request_for(&security);
        let mut headers = HeaderMap::new();

        basic_plugin()
            .authenticate(&context(&request, &binding), &mut headers)
            .await
            .expect("the IdP answers");

        mock.assert();
        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer tok-basic")
        );
    }

    #[cfg(not(feature = "fips"))]
    #[tokio::test]
    async fn a_fetched_token_is_injected_and_cached() {
        let idp = MockServer::start();
        let mock = token_mock(&idp, "tok-first", 3600);
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let binding = config(&idp);
        let plugin = basic_plugin();
        let request = request_for(&security);
        let plugin_context = context(&request, &binding);
        let mut headers = HeaderMap::new();

        plugin
            .authenticate(&plugin_context, &mut headers)
            .await
            .expect("the IdP answers");
        mock.assert();

        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer tok-first"),
            "the fetched token is injected"
        );

        let key = build_cache_key(&security, ClientAuthMethod::Basic, &binding);
        assert_eq!(
            plugin.cached_token(&key).as_ref().map(SecretString::expose),
            Some("tok-first"),
            "the token is cached for the next request"
        );
    }

    #[tokio::test]
    async fn a_failed_fetch_is_never_cached() {
        let idp = MockServer::start();
        idp.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(401).body(r#"{"error":"invalid_client"}"#);
        });
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let binding = config(&idp);
        let plugin = basic_plugin();
        let mut headers = HeaderMap::new();

        let request = request_for(&security);
        let error = plugin
            .authenticate(&context(&request, &binding), &mut headers)
            .await
            .expect_err("the IdP refuses the client");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::DownstreamError);
        assert_eq!(error.status().as_u16(), 502);
        assert!(headers.is_empty(), "nothing is forwarded unauthenticated");
        assert!(
            !error.detail().contains("the-client-secret"),
            "the client secret never reaches the detail: {}",
            error.detail()
        );
        assert!(
            !error.detail().contains("invalid_client"),
            "the IdP's response body never reaches the detail: {}",
            error.detail()
        );

        let key = build_cache_key(&security, ClientAuthMethod::Basic, &binding);
        assert!(
            plugin.cached_token(&key).is_none(),
            "a failed fetch is not cached"
        );
    }

    #[cfg(not(feature = "fips"))]
    #[tokio::test]
    async fn an_uninjectable_token_is_not_cached_and_the_next_request_re_fetches() {
        let idp = MockServer::start();
        // A newline cannot be a header value, so the IdP's token is fetched
        // successfully and still cannot be injected.
        let mock = idp.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"access_token":"tok\nunusable","expires_in":3600,"token_type":"Bearer"}"#,
                );
        });
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let binding = config(&idp);
        let plugin = basic_plugin();
        let request = request_for(&security);
        let mut headers = HeaderMap::new();

        let first = plugin
            .authenticate(&context(&request, &binding), &mut headers)
            .await
            .expect_err("the token cannot be injected");

        assert_eq!(first.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert!(headers.is_empty(), "nothing is forwarded unauthenticated");
        let key = build_cache_key(&security, ClientAuthMethod::Basic, &binding);
        assert!(
            plugin.cached_token(&key).is_none(),
            "a token that was never proven injectable is not cached"
        );

        let mut retried = HeaderMap::new();
        let second = plugin
            .authenticate(&context(&request, &binding), &mut retried)
            .await
            .expect_err("the IdP keeps issuing the unusable token");

        assert_eq!(second.kind(), first.kind());
        assert_eq!(
            mock.calls(),
            2,
            "the second request re-fetches instead of replaying the cached failure"
        );
    }

    #[tokio::test]
    async fn an_empty_resolved_credential_is_a_gateway_side_credential_failure() {
        let idp = MockServer::start();
        let token = token_mock(&idp, "tok-never-fetched", 3600);
        // The store answers the secret reference — with nothing in it.
        let store: Option<Arc<dyn CredStoreClientV1>> = Some(Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
                (
                    "cred://vendor-client-id".to_owned(),
                    "the-client-id".to_owned(),
                ),
                ("cred://vendor-client-secret".to_owned(), String::new()),
            ]),
        ));
        let plugin = OAuth2ClientCredAuthPlugin::new(
            store,
            ClientAuthMethod::Basic,
            Duration::from_secs(300),
            16,
        );
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let binding = config(&idp);
        let request = request_for(&security);
        let mut headers = HeaderMap::new();

        let error = plugin
            .authenticate(&context(&request, &binding), &mut headers)
            .await
            .expect_err("the resolved client secret is empty");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert_eq!(error.status().as_u16(), 500);
        assert!(
            !error.detail().contains("client_secret"),
            "the toolkit's wording about the credential never reaches the detail: {}",
            error.detail()
        );
        assert!(
            error.detail().contains(&request.alias),
            "the detail names the upstream the credential was resolved for: {}",
            error.detail()
        );
        assert!(headers.is_empty(), "nothing is forwarded unauthenticated");
        assert_eq!(
            token.calls(),
            0,
            "the failure is detected before any identity provider is contacted"
        );

        let key = build_cache_key(&security, ClientAuthMethod::Basic, &binding);
        assert!(
            plugin.cached_token(&key).is_none(),
            "no token was fetched, so nothing is cached"
        );
    }

    #[test]
    fn an_empty_credential_rejected_by_the_token_source_is_not_an_identity_provider_failure() {
        let request = request_for(&security(Uuid::new_v4(), Uuid::new_v4()));

        let error = token_fetch_error(
            &request,
            TokenError::ConfigError("client_secret must not be empty".to_owned()),
        );

        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert_eq!(error.status().as_u16(), 500);
        assert!(
            !error.detail().contains("must not be empty"),
            "the variant payload is dropped with the rest: {}",
            error.detail()
        );
        assert_eq!(error.extensions()["phase"], "auth_plugin_token_fetch");
    }

    #[tokio::test]
    async fn the_plugin_debug_output_carries_no_credential_material() {
        // The credential material the plugin actually holds is a cached token and
        // the credential store the resolver is wired to, so those are the values
        // this test puts into the `Debug` input. The client secret the store
        // holds is deliberately *not* asserted on: it never enters this `Debug`
        // value — the resolver prints only *whether* a store is wired — so an
        // assertion about it could never fail, and this test used to carry
        // exactly such a vacuous assertion.
        let plugin = basic_plugin();
        plugin.cache.put(
            "tenant:subject:form:hash",
            CachedToken {
                key: "tenant:subject:form:hash".to_owned(),
                token: SecretString::new("tok-live-in-cache"),
            },
            Some(Duration::from_secs(300)),
        );

        let debug = format!("{plugin:?}");

        assert!(
            !debug.contains("tok-live-in-cache"),
            "a cached token is not printable: {debug}"
        );
        assert!(
            debug.contains("client: true"),
            "the resolver publishes only whether a store is wired: {debug}"
        );
    }

    #[test]
    fn the_token_cache_config_defaults_to_the_adr_values() {
        let config = TokenCacheConfig::default();

        assert_eq!(config.ttl, Duration::from_secs(300));
        assert_eq!(config.capacity, 10_000);
    }

    #[test]
    fn an_unusable_token_is_reported_as_a_missing_credential() {
        let mut headers = HeaderMap::new();

        let error = inject_bearer(&mut headers, &SecretString::new("bad token\n"))
            .expect_err("a newline is not a header value");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert!(headers.is_empty(), "nothing is forwarded unauthenticated");
    }

    #[test]
    fn the_error_kind_of_an_idp_failure_is_named_without_its_payload() {
        for (error, kind) in [
            (TokenError::Http("OAuth2 token HTTP 401".to_owned()), "http"),
            (TokenError::ConfigError("no endpoint".to_owned()), "config"),
            (
                TokenError::InvalidResponse("short".to_owned()),
                "invalid_response",
            ),
        ] {
            assert_eq!(token_error_kind(&error), kind);
            assert!(!kind.contains("401"), "the kind carries no payload");
        }
    }

    #[test]
    fn the_form_variant_uses_the_form_identifier_in_its_cache_key() {
        let security = security(Uuid::new_v4(), Uuid::new_v4());
        let config = json!({"token_endpoint": "https://idp.example.com/token"});

        assert_ne!(
            auth_method_tag(ClientAuthMethod::Form),
            auth_method_tag(ClientAuthMethod::Basic),
            "the tag is what keeps the two variants apart"
        );
        assert_eq!(
            build_cache_key(&security, ClientAuthMethod::Form, &config),
            build_cache_key(&security, ClientAuthMethod::Form, &config),
            "the same tuple is the same entry"
        );
    }
}
