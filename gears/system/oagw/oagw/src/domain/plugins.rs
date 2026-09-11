//! The plugin system on the proxy path (ADR-0002) and the built-in plugins.
//!
//! # Execution order (ADR-0002 "Execution Order")
//!
//! ```text
//! incoming request
//!   → auth plugin      (credential injection)
//!   → guard plugins    (validation, may reject)
//!   → transform plugins (modify the request)
//!   → the upstream call
//!   → guard plugins    (validate the response)
//!   → transform plugins (modify the response, or the error)
//!   → return to the client
//! ```
//!
//! Upstream plugins run before route plugins (`[U1, U2] + [R1, R2]` is
//! `[U1, U2, R1, R2]`). [`PluginPipeline`] is the only place that order is
//! written down, and [`DataPlaneService::proxy`] is the only caller.
//!
//! The error phase is reached from three places only: a request phase that
//! rejected, a response phase that rejected, and an upstream call that failed.
//! A request refused *before* a chain was bound to it (a CORS 403, a 429, an
//! unresolvable plugin reference) and a tunneled upgrade never reach it, so a
//! plugin cannot annotate a problem document its own chain did not produce.
//!
//! # What a plugin sees
//!
//! The plugin traits are intentionally narrow: a plugin gets the request
//! **headers**, the identity the proxy call was made under and the
//! configuration of its own binding, and nothing else. The request body and the
//! URL are not handed over, so no plugin in this slice can read or rewrite them
//! (the ADR's Starlark plugins are a later slice). Two consequences worth
//! stating: an API key can be injected into a *header* but not into a *query
//! parameter* (`PRD §5.2` names both — the query form is deferred with the body
//! access it would need), and a transform plugin cannot rewrite the path.
//!
//! # Credential resolution (DESIGN "Secret Access Control")
//!
//! Auth configuration names secrets by reference (`cred://…`). Resolving a
//! reference is the job of the `cred_store` gear, which [`oagw`] reaches only
//! through the [`credstore_sdk`] client *type* — a deployment wires an instance,
//! and the data plane is built by [`DataPlaneService::new`] with none. Rather
//! than pretending a lookup happened, the auth plugins depend on the
//! [`CredentialSource`] seam: [`CredStoreCredentialSource`] is the real adapter
//! over `cred_store` for a deployment that wires it, and without one every
//! `cred://` reference is reported as unresolvable — a 401 problem document that
//! names the reference, never a synthetic credential.
//!
//! # Deferred
//!
//! * custom (Starlark) plugins — the registries accept external Rust
//!   implementations of the three traits, which is what ADR-0002 asks for;
//!   the Starlark interpreter is not a dependency of this crate.
//! * the catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`,
//!   `logging`, `metrics`) are constants below and resolve to nothing: DESIGN
//!   lists them as reserved, with the behaviour they name implemented as core
//!   data-plane logic (CORS in [`crate::domain::cors`], the deadline in
//!   [`DataPlaneService`], metrics and logging in the host).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_security::SecurityContext;
use url::Url;
use uuid::Uuid;

use crate::config::TokenCacheConfig;
use crate::domain::model::{
    AuthConfig, BoundPluginBinding, PluginBinding, PluginChain as PluginChainDocument, PluginKind,
};
use crate::domain::resolution::{DialPolicy, enforce_url_policy};
use crate::error::{OagwError, REQUIRED_HEADER_MISSING};

/// `AuthPlugin` that injects nothing (DESIGN "Built-in Plugins").
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `AuthPlugin` that injects an API key from a credential reference.
pub const API_KEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `AuthPlugin` for the `OAuth2` client-credentials flow, `Form` client auth.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// `AuthPlugin` for the `OAuth2` client-credentials flow, `Basic` client auth.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// `GuardPlugin` that enforces required request and response headers (ADR-0009).
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// `TransformPlugin` that injects and propagates `X-Request-ID`.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Auth identifiers DESIGN catalogs without a backing implementation: binding
/// one resolves to nothing and the request is refused with
/// `plugin.not_found.v1`.
pub const CATALOG_AUTH_PLUGIN_IDS: [&str; 2] = [
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
];

/// Guard identifiers DESIGN catalogs without a backing implementation: the
/// behaviour they name is core data-plane logic.
pub const CATALOG_GUARD_PLUGIN_IDS: [&str; 2] = [
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
];

/// Transform identifiers DESIGN catalogs without a backing implementation: the
/// behaviour they name is core data-plane instrumentation.
pub const CATALOG_TRANSFORM_PLUGIN_IDS: [&str; 2] = [
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
];

/// The request header the `request_id.v1` transform works on.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// The default header an API key is injected into.
const API_KEY_HEADER: &str = "x-api-key";

/// How long before its reported expiry a cached token is retired (ADR-0008
/// "Gear-Level Configuration": the safety margin of the `min(config_ttl,
/// expires_in − 30s)` rule).
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(30);

// ── Contexts ─────────────────────────────────────────────────────────────────

