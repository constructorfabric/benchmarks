//! Built-in plugin implementations (ADR-0002, ADR-0008, ADR-0009) and the
//! in-process registries that resolve named plugin identifiers.
//!
//! Custom (Starlark) plugins are stored and validated but not executable in
//! this MVP build (no Starlark runtime in the dependency set); binding one
//! fails closed with `503 PluginNotFound`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use toolkit_http::HttpClientConfig;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{
    AuthPlugin, CODE_REQUIRED_HEADER_MISSING, GuardPlugin, PluginError, RequestCtx, ResponseCtx,
    TransformPlugin,
};
use crate::gts;
use crate::infra::oauth2_auth::OAuth2ClientCredAuthPlugin;
use crate::infra::secrets::lookup_secret;

/// Registry of named auth plugins.
#[derive(Clone)]
pub struct AuthPluginRegistry {
    builtins: Arc<BTreeMap<String, Arc<dyn AuthPlugin>>>,
}

impl AuthPluginRegistry {
    /// Build the registry with the default built-ins.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_http_config: Option<HttpClientConfig>,
        token_cache: TokenCacheConfig,
        allow_insecure_token_endpoint: bool,
    ) -> Self {
        let mut builtins: BTreeMap<String, Arc<dyn AuthPlugin>> = BTreeMap::new();
        builtins.insert(gts::AUTH_NOOP_ID.to_owned(), Arc::new(NoopAuthPlugin));
        builtins.insert(
            gts::AUTH_APIKEY_ID.to_owned(),
            Arc::new(ApiKeyAuthPlugin {
                credstore: credstore.clone(),
            }),
        );
        let form = OAuth2ClientCredAuthPlugin::new(
            gts::AUTH_OAUTH2_ID,
            credstore.clone(),
            toolkit_auth::oauth2::ClientAuthMethod::Form,
            token_cache,
            token_http_config.clone(),
            allow_insecure_token_endpoint,
        );
        builtins.insert(gts::AUTH_OAUTH2_ID.to_owned(), Arc::new(form));
        let basic = OAuth2ClientCredAuthPlugin::new(
            gts::AUTH_OAUTH2_BASIC_ID,
            credstore,
            toolkit_auth::oauth2::ClientAuthMethod::Basic,
            token_cache,
            token_http_config,
            allow_insecure_token_endpoint,
        );
        builtins.insert(gts::AUTH_OAUTH2_BASIC_ID.to_owned(), Arc::new(basic));
        Self {
            builtins: Arc::new(builtins),
        }
    }

    /// Resolve a named auth plugin identifier. `None` for catalog-only ids
    /// (`basic.v1`, `bearer.v1`) and for custom (Starlark) plugins.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.builtins.get(id).cloned()
    }
}

/// Registry of named guard plugins.
#[derive(Clone)]
pub struct GuardPluginRegistry {
    builtins: Arc<BTreeMap<String, Arc<dyn GuardPlugin>>>,
}

impl GuardPluginRegistry {
    /// Build the registry with the default built-ins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut builtins: BTreeMap<String, Arc<dyn GuardPlugin>> = BTreeMap::new();
        builtins.insert(
            gts::GUARD_REQUIRED_HEADERS_ID.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self {
            builtins: Arc::new(builtins),
        }
    }

    /// Resolve a named guard plugin identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.builtins.get(id).cloned()
    }
}

/// Registry of named transform plugins.
#[derive(Clone)]
pub struct TransformPluginRegistry {
    builtins: Arc<BTreeMap<String, Arc<dyn TransformPlugin>>>,
}

impl TransformPluginRegistry {
    /// Build the registry with the default built-ins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut builtins: BTreeMap<String, Arc<dyn TransformPlugin>> = BTreeMap::new();
        builtins.insert(
            gts::TRANSFORM_REQUEST_ID_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );
        Self {
            builtins: Arc::new(builtins),
        }
    }

    /// Resolve a named transform plugin identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.builtins.get(id).cloned()
    }
}

/// No-op auth plugin — no credential injection.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        gts::AUTH_NOOP_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}

/// API-key auth plugin (injects a resolved key as a header).
///
/// Config keys: `header_name` (default `X-API-Key`), `api_key_ref` (required,
/// `cred://...`); `api_key_ref` may alternatively be spelled `secret_ref`.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

const DEFAULT_HEADER_NAME: &str = "X-API-Key";

impl ApiKeyAuthPlugin {
    fn header_name(config: &BTreeMap<String, serde_json::Value>) -> String {
        config
            .get("header_name")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map_or_else(|| DEFAULT_HEADER_NAME.to_owned(), str::to_owned)
    }

