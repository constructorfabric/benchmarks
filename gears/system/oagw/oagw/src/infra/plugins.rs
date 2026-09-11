//! Built-in plugin implementations.
//!
//! * Auth — `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`
//! * Guard — `required_headers` (the only guard identifier that is bindable)
//! * Transform — `request_id`
//!
//! `basic` / `bearer` auth and the `timeout` / `cors` guard and `logging` /
//! `metrics` transform identifiers are catalog entries only (see
//! [`crate::ids`]); they are registered by [`crate::domain::plugin::ControlPlane`]
//! as recognized-but-inert, so binding one cannot be mistaken for an unknown
//! plugin.

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};

use crate::credstore_client::{SharedCredentialStore, resolve};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, ProxyContext, TransformPlugin};
use crate::domain::query::OutboundQuery;
use crate::error::{DomainError, ErrorKind};
use crate::infra::token_cache::TokenCache;

/// `Authorization` — the header every token-bearing plugin writes.
pub const AUTHORIZATION: &str = "authorization";

/// Slack before expiry applied when caching a token (ADR 0008).
pub const TOKEN_TTL_SLACK_SECS: u64 = 30;

/// Reads a string field out of a plugin config object.
fn config_str<'a>(config: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(value) = config.get(*key).and_then(Value::as_str)
            && !value.trim().is_empty() {
                return Some(value.trim());
            }
    }
    None
}

