//! The OAuth2 client-credentials auth plugin in its `Form` and `Basic`
//! variants (`cpt-cf-oagw-dod-oauth2-token-cache`,
//! `cpt-cf-oagw-algo-oauth2-token-acquisition`,
//! `cpt-cf-oagw-state-token-cache-entry`, ADR 0008).
//!
//! One implementation registered under two plugin ids: the variant decides how
//! the client credentials reach the token endpoint and the auth-method tag of
//! the ADR 0008 cache key, so the two variants never share an entry. The token
//! is fetched with the one-shot [`toolkit_auth::oauth2::fetch_token`] — no
//! background watcher is spawned — and cached in the plugin's own
//! `pingora_memory_cache` instance under the four-component key with the
//! [`CachedToken`] key verified on every hit.
// @cpt-begin:cpt-cf-oagw-dod-oauth2-token-cache:p1:inst-full

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_http::{HttpClientConfig, TransportSecurity};
use url::Url;

use crate::domain::error::OagwError;
use crate::domain::model::CRED_REF_SCHEME;
use crate::domain::plugin::{AUTH_PLUGIN_TYPE, AuthPlugin, RequestContext};
use crate::infra::plugin::registry::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::infra::plugin::token_cache::{
    CachedToken, TokenCacheConfig, cache_key, resolve_secret, ttl,
};

/// The config key of the direct token endpoint URL.
const TOKEN_ENDPOINT_KEY: &str = "token_endpoint";
/// The config key of the OIDC issuer URL resolved by discovery.
const ISSUER_URL_KEY: &str = "issuer_url";
/// The config key of the `cred://` reference of the client id.
const CLIENT_ID_REF_KEY: &str = "client_id_ref";
/// The config key of the `cred://` reference of the client secret.
const CLIENT_SECRET_REF_KEY: &str = "client_secret_ref";
/// The config key of the optional space-separated scope list.
const SCOPES_KEY: &str = "scopes";

/// The scheme prefix of the injected value.
const BEARER_PREFIX: &str = "Bearer ";
/// The auth-method tag of the `Basic` variant in the ADR 0008 cache key.
pub const BASIC_AUTH_METHOD_TAG: &str = "basic";
/// The auth-method tag of the `Form` variant in the ADR 0008 cache key.
pub const FORM_AUTH_METHOD_TAG: &str = "form";

/// The parsed OAuth2 binding configuration.
#[derive(Debug, Clone)]
struct OAuth2Binding {
    /// The direct token endpoint, or `None` when discovery is configured.
    token_endpoint: Option<Url>,
    /// The OIDC issuer URL resolved by discovery, or `None` when the endpoint
    /// is configured directly.
    issuer_url: Option<Url>,
    /// The `cred://` reference of the client id.
    client_id_ref: String,
    /// The `cred://` reference of the client secret.
    client_secret_ref: String,
    /// The requested scopes, in the order the binding lists them.
    scopes: Vec<String>,
}

/// The auth plugin acquiring an OAuth2 client-credentials token, in the `Form`
/// or the `Basic` variant (ADR 0008).
///
/// The plugin owns its cache instance, sized by the [`TokenCacheConfig`] the
/// gear threads in at construction; the cache holds [`CachedToken`] values whose
/// key is verified on every hit and whose token is a `SecretString`. `Debug` is
/// implemented by hand and prints no credential material and no cache content.
pub struct OAuth2ClientCredAuthPlugin {
    cred_store: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    http_config: Option<HttpClientConfig>,
    cache_config: TokenCacheConfig,
    cache: MemoryCache<String, CachedToken>,
}

/// The auth-method tag of one client auth method, the third component of the
/// ADR 0008 cache key.
#[must_use]
pub fn auth_method_tag(auth_method: ClientAuthMethod) -> &'static str {
    match auth_method {
        ClientAuthMethod::Basic => BASIC_AUTH_METHOD_TAG,
        ClientAuthMethod::Form => FORM_AUTH_METHOD_TAG,
    }
}