/// The request a plugin runs against.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Request headers, as they stand. Auth and transform plugins may change
    /// them; a guard may only read them.
    pub headers: HeaderMap,
    /// Method of the proxied request.
    pub method: Method,
    /// Tenant the proxy call was made for.
    pub tenant_id: Uuid,
    /// Authenticated caller, when the transport identified one.
    pub subject_id: Option<Uuid>,
    /// Upstream the request resolves to.
    pub upstream_id: Uuid,
    /// Route the request matched.
    pub route_id: Uuid,
    /// Security context of the caller, when the transport provided one: a
    /// credential source needs it to resolve a tenant-scoped reference.
    pub security: Option<SecurityContext>,
    /// Configuration of the binding the plugin is invoked for
    /// (`plugins.items[].config`), absent when the binding carries none.
    pub config: Option<serde_json::Value>,
}

impl RequestContext {
    /// The security context of the caller, or the 401 a plugin that cannot act
    /// without one returns.
    ///
    /// # Errors
    ///
    /// Returns the 401 [`OagwError`] when the transport provided no security
    /// context, which is the only honest answer for a credential the caller
    /// cannot be tied to.
    pub fn security(&self) -> Result<&SecurityContext, OagwError> {
        self.security.as_ref().ok_or_else(|| {
            OagwError::authentication_failed(
                "the request carries no security context, so no credential can be resolved for it",
            )
        })
    }
}

/// The upstream response a guard or a transform plugin runs against.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Status the upstream returned.
    pub status: http::StatusCode,
    /// Response headers, as they stand.
    pub headers: HeaderMap,
    /// `X-Request-ID` the request carried or was given, so a response plugin can
    /// echo it back without re-deriving it.
    pub request_id: Option<String>,
    /// Configuration of the binding the plugin is invoked for.
    pub config: Option<serde_json::Value>,
}

/// The gateway error a transform plugin may annotate (`transform_error`).
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// The error the client is about to see.
    pub error: OagwError,
    /// Configuration of the binding the plugin is invoked for.
    pub config: Option<serde_json::Value>,
}

/// What a guard decided.
#[must_use]
#[derive(Debug, Clone)]
pub enum GuardDecision {
    /// The request (or the response) may proceed.
    Allow,
    /// It is refused, with the problem document the client gets.
    Reject(OagwError),
}

impl GuardDecision {
    /// Whether the guard allowed.
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Flatten the decision into the error the caller returns, so a rejection
    /// and an internal plugin failure travel the same path.
    ///
    /// # Errors
    ///
    /// Returns the [`OagwError`] of a [`GuardDecision::Reject`].
    pub fn into_result(self) -> Result<(), OagwError> {
        match self {
            Self::Allow => Ok(()),
            Self::Reject(error) => Err(error),
        }
    }
}

// ── Traits (ADR-0002 "Plugin Traits") ────────────────────────────────────────

/// Credential injection: one plugin per upstream, run before the guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short name of the plugin (`apikey`), as DESIGN's built-in table spells it.
    fn id(&self) -> &'static str;

    /// GTS identifier the plugin is resolved by in a `plugins.items[]` chain.
    fn plugin_type(&self) -> &str;

    /// Inject the credentials of the upstream into the request.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`OagwError`] when the credentials cannot be produced:
    /// a 401 for a reference that does not resolve, 500 for a configuration the
    /// plugin cannot act on.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
}

/// Validation and policy enforcement: may reject, run after the auth plugin.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short name of the plugin (`required_headers`).
    fn id(&self) -> &'static str;

    /// GTS identifier the plugin is resolved by in a `plugins.items[]` chain.
    fn plugin_type(&self) -> &str;

    /// Validate the request before it is sent upstream.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`OagwError`] when the guard itself fails, never for a
    /// rejected request — that is a [`GuardDecision::Reject`].
    async fn guard_request(&self, ctx: &mut RequestContext) -> Result<GuardDecision, OagwError>;

    /// Validate the upstream response before it is returned.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`OagwError`] when the guard itself fails.
    async fn guard_response(&self, ctx: &mut ResponseContext) -> Result<GuardDecision, OagwError>;
}

/// Request, response and error mutation: run around the upstream call.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short name of the plugin (`request_id`).
    fn id(&self) -> &'static str;

    /// GTS identifier the plugin is resolved by in a `plugins.items[]` chain.
    fn plugin_type(&self) -> &str;

    /// Modify the request before it is sent upstream.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`OagwError`] when the transformation cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;

    /// Modify the response before it is returned.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`OagwError`] when the transformation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError>;

    /// Annotate an error before it is returned.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`OagwError`] when the annotation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError>;
}

// ── Credential seam ──────────────────────────────────────────────────────────