/// Reads a string field, also accepting a single-element array, so a config
/// written for either shape keeps working.
fn config_str_or_list(config: &Value, key: &str) -> Option<String> {
    if let Some(value) = config_str(config, &[key]) {
        return Some(value.to_owned());
    }
    config.get(key).and_then(Value::as_array).map(|entries| {
        entries
            .iter()
            .filter_map(Value::as_str)
            .map(|entry| entry.trim().to_owned())
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// No authentication: requests are forwarded as the caller sent them.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        crate::ids::AUTH_PLUGIN_NOOP
    }

    async fn authenticate(
        &self,
        _ctx: &ProxyContext,
        _config: &Value,
        _headers: &mut HeaderMap,
        _query: &mut OutboundQuery,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

/// API-key injection into a header or a query parameter.
///
/// Accepted configuration keys (aliases are tolerated because the
/// specification does not pin the key names):
///
/// * `secret_ref` — the credential-store reference (required)
/// * `location` / `in` / `source` — `header` (default) or `query`
/// * `name` / `header` / `param` — the parameter name
///   (default `X-API-Key` for headers, `api_key` for queries)
/// * `prefix` / `scheme` — a value prefix such as `Bearer`
pub struct ApiKeyAuthPlugin {
    credential_store: SharedCredentialStore,
}

impl ApiKeyAuthPlugin {
    /// Build the plugin bound to `credential_store`.
    #[must_use]
    pub const fn new(credential_store: SharedCredentialStore) -> Self {
        Self { credential_store }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        crate::ids::AUTH_PLUGIN_APIKEY
    }

    async fn authenticate(
        &self,
        ctx: &ProxyContext,
        config: &Value,
        headers: &mut HeaderMap,
        query: &mut OutboundQuery,
    ) -> Result<(), DomainError> {
        let Some(secret_ref) = config_str(config, &["secret_ref", "secretRef", "credential_ref"])
        else {
            return Err(DomainError::new(
                ErrorKind::Validation,
                "apikey auth plugin requires a `secret_ref`",
            ));
        };
        let secret = resolve(&self.credential_store, &ctx.security, secret_ref).await?;
        let value = std::str::from_utf8(secret.as_bytes())
            .map_err(|_| DomainError::new(ErrorKind::SecretNotFound, "api key is not valid UTF-8"))?
            .trim()
            .to_owned();
        if value.is_empty() {
            return Err(DomainError::new(
                ErrorKind::SecretNotFound,
                format!("credential {secret_ref:?} resolved to an empty api key"),
            ));
        }

        let mut injected = String::new();
        if let Some(prefix) = config_str(config, &["prefix", "scheme", "value_prefix"]) {
            injected.push_str(prefix);
            injected.push(' ');
        }
        injected.push_str(&value);

        let location = config_str(config, &["location", "in", "source"]).unwrap_or("header");
        if location.eq_ignore_ascii_case("query") {
            let name = config_str(config, &["name", "param", "param_name", "key", "key_name"])
                .unwrap_or("api_key")
                .to_owned();
            query.set_query_param(&name, &injected);
            headers.remove(AUTHORIZATION);
            return Ok(());
        }

        let name = config_str(config, &["name", "header", "header_name"])
            .unwrap_or("X-API-Key")
            .to_ascii_lowercase();
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            DomainError::new(
                ErrorKind::Validation,
                format!("apikey plugin configuration names an invalid header {name:?}"),
            )
        })?;
        if let Ok(value) = HeaderValue::from_str(&injected) {
            headers.insert(name, value);
        }
        Ok(())
    }
}

/// OAuth2 client credentials (RFC 6749 §4.4).
///
/// Implements both ADR 0008 variants: `oauth2_client_cred.v1` sends the client
/// credentials as form fields, `oauth2_client_cred_basic.v1` as HTTP Basic.
/// Tokens are cached per `(subject_tenant_id, subject_id, auth_method, config)`
/// and never logged.
pub struct OAuth2ClientCredAuthPlugin {
    credential_store: SharedCredentialStore,
    token_cache: std::sync::Arc<TokenCache>,
    cache_ttl: std::time::Duration,
    basic: bool,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("basic", &self.basic)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Build the plugin bound to `credential_store` and `token_cache`.
    #[must_use]
    pub fn new(
        credential_store: SharedCredentialStore,
        token_cache: std::sync::Arc<TokenCache>,
    ) -> Self {
        Self::with_cache_ttl(
            credential_store,
            token_cache,
            std::time::Duration::from_secs(300),
        )
    }

    /// Build the plugin with an explicit maximum cache TTL.
    #[must_use]
    pub fn with_cache_ttl(
        credential_store: SharedCredentialStore,
        token_cache: std::sync::Arc<TokenCache>,
        cache_ttl: std::time::Duration,
    ) -> Self {
        Self {
            credential_store,
            token_cache,
            cache_ttl,
            basic: false,
        }
    }

    /// Build the `oauth2_client_cred_basic` variant.
    #[must_use]
    pub fn basic(
        credential_store: SharedCredentialStore,
        token_cache: std::sync::Arc<TokenCache>,
        cache_ttl: std::time::Duration,
    ) -> Self {
        let mut this = Self::with_cache_ttl(credential_store, token_cache, cache_ttl);
        this.basic = true;
        this
    }
}

/// A stable digest of the auth-plugin configuration.
///
/// `DefaultHasher` is not guaranteed stable across releases, so the digest is
/// derived from the canonical configuration text. The digest never leaves the
/// process and never contains the credential values themselves (those live in
/// the credential store, referenced by name).
fn config_digest(config: &Value) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&config.to_string(), &mut hasher);
    format!("{:016x}", std::hash::Hasher::finish(&hasher))
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        if self.basic {
            crate::ids::AUTH_PLUGIN_OAUTH2_CC_BASIC
        } else {
            crate::ids::AUTH_PLUGIN_OAUTH2_CC
        }
    }

    async fn authenticate(
        &self,
        ctx: &ProxyContext,
        config: &Value,
        headers: &mut HeaderMap,
        _query: &mut OutboundQuery,
    ) -> Result<(), DomainError> {
        let client_id_ref =
            config_str(config, &["client_id_ref", "clientIdRef"]).ok_or_else(|| {
                DomainError::new(
                    ErrorKind::Validation,
                    "oauth2 client credentials plugin requires `client_id_ref`",
                )
            })?;
        let client_secret_ref = config_str(
            config,
            &[
                "client_secret_ref",
                "clientSecretRef",
                "client_id_secret_ref",
            ],
        )
        .ok_or_else(|| {
            DomainError::new(
                ErrorKind::Validation,
                "oauth2 client credentials plugin requires `client_secret_ref`",
            )
        })?;
        let scopes = config_str_or_list(config, "scopes")
            .unwrap_or_default()
            .split([',', ' '])
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let endpoint = config_str(config, &["token_endpoint", "tokenEndpoint"]);
        let issuer = config_str(config, &["issuer_url", "issuerUrl"]);
        if endpoint.is_none() && issuer.is_none() {
            return Err(DomainError::new(
                ErrorKind::Validation,
                "oauth2 client credentials plugin requires `token_endpoint` or `issuer_url`",
            ));
        }
        if endpoint.is_some() && issuer.is_some() {
            return Err(DomainError::new(
                ErrorKind::Validation,
                "`token_endpoint` and `issuer_url` are mutually exclusive",
            ));
        }

        let key = format!(
            "{}:{}:{}:{}",
            ctx.security.subject_tenant_id(),
            ctx.security.subject_id(),
            if self.basic { "basic" } else { "form" },
            config_digest(config),
        );

        if let Some(cached) = self.token_cache.get(&key)
            && let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", cached.token.expose())) {
                headers.insert(HeaderName::from_static(AUTHORIZATION), value);
                return Ok(());
            }

        let client_id = resolve(&self.credential_store, &ctx.security, client_id_ref).await?;
        let client_id = decode_utf8(&client_id, "oauth2 client id")?;
        let client_secret =
            resolve(&self.credential_store, &ctx.security, client_secret_ref).await?;
        let client_secret = decode_utf8(&client_secret, "oauth2 client secret")?;

        let token_endpoint = match endpoint {
            Some(url) => Some(parse_url(url, "token_endpoint")?),
            None => None,
        };
        let issuer_url = match issuer {
            Some(url) => Some(parse_url(url, "issuer_url")?),
            None => None,
        };

        let oauth_config = OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: if self.basic {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            },
            ..OAuthClientConfig::default()
        };

        let fetched = fetch_token(oauth_config).await.map_err(|err| {
            // `TokenError` renders without any token value.
            DomainError::new(
                ErrorKind::Authentication,
                format!("oauth2 client credentials token exchange failed: {err}"),
            )
        })?;

        let lifetime = fetched.expires_in.as_secs().min(self.cache_ttl.as_secs());
        if lifetime > TOKEN_TTL_SLACK_SECS {
            let ttl = std::time::Duration::from_secs(lifetime - TOKEN_TTL_SLACK_SECS);
            self.token_cache
                .put(key, fetched.bearer.expose().to_owned(), ttl);
        }

        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", fetched.bearer.expose())) {
            headers.insert(HeaderName::from_static(AUTHORIZATION), value);
        }
        Ok(())
    }
}

