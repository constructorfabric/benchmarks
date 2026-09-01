//! The `OAuth2` Client Credentials auth plugin (`ADR`-0008).
//!
//! One cache per plugin family, shared by every binding of the deployment: a
//! cache miss resolves the `cred://` references, performs a single
//! `fetch_token` exchange, and stores the bearer value for
//! `min(config_ttl, expires_in − 30s)`. The cache key encodes the tenant, the
//! subject, the client-auth method and a hash of the binding config, and the
//! cached entry carries the key back so a hash collision degrades to a miss
//! instead of handing one tenant's token to another.

use std::sync::Arc;

use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::oauth2::{
    ClientAuthMethod, FetchedToken, OAuthClientConfig, SecretString, TokenError, fetch_token,
};

use crate::domain::dto::ProxyContext;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::plugin::{AuthOutcome, AuthPlugin};
use crate::infra::plugin::secrets::SecretResolver;

/// Safety margin subtracted from the IdP-reported `expires_in` (`ADR`-0008):
/// a token that is nearly expired when cached is worse than no cache at all.
pub const EXPIRY_MARGIN_SECS: u64 = 30;

/// Ceiling for a cached token's lifetime (`ADR`-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// `token_cache_ttl_secs`: maximum seconds a token is served from cache.
    pub ttl_secs: u64,
    /// `token_cache_capacity`: maximum number of cached tokens.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 300,
            capacity: 10_000,
        }
    }
}

/// A cached token, carrying the key it was stored under (`ADR`-0008).
#[derive(Clone)]
pub struct CachedToken {
    /// The key the entry was stored under, verified on read.
    pub key: String,
    /// The bearer value, wrapped so it never lands in a log.
    pub token: SecretString,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// The binding configuration of the plugin (`ADR`-0008, config keys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuth2PluginConfig {
    /// Direct token endpoint URL, mutually exclusive with `issuer_url`.
    pub token_endpoint: Option<url::Url>,
    /// OIDC issuer URL, mutually exclusive with `token_endpoint`.
    pub issuer_url: Option<url::Url>,
    /// `cred://` reference of the client id.
    pub client_id_ref: String,
    /// `cred://` reference of the client secret.
    pub client_secret_ref: String,
    /// Space-separated scopes.
    pub scopes: String,
}

impl OAuth2PluginConfig {
    /// Parse the binding configuration.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] for a missing key, a URL that
    /// cannot be parsed, or both endpoint spellings at once.
    pub fn parse(config: Option<&Value>) -> Result<Self, DomainError> {
        let config = config.cloned().unwrap_or(Value::Null);
        let reference = |key: &str| -> Result<String, DomainError> {
            config
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    DomainError::validation(format!(
                        "oauth2 auth config requires a '{key}' credential reference"
                    ))
                })
        };
        let client_id_ref = reference("client_id_ref")?;
        let client_secret_ref = reference("client_secret_ref")?;
        let parse_url = |key: &str| -> Result<Option<url::Url>, DomainError> {
            config
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    url::Url::parse(value).map_err(|_| {
                        DomainError::validation(format!("'{key}' is not a URL: {value}"))
                    })
                })
                .transpose()
        };
        let token_endpoint = parse_url("token_endpoint")?;
        let issuer_url = parse_url("issuer_url")?;
        if token_endpoint.is_some() && issuer_url.is_some() {
            return Err(DomainError::validation(
                "oauth2 auth config accepts one of 'token_endpoint' or 'issuer_url'",
            ));
        }
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(DomainError::validation(
                "oauth2 auth config requires one of 'token_endpoint' or 'issuer_url'",
            ));
        }
        let scopes = config
            .get("scopes")
            .and_then(Value::as_str)
            .map_or_else(String::new, |value| value.trim().to_owned());
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes,
        })
    }

    /// Deterministic hash of the configuration, for the cache key (`ADR`-0008).
    ///
    /// The map is serialized with sorted keys — `serde_json` preserves insertion
    /// order for `Map`, so the canonical form is built explicitly.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let ordered = serde_json::Map::from_iter([
            (
                "client_id_ref".to_owned(),
                Value::String(self.client_id_ref.clone()),
            ),
            (
                "client_secret_ref".to_owned(),
                Value::String(self.client_secret_ref.clone()),
            ),
            (
                "issuer_url".to_owned(),
                self.issuer_url
                    .as_ref()
                    .map_or(Value::Null, |url| Value::String(url.to_string())),
            ),
            ("scopes".to_owned(), Value::String(self.scopes.clone())),
            (
                "token_endpoint".to_owned(),
                self.token_endpoint
                    .as_ref()
                    .map_or(Value::Null, |url| Value::String(url.to_string())),
            ),
        ]);
        let canonical = Value::Object(ordered).to_string();
        stable_hash(canonical.as_bytes())
    }
}

