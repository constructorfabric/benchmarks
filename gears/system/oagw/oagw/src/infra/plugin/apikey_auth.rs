// Updated: 2026-09-01 by Constructor Tech
//! `apikey` auth plugin (PRD `cpt-cf-oagw-fr-auth-injection`).
//!
//! Injects a static API key into the outbound request. The key itself lives in
//! the credstore and is referenced by key, so it never appears in the OAGW
//! configuration, never in a log line, and never in an error message.
//!
//! Config keys (`ctx.config`):
//!
//! | Key | Required | Description |
//! |---|---|---|
//! | `api_key_ref` | yes (or `key_ref`) | credstore key holding the API key |
//! | `header` | no | header to inject into; default `Authorization` |
//! | `scheme` | no | value prefix; default `Bearer` when the header is `Authorization`, none otherwise |
//! | `in` | no | `header` (default) or `query` |
//! | `query` | no | query parameter name when `in` is `query` |

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// Header the key is injected into when the configuration names no other.
pub const DEFAULT_HEADER: &str = "authorization";
/// Query parameter used when the plugin is configured for query injection.
pub const DEFAULT_QUERY_PARAM: &str = "api_key";

/// The API-key injection plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Option<Arc<dyn CredStoreClientV1>>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credential store handle is opaque and carries no secrets, but
        // there is nothing useful to say about it either.
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    #[must_use]
    pub fn new(credstore: Option<Arc<dyn CredStoreClientV1>>) -> Self {
        Self { credstore }
    }

    /// Read a config string, tolerating a JSON string or a bare scalar.
    fn config_str(config: &serde_json::Value, key: &str) -> Option<String> {
        config.get(key).and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
    }

    /// Strip an optional `cred://` scheme prefix: the credstore's own key
    /// namespace is the bare `[a-zA-Z0-9_-]+` form.
    fn bare_key(raw: &str) -> String {
        raw.trim()
            .trim_start_matches("cred://")
            .trim_start_matches("secret://")
            .to_owned()
    }

    async fn resolve_secret(&self, ctx: &RequestContext, key: &str) -> Result<String, PluginError> {
        let Some(credstore) = &self.credstore else {
            return Err(PluginError::Infrastructure(
                "no credential store is available to resolve the API key reference".to_owned(),
            ));
        };
        let reference = SecretRef::new(Self::bare_key(key)).map_err(|e| PluginError::Rejected {
            status: http::StatusCode::BAD_REQUEST,
            code: "INVALID_SECRET_REF".to_owned(),
            message: e.to_string(),
        })?;
        match credstore.get(&ctx.security_context, &reference).await {
            Ok(Some(found)) => Ok(String::from_utf8_lossy(found.value.as_bytes()).into_owned()),
            Ok(None) => Err(PluginError::Infrastructure(format!(
                "referenced secret '{key}' was not found"
            ))),
            Err(err) => Err(PluginError::Infrastructure(format!(
                "credential store lookup failed: {err}"
            ))),
        }
    }
}