/// Where an auth plugin obtains the value a `cred://` reference names.
///
/// ADR-0008 resolves every reference through the `cred_store` gear at request
/// time; the SDK type is a dependency of this crate but no instance is wired
/// into the data plane it builds, so the seam is what the plugins depend on and
/// [`CredStoreCredentialSource`] is what a deployment wires.
#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Resolve `reference` to its value.
    ///
    /// `Ok(None)` means the reference exists in no store the caller can see —
    /// the single not-found surface `cred_store` reports to prevent
    /// enumeration.
    ///
    /// # Errors
    ///
    /// Returns a 401 [`OagwError`] when the reference is malformed or the store
    /// cannot be reached.
    async fn resolve(
        &self,
        security: &SecurityContext,
        reference: &str,
    ) -> Result<Option<SecretString>, OagwError>;
}

/// The credential source backed by the `cred_store` gear.
pub struct CredStoreCredentialSource {
    client: Arc<dyn CredStoreClientV1>,
}

impl CredStoreCredentialSource {
    /// Wrap a `cred_store` client.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl CredentialSource for CredStoreCredentialSource {
    async fn resolve(
        &self,
        security: &SecurityContext,
        reference: &str,
    ) -> Result<Option<SecretString>, OagwError> {
        // The references documents spell (`cred://partner-openai-key`) carry a
        // scheme `SecretRef` does not accept, so the scheme is the consumer's
        // spelling and the bare key is what the store is asked for.
        let key = reference.strip_prefix("cred://").unwrap_or(reference);
        let key = SecretRef::new(key).map_err(|error| {
            OagwError::authentication_failed(format!(
                "the credential reference `{reference}` is not a valid secret key: {error}"
            ))
        })?;
        match self.client.get(security, &key).await {
            Ok(Some(secret)) => Ok(Some(SecretString::new(
                String::from_utf8_lossy(secret.value.as_bytes()).into_owned(),
            ))),
            Ok(None) => Ok(None),
            Err(error) => Err(OagwError::authentication_failed(format!(
                "the credential `{reference}` could not be resolved: {error}"
            ))),
        }
    }
}

// ── Built-in auth plugins ────────────────────────────────────────────────────

/// `noop.v1`: the upstream needs no credentials.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        Ok(())
    }
}

/// `apikey.v1`: inject an API key read from a credential reference into a
/// request header.
pub struct ApiKeyAuthPlugin {
    credentials: Option<Arc<dyn CredentialSource>>,
}

impl ApiKeyAuthPlugin {
    /// A plugin that resolves its reference through `credentials`.
    #[must_use]
    pub fn new(credentials: Option<Arc<dyn CredentialSource>>) -> Self {
        Self { credentials }
    }

    /// The binding configuration of one request.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`OagwError`] when the binding carries no usable
    /// configuration.
    fn binding(&self, ctx: &RequestContext) -> Result<ApiKeyBinding, OagwError> {
        let document = ctx.config.as_ref().ok_or_else(|| {
            OagwError::internal(format!(
                "the `{}` binding carries no configuration: `secret_ref` is required",
                self.id()
            ))
        })?;
        let reference = document
            .get("secret_ref")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                OagwError::internal(format!(
                    "the `{}` binding configuration carries no `secret_ref`",
                    self.id()
                ))
            })?;
        let header = match document
            .get("header_name")
            .and_then(serde_json::Value::as_str)
        {
            Some(name) => HeaderName::from_lowercase(name.to_ascii_lowercase().as_bytes())
                .map_err(|_| {
                    OagwError::internal(format!(
                        "the `{}` binding configuration names the invalid header `{name}`",
                        self.id()
                    ))
                })?,
            None => HeaderName::from_static(API_KEY_HEADER),
        };
        Ok(ApiKeyBinding {
            reference: reference.to_owned(),
            header,
        })
    }
}

/// The configuration of one binding of the API-key plugin.
struct ApiKeyBinding {
    /// The `cred://` reference the key is read from.
    reference: String,
    /// The header the key is injected into.
    header: HeaderName,
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        API_KEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let binding = self.binding(ctx)?;
        let source = self.credentials.as_ref().ok_or_else(|| {
            OagwError::authentication_failed(format!(
                "the data plane has no credential source wired, so `{}` cannot be resolved",
                binding.reference
            ))
        })?;
        let security = ctx.security()?;
        let Some(secret) = source.resolve(security, &binding.reference).await? else {
            return Err(OagwError::authentication_failed(format!(
                "the credential `{}` does not resolve for this tenant",
                binding.reference
            )));
        };
        let value = HeaderValue::from_str(secret.expose()).map_err(|_| {
            OagwError::authentication_failed(
                "the resolved credential is not a valid HTTP header value",
            )
        })?;
        ctx.headers.insert(binding.header, value);
        Ok(())
    }
}