impl fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .field("cache_config", &self.cache_config)
            .finish()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds one variant of the plugin over the cred-store client and the
    /// cache configuration the gear threads in at startup.
    #[must_use]
    pub fn new(
        cred_store: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        http_config: Option<HttpClientConfig>,
        cache_config: TokenCacheConfig,
    ) -> Self {
        Self {
            cred_store,
            auth_method,
            http_config,
            cache_config,
            cache: MemoryCache::new(cache_config.capacity),
        }
    }

    /// Reads the OAuth2 binding configuration from the binding `config` object.
    ///
    /// A configuration unusable at request time — both or neither of
    /// `token_endpoint` and `issuer_url`, a missing `client_id_ref` or
    /// `client_secret_ref`, a value that is not a `cred://` reference, a value
    /// of another type, or a token endpoint and issuer URL that are not `https`
    /// — is a 400 validation error naming the offending key and echoing no
    /// credential material.
    fn parse(
        config: &BTreeMap<String, Value>,
        allow_plaintext_idp: bool,
    ) -> Result<OAuth2Binding, OagwError> {
        let token_endpoint = match optional_string(config, TOKEN_ENDPOINT_KEY)? {
            Some(url) => Some(parse_url(TOKEN_ENDPOINT_KEY, &url, allow_plaintext_idp)?),
            None => None,
        };
        let issuer_url = match optional_string(config, ISSUER_URL_KEY)? {
            Some(url) => Some(parse_url(ISSUER_URL_KEY, &url, allow_plaintext_idp)?),
            None => None,
        };
        match (&token_endpoint, &issuer_url) {
            (Some(_), Some(_)) => {
                return Err(invalid_config(
                    TOKEN_ENDPOINT_KEY,
                    "and 'issuer_url' are mutually exclusive: exactly one is required",
                ));
            }
            (None, None) => {
                return Err(invalid_config(
                    ISSUER_URL_KEY,
                    "or 'token_endpoint' is required: exactly one is required",
                ));
            }
            (Some(_), None) | (None, Some(_)) => {}
        }
        Ok(OAuth2Binding {
            token_endpoint,
            issuer_url,
            client_id_ref: cred_ref(config, CLIENT_ID_REF_KEY)?,
            client_secret_ref: cred_ref(config, CLIENT_SECRET_REF_KEY)?,
            scopes: optional_string(config, SCOPES_KEY)?
                .map(|scopes| scopes.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
        })
    }

    /// Whether the IdP HTTP client this plugin was constructed with explicitly
    /// permits a plaintext transport.
    ///
    /// This is the only way a plaintext IdP endpoint is accepted, and it is the
    /// IdP-side counterpart of the gear's `allow_http_upstream` knob: an
    /// explicit opt-in, never the default posture. toolkit-http documents
    /// [`HttpClientConfig::for_testing`] — the one preset carrying
    /// [`TransportSecurity::AllowInsecureHttp`] — as the plaintext mock-server
    /// preset for tests, and the release threads `None` for this parameter
    /// (§1.5), so a deployed gear holds the `https` posture.
    fn allows_plaintext_idp(&self) -> bool {
        self.http_config
            .as_ref()
            .is_some_and(|config| config.transport == TransportSecurity::AllowInsecureHttp)
    }

    /// The `Authorization` header value of one bearer token.
    ///
    /// The value is read from the `expose()` slice of the [`SecretString`] the
    /// bearer token is held in; the one owned copy is the one the `http` crate
    /// itself makes inside the header value.
    fn bearer_header(token: &str) -> Result<http::HeaderValue, OagwError> {
        http::HeaderValue::from_str(&format!("{BEARER_PREFIX}{token}")).map_err(|_| {
            OagwError::protocol_error(
                "oagw.plugin.oauth2: the token endpoint returned a value the authorization header cannot carry",
            )
        })
    }

    /// Injects `Authorization: Bearer <token>` into the context headers.
    ///
    /// The bearer token is threaded as the [`SecretString`] it is held in and
    /// never copied into an owned allocation of this layer.
    fn inject(ctx: &mut RequestContext, token: &SecretString) -> Result<(), OagwError> {
        ctx.headers.insert(
            http::header::AUTHORIZATION,
            Self::bearer_header(token.expose())?,
        );
        Ok(())
    }

    /// The ADR 0008 four-component cache key of one binding and subject.
    fn key_of(&self, ctx: &RequestContext, config: &BTreeMap<String, Value>) -> String {
        cache_key(
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            auth_method_tag(self.auth_method),
            config,
        )
    }

    /// Fetches a token, caches it when its lifetime allows, and returns the
    /// bearer value as the [`SecretString`] it was issued in, moved out of the
    /// fetch result with no owned copy of the secret bytes in this layer.
    ///
    /// A failed fetch writes no cache entry, so the next request for the same
    /// key re-attempts the IdP.
    async fn fetch(
        &self,
        ctx: &RequestContext,
        binding: &OAuth2Binding,
        config: &BTreeMap<String, Value>,
    ) -> Result<SecretString, OagwError> {
        let key = self.key_of(ctx, config);

        // The plugin failure of the auth phase is mapped per the
        // failure-mapping interpretation of §1.5 (`inst-au-09`, held once by
        // the shared `token_cache::resolve_secret` and by the fetch catch
        // below), adding no row to the mapping table.
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-09
        // Resolve `client_id_ref` and `client_secret_ref` through the
        // cred-store client into `SecretString` values.
        let client_id = resolve_secret(
            &ctx.security_context,
            &self.cred_store,
            &binding.client_id_ref,
        )
        .await?;
        let client_secret = resolve_secret(
            &ctx.security_context,
            &self.cred_store,
            &binding.client_secret_ref,
        )
        .await?;
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-10
        // A reference that does not resolve is the 500 `SecretNotFound` of the
        // shared credential resolution.
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-11
        // An unreachable credential store is the 503 `LinkUnavailable` of the
        // shared credential resolution; neither message echoes credential
        // material.
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-11
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-10
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-09

        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-12
        // `toolkit_auth::oauth2::fetch_token` performs the one-shot HTTP
        // exchange with the resolved client configuration and the selected
        // auth method, returning the bearer value and `expires_in`; it spawns
        // no background watcher.
        let fetched = fetch_token(OAuthClientConfig {
            token_endpoint: binding.token_endpoint.clone(),
            issuer_url: binding.issuer_url.clone(),
            client_id: client_id.expose().to_owned(),
            client_secret,
            scopes: binding.scopes.clone(),
            auth_method: self.auth_method,
            http_config: self.http_config.clone(),
            ..OAuthClientConfig::default()
        })
        .await;
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-12
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-13
        // A failed fetch — an unreachable IdP or rejected client credentials —
        // is the 503 `LinkUnavailable`, and no cache entry is written, so the
        // next request for the same key re-attempts the IdP.
        let fetched = fetched.map_err(|_| {
            OagwError::link_unavailable(
                "oagw.plugin.oauth2: the token endpoint did not issue an access token",
            )
        })?;
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-13

        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-15
        // IF `expires_in` is above the 30-second safety margin, the token is
        // cacheable.
        let cache_ttl = ttl(self.cache_config.ttl_secs, fetched.expires_in);
        if let Some(cache_ttl) = cache_ttl {
            // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-16
            // The entry is put into the cache under the four-component key with
            // the computed TTL, the token held as a `SecretString` that is
            // zeroed on eviction.
            // @cpt-begin:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-01
            // FROM `Absent` TO `Cached`: the fetch for this key succeeded and
            // the IdP reported an `expires_in` above the safety margin, and the
            // entry carries the computed TTL
            // `min(token_cache_ttl_secs, expires_in − 30)`.
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(cache_ttl),
            );
            // @cpt-end:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-01
            // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-16
        } else {
            // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-17
            // ELSE the token is used for this request only and no cache entry
            // is written, because a token this close to expiry would be stale
            // on its next use.
            // @cpt-begin:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-02
            // FROM `Absent` TO `Absent`: no entry is written for a lifetime at
            // or below the safety margin, so the next request for the same key
            // re-attempts the fetch.
            // @cpt-end:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-02
            // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-17
        }
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-15
        // The bearer value is returned as the `SecretString` it was issued in,
        // moved out of the fetch result: no owned plain copy of the secret
        // bytes is created outside the cached entry.
        Ok(fetched.bearer)
    }
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        }
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-07
        // The resolved plugin is the OAuth2 client-credentials auth plugin, in
        // its `Form` or its `Basic` variant.
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-08
        // The OAuth2 token-acquisition algorithm runs: it validates the
        // configuration, consults the internal token cache and injects the
        // bearer token.
        let config = ctx.config.clone();
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-01
        // Parse the OAuth2 config keys: `token_endpoint` and `issuer_url`
        // (mutually exclusive, exactly one required), `client_id_ref` and
        // `client_secret_ref` (both required `cred://` references) and
        // `scopes` (optional, space-separated).
        let binding = Self::parse(&config, self.allows_plaintext_idp())?;
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-02
        // A configuration unusable at request time is a 400 validation error
        // naming the offending key and echoing no credential material.
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-02
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-01

        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-03
        // Build the four-component cache key of ADR 0008: the subject tenant
        // id, the subject id, the auth-method tag, and the deterministic hash
        // of the sorted config key/value pairs.
        let key = self.key_of(ctx, &config);
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-03
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-04
        // Look the key up in the plugin's `pingora_memory_cache` instance.
        let (entry, _status) = self.cache.get(&key);
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-04
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-05
        // IF the lookup hits AND the stored `CachedToken.key` equals the lookup
        // key.
        if let Some(entry) = entry.filter(|entry| entry.key == key) {
            // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-06
            // The bearer token is injected into the context headers and success
            // is returned, with no IdP call and no credential-store lookup.
            // @cpt-begin:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-05
            // FROM `Cached` TO `Cached`: a hit whose stored key matches is
            // served, the entry's TTL is unchanged, and no refresh, no re-fetch
            // and no invalidation happens, because the release has no
            // cache-invalidation mechanism.
            // The token is read through its `expose()` slice, so no owned plain
            // copy of the secret bytes leaves the cached `SecretString`.
            Self::inject(ctx, &entry.token)?;
            return Ok(());
            // @cpt-end:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-05
            // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-06
        }
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-07
        // ELSE IF the lookup hits AND the stored key does not equal the lookup
        // key.
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-08
        // The entry is treated as a miss and discarded, so a hash collision
        // degrades to a miss and never to another tenant's token.
        // @cpt-begin:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-04
        // FROM `Cached` TO `Absent`: the stored `CachedToken.key` does not
        // equal the key being looked up, the entry is discarded and the lookup
        // is treated as a miss.
        self.cache.remove(&key);
        // @cpt-end:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-04
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-08
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-07
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-05

        let token = self.fetch(ctx, &binding, &config).await?;
        // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-18
        // `Authorization: Bearer <token>` is injected into the context headers
        // and success is returned.
        Self::inject(ctx, &token)?;
        Ok(())
        // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-18
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-08
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-07
    }
}