fn decode_utf8(secret: &credstore_sdk::SecretValue, what: &str) -> Result<String, DomainError> {
    std::str::from_utf8(secret.as_bytes())
        .map(str::trim)
        .map(str::to_owned)
        .map_err(|_| {
            DomainError::new(
                ErrorKind::SecretNotFound,
                format!("{what} is not valid UTF-8"),
            )
        })
}

fn parse_url(raw: &str, what: &str) -> Result<url::Url, DomainError> {
    url::Url::parse(raw).map_err(|_| {
        DomainError::new(
            ErrorKind::Validation,
            format!("{what} {raw:?} is not a valid URL"),
        )
    })
}

/// `required_headers` guard (ADR 0009).
///
/// `required_request_headers` is validated on the way out — a miss is a `400`;
/// `required_response_headers` on the way back — a miss is a `502`. Values are
/// comma-separated, trimmed and lowercased; a blank configuration is fail-open.
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        crate::ids::GUARD_PLUGIN_REQUIRED_HEADERS
    }

    async fn check_request(
        &self,
        _ctx: &ProxyContext,
        config: &Value,
        _method: &Method,
        headers: &HeaderMap,
    ) -> Result<(), DomainError> {
        match first_missing(config, "required_request_headers", headers) {
            Some(name) => Err(DomainError::new(
                ErrorKind::Validation,
                format!(
                    "required request header `{name}` is missing (code REQUIRED_HEADER_MISSING)"
                ),
            )),
            None => Ok(()),
        }
    }

    async fn check_response(
        &self,
        _ctx: &ProxyContext,
        config: &Value,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Result<(), DomainError> {
        match first_missing(config, "required_response_headers", headers) {
            Some(name) => Err(DomainError::new(
                ErrorKind::Downstream,
                format!(
                    "upstream responded {} without required response header `{name}` (code REQUIRED_HEADER_MISSING)",
                    status.as_u16()
                ),
            )),
            None => Ok(()),
        }
    }
}

/// The first required header that is absent, compared case-insensitively and
/// by presence only.
fn first_missing(config: &Value, key: &str, headers: &HeaderMap) -> Option<String> {
    let raw = config.get(key).and_then(Value::as_str)?;
    let names: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    names
        .into_iter()
        .find(|name| {
            !name
                .parse::<HeaderName>()
                .map(|parsed| headers.contains_key(&parsed))
                .unwrap_or(false)
        })
        .map(str::to_owned)
}