impl Default for ApiKeyAuthPlugin {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        "apikey"
    }

    fn plugin_type(&self) -> &'static str {
        crate::gts::AUTH_APIKEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = ctx
            .config
            .get("auth")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let Some(key_ref) = Self::config_str(&config, "api_key_ref")
            .or_else(|| Self::config_str(&config, "key_ref"))
        else {
            return Err(PluginError::Rejected {
                status: http::StatusCode::BAD_REQUEST,
                code: "MISSING_API_KEY_REF".to_owned(),
                message: "the apikey auth plugin requires `api_key_ref` in its config".to_owned(),
            });
        };
        let key = self.resolve_secret(ctx, &key_ref).await?;

        let location = Self::config_str(&config, "in").unwrap_or_else(|| "header".to_owned());
        if location == "query" {
            let param = Self::config_str(&config, "query")
                .unwrap_or_else(|| DEFAULT_QUERY_PARAM.to_owned());
            let mut pairs: Vec<(String, String)> = form_urlencoded::parse(ctx.query.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            pairs.push((param.clone(), key));
            ctx.query = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs)
                .finish();
            return Ok(());
        }

        let header = Self::config_str(&config, "header")
            .map(|h| h.to_ascii_lowercase())
            .unwrap_or_else(|| DEFAULT_HEADER.to_owned());
        let name =
            http::HeaderName::from_bytes(header.as_bytes()).map_err(|_| PluginError::Rejected {
                status: http::StatusCode::BAD_REQUEST,
                code: "INVALID_HEADER_NAME".to_owned(),
                message: format!("'{header}' is not a valid header name"),
            })?;
        let value = match Self::config_str(&config, "scheme") {
            Some(scheme) if scheme.is_empty() => key,
            Some(scheme) => format!("{scheme} {key}"),
            // No explicit scheme: `Authorization` gets `Bearer`, everything
            // else carries the raw key.
            None if header == DEFAULT_HEADER => format!("Bearer {key}"),
            None => key,
        };
        if let Ok(parsed) = http::HeaderValue::from_str(&value) {
            ctx.headers.insert(name, parsed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::MockCredStoreClient;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn injects_a_bearer_key_from_the_credstore() {
        let plugin = ApiKeyAuthPlugin::new(Some(Arc::new(MockCredStoreClient::with_secrets(
            vec![("openai-key".to_owned(), "sk-123".to_owned())],
        ))));
        let mut ctx = crate::infra::plugin::test_support::request_context();
        ctx.config.insert(
            "auth".to_owned(),
            serde_json::json!({ "api_key_ref": "openai-key" }),
        );
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer sk-123"
        );
    }

    #[tokio::test]
    async fn honours_a_cred_prefixed_reference_and_custom_header() {
        let plugin = ApiKeyAuthPlugin::new(Some(Arc::new(MockCredStoreClient::with_secrets(
            vec![("openai-key".to_owned(), "sk-123".to_owned())],
        ))));
        let mut ctx = crate::infra::plugin::test_support::request_context();
        ctx.config.insert(
            "auth".to_owned(),
            serde_json::json!({ "api_key_ref": "cred://openai-key", "header": "x-api-key", "scheme": "" }),
        );
        plugin.authenticate(&mut ctx).await.unwrap();
        assert!(ctx.headers.get(http::header::AUTHORIZATION).is_none());
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-123");
    }

    #[tokio::test]
    async fn can_inject_into_the_query_string() {
        let plugin = ApiKeyAuthPlugin::new(Some(Arc::new(MockCredStoreClient::with_secrets(
            vec![("k".to_owned(), "v1".to_owned())],
        ))));
        let mut ctx = crate::infra::plugin::test_support::request_context();
        ctx.query = "a=1".to_owned();
        ctx.config.insert(
            "auth".to_owned(),
            serde_json::json!({ "api_key_ref": "k", "in": "query", "query": "key" }),
        );
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.query, "a=1&key=v1");
    }

    #[tokio::test]
    async fn missing_reference_is_rejected() {
        let plugin = ApiKeyAuthPlugin::new(None);
        let mut ctx = crate::infra::plugin::test_support::request_context();
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(
            err,
            PluginError::Rejected {
                status: http::StatusCode::BAD_REQUEST,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unknown_secret_is_an_infrastructure_failure() {
        let plugin = ApiKeyAuthPlugin::new(Some(Arc::new(MockCredStoreClient::empty())));
        let mut ctx = crate::infra::plugin::test_support::request_context();
        ctx.config.insert(
            "auth".to_owned(),
            serde_json::json!({ "api_key_ref": "nope" }),
        );
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::Infrastructure(_)));
    }

    #[allow(dead_code)]
    fn _unused(_m: BTreeMap<String, serde_json::Value>) {}
}
