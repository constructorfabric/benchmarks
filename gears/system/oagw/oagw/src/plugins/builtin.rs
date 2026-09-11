//! The six built-in plugin implementations the built-in catalogue backs.
//!
//! Realizes `cpt-cf-oagw-dod-builtin-catalogue`: the four auth identifiers,
//! the one guard identifier, and the one transform identifier that a registry
//! backs at initialization, each backed by the implementation its identifier
//! names and nothing else. The six catalog-only identifiers of the same
//! catalogue have no implementation here — `basic` and `bearer` have no
//! backing `AuthPlugin` anywhere, `timeout` and `cors` are core data-plane
//! behaviour, and `logging` and `metrics` are core data-plane instrumentation.
//!
//! Every implementation is stateless in the credential sense: material enters
//! through [`crate::plugins::credential::resolve_credential`] and leaves in
//! the context's headers, and no failure value names a reference, a material,
//! or an endpoint.

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use serde_json::Value;
use toolkit_auth::{ClientAuthMethod, OAuthClientConfig, SecretString, TokenError, fetch_token};
use toolkit_security::SecurityContext;
use url::Url;

use std::collections::BTreeMap;

use crate::domain::context::{AuthContext, RequestContext, ResponseContext};
use crate::domain::error::ErrorContext;
use crate::domain::plugin_contract::{
    AuthPlugin, GuardDecision, GuardPlugin, PluginFailure, PluginPhase, TransformPlugin,
};
use crate::plugins::credential::{credential_key, resolve_credential};
use crate::plugins::token_cache::{TokenCache, cache_key};

/// The header the `apikey` variant injects when the configuration names none.
pub const DEFAULT_API_KEY_HEADER: &str = "x-api-key";

/// The header the `request_id` transform propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The code a required-headers rejection carries, in either phase (ADR 0009).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// The configuration key the `apikey` variant reads its reference from.
pub const KEY_CREDENTIAL_REF: &str = "credential_ref";
/// The configuration key the `apikey` variant reads its header name from.
pub const KEY_HEADER_NAME: &str = "header_name";
/// The configuration key of the OAuth2 client identifier reference.
pub const KEY_CLIENT_ID_REF: &str = "client_id_ref";
/// The configuration key of the OAuth2 client secret reference.
pub const KEY_CLIENT_SECRET_REF: &str = "client_secret_ref";
/// The configuration key of the direct token endpoint.
pub const KEY_TOKEN_ENDPOINT: &str = "token_endpoint";
/// The configuration key of the OIDC issuer the token endpoint is discovered
/// from.
pub const KEY_ISSUER_URL: &str = "issuer_url";
/// The configuration key of the space-separated scope list.
pub const KEY_SCOPES: &str = "scopes";
/// The configuration key of the request-phase header list.
pub const KEY_REQUIRED_REQUEST_HEADERS: &str = "required_request_headers";
/// The configuration key of the response-phase header list.
pub const KEY_REQUIRED_RESPONSE_HEADERS: &str = "required_response_headers";

/// Reads one string-valued key from a plugin configuration.
fn string_field<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str)
}

/// The auth method tag a Client Credentials variant carries in its cache key.
fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

/// The security context one request's credential resolution runs under.
///
/// The context is the projection of the `AuthContext` the data plane built:
/// the subject tenant it was authenticated under and the subject identifier it
/// carried, or the anonymous context when the request carried no subject. It
/// is what the credential store applies its own sharing policy to.
fn security_context(ctx: &AuthContext) -> Result<SecurityContext, PluginFailure> {
    SecurityContext::builder()
        .subject_id(ctx.subject_id().unwrap_or_default())
        .subject_tenant_id(ctx.tenant_id)
        .build()
        .map_err(|_| PluginFailure::Configuration {
            reason: String::from("the request carried no resolvable identity"),
        })
}

/// The auth plugin that performs no authentication.
///
/// The variant exists so an upstream that needs no credential can still carry
/// the one auth slot every upstream has, and so an `auth.type` naming it is
/// resolved rather than rejected.
pub struct NoopAuthPlugin;

#[async_trait::async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(phase, PluginPhase::Auth)
    }

    async fn authenticate(
        &self,
        _ctx: &mut AuthContext,
        _config: &Value,
    ) -> Result<(), PluginFailure> {
        Ok(())
    }
}

/// The auth plugin that injects an API key resolved from the credential store.
///
/// The configuration names the reference the key is held under and the header
/// it is injected as. The reference is validated for shape and resolved at
/// request time; the material is injected and never stored anywhere else.
pub struct ApiKeyAuthPlugin {
    store: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin over the store its references resolve through.
    #[must_use]
    pub fn new(store: Arc<dyn CredStoreClientV1>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(phase, PluginPhase::Auth)
    }