/// `request_id` transform: propagate or mint `X-Request-ID`.
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        crate::ids::TRANSFORM_PLUGIN_REQUEST_ID
    }

    async fn transform_request(
        &self,
        _ctx: &ProxyContext,
        _config: &Value,
        headers: &mut HeaderMap,
    ) -> Result<(), DomainError> {
        if !headers.contains_key("x-request-id") {
            let value = format!("oagw-{}", uuid::Uuid::new_v4());
            if let Ok(parsed) = HeaderValue::from_str(&value) {
                headers.insert(HeaderName::from_static("x-request-id"), parsed);
            }
        }
        Ok(())
    }
}

/// The executable form of a stored custom plugin definition.
///
/// Custom plugins are tenant-defined and identified by a UUID-backed GTS
/// instance id (`…~{uuid}`); they resolve out of the definition store rather
/// than the in-process registry (DESIGN §3.1 "Resolution Algorithm"). This
/// release has no Starlark interpreter — the toolchain carries none — so a
/// definition's `config` document *is* its program, read as data and applied
/// with no evaluation, no I/O and no imports:
///
/// * a **transform** definition reads a [`HeaderTransform`], the same
///   `set` / `add` / `remove` vocabulary an upstream's `headers` block uses;
/// * a **guard** definition reads `required_request_headers` /
///   `required_response_headers`, the `required_headers.v1` vocabulary.
///
/// A definition carrying only Starlark `source_code` therefore has nothing to
/// run and fails the request with [`ErrorKind::PluginNotFound`] rather than
/// passing silently through a chain that did not execute it.
pub struct DefinitionPlugin {
    id: String,
    kind: DefinitionKind,
    transform: crate::domain::model::HeaderTransform,
    request_headers: Vec<String>,
    response_headers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefinitionKind {
    Guard,
    Transform,
}

impl DefinitionPlugin {
    /// Build the guard half of `definition`.
    #[must_use]
    pub fn guard(definition: &crate::domain::model::PluginDefinition) -> Self {
        Self::new(definition, DefinitionKind::Guard)
    }

    /// Build the transform half of `definition`.
    #[must_use]
    pub fn transform(definition: &crate::domain::model::PluginDefinition) -> Self {
        Self::new(definition, DefinitionKind::Transform)
    }

    fn new(definition: &crate::domain::model::PluginDefinition, kind: DefinitionKind) -> Self {
        let config = &definition.config;
        let (request_headers, response_headers) = match kind {
            DefinitionKind::Guard => (
                required_list(config, "required_request_headers"),
                required_list(config, "required_response_headers"),
            ),
            DefinitionKind::Transform => (Vec::new(), Vec::new()),
        };
        Self {
            id: definition.plugin_ref.clone(),
            kind,
            transform: serde_json::from_value(config.clone())
                .unwrap_or_default(),
            request_headers,
            response_headers,
        }
    }

    /// `true` when the definition carries no instruction this release runs.
    #[must_use]
    pub fn is_inert(&self) -> bool {
        match self.kind {
            DefinitionKind::Transform => {
                self.transform.set.is_empty()
                    && self.transform.add.is_empty()
                    && self.transform.remove.is_empty()
            }
            DefinitionKind::Guard => {
                self.request_headers.is_empty() && self.response_headers.is_empty()
            }
        }
    }

    /// The failure for a definition whose `config` names nothing to run.
    fn inert(&self) -> DomainError {
        DomainError::new(
            ErrorKind::PluginNotFound,
            format!(
                "custom plugin {:?} has no executable configuration in this release; \
                 it declares Starlark source only",
                self.id
            ),
        )
    }
}

#[async_trait]
impl GuardPlugin for DefinitionPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    async fn check_request(
        &self,
        _ctx: &ProxyContext,
        _config: &Value,
        _method: &Method,
        headers: &HeaderMap,
    ) -> Result<(), DomainError> {
        let missing = self
            .request_headers
            .iter()
            .find(|name| !contains(headers, name));
        if let Some(name) = missing {
            return Err(DomainError::new(
                ErrorKind::Validation,
                format!(
                    "required request header `{name}` is missing (code REQUIRED_HEADER_MISSING)"
                ),
            ));
        }
        if self.request_headers.is_empty() && self.response_headers.is_empty() {
            return Err(self.inert());
        }
        Ok(())
    }

