//! Built-in plugin implementations (DESIGN §3.3 "Built-in Plugins").
//!
//! Six built-ins across the three traits: `noop`/`apikey`/`oauth2_client_cred`
//! (+ its `basic` variant) as auth plugins, `required_headers` as a guard and
//! `request_id` as a transform. The catalog-only identifiers have no entry
//! here, so binding them fails instead of silently proxying.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;

use crate::domain::error::{DomainError, DomainResult, codes};
use crate::domain::plugin::ids;
use crate::domain::plugin::{
    AuthPlugin, AuthPluginRegistry, GuardDecision, GuardPlugin, GuardPluginRegistry,
    RequestContext, ResponseContext, TransformPlugin, TransformPluginRegistry,
};

/// Resolves `cred://` references to their secret values.
///
/// The gear wiring supplies the CredStore-backed implementation; tests and
/// plugins that take static values use `NullCredentialResolver`.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Returns the secret value for a `cred://` reference.
    ///
    /// # Errors
    ///
    /// Returns `SecretNotFound` when the reference is absent from the store.
    async fn resolve(
        &self,
        reference: &str,
        tenant_id: &str,
        subject_id: Option<&str>,
    ) -> DomainResult<String>;
}

/// A resolver that always fails — used when no credential store is wired.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullCredentialResolver;

#[async_trait]
impl CredentialResolver for NullCredentialResolver {
    async fn resolve(
        &self,
        reference: &str,
        _tenant_id: &str,
        _subject_id: Option<&str>,
    ) -> DomainResult<String> {
        Err(DomainError::SecretNotFound(reference.to_owned()))
    }
}

/// Extracts a string field from a plugin configuration object.
fn config_str(config: &serde_json::Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Splits a comma-separated configuration value into lowercased entries.
fn header_names(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

// === Auth ===

/// `noop` auth plugin: injects nothing (DESIGN §3.3 built-ins).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        ids::AUTH_NOOP
    }

    fn plugin_type(&self) -> &str {
        "auth"
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> DomainResult<()> {
        Ok(())
    }
}

/// `apikey` auth plugin: injects a static or credential-store API key.
///
/// Config keys: `api_key` (static value), `api_key_ref` (`cred://` reference),
/// `header` (target header, default `Authorization`) and `scheme` (prefix
/// prepended to the key, default `Bearer`).
#[derive(Clone)]
pub struct ApiKeyAuthPlugin {
    resolver: Arc<dyn CredentialResolver>,
}

impl ApiKeyAuthPlugin {
    /// Creates the plugin bound to a credential resolver.
    #[must_use]
    pub fn new(resolver: Arc<dyn CredentialResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        ids::AUTH_APIKEY
    }

    fn plugin_type(&self) -> &str {
        "auth"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> DomainResult<()> {
        let header = config_str(&ctx.config, "header")
            .unwrap_or_else(|| "authorization".to_owned())
            .to_ascii_lowercase();
        let scheme = config_str(&ctx.config, "scheme").unwrap_or_else(|| "Bearer".to_owned());
        let static_key = config_str(&ctx.config, "api_key");
        let reference = config_str(&ctx.config, "api_key_ref");
        let key = match (static_key, reference) {
            (Some(value), _) => value,
            (None, Some(reference)) => {
                self.resolver
                    .resolve(&reference, &ctx.tenant_id, ctx.subject_id.as_deref())
                    .await?
            }
            (None, None) => {
                return Err(DomainError::AuthenticationFailed(
                    "apikey plugin requires either 'api_key' or 'api_key_ref'".to_owned(),
                ));
            }
        };
        let value = if scheme.is_empty() {
            key
        } else {
            format!("{scheme} {key}")
        };
        ctx.injected_headers.insert(header, value);
        Ok(())
    }
}

/// OAuth2 client-credentials auth method (ADR-0008 "Plugin Variants").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// Credentials in the request body (`Form`).
    Form,
    /// Credentials in the `Authorization` header (`Basic`).
    Basic,
}

impl ClientAuthMethod {
    /// Cache-key tag distinguishing the two variants.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }

    /// `toolkit_auth` wire spelling.
    #[must_use]
    pub const fn into_toolkit(self) -> toolkit_auth::ClientAuthMethod {
        match self {
            Self::Form => toolkit_auth::ClientAuthMethod::Form,
            Self::Basic => toolkit_auth::ClientAuthMethod::Basic,
        }
    }
}

/// A cached OAuth2 access token (ADR-0008 "Hash-Collision Safety").
///
/// The wrapper carries the cache key so a TinyUfo hash collision is detected
/// on hit instead of serving another tenant's token.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Arc<str>,
}