/// Reads one optional string config value, rejecting a value of another type.
fn optional_string(
    config: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<String>, OagwError> {
    match config.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid_config(key, "must be a string")),
    }
}

/// Reads one required `cred://` reference from the binding config.
fn cred_ref(config: &BTreeMap<String, Value>, key: &str) -> Result<String, OagwError> {
    match optional_string(config, key)? {
        Some(reference) if reference.starts_with(CRED_REF_SCHEME) => Ok(reference),
        Some(_) => Err(invalid_config(
            key,
            "must be a 'cred://' reference that the credential store resolves at request time",
        )),
        None => Err(invalid_config(
            key,
            "is required: the client credentials resolve from the credential store",
        )),
    }
}

/// Parses one configured IdP URL, rejecting a value that is not an `https` URL.
///
/// The token endpoint and the issuer URL are where the client secret is sent,
/// so a plaintext `http://` — or any other scheme — is refused with the 400
/// validation error naming the key. This is the IdP-side gate that the gear
/// carries for its own upstreams through `allow_http_upstream` and
/// `ssrf_policy`, which no binding configuration of this plugin can switch off.
///
/// The one exception is an IdP HTTP client the gear threaded in that explicitly
/// selects [`TransportSecurity::AllowInsecureHttp`], the plaintext transport
/// toolkit-http documents for test mock servers; see
/// [`OAuth2ClientCredAuthPlugin::allows_plaintext_idp`]. A deployed gear threads
/// `None` for that parameter, so `https` is enforced.
fn parse_url(key: &str, value: &str, allow_plaintext: bool) -> Result<Url, OagwError> {
    let url = Url::parse(value)
        .map_err(|_| invalid_config(key, "must be a URL the token client can call"))?;
    if allow_plaintext || url.scheme() == "https" {
        return Ok(url);
    }
    Err(invalid_config(
        key,
        "must be an 'https' URL: a plaintext token endpoint would carry the client credentials in the clear",
    ))
}