    fn key_ref(config: &BTreeMap<String, serde_json::Value>) -> Result<String, PluginError> {
        let value = config
            .get("api_key_ref")
            .or_else(|| config.get("secret_ref"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        value.ok_or_else(|| PluginError::Config {
            detail: "api_key_ref (cred://...) is required".to_owned(),
        })
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        gts::AUTH_APIKEY_ID
    }

    async fn authenticate(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let reference = Self::key_ref(ctx.config)?;
        let key = lookup_secret(&self.credstore, ctx.security, &reference).await?;
        let header_name = Self::header_name(ctx.config);
        let header_value =
            http::HeaderValue::from_str(key.expose()).map_err(|e| PluginError::Internal {
                diagnostic: format!("invalid api key header value: {e}"),
            })?;
        if let Ok(name) =
            http::header::HeaderName::from_lowercase(header_name.to_ascii_lowercase().as_bytes())
        {
            ctx.headers.insert(name, header_value);
        }
        Ok(())
    }
}

/// Required-headers guard plugin (ADR-0009).
///
/// Config keys: `required_request_headers`, `required_response_headers`
/// (comma-separated). Fail-open when absent or blank. Only the first missing
/// header is reported per rejection.
pub struct RequiredHeadersGuardPlugin;

fn split_csv(value: Option<&serde_json::Value>) -> Vec<String> {
    let Some(value) = value.and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn first_missing<'a>(names: &'a [String], headers: &http::HeaderMap) -> Option<&'a str> {
    names
        .iter()
        .find(|name| !headers.contains_key(name.as_str()))
        .map(String::as_str)
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        gts::GUARD_REQUIRED_HEADERS_ID
    }

    async fn guard_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let names = split_csv(ctx.config.get("required_request_headers"));
        let Some(missing) = first_missing(&names, ctx.headers) else {
            return Ok(());
        };
        Err(PluginError::reject(
            400,
            CODE_REQUIRED_HEADER_MISSING,
            format!("required request header {missing:?} is missing"),
        ))
    }

    async fn guard_response(&self, ctx: &mut ResponseCtx<'_>) -> Result<(), PluginError> {
        let names = split_csv(ctx.config.get("required_response_headers"));
        let Some(missing) = first_missing(&names, ctx.headers) else {
            return Ok(());
        };
        Err(PluginError::reject(
            502,
            CODE_REQUIRED_HEADER_MISSING,
            format!("required response header {missing:?} is missing"),
        ))
    }
}

/// X-Request-ID propagation transform plugin.
pub struct RequestIdTransformPlugin;

/// Return the existing `x-request-id` header value, or mint and record a
/// fresh one. Used by the transform plugin in both phases and by the data
/// plane when it seeds the id at proxy entry, so a request keeps a single
/// stable id end to end.
///
/// # Errors
///
/// Returns [`PluginError::Internal`] when the minted id cannot be encoded as
/// a header value (practically unreachable for a UUID v4).
pub fn ensure_request_id(headers: &mut http::HeaderMap) -> Result<http::HeaderValue, PluginError> {
    if let Some(value) = headers.get("x-request-id") {
        return Ok(value.clone());
    }
    let value = http::HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).map_err(|e| {
        PluginError::Internal {
            diagnostic: format!("invalid request id: {e}"),
        }
    })?;
    headers.insert("x-request-id", value.clone());
    Ok(value)
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        gts::TRANSFORM_REQUEST_ID_ID
    }

    async fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        ensure_request_id(ctx.headers)?;
        Ok(())
    }

    async fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> Result<(), PluginError> {
        ensure_request_id(ctx.headers)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_headers_split_skips_blanks() {
        let mut config = BTreeMap::new();
        config.insert(
            "required_request_headers".to_owned(),
            serde_json::Value::String("x-a, , x-b".to_owned()),
        );
        config.insert(
            "required_response_headers".to_owned(),
            serde_json::Value::String(String::new()),
        );
        assert_eq!(
            split_csv(config.get("required_request_headers")),
            vec!["x-a".to_owned(), "x-b".to_owned()]
        );
        assert!(split_csv(config.get("required_response_headers")).is_empty());
        assert!(split_csv(config.get("absent")).is_empty());
    }

    #[test]
    fn api_key_header_defaults() {
        let mut config = BTreeMap::new();
        config.insert(
            "api_key_ref".to_owned(),
            serde_json::Value::String("cred://k".to_owned()),
        );
        assert_eq!(ApiKeyAuthPlugin::header_name(&config), DEFAULT_HEADER_NAME);
        config.insert(
            "header_name".to_owned(),
            serde_json::Value::String("Authorization".to_owned()),
        );
        assert_eq!(ApiKeyAuthPlugin::header_name(&config), "Authorization");
        assert!(ApiKeyAuthPlugin::key_ref(&config).is_ok());
        let empty = BTreeMap::new();
        assert!(ApiKeyAuthPlugin::key_ref(&empty).is_err());
    }

    #[test]
    fn noop_and_named_registries_resolve_only_builtins() {
        let auth = AuthPluginRegistry::with_builtins(
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
            None,
            crate::config::TokenCacheConfig::default(),
            false,
        );
        assert!(auth.resolve(gts::AUTH_NOOP_ID).is_some());
        assert!(auth.resolve(gts::AUTH_APIKEY_ID).is_some());
        assert!(auth.resolve(gts::AUTH_OAUTH2_ID).is_some());
        assert!(auth.resolve(gts::AUTH_OAUTH2_BASIC_ID).is_some());
        assert!(auth.resolve(gts::AUTH_BASIC_ID).is_none());
        assert!(
            auth.resolve("gts.cf.core.oagw.auth_plugin.v1~00000000-0000-0000-0000-000000000000")
                .is_none()
        );
        let guards = GuardPluginRegistry::with_builtins();
        assert!(guards.resolve(gts::GUARD_REQUIRED_HEADERS_ID).is_some());
        assert!(guards.resolve(gts::GUARD_TIMEOUT_ID).is_none());
        let transforms = TransformPluginRegistry::with_builtins();
        assert!(transforms.resolve(gts::TRANSFORM_REQUEST_ID_ID).is_some());
        assert!(transforms.resolve(gts::TRANSFORM_LOGGING_ID).is_none());
    }
}