/// A cached `OAuth2` token, carrying the key it was stored under (ADR-0008
/// "Hash-Collision Safety"): the cache hashes its keys, so a hit is only used
/// when the key round-trips.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// The configuration of a binding of the `OAuth2` client-credentials plugin
/// (ADR-0008 "Plugin Config").
struct OAuth2Binding {
    token_endpoint: Option<Url>,
    issuer_url: Option<Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl OAuth2Binding {
    /// Parse a binding configuration.
    ///
    /// # Errors
    ///
    /// Returns a 500 [`OagwError`] naming the first missing or ambiguous key.
    fn parse(config: Option<&serde_json::Value>, plugin: &str) -> Result<Self, OagwError> {
        let named = |name: &str| {
            config
                .and_then(|document| document.get(name))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let (token_endpoint, issuer_url) = match (named("token_endpoint"), named("issuer_url")) {
            (Some(_), Some(_)) => {
                return Err(OagwError::internal(format!(
                    "the `{plugin}` binding configures both `token_endpoint` and `issuer_url`"
                )));
            }
            (Some(endpoint), None) => (Some(parse_url(&endpoint, plugin)?), None),
            (None, Some(issuer)) => (None, Some(parse_url(&issuer, plugin)?)),
            (None, None) => {
                return Err(OagwError::internal(format!(
                    "the `{plugin}` binding configuration carries neither `token_endpoint` nor `issuer_url`"
                )));
            }
        };
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref: required_str(config, "client_id_ref", plugin)?,
            client_secret_ref: required_str(config, "client_secret_ref", plugin)?,
            scopes: named("scopes").map_or_else(Vec::new, |scopes| {
                scopes.split(' ').map(str::to_owned).collect()
            }),
        })
    }
}

/// A string a binding configuration must carry under `name`.
fn required_str(
    config: Option<&serde_json::Value>,
    name: &str,
    plugin: &str,
) -> Result<String, OagwError> {
    config
        .and_then(|document| document.get(name))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            OagwError::internal(format!(
                "the `{plugin}` binding configuration carries no `{name}`"
            ))
        })
}

/// Parse an absolute URL out of a binding configuration.
///
/// The URL is only parsed here; whether the gateway may actually dial it is
/// decided at fetch time by [`check_token_endpoint`].
fn parse_url(value: &str, plugin: &str) -> Result<Url, OagwError> {
    Url::parse(value).map_err(|error| {
        OagwError::internal(format!(
            "the `{plugin}` binding configuration names an unusable URL `{value}`: {error}"
        ))
    })
}

/// The two `OAuth2` client-credentials plugins (`Form` and `Basic`), sharing the
/// in-process token cache of ADR-0008.
pub struct OAuth2ClientCredAuthPlugin {
    plugin_id: &'static str,
    auth_method: ClientAuthMethod,
    credentials: Option<Arc<dyn CredentialSource>>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
    /// Outbound dial policy the token endpoint is held to — the same SSRF and
    /// plaintext rules the upstream endpoints are held to.
    dial_policy: DialPolicy,
}

impl OAuth2ClientCredAuthPlugin {
    /// The `Form` variant (`oauth2_client_cred.v1`).
    #[must_use]
    pub fn form(
        credentials: Option<Arc<dyn CredentialSource>>,
        token_cache: TokenCacheConfig,
        dial_policy: DialPolicy,
    ) -> Self {
        Self::new(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            ClientAuthMethod::Form,
            credentials,
            token_cache,
            dial_policy,
        )
    }

    /// The `Basic` variant (`oauth2_client_cred_basic.v1`).
    #[must_use]
    pub fn basic(
        credentials: Option<Arc<dyn CredentialSource>>,
        token_cache: TokenCacheConfig,
        dial_policy: DialPolicy,
    ) -> Self {
        Self::new(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            ClientAuthMethod::Basic,
            credentials,
            token_cache,
            dial_policy,
        )
    }

    fn new(
        plugin_id: &'static str,
        auth_method: ClientAuthMethod,
        credentials: Option<Arc<dyn CredentialSource>>,
        token_cache: TokenCacheConfig,
        dial_policy: DialPolicy,
    ) -> Self {
        Self {
            plugin_id,
            auth_method,
            credentials,
            cache: MemoryCache::new(token_cache.capacity),
            cache_ttl: Duration::from_secs(token_cache.ttl_secs),
            dial_policy,
        }
    }