/// Builds the 400 validation error naming the offending key.
fn invalid_config(key: &str, reason: &str) -> OagwError {
    OagwError::validation_error(format!("oagw.plugin.oauth2: '{key}' {reason}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use credstore_sdk::test_util::MockCredStoreClient;
    use credstore_sdk::{
        CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef, SecretValue, SharingMode,
        WriteOptions, WritePrecondition,
    };
    use httpmock::{MockServer, prelude::*};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::*;

    const TENANT: Uuid = Uuid::from_u128(0xa11ce);
    const SUBJECT: Uuid = Uuid::from_u128(0xbeef);
    const CLIENT_SECRET: &str = "client-secret-value";

    /// A cred-store double that counts its `get` calls, so a test can observe
    /// that a cache hit performs no credential-store lookup.
    struct CountingCredStore {
        inner: MockCredStoreClient,
        gets: AtomicUsize,
    }

    impl CountingCredStore {
        fn with_secrets(creds: Vec<(String, String)>) -> Self {
            Self {
                inner: MockCredStoreClient::with_secrets(creds),
                gets: AtomicUsize::new(0),
            }
        }

        fn gets(&self) -> usize {
            self.gets.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CredStoreClientV1 for CountingCredStore {
        async fn get(
            &self,
            ctx: &SecurityContext,
            key: &SecretRef,
        ) -> Result<Option<GetSecretResponse>, CredStoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.inner.get(ctx, key).await
        }

        async fn put_opts(
            &self,
            _ctx: &SecurityContext,
            _key: &SecretRef,
            _value: SecretValue,
            _sharing: SharingMode,
            _precondition: WritePrecondition,
            _opts: WriteOptions,
        ) -> Result<(), CredStoreError> {
            Ok(())
        }
    }

    fn security_context() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(SUBJECT)
            .subject_tenant_id(TENANT)
            .build()
            .expect("a test context carries a subject and a tenant")
    }

    fn request(config: BTreeMap<String, Value>) -> RequestContext {
        let mut ctx = RequestContext::new(security_context());
        ctx.config = config;
        ctx
    }

    fn token_endpoint_config(url: &str, scopes: Option<&str>) -> BTreeMap<String, Value> {
        let mut config = BTreeMap::from([
            (TOKEN_ENDPOINT_KEY.to_owned(), Value::String(url.to_owned())),
            (
                CLIENT_ID_REF_KEY.to_owned(),
                Value::String("cred://oauth-client-id".to_owned()),
            ),
            (
                CLIENT_SECRET_REF_KEY.to_owned(),
                Value::String("cred://oauth-client-secret".to_owned()),
            ),
        ]);
        if let Some(scopes) = scopes {
            config.insert(SCOPES_KEY.to_owned(), Value::String(scopes.to_owned()));
        }
        config
    }

    fn plugin(cred_store: Arc<dyn CredStoreClientV1>) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            cred_store,
            ClientAuthMethod::Form,
            Some(HttpClientConfig::for_testing()),
            TokenCacheConfig::new(300, 128),
        )
    }

    /// A plugin built with the release posture, in which no IdP HTTP client is
    /// threaded (§1.5) and the `https` requirement on `token_endpoint` and
    /// `issuer_url` therefore holds.
    fn strict_plugin() -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            Arc::new(MockCredStoreClient::empty()),
            ClientAuthMethod::Form,
            None,
            TokenCacheConfig::new(300, 128),
        )
    }

    fn form_plugin() -> OAuth2ClientCredAuthPlugin {
        plugin(Arc::new(CountingCredStore::with_secrets(vec![
            ("oauth-client-id".to_owned(), "client-id-value".to_owned()),
            ("oauth-client-secret".to_owned(), CLIENT_SECRET.to_owned()),
        ])))
    }

    fn token_json(token: &str, expires_in: u64) -> String {
        format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
    }

    fn idp(server: &MockServer, status: u16, body: String) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(status)
                .header("content-type", "application/json")
                .body(body);
        })
    }

    fn key_of(config: &BTreeMap<String, Value>) -> String {
        cache_key(
            TENANT,
            SUBJECT,
            auth_method_tag(ClientAuthMethod::Form),
            config,
        )
    }

    #[tokio::test]
    async fn a_form_binding_fetches_and_injects_the_bearer() {
        let server = MockServer::start();
        let mock = idp(&server, 200, token_json("tok-first", 3600));
        let store = Arc::new(CountingCredStore::with_secrets(vec![
            ("oauth-client-id".to_owned(), "client-id-value".to_owned()),
            ("oauth-client-secret".to_owned(), CLIENT_SECRET.to_owned()),
        ]));
        let plugin = plugin(Arc::clone(&store) as Arc<dyn CredStoreClientV1>);
        let config = token_endpoint_config(&server.url("/token"), Some("read write"));

        let mut ctx = request(config);
        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the token is fetched");

        assert_eq!(mock.calls(), 1, "the one-shot fetch happens once");
        assert_eq!(
            store.gets(),
            2,
            "the client id and the client secret resolve"
        );
        assert_eq!(
            ctx.headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
            Some("Bearer tok-first"),
            "the bearer token is injected"
        );
        assert!(ctx.query.is_empty());
    }

    #[tokio::test]
    async fn a_second_request_is_served_from_the_cache() {
        let server = MockServer::start();
        let mock = idp(&server, 200, token_json("tok-cached", 3600));
        let store = Arc::new(CountingCredStore::with_secrets(vec![
            ("oauth-client-id".to_owned(), "client-id-value".to_owned()),
            ("oauth-client-secret".to_owned(), CLIENT_SECRET.to_owned()),
        ]));
        let plugin = plugin(Arc::clone(&store) as Arc<dyn CredStoreClientV1>);
        let config = token_endpoint_config(&server.url("/token"), None);

        let mut first = request(config.clone());
        plugin.authenticate(&mut first).await.unwrap();
        let mut second = request(config);
        plugin.authenticate(&mut second).await.unwrap();

        assert_eq!(mock.calls(), 1, "no IdP call on a hit");
        assert_eq!(store.gets(), 2, "no credential-store lookup on a hit");
        assert_eq!(
            second
                .headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
            Some("Bearer tok-cached"),
            "the cached token is served"
        );
        assert_eq!(
            first
                .headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
            second
                .headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
        );
    }

    #[tokio::test]
    async fn a_hit_with_a_matching_key_leaves_the_entry_untouched() {
        let plugin = form_plugin();
        let config = token_endpoint_config("https://idp.example.com/token", None);
        let key = key_of(&config);

        plugin.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: SecretString::new("tok-seeded"),
            },
            Some(Duration::from_secs(300)),
        );

        let mut ctx = request(config);
        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the hit is served");

        assert_eq!(
            ctx.headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
            Some("Bearer tok-seeded"),
            "the entry's value is served"
        );
    }

    #[tokio::test]
    async fn a_hash_collision_degrades_to_a_miss() {
        let server = MockServer::start();
        let mock = idp(&server, 200, token_json("tok-fetched", 3600));
        let plugin = form_plugin();
        let config = token_endpoint_config(&server.url("/token"), None);
        let key = key_of(&config);

        plugin.cache.put(
            &key,
            CachedToken {
                key: "another-tenant-key".to_owned(),
                token: SecretString::new("tok-of-another-tenant"),
            },
            Some(Duration::from_secs(300)),
        );

        let mut ctx = request(config);
        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the fetch re-runs");

        assert_eq!(mock.calls(), 1, "the colliding entry is not served");
        assert_eq!(
            ctx.headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
            Some("Bearer tok-fetched"),
            "no other tenant's token is ever injected"
        );
    }

    #[tokio::test]
    async fn a_failed_fetch_writes_no_entry_and_is_reattempted() {
        let server = MockServer::start();
        let mock = idp(&server, 401, String::new());
        let plugin = form_plugin();
        let config = token_endpoint_config(&server.url("/token"), None);

        let mut first = request(config.clone());
        let error = plugin.authenticate(&mut first).await.unwrap_err();
        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
        assert!(
            first.headers.get("authorization").is_none(),
            "no token is injected on a failed fetch"
        );

        let mut second = request(config);
        plugin
            .authenticate(&mut second)
            .await
            .expect_err("the fetch is re-attempted");
        assert_eq!(
            mock.calls(),
            2,
            "the failed fetch left no cache entry behind"
        );
    }

    #[tokio::test]
    async fn an_unreachable_idp_is_a_link_unavailable() {
        let plugin = form_plugin();
        let config = token_endpoint_config("http://127.0.0.1:9/token", None);

        let error = plugin.authenticate(&mut request(config)).await.unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
    }

    #[tokio::test]
    async fn a_token_at_or_below_the_margin_is_never_cached() {
        let server = MockServer::start();
        let mock = idp(&server, 200, token_json("tok-short", 10));
        let plugin = form_plugin();
        let config = token_endpoint_config(&server.url("/token"), None);

        let mut first = request(config.clone());
        plugin
            .authenticate(&mut first)
            .await
            .expect("the short-lived token is used");
        assert_eq!(
            first
                .headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
            Some("Bearer tok-short"),
        );

        let mut second = request(config);
        plugin
            .authenticate(&mut second)
            .await
            .expect("the token is fetched again");
        assert_eq!(
            mock.calls(),
            2,
            "no cache entry is written at or below the margin"
        );
    }

    #[tokio::test]
    async fn a_config_differing_only_in_scopes_never_shares_an_entry() {
        let server = MockServer::start();
        let mock = idp(&server, 200, token_json("tok-scoped", 3600));
        let plugin = form_plugin();

        let mut first = request(token_endpoint_config(&server.url("/token"), Some("read")));
        plugin.authenticate(&mut first).await.unwrap();
        let mut second = request(token_endpoint_config(
            &server.url("/token"),
            Some("read write"),
        ));
        plugin.authenticate(&mut second).await.unwrap();

        assert_eq!(mock.calls(), 2, "the config hash is part of the cache key");
    }

    #[tokio::test]
    async fn an_unusable_config_is_a_validation_error_naming_the_key() {
        let plugin = form_plugin();
        let server = MockServer::start();
        let endpoint = server.url("/token");

        let cases: Vec<(BTreeMap<String, Value>, &str)> = vec![
            (
                BTreeMap::from([
                    (
                        TOKEN_ENDPOINT_KEY.to_owned(),
                        Value::String(endpoint.clone()),
                    ),
                    (
                        ISSUER_URL_KEY.to_owned(),
                        Value::String("https://issuer.example.com".to_owned()),
                    ),
                ]),
                TOKEN_ENDPOINT_KEY,
            ),
            (BTreeMap::new(), ISSUER_URL_KEY),
            (
                BTreeMap::from([
                    (
                        TOKEN_ENDPOINT_KEY.to_owned(),
                        Value::String(endpoint.clone()),
                    ),
                    (
                        CLIENT_SECRET_REF_KEY.to_owned(),
                        Value::String("cred://secret".to_owned()),
                    ),
                ]),
                CLIENT_ID_REF_KEY,
            ),
            (
                BTreeMap::from([
                    (
                        TOKEN_ENDPOINT_KEY.to_owned(),
                        Value::String(endpoint.clone()),
                    ),
                    (
                        CLIENT_ID_REF_KEY.to_owned(),
                        Value::String("cred://client".to_owned()),
                    ),
                ]),
                CLIENT_SECRET_REF_KEY,
            ),
            (
                BTreeMap::from([
                    (
                        TOKEN_ENDPOINT_KEY.to_owned(),
                        Value::String(endpoint.clone()),
                    ),
                    (
                        CLIENT_ID_REF_KEY.to_owned(),
                        Value::String("client".to_owned()),
                    ),
                    (
                        CLIENT_SECRET_REF_KEY.to_owned(),
                        Value::String("cred://secret".to_owned()),
                    ),
                ]),
                CLIENT_ID_REF_KEY,
            ),
            (
                BTreeMap::from([
                    (
                        TOKEN_ENDPOINT_KEY.to_owned(),
                        Value::String("not a url".to_owned()),
                    ),
                    (
                        CLIENT_ID_REF_KEY.to_owned(),
                        Value::String("cred://client".to_owned()),
                    ),
                    (
                        CLIENT_SECRET_REF_KEY.to_owned(),
                        Value::String("cred://secret".to_owned()),
                    ),
                ]),
                TOKEN_ENDPOINT_KEY,
            ),
            (
                BTreeMap::from([
                    (TOKEN_ENDPOINT_KEY.to_owned(), Value::from(7)),
                    (
                        CLIENT_ID_REF_KEY.to_owned(),
                        Value::String("cred://client".to_owned()),
                    ),
                    (
                        CLIENT_SECRET_REF_KEY.to_owned(),
                        Value::String("cred://secret".to_owned()),
                    ),
                ]),
                TOKEN_ENDPOINT_KEY,
            ),
        ];

        for (config, key) in cases {
            let error = plugin.authenticate(&mut request(config)).await.unwrap_err();
            assert_eq!(error.mapping().variant, "ValidationError", "{key}");
            assert_eq!(error.status(), 400, "{key}");
            assert!(
                error.to_string().contains(key),
                "the failure names the offending key '{key}': {error}"
            );
            assert!(
                !error.to_string().contains(CLIENT_SECRET),
                "no credential material is echoed for '{key}': {error}"
            );
        }
    }

    /// The token endpoint and the issuer URL are where the client credentials
    /// are sent, so a plaintext IdP is a 400 validation error naming the key
    /// instead of an exchange the gateway would perform in the clear.
    #[tokio::test]
    async fn a_plaintext_idp_url_is_a_validation_error_naming_the_key() {
        let plugin = strict_plugin();

        let cases: Vec<(BTreeMap<String, Value>, &str)> = vec![
            (
                token_endpoint_config("http://idp.example/token", None),
                TOKEN_ENDPOINT_KEY,
            ),
            (
                BTreeMap::from([
                    (
                        ISSUER_URL_KEY.to_owned(),
                        Value::String("http://idp.example".to_owned()),
                    ),
                    (
                        CLIENT_ID_REF_KEY.to_owned(),
                        Value::String("cred://client".to_owned()),
                    ),
                    (
                        CLIENT_SECRET_REF_KEY.to_owned(),
                        Value::String("cred://secret".to_owned()),
                    ),
                ]),
                ISSUER_URL_KEY,
            ),
        ];

        for (config, key) in cases {
            let error = plugin.authenticate(&mut request(config)).await.unwrap_err();

            assert_eq!(error.mapping().variant, "ValidationError", "{key}");
            assert_eq!(error.status(), 400, "{key}");
            assert!(
                error.to_string().contains(key),
                "the failure names the offending key '{key}': {error}"
            );
            assert!(
                error.to_string().contains("https"),
                "the failure names the required scheme: {error}"
            );
        }
    }

    /// An `https` IdP still parses: the unresolvable credential reference of the
    /// empty store proves the configuration was accepted and the chain moved on
    /// to the credential resolution.
    #[tokio::test]
    async fn an_https_idp_url_still_validates() {
        let plugin = strict_plugin();
        let config = token_endpoint_config("https://idp.example.com/token", None);

        let error = plugin.authenticate(&mut request(config)).await.unwrap_err();

        assert_eq!(error.mapping().variant, "SecretNotFound");
        assert_eq!(error.status(), 500);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[tokio::test]
    async fn an_unresolvable_client_reference_is_a_secret_not_found() {
        let plugin = plugin(Arc::new(MockCredStoreClient::empty()));
        let server = MockServer::start();
        let config = token_endpoint_config(&server.url("/token"), None);

        let error = plugin.authenticate(&mut request(config)).await.unwrap_err();

        assert_eq!(error.mapping().variant, "SecretNotFound");
        assert_eq!(error.status(), 500);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[tokio::test]
    async fn an_unreachable_credential_store_is_a_link_unavailable() {
        let plugin = plugin(Arc::new(MockCredStoreClient::always_failing()));
        let server = MockServer::start();
        let config = token_endpoint_config(&server.url("/token"), None);

        let error = plugin.authenticate(&mut request(config)).await.unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
    }

    #[tokio::test]
    async fn no_failure_message_echoes_credential_material() {
        let server = MockServer::start();
        let _mock = idp(&server, 500, String::new());
        let plugin = form_plugin();
        let config = token_endpoint_config(&server.url("/token"), None);

        let error = plugin.authenticate(&mut request(config)).await.unwrap_err();

        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(CLIENT_SECRET),
            "the client secret never reaches a failure message: {rendered}"
        );
        assert!(
            !rendered.contains("client-id-value"),
            "the client id never reaches a failure message: {rendered}"
        );
    }

    /// §6: an OAuth2 binding carrying both or neither of `token_endpoint` and
    /// `issuer_url`, or missing `client_id_ref` or `client_secret_ref`, is
    /// answered 400 with the general validation type before any credential-store
    /// or IdP call is made.
    #[tokio::test]
    async fn an_unusable_config_is_rejected_before_any_lookup() {
        let server = MockServer::start();
        let mock = idp(&server, 200, token_json("tok-never-fetched", 3600));
        let store = Arc::new(CountingCredStore::with_secrets(vec![
            ("oauth-client-id".to_owned(), "client-id-value".to_owned()),
            ("oauth-client-secret".to_owned(), CLIENT_SECRET.to_owned()),
        ]));
        let plugin = plugin(Arc::clone(&store) as Arc<dyn CredStoreClientV1>);
        let endpoint = server.url("/token");

        let cases: Vec<BTreeMap<String, Value>> = vec![
            BTreeMap::from([
                (
                    TOKEN_ENDPOINT_KEY.to_owned(),
                    Value::String(endpoint.clone()),
                ),
                (
                    ISSUER_URL_KEY.to_owned(),
                    Value::String("https://issuer.example.com".to_owned()),
                ),
            ]),
            BTreeMap::new(),
            BTreeMap::from([
                (
                    TOKEN_ENDPOINT_KEY.to_owned(),
                    Value::String(endpoint.clone()),
                ),
                (
                    CLIENT_SECRET_REF_KEY.to_owned(),
                    Value::String("cred://secret".to_owned()),
                ),
            ]),
            BTreeMap::from([
                (
                    TOKEN_ENDPOINT_KEY.to_owned(),
                    Value::String(endpoint.clone()),
                ),
                (
                    CLIENT_ID_REF_KEY.to_owned(),
                    Value::String("cred://client".to_owned()),
                ),
            ]),
        ];

        for config in cases {
            let error = plugin.authenticate(&mut request(config)).await.unwrap_err();
            assert_eq!(error.mapping().variant, "ValidationError");
            assert_eq!(error.status(), 400);
            assert_eq!(
                error.gts_type(),
                "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
            );
        }

        assert_eq!(
            store.gets(),
            0,
            "no credential reference is resolved for an unusable config"
        );
        assert_eq!(
            mock.calls(),
            0,
            "no IdP call is made for an unusable config"
        );
    }

    #[test]
    fn the_two_variants_are_two_separately_registered_ids() {
        let form = form_plugin();
        let basic = OAuth2ClientCredAuthPlugin::new(
            Arc::new(MockCredStoreClient::empty()),
            ClientAuthMethod::Basic,
            Some(HttpClientConfig::for_testing()),
            TokenCacheConfig::new(300, 128),
        );

        assert_eq!(form.id(), OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID);
        assert_eq!(basic.id(), OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID);
        assert_ne!(form.id(), basic.id());
        assert_eq!(form.plugin_type(), AUTH_PLUGIN_TYPE);
        assert_eq!(basic.plugin_type(), AUTH_PLUGIN_TYPE);
        assert_eq!(
            auth_method_tag(ClientAuthMethod::Form),
            FORM_AUTH_METHOD_TAG
        );
        assert_eq!(
            auth_method_tag(ClientAuthMethod::Basic),
            BASIC_AUTH_METHOD_TAG
        );
        assert_ne!(
            key_of(&BTreeMap::new()),
            cache_key(
                TENANT,
                SUBJECT,
                auth_method_tag(ClientAuthMethod::Basic),
                &BTreeMap::new()
            ),
            "the two variants never share an entry"
        );
    }

    #[test]
    fn the_cache_is_sized_by_the_gear_configuration() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            Arc::new(MockCredStoreClient::empty()),
            ClientAuthMethod::Form,
            None,
            TokenCacheConfig::new(60, 7),
        );
        assert_eq!(plugin.cache_config, TokenCacheConfig::new(60, 7));
        assert!(plugin.http_config.is_none());
        let _ = &plugin.cache;
    }
}
// @cpt-end:cpt-cf-oagw-dod-oauth2-token-cache:p1:inst-full