/// FNV-1a over a byte slice: stable across processes, unlike `DefaultHasher`.
fn stable_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Cache-key tag of a client-auth method (`ADR`-0008).
#[must_use]
pub fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

/// Cache key of one (tenant, subject, auth method, config) tuple.
#[must_use]
pub fn build_cache_key(
    tenant: uuid::Uuid,
    subject: uuid::Uuid,
    auth_method: ClientAuthMethod,
    config: &OAuth2PluginConfig,
) -> String {
    format!(
        "{}:{}:{}:{:016x}",
        tenant,
        subject,
        auth_method_tag(auth_method),
        config.fingerprint()
    )
}

/// The client-credentials exchange, as a port.
///
/// The plugin drives the cache and the credential resolution; the `IdP` round
/// trip stays behind this trait so a test can stand in for the token endpoint.
#[async_trait::async_trait]
pub trait TokenExchanger: Send + Sync {
    /// Exchanges `config` for a bearer token and its reported lifetime.
    ///
    /// # Errors
    /// The error of the underlying `OAuth2` client, mapped by the caller.
    async fn exchange(&self, config: OAuthClientConfig) -> Result<FetchedToken, TokenError>;
}

/// [`TokenExchanger`] over `toolkit_auth`'s `fetch_token`.
#[derive(Debug, Default)]
pub struct FetchTokenExchanger {
    http_config: Option<toolkit_http::HttpClientConfig>,
}

impl FetchTokenExchanger {
    /// An exchanger whose token requests use `http_config`.
    #[must_use]
    pub const fn new(http_config: Option<toolkit_http::HttpClientConfig>) -> Self {
        Self { http_config }
    }
}

#[async_trait::async_trait]
impl TokenExchanger for FetchTokenExchanger {
    async fn exchange(&self, mut config: OAuthClientConfig) -> Result<FetchedToken, TokenError> {
        if config.http_config.is_none() {
            config.http_config = self.http_config.clone();
        }
        fetch_token(config).await
    }
}