    /// The cache key of one binding (ADR-0008 "Cache Key Design"): tenant,
    /// subject, client-auth method and a deterministic hash of the binding
    /// configuration, so two upstreams with different scopes never share a
    /// token.
    fn cache_key(&self, ctx: &RequestContext, binding: &OAuth2Binding) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.tenant_id,
            ctx.subject_id
                .map_or_else(String::new, |subject| subject.to_string()),
            self.auth_method_name(),
            config_hash(binding),
        )
    }

    /// The tag the cache key carries for the client-auth method.
    const fn auth_method_name(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "basic",
            ClientAuthMethod::Form => "form",
        }
    }

    /// The cached token for `key`, when it is still live *and* the key round-trips.
    fn cached(&self, key: &str) -> Option<SecretString> {
        let (cached, status) = self.cache.get(key);
        let token = cached.filter(|cached| cached.key == key);
        match (token, status.is_hit()) {
            (Some(cached), true) => Some(cached.token),
            _ => None,
        }
    }

    /// Resolve both credentials of a binding through the credential source.
    async fn resolve_credentials(
        &self,
        ctx: &RequestContext,
        binding: &OAuth2Binding,
    ) -> Result<(String, String), OagwError> {
        let source = self.credentials.as_ref().ok_or_else(|| {
            OagwError::authentication_failed(format!(
                "the data plane has no credential source wired, so `{}` cannot resolve `{}`",
                self.plugin_type(),
                binding.client_id_ref
            ))
        })?;
        let security = ctx.security()?;
        let client_id = resolved(source, security, &binding.client_id_ref).await?;
        let client_secret = resolved(source, security, &binding.client_secret_ref).await?;
        Ok((client_id, client_secret))
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        "oauth2_client_cred"
    }

    fn plugin_type(&self) -> &str {
        self.plugin_id
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let binding = OAuth2Binding::parse(ctx.config.as_ref(), self.id())?;
        let key = self.cache_key(ctx, &binding);
        if let Some(token) = self.cached(&key) {
            inject_bearer(ctx, &token)?;
            return Ok(());
        }
        let (client_id, client_secret) = self.resolve_credentials(ctx, &binding).await?;
        check_token_endpoint(self.dial_policy, &binding)?;
        let fetched = fetch_token(OAuthClientConfig {
            token_endpoint: binding.token_endpoint,
            issuer_url: binding.issuer_url,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: binding.scopes,
            auth_method: self.auth_method,
            ..OAuthClientConfig::default()
        })
        .await
        .map_err(|error| {
            OagwError::authentication_failed(format!(
                "the token endpoint of `{}` refused the exchange: {error}",
                self.plugin_type()
            ))
        })?;
        // `min(config_ttl, expires_in − 30s)`: a token the IdP retires within
        // the safety margin is never cached, so the next request re-asks.
        let ttl = fetched
            .expires_in
            .checked_sub(TOKEN_EXPIRY_MARGIN)
            .map(|remaining| remaining.min(self.cache_ttl));
        if let Some(ttl) = ttl {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }
        inject_bearer(ctx, &fetched.bearer)
    }
}

/// Resolve one credential reference, turning a missing secret into the 401 the
/// caller sees.
async fn resolved(
    source: &Arc<dyn CredentialSource>,
    security: &SecurityContext,
    reference: &str,
) -> Result<String, OagwError> {
    let secret = source.resolve(security, reference).await?;
    secret.map_or_else(
        || {
            Err(OagwError::authentication_failed(format!(
                "the credential `{reference}` does not resolve for this tenant"
            )))
        },
        |secret| Ok(secret.expose().to_owned()),
    )
}

/// Inject `Authorization: Bearer <token>` into the request headers.
fn inject_bearer(ctx: &mut RequestContext, token: &SecretString) -> Result<(), OagwError> {
    let value = format!("Bearer {}", token.expose());
    let value = HeaderValue::from_str(&value).map_err(|_| {
        OagwError::authentication_failed("the access token is not a valid HTTP header value")
    })?;
    ctx.headers.insert(http::header::AUTHORIZATION, value);
    Ok(())
}

/// Hold the endpoint a binding names to the same outbound policy as an
/// upstream endpoint.
///
/// A credential exchange is the most valuable request this gear ever makes, so
/// the endpoint it dials is *not* exempt from the SSRF and plaintext rules an
/// upstream `server` member is held to: a `token_endpoint` pointed at a
/// loopback address under an enabled policy is refused, and so is a plaintext
/// one while `gears.oagw.config.allow_http_upstream` is unset.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] for a restricted host and a 500 for a gated
/// scheme — the binding is a stored misconfiguration, not a caller mistake.
fn check_token_endpoint(policy: DialPolicy, binding: &OAuth2Binding) -> Result<(), OagwError> {
    let url = binding
        .token_endpoint
        .as_ref()
        .or(binding.issuer_url.as_ref())
        .ok_or_else(|| {
            OagwError::internal(
                "the `oauth2_client_cred` binding configuration carries neither `token_endpoint` \
                 nor `issuer_url`",
            )
        })?;
    enforce_url_policy(policy, url).map_err(|error| match error.status_code() {
        // A stored binding naming a dial-forbidden endpoint is a configuration
        // mistake, not the caller's: the 400 the policy produces for the
        // endpoint becomes the 500 a stored misconfiguration carries.
        400 => OagwError::internal(error.detail().to_owned()),
        _ => error,
    })
}

/// A deterministic hash of a binding configuration, keys sorted (ADR-0008
/// "Cache Key Design": different configs must not share a cache entry).
fn config_hash(binding: &OAuth2Binding) -> u64 {
    let document = serde_json::json!({
        "token_endpoint": binding.token_endpoint.as_ref().map(Url::as_str),
        "issuer_url": binding.issuer_url.as_ref().map(Url::as_str),
        "client_id_ref": binding.client_id_ref,
        "client_secret_ref": binding.client_secret_ref,
        "scopes": binding.scopes,
    });
    let mut hasher = DefaultHasher::new();
    document.to_string().hash(&mut hasher);
    hasher.finish()
}