    async fn check_response(
        &self,
        _ctx: &ProxyContext,
        _config: &Value,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Result<(), DomainError> {
        let missing = self
            .response_headers
            .iter()
            .find(|name| !contains(headers, name));
        if let Some(name) = missing {
            return Err(DomainError::new(
                ErrorKind::Downstream,
                format!(
                    "upstream responded {} without required response header `{name}` (code REQUIRED_HEADER_MISSING)",
                    status.as_u16()
                ),
            ));
        }
        if self.request_headers.is_empty() && self.response_headers.is_empty() {
            return Err(self.inert());
        }
        Ok(())
    }
}

#[async_trait]
impl TransformPlugin for DefinitionPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    async fn transform_request(
        &self,
        _ctx: &ProxyContext,
        _config: &Value,
        headers: &mut HeaderMap,
    ) -> Result<(), DomainError> {
        if self.is_inert() {
            return Err(self.inert());
        }
        apply(&self.transform, headers);
        Ok(())
    }

    async fn transform_response(
        &self,
        _ctx: &ProxyContext,
        _config: &Value,
        _status: StatusCode,
        headers: &mut HeaderMap,
    ) -> Result<(), DomainError> {
        if self.is_inert() {
            return Err(self.inert());
        }
        apply(&self.transform, headers);
        Ok(())
    }
}

/// Apply a header transform, ignoring names or values that are not wire-legal.
fn apply(transform: &crate::domain::model::HeaderTransform, headers: &mut HeaderMap) {
    for name in &transform.remove {
        if let Ok(parsed) = name.parse::<HeaderName>() {
            headers.remove(&parsed);
        }
    }
    for (name, value) in &transform.add {
        if let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), HeaderValue::from_str(value)) {
            headers.append(name, value);
        }
    }
    for (name, value) in &transform.set {
        if let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), HeaderValue::from_str(value)) {
            headers.insert(name, value);
        }
    }
}

/// Case-insensitive presence test that tolerates a name this release cannot
/// parse — a header the operator asked for but never spelled correctly is
/// reported as missing rather than crashing the request.
fn contains(headers: &HeaderMap, name: &str) -> bool {
    name.parse::<HeaderName>()
        .map(|parsed| headers.contains_key(&parsed))
        .unwrap_or(false)
}

/// A required-header list, accepted as a comma-separated string or an array.
fn required_list(config: &Value, key: &str) -> Vec<String> {
    match config.get(key) {
        Some(Value::String(raw)) => split_names(raw),
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(Value::as_str)
            .flat_map(split_names)
            .collect(),
        _ => Vec::new(),
    }
}

fn split_names(raw: &str) -> Vec<String> {
    split_commas(raw).collect()
}