/// Configuration of the OAuth2 client-credentials plugin (ADR-0008).
///
/// # Errors
///
/// Returns a validation error when the config omits required keys or names
/// both `token_endpoint` and `issuer_url`.
fn parse_oauth2_config(config: &serde_json::Value) -> DomainResult<ParsedOAuth2Config> {
    let endpoint = config
        .get("token_endpoint")
        .and_then(serde_json::Value::as_str);
    let issuer = config.get("issuer_url").and_then(serde_json::Value::as_str);
    let client_id_ref = config
        .get("client_id_ref")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DomainError::AuthenticationFailed("client_id_ref is required".to_owned()))?;
    let client_secret_ref = config
        .get("client_secret_ref")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DomainError::AuthenticationFailed("client_secret_ref is required".to_owned())
        })?;
    if endpoint.is_some() && issuer.is_some() {
        return Err(DomainError::Validation(
            "oauth2 config accepts either 'token_endpoint' or 'issuer_url', not both".to_owned(),
        ));
    }
    if endpoint.is_none() && issuer.is_none() {
        return Err(DomainError::Validation(
            "oauth2 config requires 'token_endpoint' or 'issuer_url'".to_owned(),
        ));
    }
    Ok(ParsedOAuth2Config {
        token_endpoint: endpoint.map(str::to_owned),
        issuer_url: issuer.map(str::to_owned),
        client_id_ref: client_id_ref.to_owned(),
        client_secret_ref: client_secret_ref.to_owned(),
        scopes: config
            .get("scopes")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    })
}

/// Parsed `ctx.config` for the OAuth2 client-credentials plugins.
#[derive(Debug, Clone)]
struct ParsedOAuth2Config {
    token_endpoint: Option<String>,
    issuer_url: Option<String>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

/// OAuth2 client-credentials auth plugin with an internal token cache
/// (ADR-0008). One instance per client-auth method; both share the cache.
pub struct OAuth2ClientCredAuthPlugin {
    resolver: Arc<dyn CredentialResolver>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Creates the plugin with a cache of `capacity` entries (ADR-0008).
    #[must_use]
    pub fn new(
        resolver: Arc<dyn CredentialResolver>,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        capacity: usize,
    ) -> Self {
        Self {
            resolver,
            auth_method,
            cache: MemoryCache::new(capacity.max(1)),
            cache_ttl,
        }
    }

    /// The GTS identifier of this variant.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => ids::AUTH_OAUTH2_FORM,
            ClientAuthMethod::Basic => ids::AUTH_OAUTH2_BASIC,
        }
    }

    /// Cache key covering tenant, subject, method and config (ADR-0008).
    fn build_cache_key(&self, ctx: &RequestContext) -> String {
        let mut keys = Vec::new();
        if let serde_json::Value::Object(map) = &ctx.config {
            for (key, value) in map {
                keys.push(format!("{key}={value}"));
            }
        }
        keys.sort();
        format!(
            "{}:{}:{}:{}",
            ctx.tenant_id,
            ctx.subject_id.clone().unwrap_or_default(),
            self.auth_method.tag(),
            keys.join(",")
        )
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.id()
    }

    fn plugin_type(&self) -> &str {
        "auth"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> DomainResult<()> {
        let parsed = parse_oauth2_config(&ctx.config)?;
        let key = self.build_cache_key(ctx);
        if let (Some(entry), _) = self.cache.get(&key)
            // Defense-in-depth: a hash collision is treated as a miss.
            && entry.key == key
        {
            ctx.injected_headers.insert(
                "authorization".to_owned(),
                format!("Bearer {}", entry.token),
            );
            return Ok(());
        }
        let client_id = self
            .resolver
            .resolve(
                &parsed.client_id_ref,
                &ctx.tenant_id,
                ctx.subject_id.as_deref(),
            )
            .await?;
        let client_secret = self
            .resolver
            .resolve(
                &parsed.client_secret_ref,
                &ctx.tenant_id,
                ctx.subject_id.as_deref(),
            )
            .await?;
        let oauth_config = toolkit_auth::OAuthClientConfig {
            token_endpoint: parsed
                .token_endpoint
                .as_deref()
                .and_then(|value| value.parse().ok()),
            issuer_url: parsed
                .issuer_url
                .as_deref()
                .and_then(|value| value.parse().ok()),
            client_id,
            client_secret: toolkit_auth::SecretString::new(client_secret),
            scopes: parsed.scopes.clone(),
            auth_method: self.auth_method.into_toolkit(),
            ..toolkit_auth::OAuthClientConfig::default()
        };
        let fetched = toolkit_auth::fetch_token(oauth_config)
            .await
            .map_err(|error| DomainError::AuthenticationFailed(error.to_string()))?;
        let ttl = self
            .cache_ttl
            .min(fetched.expires_in.saturating_sub(Duration::from_secs(30)))
            .max(Duration::from_secs(1));
        let token: Arc<str> = Arc::from(fetched.bearer.expose());
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: token.clone(),
            },
            Some(ttl),
        );
        ctx.injected_headers
            .insert("authorization".to_owned(), format!("Bearer {token}"));
        Ok(())
    }
}