// ── Built-in guard plugin ────────────────────────────────────────────────────

/// `required_headers.v1`: reject a request (400) or a response (502) that is
/// missing a configured header (ADR-0009).
///
/// Unconfigured is the same as satisfied: an absent or blank config key is a
/// no-op phase, the fail-open the ADR specifies.
pub struct RequiredHeadersGuardPlugin;

/// The comma-separated header names a binding config names under `key`.
fn required_headers(config: Option<&serde_json::Value>, key: &str) -> Option<Vec<String>> {
    let raw = config?.get(key)?.as_str()?;
    let names: Vec<String> = raw
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    (!names.is_empty()).then_some(names)
}

/// The problem document of a missing header: 400 in the request phase, 502 in
/// the response phase, both carrying the `REQUIRED_HEADER_MISSING` code of
/// ADR-0009 as their `reason`.
fn missing_header(header: &str, phase: Phase) -> OagwError {
    match phase {
        Phase::Request => {
            OagwError::validation(format!("the required request header `{header}` is missing"))
        }
        Phase::Response => OagwError::protocol_error(format!(
            "the upstream response is missing the required header `{header}`"
        )),
    }
    .with_reason(REQUIRED_HEADER_MISSING)
    .with_invalid_value(header.to_owned())
}

/// The phase a guard runs in, which decides the status of its rejection.
#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Before the request is sent upstream: a 400.
    Request,
    /// Before the response is returned: a 502.
    Response,
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        let Some(required) = required_headers(ctx.config.as_ref(), "required_request_headers")
        else {
            return Ok(GuardDecision::Allow);
        };
        let missing = required
            .iter()
            .find(|name| !ctx.headers.contains_key(name.as_str()));
        Ok(missing.map_or(GuardDecision::Allow, |header| {
            GuardDecision::Reject(missing_header(header, Phase::Request))
        }))
    }

    async fn guard_response(&self, ctx: &mut ResponseContext) -> Result<GuardDecision, OagwError> {
        let Some(required) = required_headers(ctx.config.as_ref(), "required_response_headers")
        else {
            return Ok(GuardDecision::Allow);
        };
        let missing = required
            .iter()
            .find(|name| !ctx.headers.contains_key(name.as_str()));
        Ok(missing.map_or(GuardDecision::Allow, |header| {
            GuardDecision::Reject(missing_header(header, Phase::Response))
        }))
    }
}

// ── Built-in transform plugin ────────────────────────────────────────────────

/// `request_id.v1`: give every proxied request an `X-Request-ID` and echo it on
/// the response, propagating one the caller supplied.
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        if !ctx.headers.contains_key(REQUEST_ID_HEADER) {
            let value = HeaderValue::from_str(&Uuid::new_v4().to_string())
                .map_err(|_| OagwError::internal("a generated request id is not a header value"))?;
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        let Some(request_id) = ctx.request_id.clone() else {
            return Ok(());
        };
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
        Ok(())
    }
}

// ── Registries (ADR-0002 "Plugin Loading") ───────────────────────────────────

/// The auth plugins the data plane can resolve.
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// The built-ins of DESIGN "Built-in Plugins", resolving their credentials
    /// through `config.credentials` and caching tokens as `config.token_cache`
    /// allows.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::with_builtins_for(&AuthPluginConfig::default())
    }

    /// The built-ins for a concrete gear-level configuration (ADR-0008
    /// "Registry Integration": the cache configuration is threaded through the
    /// registry into the plugin constructors).
    #[must_use]
    pub fn with_builtins_for(config: &AuthPluginConfig) -> Self {
        let credentials = config.credentials.clone();
        let token_cache = config.token_cache;
        let dial_policy = config.dial_policy;
        Self::with_plugins(vec![
            Arc::new(NoopAuthPlugin),
            Arc::new(ApiKeyAuthPlugin::new(credentials.clone())),
            Arc::new(OAuth2ClientCredAuthPlugin::form(
                credentials.clone(),
                token_cache,
                dial_policy,
            )),
            Arc::new(OAuth2ClientCredAuthPlugin::basic(
                credentials,
                token_cache,
                dial_policy,
            )),
        ])
    }

    /// A registry over the given plugins, built-ins included or not.
    #[must_use]
    pub fn with_plugins(plugins: Vec<Arc<dyn AuthPlugin>>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|plugin| (plugin.plugin_type().to_owned(), plugin))
                .collect(),
        }
    }

    /// Resolve a plugin reference, or `None` when no registered plugin carries it.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(reference).cloned()
    }
}

/// The guard plugins the data plane can resolve.
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// The built-ins of DESIGN "Built-in Plugins" (ADR-0009 "Registry
    /// Integration": `required_headers.v1` is the only entry).
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::with_plugins(vec![Arc::new(RequiredHeadersGuardPlugin)])
    }

    /// A registry over the given plugins.
    #[must_use]
    pub fn with_plugins(plugins: Vec<Arc<dyn GuardPlugin>>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|plugin| (plugin.plugin_type().to_owned(), plugin))
                .collect(),
        }
    }

    /// Resolve a plugin reference, or `None` when no registered plugin carries it.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(reference).cloned()
    }
}