    async fn authenticate(&self, ctx: &mut AuthContext, config: &Value) -> Result<(), PluginFailure> {
        let reference =
            string_field(config, KEY_CREDENTIAL_REF).ok_or(PluginFailure::Configuration {
                reason: String::from("credential_ref is absent"),
            })?;
        credential_key(reference)?;
        let header = string_field(config, KEY_HEADER_NAME).unwrap_or(DEFAULT_API_KEY_HEADER);

        // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-shape
        let context = security_context(ctx)?;
        let material = resolve_credential(Arc::clone(&self.store), &context, reference).await?;
        // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-shape

        ctx.set_header(header, material.expose());
        Ok(())
    }
}

/// The Client Credentials auth plugin, in its `Form` and `Basic` variants.
///
/// One instance per variant. The instance owns the token cache it serves and
/// the credential store its two references resolve through; it spawns nothing
/// and holds nothing across requests but the cache.
pub struct OAuth2ClientCredAuthPlugin {
    store: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    tag: &'static str,
    cache: TokenCache,
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds one variant over the store, the client-auth method, and the
    /// cache the registry was constructed with.
    #[must_use]
    pub fn new(
        store: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache: TokenCache,
    ) -> Self {
        Self {
            store,
            auth_method,
            tag: auth_method_tag(auth_method),
            cache,
        }
    }
}

#[async_trait::async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(phase, PluginPhase::Auth)
    }

    // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-invoke
    // The entry the Data Plane's invocation of `authenticate()` reaches: this
    // method is the boundary the flow names, and every step below it is the
    // credential-resolution contract it exposes.
    async fn authenticate(&self, ctx: &mut AuthContext, config: &Value) -> Result<(), PluginFailure> {
        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-shape
        let client_id_ref = credential_field(config, KEY_CLIENT_ID_REF)?;
        let client_secret_ref = credential_field(config, KEY_CLIENT_SECRET_REF)?;
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-shape

        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-key
        let key = cache_key(ctx.tenant_id, ctx.subject_id(), self.tag, config);
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-key

        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-hit-if
        // A hit whose stored key equals the lookup key is served with no
        // credential-store call and no exchange call at all.
        if let Some(token) = self.cache.lookup(&key) {
            // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-hit-if
            // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-hit
            inject_bearer(ctx, token.expose());
            return Ok(());
            // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-hit
        }
        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-miss-else
        let context = security_context(ctx)?;
        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-resolve
        // The two references resolve through the credential routine in stored
        // order, and the first one the store cannot answer fails the plugin.
        let client_id =
            resolve_credential(Arc::clone(&self.store), &context, client_id_ref).await?;
        let client_secret =
            resolve_credential(Arc::clone(&self.store), &context, client_secret_ref).await?;
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-resolve
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-miss-else

        let client_config =
            client_credentials_config(config, self.auth_method, client_id, client_secret)?;

        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch-try
        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch
        let exchanged = fetch_token(client_config).await;
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch-try

        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch-catch
        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch-catch-handle
        let fetched = exchanged.map_err(exchange_failure)?;
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch-catch-handle
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-fetch-catch

        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-ttl
        // The entry's lifetime is the ceiling or the reported lifetime less the
        // margin, whichever is shorter; the cache refuses the entry itself when
        // the reported lifetime is at or below the margin.
        let bearer = fetched.bearer;
        let lifetime = fetched.expires_in;
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-ttl

        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-store-if
        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-store
        self.cache.store(&key, bearer.clone(), lifetime);
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-store
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-store-if

        // @cpt-begin:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-return
        inject_bearer(ctx, bearer.expose());
        Ok(())
        // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-return
    }
    // @cpt-end:cpt-cf-oagw-flow-oauth2-token-cache:p1:inst-tc-invoke
}

/// Reads one credential reference out of the plugin configuration, refusing a
/// configuration that names none.
fn credential_field<'a>(config: &'a Value, key: &str) -> Result<&'a str, PluginFailure> {
    let reference = string_field(config, key).ok_or(PluginFailure::Configuration {
        reason: format!("{key} is absent"),
    })?;
    credential_key(reference)?;
    Ok(reference)
}

/// Builds the client-credentials configuration the one-shot exchange runs
/// with.
fn client_credentials_config(
    config: &Value,
    auth_method: ClientAuthMethod,
    client_id: SecretString,
    client_secret: SecretString,
) -> Result<OAuthClientConfig, PluginFailure> {
    let token_endpoint = string_field(config, KEY_TOKEN_ENDPOINT);
    let issuer_url = string_field(config, KEY_ISSUER_URL);
    if token_endpoint.is_none() && issuer_url.is_none() {
        return Err(PluginFailure::Configuration {
            reason: String::from("one of token_endpoint or issuer_url is required"),
        });
    }
    let parse = |value: &str, key: &str| -> Result<Url, PluginFailure> {
        Url::parse(value).map_err(|_| PluginFailure::Configuration {
            reason: format!("{key} is not a URL"),
        })
    };
    Ok(OAuthClientConfig {
        token_endpoint: token_endpoint
            .map(|value| parse(value, KEY_TOKEN_ENDPOINT))
            .transpose()?,
        issuer_url: issuer_url
            .map(|value| parse(value, KEY_ISSUER_URL))
            .transpose()?,
        client_id: client_id.expose().to_owned(),
        client_secret,
        scopes: scopes(config),
        auth_method,
        ..OAuthClientConfig::default()
    })
}