// === Guards ===

/// `required_headers` guard plugin (ADR-0009).
///
/// Checks presence only, case-insensitively; an unconfigured phase is a no-op
/// (fail-open) and only the first missing header is reported.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        ids::GUARD_REQUIRED_HEADERS
    }

    fn plugin_type(&self) -> &str {
        "guard"
    }

    async fn guard_request(&self, ctx: &RequestContext) -> DomainResult<GuardDecision> {
        let Some(required) = config_str(&ctx.config, "required_request_headers") else {
            return Ok(GuardDecision::Allow);
        };
        for name in header_names(&required) {
            if !ctx.headers.contains_key(&name) {
                return Ok(GuardDecision::reject(
                    400,
                    codes::ROUTE_REJECTED,
                    format!("required request header {name:?} is missing"),
                ));
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> DomainResult<GuardDecision> {
        let Some(required) = config_str(&ctx.config, "required_response_headers") else {
            return Ok(GuardDecision::Allow);
        };
        for name in header_names(&required) {
            if !ctx.headers.contains_key(&name) {
                return Ok(GuardDecision::reject(
                    502,
                    codes::PROTOCOL_ERROR,
                    format!("upstream response is missing required header {name:?}"),
                ));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

// === Transforms ===

/// `request_id` transform plugin: propagates or mints `X-Request-Id`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        ids::TRANSFORM_REQUEST_ID
    }

    fn plugin_type(&self) -> &str {
        "transform"
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> DomainResult<()> {
        const HEADER: &str = "x-request-id";
        if ctx.headers.contains_key(HEADER) || ctx.injected_headers.contains_key(HEADER) {
            return Ok(());
        }
        let minted = uuid::Uuid::new_v4().to_string();
        ctx.injected_headers.insert(HEADER.to_owned(), minted);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> DomainResult<()> {
        const HEADER: &str = "x-request-id";
        if let Some(value) = ctx.headers.get(HEADER) {
            ctx.injected_headers
                .insert(HEADER.to_owned(), value.clone());
        }
        Ok(())
    }
}

/// Builds the `AuthPluginRegistry` with the built-in auth plugins.
#[must_use]
pub fn auth_registry_with_builtins(
    resolver: Arc<dyn CredentialResolver>,
    token_cache_ttl: Duration,
    token_cache_capacity: usize,
) -> AuthPluginRegistry {
    AuthPluginRegistry::new(vec![
        Arc::new(NoopAuthPlugin),
        Arc::new(ApiKeyAuthPlugin::new(resolver.clone())),
        Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver.clone(),
            ClientAuthMethod::Form,
            token_cache_ttl,
            token_cache_capacity,
        )),
        Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver,
            ClientAuthMethod::Basic,
            token_cache_ttl,
            token_cache_capacity,
        )),
    ])
}

/// Builds the `GuardPluginRegistry` with the built-in guard plugins.
///
/// `timeout` and `cors` are core data-plane logic, not guard plugins
/// (DESIGN §3.3), so they are deliberately absent.
#[must_use]
pub fn guard_registry_with_builtins() -> GuardPluginRegistry {
    GuardPluginRegistry::new(vec![Arc::new(RequiredHeadersGuardPlugin)])
}

/// Builds the `TransformPluginRegistry` with the built-in transform plugins.
///
/// `logging` and `metrics` are core data-plane instrumentation.
#[must_use]
pub fn transform_registry_with_builtins() -> TransformPluginRegistry {
    TransformPluginRegistry::new(vec![Arc::new(RequestIdTransformPlugin)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Resolver backed by a fixed table, for plugin tests.
    #[derive(Default)]
    struct TableResolver {
        secrets: std::sync::Mutex<BTreeMap<String, String>>,
    }

    impl TableResolver {
        fn with(key: &str, value: &str) -> Arc<Self> {
            let mut secrets = BTreeMap::new();
            secrets.insert(key.to_owned(), value.to_owned());
            Arc::new(Self {
                secrets: std::sync::Mutex::new(secrets),
            })
        }
    }

    #[async_trait]
    impl CredentialResolver for TableResolver {
        async fn resolve(
            &self,
            reference: &str,
            _tenant_id: &str,
            _subject_id: Option<&str>,
        ) -> DomainResult<String> {
            self.secrets
                .lock()
                .expect("lock")
                .get(reference)
                .cloned()
                .ok_or_else(|| DomainError::SecretNotFound(reference.to_owned()))
        }
    }

    fn ctx(config: serde_json::Value) -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            headers: BTreeMap::new(),
            injected_headers: BTreeMap::new(),
            removed_headers: Vec::new(),
            config,
            tenant_id: "tenant-a".to_owned(),
            subject_id: Some("subject-1".to_owned()),
        }
    }

    #[tokio::test]
    async fn noop_auth_injects_nothing() {
        let plugin = NoopAuthPlugin;
        let mut context = ctx(serde_json::Value::Null);
        plugin.authenticate(&mut context).await.expect("ok");
        assert!(context.injected_headers.is_empty());
    }

    #[tokio::test]
    async fn apikey_injects_the_configured_header() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(NullCredentialResolver));
        let mut context = ctx(serde_json::json!({ "api_key": "secret-key" }));
        plugin.authenticate(&mut context).await.expect("ok");
        assert_eq!(
            context
                .injected_headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer secret-key")
        );
    }

    #[tokio::test]
    async fn apikey_resolves_credential_references() {
        let plugin = ApiKeyAuthPlugin::new(TableResolver::with("cred://key", "resolved-key"));
        let mut context = ctx(serde_json::json!({
            "api_key_ref": "cred://key",
            "header": "X-Api-Key",
            "scheme": ""
        }));
        plugin.authenticate(&mut context).await.expect("ok");
        assert_eq!(
            context
                .injected_headers
                .get("x-api-key")
                .map(String::as_str),
            Some("resolved-key")
        );
    }

    #[tokio::test]
    async fn apikey_reports_missing_secrets() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(NullCredentialResolver));
        let mut context = ctx(serde_json::json!({ "api_key_ref": "cred://absent" }));
        let error = plugin
            .authenticate(&mut context)
            .await
            .expect_err("missing");
        assert_eq!(error.status_code(), 500);
    }

    #[tokio::test]
    async fn apikey_requires_a_key_source() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(NullCredentialResolver));
        let mut context = ctx(serde_json::json!({}));
        let error = plugin.authenticate(&mut context).await.expect_err("no key");
        assert_eq!(error.status_code(), 401);
    }

    #[tokio::test]
    async fn oauth2_config_is_validated() {
        assert!(parse_oauth2_config(&serde_json::json!({})).is_err());
        assert!(
            parse_oauth2_config(&serde_json::json!({
                "token_endpoint": "https://idp/token",
                "issuer_url": "https://idp",
                "client_id_ref": "cred://id",
                "client_secret_ref": "cred://secret"
            }))
            .is_err()
        );
        let parsed = parse_oauth2_config(&serde_json::json!({
            "token_endpoint": "https://idp/token",
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "scopes": "read write"
        }))
        .expect("valid");
        assert_eq!(parsed.scopes, vec!["read".to_owned(), "write".to_owned()]);
    }

    #[tokio::test]
    async fn oauth2_variants_declare_their_own_ids() {
        let form = OAuth2ClientCredAuthPlugin::new(
            Arc::new(NullCredentialResolver),
            ClientAuthMethod::Form,
            Duration::from_secs(300),
            16,
        );
        let basic = OAuth2ClientCredAuthPlugin::new(
            Arc::new(NullCredentialResolver),
            ClientAuthMethod::Basic,
            Duration::from_secs(300),
            16,
        );
        assert_eq!(form.id(), ids::AUTH_OAUTH2_FORM);
        assert_eq!(basic.id(), ids::AUTH_OAUTH2_BASIC);
        assert_ne!(form.id(), basic.id());
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_missing_request_headers() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut context = ctx(serde_json::json!({
            "required_request_headers": "X-Request-Id, X-Api-Version"
        }));
        context
            .headers
            .insert("x-request-id".to_owned(), "1".to_owned());
        let decision = plugin.guard_request(&context).await.expect("ran");
        assert!(matches!(
            decision,
            GuardDecision::Reject { status: 400, .. }
        ));
        context
            .headers
            .insert("x-api-version".to_owned(), "1".to_owned());
        assert_eq!(
            plugin.guard_request(&context).await.expect("ran"),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn required_headers_guard_is_fail_open_when_unconfigured() {
        let plugin = RequiredHeadersGuardPlugin;
        assert_eq!(
            plugin
                .guard_request(&ctx(serde_json::Value::Null))
                .await
                .expect("ran"),
            GuardDecision::Allow
        );
        assert_eq!(
            plugin
                .guard_request(&ctx(serde_json::json!({
                    "required_request_headers": "  ,  "
                })))
                .await
                .expect("ran"),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_missing_response_headers() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut response = ResponseContext {
            status: 200,
            headers: BTreeMap::new(),
            injected_headers: BTreeMap::new(),
            config: serde_json::json!({ "required_response_headers": "content-type" }),
        };
        let decision = plugin.guard_response(&response).await.expect("ran");
        assert!(matches!(
            decision,
            GuardDecision::Reject { status: 502, .. }
        ));
        response
            .headers
            .insert("content-type".to_owned(), "application/json".to_owned());
        assert_eq!(
            plugin.guard_response(&response).await.expect("ran"),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn request_id_transform_mints_and_propagates() {
        let plugin = RequestIdTransformPlugin;
        let mut context = ctx(serde_json::Value::Null);
        plugin.transform_request(&mut context).await.expect("ok");
        assert!(
            context
                .injected_headers
                .get("x-request-id")
                .is_some_and(|value| !value.is_empty())
        );
        // Existing request ids are propagated, not replaced.
        let mut propagated = ctx(serde_json::Value::Null);
        propagated
            .headers
            .insert("x-request-id".to_owned(), "from-client".to_owned());
        plugin.transform_request(&mut propagated).await.expect("ok");
        assert!(propagated.injected_headers.is_empty());

        let mut response = ResponseContext {
            status: 200,
            headers: BTreeMap::from([("x-request-id".to_owned(), "from-client".to_owned())]),
            injected_headers: BTreeMap::new(),
            config: serde_json::Value::Null,
        };
        plugin.transform_response(&mut response).await.expect("ok");
        assert_eq!(
            response
                .injected_headers
                .get("x-request-id")
                .map(String::as_str),
            Some("from-client")
        );
    }

    #[tokio::test]
    async fn built_in_registries_resolve_only_builtins() {
        let resolver = Arc::new(TableResolver::default()) as Arc<dyn CredentialResolver>;
        let auth = auth_registry_with_builtins(resolver, Duration::from_secs(300), 10_000);
        assert_eq!(auth.ids().len(), 4);
        for id in [
            ids::AUTH_NOOP,
            ids::AUTH_APIKEY,
            ids::AUTH_OAUTH2_FORM,
            ids::AUTH_OAUTH2_BASIC,
        ] {
            assert!(auth.get(id).is_some(), "{id} must be registered");
        }
        for catalog in ids::CATALOG_ONLY_AUTH {
            assert!(auth.get(catalog).is_none(), "{catalog} must not resolve");
        }
        let guards = guard_registry_with_builtins();
        assert!(guards.get(ids::GUARD_REQUIRED_HEADERS).is_some());
        for catalog in ids::CATALOG_ONLY_GUARD {
            assert!(guards.get(catalog).is_none());
        }
        let transforms = transform_registry_with_builtins();
        assert!(transforms.get(ids::TRANSFORM_REQUEST_ID).is_some());
        for catalog in ids::CATALOG_ONLY_TRANSFORM {
            assert!(transforms.get(catalog).is_none());
        }
    }
}