/// The transform plugins the data plane can resolve.
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// The built-ins of DESIGN "Built-in Plugins".
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::with_plugins(vec![Arc::new(RequestIdTransformPlugin)])
    }

    /// A registry over the given plugins.
    #[must_use]
    pub fn with_plugins(plugins: Vec<Arc<dyn TransformPlugin>>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|plugin| (plugin.plugin_type().to_owned(), plugin))
                .collect(),
        }
    }

    /// Resolve a plugin reference, or `None` when no registered plugin carries it.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(reference).cloned()
    }
}

/// The three registries of one data plane.
pub struct PluginRegistries {
    /// Auth plugins.
    pub auth: AuthPluginRegistry,
    /// Guard plugins.
    pub guard: GuardPluginRegistry,
    /// Transform plugins.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// The built-in registries (ADR-0002 "Built-in Plugins").
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::with_builtins_for(&AuthPluginConfig::default())
    }

    /// The built-in registries for a concrete gear-level configuration.
    #[must_use]
    pub fn with_builtins_for(config: &AuthPluginConfig) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins_for(config),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }

    /// Registries over the given plugins, for a data plane that replaces the
    /// built-ins with its own (ADR-0002 "Plugin Loading": external plugins
    /// register through dependency injection).
    #[must_use]
    pub fn new(
        auth: AuthPluginRegistry,
        guard: GuardPluginRegistry,
        transform: TransformPluginRegistry,
    ) -> Self {
        Self {
            auth,
            guard,
            transform,
        }
    }
}

/// The gear-level configuration the auth plugins read (ADR-0008
/// "Gear-Level Configuration").
#[derive(Default)]
pub struct AuthPluginConfig {
    /// Where `cred://` references resolve. `None` keeps the data plane honest:
    /// every reference is reported as unresolvable rather than guessed.
    pub credentials: Option<Arc<dyn CredentialSource>>,
    /// The in-process token cache of the `OAuth2` plugins.
    pub token_cache: TokenCacheConfig,
    /// Outbound dial policy the `OAuth2` token endpoint is held to: the same
    /// SSRF and plaintext rules the upstream endpoints are held to.
    pub dial_policy: DialPolicy,
}

impl std::fmt::Debug for AuthPluginConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthPluginConfig")
            .field(
                "credentials",
                &self.credentials.as_ref().map_or("none", |_| "wired"),
            )
            .field("token_cache", &self.token_cache)
            .field("dial_policy", &self.dial_policy)
            .finish()
    }
}

// ── The pipeline ─────────────────────────────────────────────────────────────

/// One plugin of a chain, with the configuration of its binding.
struct BoundPlugin<P: ?Sized> {
    plugin: Arc<P>,
    config: Option<serde_json::Value>,
}

/// The plugin chain of one request, resolved from the upstream and route
/// documents before the request is processed.
///
/// Resolving once per request keeps the hot path free of map lookups and turns
/// an unresolvable reference into a rejection *before* the body is read.
pub struct PluginPipeline {
    auth: Vec<BoundPlugin<dyn AuthPlugin>>,
    guards: Vec<BoundPlugin<dyn GuardPlugin>>,
    transforms: Vec<BoundPlugin<dyn TransformPlugin>>,
}

impl std::fmt::Debug for PluginPipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The plugins are reported by count only: their identities are their
        // configuration's business, and no credential ever reaches a log.
        formatter
            .debug_struct("PluginPipeline")
            .field("auth", &self.auth.len())
            .field("guards", &self.guards.len())
            .field("transforms", &self.transforms.len())
            .finish()
    }
}

impl PluginPipeline {
    /// Resolve the chain of one request: the upstream chain first, then the
    /// route chain, in the order each document lists its bindings.
    ///
    /// # Errors
    ///
    /// Returns the 503 [`OagwError`] of the first binding no registry of the
    /// matching kind can resolve.
    pub fn resolve(
        registries: &PluginRegistries,
        upstream: Option<&PluginChainDocument>,
        route: Option<&PluginChainDocument>,
        upstream_auth: Option<&AuthConfig>,
    ) -> Result<Self, OagwError> {
        let mut pipeline = Self {
            auth: Vec::new(),
            guards: Vec::new(),
            transforms: Vec::new(),
        };
        // ADR-0008 binds the authentication plugin through `upstream.auth`, not
        // through the `plugins` chain, and it authenticates before anything else
        // the chain binds — so it is bound first, before ADR-0002's upstream
        // chain.
        if let Some(auth) = upstream_auth {
            let reference = auth.auth_type.as_deref().unwrap_or_default();
            if reference.is_empty() {
                return Err(OagwError::plugin_not_found(
                    "the upstream `auth` member names no plugin `type`",
                ));
            }
            pipeline.bind(
                registries,
                &PluginBinding::Bound(BoundPluginBinding {
                    plugin_ref: reference.to_owned(),
                    config: auth.config.clone(),
                }),
            )?;
        }
        // ADR-0002: upstream plugins execute before route plugins.
        let bindings = upstream
            .into_iter()
            .flat_map(|chain| chain.items.iter())
            .chain(route.into_iter().flat_map(|chain| chain.items.iter()));
        for binding in bindings {
            pipeline.bind(registries, binding)?;
        }
        Ok(pipeline)
    }