/// Injects an `OAuth2` bearer token acquired through the client-credentials flow.
pub struct OAuth2ClientCredAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
    exchanger: Arc<dyn TokenExchanger>,
    auth_method: ClientAuthMethod,
    cache: Arc<MemoryCache<String, CachedToken>>,
    cache_ttl: std::time::Duration,
    identity: (uuid::Uuid, uuid::Uuid),
    config: OAuth2PluginConfig,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build the plugin for one upstream binding, sharing `cache` with every
    /// other binding of the same family.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when the binding configuration is
    /// malformed.
    pub fn new(
        resolver: Arc<dyn SecretResolver>,
        auth_method: ClientAuthMethod,
        http_config: Option<toolkit_http::HttpClientConfig>,
        cache: Arc<MemoryCache<String, CachedToken>>,
        cache_config: TokenCacheConfig,
        identity: (uuid::Uuid, uuid::Uuid),
        config: Option<&Value>,
    ) -> Result<Self, DomainError> {
        Self::with_exchanger(
            resolver,
            auth_method,
            Arc::new(FetchTokenExchanger::new(http_config)),
            cache,
            cache_config,
            identity,
            config,
        )
    }

    /// Build the plugin over an explicit [`TokenExchanger`], for tests.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when the binding configuration is
    /// malformed.
    pub fn with_exchanger(
        resolver: Arc<dyn SecretResolver>,
        auth_method: ClientAuthMethod,
        exchanger: Arc<dyn TokenExchanger>,
        cache: Arc<MemoryCache<String, CachedToken>>,
        cache_config: TokenCacheConfig,
        identity: (uuid::Uuid, uuid::Uuid),
        config: Option<&Value>,
    ) -> Result<Self, DomainError> {
        let config = OAuth2PluginConfig::parse(config)?;
        Ok(Self {
            resolver,
            exchanger,
            auth_method,
            cache,
            cache_ttl: std::time::Duration::from_secs(cache_config.ttl_secs),
            identity,
            config,
        })
    }

    /// The cache key this binding uses (`ADR`-0008, cache key design).
    #[must_use]
    pub fn cache_key(&self) -> String {
        build_cache_key(
            self.identity.0,
            self.identity.1,
            self.auth_method,
            &self.config,
        )
    }

    /// The token cache this binding stores into, shared with its family.
    #[must_use]
    pub fn cache_arc(&self) -> Arc<MemoryCache<String, CachedToken>> {
        Arc::clone(&self.cache)
    }

    /// TTL a fetched token is cached for: the configured ceiling, shortened to
    /// the IdP-reported lifetime minus the safety margin.
    #[must_use]
    pub fn ttl_for(&self, expires_in: std::time::Duration) -> Option<std::time::Duration> {
        let lifetime =
            expires_in.saturating_sub(std::time::Duration::from_secs(EXPIRY_MARGIN_SECS));
        (lifetime.is_zero() || lifetime < self.cache_ttl)
            .then_some(lifetime)
            .filter(|ttl| !ttl.is_zero())
            .or((lifetime >= self.cache_ttl).then_some(self.cache_ttl))
    }

    /// Fetch and cache a token (`ADR`-0008, authentication flow).
    ///
    /// # Errors
    /// Returns [`DomainError::SecretNotFound`] for an unresolvable credential
    /// and [`DomainError::ServiceUnavailable`] when the `IdP` cannot be reached.
    pub async fn fetch(&self) -> Result<String, DomainError> {
        let key = self.cache_key();
        let (hit, _) = self.cache.get(&key);
        if let Some(entry) = hit
            && entry.key == key
        {
            return Ok(entry.token.expose().to_owned());
        }

        let tenant = self.identity.0;
        let subject = self.identity.1;
        let client_id = self
            .resolver
            .resolve(tenant, subject, &self.config.client_id_ref)
            .await?;
        let client_secret = self
            .resolver
            .resolve(tenant, subject, &self.config.client_secret_ref)
            .await?;
        let config = OAuthClientConfig {
            token_endpoint: self.config.token_endpoint.clone(),
            issuer_url: self.config.issuer_url.clone(),
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: self
                .config
                .scopes
                .split_whitespace()
                .map(String::from)
                .collect(),
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: std::time::Duration::default(),
            jitter_max: std::time::Duration::default(),
            min_refresh_period: std::time::Duration::default(),
            default_ttl: std::time::Duration::from_mins(5),
            http_config: None,
        };

        let fetched = self.exchanger.exchange(config).await.map_err(|error| {
            DomainError::ServiceUnavailable {
                detail: "the token endpoint rejected the client-credentials exchange".to_owned(),
                cause: Some(Box::new(std::io::Error::other(error.to_string()))),
            }
        })?;
        let token = fetched.bearer.expose().to_owned();
        if let Some(ttl) = self.ttl_for(fetched.expires_in) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: SecretString::new(token.clone()),
                },
                Some(ttl),
            );
        }
        Ok(token)
    }
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .field("identity", &self.identity)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn gts_id(&self) -> String {
        match self.auth_method {
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        }
        .to_owned()
    }

    async fn authenticate(&self, request: &mut ProxyContext) -> Result<AuthOutcome, DomainError> {
        let token = self.fetch().await?;
        request
            .headers
            .insert("authorization".to_owned(), format!("Bearer {token}"));
        Ok(AuthOutcome {
            subject: Some(request.subject.to_string()),
            forwarded_headers: std::collections::BTreeMap::from([(
                "authorization".to_owned(),
                format!("Bearer {token}"),
            )]),
        })
    }
}