/// The scope list the configuration asks for: space-separated, or an array.
fn scopes(config: &Value) -> Vec<String> {
    match config.get(KEY_SCOPES) {
        Some(Value::String(value)) => value.split_whitespace().map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Maps a failed exchange onto the typed failures the catalogue names.
///
/// The token endpoint's answer is not the credential store's decline: an
/// exchange that fails after it was sent is an availability failure of the
/// token path, and one that was refused before it was sent is a configuration
/// failure. The mapped value carries no endpoint URL and no credential
/// material.
fn exchange_failure(error: TokenError) -> PluginFailure {
    match error {
        TokenError::ConfigError(_) => PluginFailure::Configuration {
            reason: String::from("the token exchange configuration is invalid"),
        },
        _ => PluginFailure::Unavailable,
    }
}

/// Writes the bearer value into the context's authorization header.
fn inject_bearer(ctx: &mut AuthContext, bearer: &str) {
    ctx.set_header("authorization", format!("Bearer {bearer}"));
}

/// The guard plugin that rejects a request or an upstream response that omits
/// a configured header (ADR 0009).
///
/// Both phases are independent and fail open: absent or blank configuration is
/// a no-op in the phase that ran. Only presence is checked, case-insensitively,
/// and only the first missing header is reported.
pub struct RequiredHeadersGuardPlugin;

/// Reads the comma-separated header list one phase checks for.
fn required_headers(config: &Value, key: &str) -> Vec<String> {
    match string_field(config, key) {
        Some(value) => value
            .split(',')
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .filter(|name| !name.is_empty())
            .collect(),
        None => Vec::new(),
    }
}

/// The first configured header the context does not carry, if any.
///
/// Both contexts spell their names lowercased, and the configured list is
/// lowercased when it is read, so the comparison is the case-insensitive one
/// ADR 0009 asks for.
fn first_missing(required: &[String], headers: &BTreeMap<String, String>) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.contains_key(*name))
        .cloned()
}

impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(phase, PluginPhase::GuardRequest | PluginPhase::GuardResponse)
    }

    fn guard_request(&self, ctx: &RequestContext, config: &Value) -> GuardDecision {
        let required = required_headers(config, KEY_REQUIRED_REQUEST_HEADERS);
        match first_missing(&required, &ctx.headers) {
            None => GuardDecision::Allow,
            Some(missing) => GuardDecision::reject(REQUIRED_HEADER_MISSING, &missing),
        }
    }

    fn guard_response(&self, ctx: &ResponseContext, config: &Value) -> GuardDecision {
        let required = required_headers(config, KEY_REQUIRED_RESPONSE_HEADERS);
        match first_missing(&required, &ctx.headers) {
            None => GuardDecision::Allow,
            Some(missing) => GuardDecision::reject(REQUIRED_HEADER_MISSING, &missing),
        }
    }
}

/// The transform plugin that propagates the request identifier.
///
/// A request that arrived with one keeps it; a request that arrived without one
/// is given a fresh identifier, which the response carries as well. No
/// configuration is required.
pub struct RequestIdTransformPlugin;

impl TransformPlugin for RequestIdTransformPlugin {
    fn declares(&self, phase: PluginPhase) -> bool {
        matches!(
            phase,
            PluginPhase::TransformRequest
                | PluginPhase::TransformResponse
                | PluginPhase::TransformError
        )
    }

    fn transform_request(&self, ctx: &mut RequestContext, _config: &Value) {
        if ctx.header(REQUEST_ID_HEADER).is_none() {
            ctx.set_header(REQUEST_ID_HEADER, uuid::Uuid::new_v4().to_string());
        }
    }

    fn transform_response(&self, ctx: &mut ResponseContext, _config: &Value) {
        if ctx.header(REQUEST_ID_HEADER).is_none() {
            ctx.set_header(REQUEST_ID_HEADER, uuid::Uuid::new_v4().to_string());
        }
    }

    fn transform_error(&self, ctx: &mut ErrorContext, _config: &Value) {
        // The error context carries no header set: its correlation slot is the
        // trace identifier, so the identifier propagates there.
        if ctx.trace_id.is_none() {
            ctx.trace_id = Some(uuid::Uuid::new_v4().to_string());
        }
    }
}