    /// Bind one binding to a plugin of the kind its GTS identifier names.
    fn bind(
        &mut self,
        registries: &PluginRegistries,
        binding: &PluginBinding,
    ) -> Result<(), OagwError> {
        let reference = binding.plugin_ref();
        let config = binding.config().cloned();
        let kind = reference_kind(reference).ok_or_else(|| {
            OagwError::plugin_not_found(format!(
                "plugin reference `{reference}` names no plugin kind"
            ))
        })?;
        match kind {
            PluginKind::Auth => {
                let plugin = registries
                    .auth
                    .resolve(reference)
                    .ok_or_else(|| unbound(reference))?;
                self.auth.push(BoundPlugin { plugin, config });
            }
            PluginKind::Guard => {
                let plugin = registries
                    .guard
                    .resolve(reference)
                    .ok_or_else(|| unbound(reference))?;
                self.guards.push(BoundPlugin { plugin, config });
            }
            PluginKind::Transform => {
                let plugin = registries
                    .transform
                    .resolve(reference)
                    .ok_or_else(|| unbound(reference))?;
                self.transforms.push(BoundPlugin { plugin, config });
            }
        }
        Ok(())
    }

    /// Whether the chain binds no plugin at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.auth.is_empty() && self.guards.is_empty() && self.transforms.is_empty()
    }

    /// Run the auth phase.
    ///
    /// # Errors
    ///
    /// Returns the error of the first plugin that could not authenticate.
    pub async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        for step in &self.auth {
            let previous = ctx.config.take();
            ctx.config.clone_from(&step.config);
            let outcome = step.plugin.authenticate(ctx).await;
            ctx.config = previous;
            outcome?;
        }
        Ok(())
    }

    /// Run the guard phase on the request.
    ///
    /// # Errors
    ///
    /// Returns the problem document of the first guard that rejected.
    pub async fn guard_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        for step in &self.guards {
            let previous = ctx.config.take();
            ctx.config.clone_from(&step.config);
            let outcome = step.plugin.guard_request(ctx).await;
            ctx.config = previous;
            let decision = outcome?;
            decision.into_result()?;
        }
        Ok(())
    }

    /// Run the guard phase on the response.
    ///
    /// # Errors
    ///
    /// Returns the problem document of the first guard that rejected.
    pub async fn guard_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        for step in &self.guards {
            let previous = ctx.config.take();
            ctx.config.clone_from(&step.config);
            let outcome = step.plugin.guard_response(ctx).await;
            ctx.config = previous;
            let decision = outcome?;
            decision.into_result()?;
        }
        Ok(())
    }

    /// Run the transform phase on the request.
    ///
    /// # Errors
    ///
    /// Returns the error of the first plugin that failed.
    pub async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        for step in &self.transforms {
            let previous = ctx.config.take();
            ctx.config.clone_from(&step.config);
            let outcome = step.plugin.transform_request(ctx).await;
            ctx.config = previous;
            outcome?;
        }
        Ok(())
    }

    /// Run the transform phase on the response.
    ///
    /// # Errors
    ///
    /// Returns the error of the first plugin that failed.
    pub async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        for step in &self.transforms {
            let previous = ctx.config.take();
            ctx.config.clone_from(&step.config);
            let outcome = step.plugin.transform_response(ctx).await;
            ctx.config = previous;
            outcome?;
        }
        Ok(())
    }

    /// Run the transform phase on an error.
    ///
    /// # Errors
    ///
    /// Returns the error of the first plugin that failed, and otherwise the
    /// (possibly annotated) error the client gets.
    pub async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError> {
        for step in &self.transforms {
            let previous = ctx.config.take();
            ctx.config.clone_from(&step.config);
            let outcome = step.plugin.transform_error(ctx).await;
            ctx.config = previous;
            outcome?;
        }
        Ok(())
    }
}

/// The 503 of a binding no registry resolves (DESIGN "Error Response Format").
fn unbound(reference: &str) -> OagwError {
    OagwError::plugin_not_found(format!(
        "plugin reference `{reference}` resolves to no plugin of this gateway"
    ))
}

/// The kind a plugin reference names, read off its GTS base type
/// (`gts.cf.core.oagw.{kind}_plugin.v1~`).
fn reference_kind(reference: &str) -> Option<PluginKind> {
    [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform]
        .into_iter()
        .find(|kind| reference.starts_with(kind.gts_base_type()))
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;