fn split_commas(raw: &str) -> impl Iterator<Item = String> + '_ {
    raw.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PluginDefinition;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit_security::SecurityContext;

    fn store_with(entries: &[(&str, &str)]) -> SharedCredentialStore {
        std::sync::Arc::new(MockCredStoreClient::with_secrets(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        ))
    }

    fn test_context() -> ProxyContext {
        ProxyContext {
            security: SecurityContext::anonymous(),
            tenant_id: uuid::Uuid::nil(),
            alias: "api.example.test".to_owned(),
            upstream_id: uuid::Uuid::nil(),
            route_id: None,
            endpoint_host: "api.example.test".to_owned(),
            outbound_path: "/v1/x".to_owned(),
            client_ip: "127.0.0.1".to_owned(),
        }
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// A custom plugin definition with `plugin_ref` already derived.
    fn definition(kind: crate::domain::model::PluginType, config: Value) -> PluginDefinition {
        PluginDefinition {
            id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            plugin_ref: format!(
                "{}{}",
                kind.resource_type(),
                "0f0e0d0c-0b0a-49f8-8765-432109876543"
            ),
            plugin_type: kind,
            name: "custom".to_owned(),
            description: String::new(),
            config,
            config_schema: None,
            source_code: None,
            version: 1,
            enabled: true,
            created_at: None,
        }
    }

    #[tokio::test]
    async fn a_custom_transform_definition_applies_its_document() {
        let plugin = DefinitionPlugin::transform(&definition(
            crate::domain::model::PluginType::Transform,
            serde_json::json!({
                "add": {"x-added": "by-plugin"},
                "set": {"x-replaced": "second"},
                "remove": ["x-stripped"]
            }),
        ));
        let mut headers = HeaderMap::new();
        headers.insert("x-replaced", HeaderValue::from_static("first"));
        headers.insert("x-stripped", HeaderValue::from_static("gone"));
        plugin
            .transform_request(&test_context(), &Value::Null, &mut headers)
            .await
            .expect("the definition runs");
        assert_eq!(header(&headers, "x-added"), Some("by-plugin"));
        assert_eq!(header(&headers, "x-replaced"), Some("second"));
        assert!(headers.get("x-stripped").is_none());
    }

    #[tokio::test]
    async fn a_custom_guard_definition_rejects_a_missing_required_header() {
        let plugin = DefinitionPlugin::guard(&definition(
            crate::domain::model::PluginType::Guard,
            serde_json::json!({"required_request_headers": "x-must, x-also"}),
        ));
        let mut headers = HeaderMap::new();
        headers.insert("x-mandatory", HeaderValue::from_static("1"));
        let err = plugin
            .check_request(&test_context(), &Value::Null, &Method::GET, &headers)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert!(err.to_string().contains("`x-must`"), "{err}");
    }

    #[tokio::test]
    async fn a_custom_guard_definition_accepts_a_complete_request() {
        let plugin = DefinitionPlugin::guard(&definition(
            crate::domain::model::PluginType::Guard,
            serde_json::json!({"required_request_headers": ["x-mandatory"]}),
        ));
        let mut headers = HeaderMap::new();
        headers.insert("x-mandatory", HeaderValue::from_static("1"));
        plugin
            .check_request(&test_context(), &Value::Null, &Method::GET, &headers)
            .await
            .expect("the required header is present");
    }

    #[tokio::test]
    async fn a_starlark_only_definition_fails_rather_than_passing_silently() {
        let mut source = definition(crate::domain::model::PluginType::Transform, serde_json::json!({}));
        source.source_code = Some("def transform_request(ctx, headers):\n    return None\n".to_owned());
        let plugin = DefinitionPlugin::transform(&source);
        let mut headers = HeaderMap::new();
        let err = plugin
            .transform_request(&test_context(), &Value::Null, &mut headers)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PluginNotFound);
        assert!(err.to_string().contains("Starlark"), "{err}");
    }

    #[test]
    fn config_digest_is_order_insensitive_and_credential_free() {
        let a = serde_json::json!({
            "token_endpoint": "https://auth.example.test/token",
            "client_id_ref": "a",
            "client_secret_ref": "b"
        });
        let b = serde_json::json!({
            "client_secret_ref": "b",
            "client_id_ref": "a",
            "token_endpoint": "https://auth.example.test/token"
        });
        let c = serde_json::json!({
            "token_endpoint": "https://auth.example.test/token",
            "client_id_ref": "a",
            "client_secret_ref": "z"
        });
        assert_eq!(config_digest(&a), config_digest(&b));
        assert_ne!(config_digest(&a), config_digest(&c));
    }

    #[test]
    fn a_missing_required_headers_block_is_fail_open() {
        let headers = HeaderMap::new();
        assert_eq!(
            first_missing(&serde_json::json!({}), "required_request_headers", &headers),
            None
        );
        assert_eq!(
            first_missing(
                &serde_json::json!({"required_request_headers": "   "}),
                "required_request_headers",
                &headers
            ),
            None
        );
    }

    #[test]
    fn required_request_headers_name_the_first_absent_one() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("v"));
        assert_eq!(
            first_missing(
                &serde_json::json!({"required_request_headers": "X-Api-Key, X-Tenant"}),
                "required_request_headers",
                &headers
            )
            .as_deref(),
            Some("X-Tenant")
        );
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_a_request_missing_its_header() {
        let ctx = test_context();
        let error = RequiredHeadersGuardPlugin
            .check_request(
                &ctx,
                &serde_json::json!({"required_request_headers": "x-tenant-id"}),
                &Method::GET,
                &HeaderMap::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.status(), 400);
        assert!(error.to_string().contains("REQUIRED_HEADER_MISSING"));
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_a_response_missing_its_header() {
        let ctx = test_context();
        let error = RequiredHeadersGuardPlugin
            .check_response(
                &ctx,
                &serde_json::json!({"required_response_headers": "x-request-id"}),
                StatusCode::OK,
                &HeaderMap::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Downstream);
        assert_eq!(error.status(), 502);
    }

    #[tokio::test]
    async fn required_headers_guard_accepts_a_complete_response() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("r1"));
        RequiredHeadersGuardPlugin
            .check_response(
                &test_context(),
                &serde_json::json!({"required_response_headers": "X-Request-Id"}),
                StatusCode::OK,
                &headers,
            )
            .await
            .expect("pass");
    }

    #[tokio::test]
    async fn apikey_plugin_requires_a_secret_ref() {
        let mut headers = HeaderMap::new();
        let mut query = OutboundQuery::default();
        let error = ApiKeyAuthPlugin::new(store_with(&[]))
            .authenticate(
                &test_context(),
                &serde_json::json!({}),
                &mut headers,
                &mut query,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
    }

    #[tokio::test]
    async fn apikey_plugin_injects_into_a_header() {
        let plugin = ApiKeyAuthPlugin::new(store_with(&[("k", "sk-live-abc")]));
        let mut headers = HeaderMap::new();
        let mut query = OutboundQuery::default();
        plugin
            .authenticate(
                &test_context(),
                &serde_json::json!({"secret_ref": "k", "name": "X-Key", "prefix": "Bearer"}),
                &mut headers,
                &mut query,
            )
            .await
            .expect("inject");
        assert_eq!(header(&headers, "x-key"), Some("Bearer sk-live-abc"));
    }

    #[tokio::test]
    async fn apikey_plugin_defaults_to_the_x_api_key_header() {
        let plugin = ApiKeyAuthPlugin::new(store_with(&[("k", "sk-live-abc")]));
        let mut headers = HeaderMap::new();
        let mut query = OutboundQuery::default();
        plugin
            .authenticate(
                &test_context(),
                &serde_json::json!({"secret_ref": "cred://k"}),
                &mut headers,
                &mut query,
            )
            .await
            .expect("inject");
        assert_eq!(header(&headers, "x-api-key"), Some("sk-live-abc"));
        assert!(query.render().is_none());
    }

    #[tokio::test]
    async fn apikey_plugin_can_inject_into_the_query_instead() {
        let plugin = ApiKeyAuthPlugin::new(store_with(&[("k", "secret")]));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer stale"));
        let mut query = OutboundQuery::parse(Some("model=gpt-4"));
        plugin
            .authenticate(
                &test_context(),
                &serde_json::json!({"secret_ref": "k", "location": "query", "name": "key"}),
                &mut headers,
                &mut query,
            )
            .await
            .expect("inject");
        // Injected into the query, and the inbound Authorization dropped so the
        // caller's own credentials cannot leak to the upstream.
        assert_eq!(query.render().as_deref(), Some("model=gpt-4&key=secret"));
        assert_eq!(query.names(), vec!["model", "key"]);
        assert!(headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn apikey_failures_never_leak_the_credential_value() {
        let plugin = ApiKeyAuthPlugin::new(store_with(&[("k", "sk-live-abc")]));
        let mut headers = HeaderMap::new();
        let mut query = OutboundQuery::default();
        let error = plugin
            .authenticate(
                &test_context(),
                &serde_json::json!({"secret_ref": "missing"}),
                &mut headers,
                &mut query,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::SecretNotFound);
        let rendered = error
            .problem_body()
            .values()
            .map(serde_json::Value::to_string)
            .collect::<String>();
        assert!(!rendered.contains("sk-live-abc"));
        assert!(!format!("{error:?}").contains("sk-live-abc"));
    }

    #[tokio::test]
    async fn noop_plugin_leaves_the_request_alone() {
        let mut headers = HeaderMap::new();
        headers.insert("x-trace", HeaderValue::from_static("1"));
        let mut query = OutboundQuery::parse(Some("a=1"));
        NoopAuthPlugin
            .authenticate(
                &test_context(),
                &serde_json::json!({}),
                &mut headers,
                &mut query,
            )
            .await
            .expect("noop");
        assert_eq!(header(&headers, "x-trace"), Some("1"));
        assert_eq!(query.render().as_deref(), Some("a=1"));
    }

    #[tokio::test]
    async fn request_id_transform_propagates_or_mints() {
        let ctx = test_context();
        let mut headers = HeaderMap::new();
        RequestIdTransformPlugin
            .transform_request(&ctx, &serde_json::json!({}), &mut headers)
            .await
            .expect("mint");
        assert!(header(&headers, "x-request-id").is_some());

        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("client-provided"));
        RequestIdTransformPlugin
            .transform_request(&ctx, &serde_json::json!({}), &mut headers)
            .await
            .expect("propagate");
        assert_eq!(header(&headers, "x-request-id"), Some("client-provided"));
    }

    #[tokio::test]
    async fn oauth2_plugin_rejects_an_incomplete_configuration() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            store_with(&[]),
            std::sync::Arc::new(TokenCache::new(4)),
        );
        let cases = [
            serde_json::json!({"token_endpoint": "https://auth/token"}),
            serde_json::json!({"client_id_ref": "a", "client_secret_ref": "b"}),
            serde_json::json!({
                "token_endpoint": "https://auth/token",
                "issuer_url": "https://auth",
                "client_id_ref": "a",
                "client_secret_ref": "b"
            }),
        ];
        for config in cases {
            let mut headers = HeaderMap::new();
            let mut query = OutboundQuery::default();
            let error = plugin
                .authenticate(&test_context(), &config, &mut headers, &mut query)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "for {config}");
            assert!(headers.get("authorization").is_none());
        }
    }

    #[tokio::test]
    async fn oauth2_plugin_rejects_an_unresolvable_credential() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            store_with(&[]),
            std::sync::Arc::new(TokenCache::new(4)),
        );
        let mut headers = HeaderMap::new();
        let mut query = OutboundQuery::default();
        let error = plugin
            .authenticate(
                &test_context(),
                &serde_json::json!({
                    "token_endpoint": "https://auth/token",
                    "client_id_ref": "nope",
                    "client_secret_ref": "nope"
                }),
                &mut headers,
                &mut query,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::SecretNotFound);
        assert!(headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn oauth2_plugin_caches_the_token_it_fetched() {
        // `httpmock` starts a real token endpoint so the exchange runs for real.
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-1","expires_in":3600,"token_type":"Bearer"}"#);
        });

        let cache = std::sync::Arc::new(TokenCache::new(4));
        let plugin = OAuth2ClientCredAuthPlugin::with_cache_ttl(
            store_with(&[("id", "client-1"), ("secret", "shh")]),
            cache.clone(),
            std::time::Duration::from_secs(300),
        );
        let config = serde_json::json!({
            "token_endpoint": server.url("/token"),
            "client_id_ref": "id",
            "client_secret_ref": "secret",
            "scopes": ["read", "write"]
        });

        let mut headers = HeaderMap::new();
        let mut query = OutboundQuery::default();
        plugin
            .authenticate(&test_context(), &config, &mut headers, &mut query)
            .await
            .expect("first exchange");
        assert_eq!(header(&headers, "authorization"), Some("Bearer tok-1"));

        // A second call is served from the cache, so the endpoint is never
        // contacted again even though its mock would now fail.
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(500).body("no more tokens");
        });
        let mut headers = HeaderMap::new();
        let mut query = OutboundQuery::default();
        plugin
            .authenticate(&test_context(), &config, &mut headers, &mut query)
            .await
            .expect("cached");
        assert_eq!(header(&headers, "authorization"), Some("Bearer tok-1"));
    }

    #[test]
    fn oauth2_variants_register_under_their_own_ids() {
        let store = store_with(&[]);
        let cache = std::sync::Arc::new(TokenCache::new(4));
        assert_eq!(
            OAuth2ClientCredAuthPlugin::new(store.clone(), cache.clone()).id(),
            crate::ids::AUTH_PLUGIN_OAUTH2_CC
        );
        assert_eq!(
            OAuth2ClientCredAuthPlugin::basic(store, cache, std::time::Duration::from_secs(300))
                .id(),
            crate::ids::AUTH_PLUGIN_OAUTH2_CC_BASIC
        );
    }

    #[test]
    fn oauth2_debug_output_redacts_the_client_secret() {
        let config = OAuthClientConfig {
            client_id: "client-1".to_owned(),
            client_secret: SecretString::new("shh".to_owned()),
            ..OAuthClientConfig::default()
        };
        let rendered = format!("{config:?}");
        assert!(rendered.contains("REDACTED"));
        assert!(!rendered.contains("shh"));
    }
}
