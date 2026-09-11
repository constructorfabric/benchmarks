//! `cf.core.oagw.apikey.v1` — API key injection into a header or a query
//! parameter.
//!
//! Binding configuration:
//!
//! | Key          | Required | Description                                        |
//! |--------------|----------|----------------------------------------------------|
//! | `secret_ref` | yes      | `cred://` reference holding the key                |
//! | `location`   | no       | `header` (default) or `query`                      |
//! | `name`       | no       | Header / parameter name (default `Authorization`)  |
//! | `prefix`     | no       | Literal prefix, e.g. `Bearer ` (header only)       |

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginError, PluginResult, RequestContext};

use super::credstore::resolve_secret;

/// Default header used when the binding does not name one.
const DEFAULT_HEADER: &str = "authorization";
/// Default query parameter used when `location` is `query`.
const DEFAULT_QUERY_PARAM: &str = "api_key";

/// Injects a static API key resolved from the credstore.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Wire the plugin to the credential store.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        APIKEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult {
        let secret_ref = ctx.config_str("secret_ref").ok_or_else(|| {
            PluginError::Rejected(DomainError::authentication_failed(
                "apikey auth plugin requires a 'secret_ref' config key",
            ))
        })?;
        let location = ctx.config_str("location").unwrap_or("header");
        let name = ctx.config_str("name").map(str::to_owned);
        let prefix = ctx.config_str("prefix").unwrap_or_default().to_owned();

        let key = resolve_secret(&self.credstore, &ctx.security_context, secret_ref).await?;

        match location {
            "header" => {
                let header = name.unwrap_or_else(|| DEFAULT_HEADER.to_owned());
                ctx.headers.set(&header, format!("{prefix}{key}"));
            }
            "query" => {
                let param = name.unwrap_or_else(|| DEFAULT_QUERY_PARAM.to_owned());
                // Replace rather than append so a retry cannot accumulate keys.
                ctx.query.retain(|(k, _)| k != &param);
                ctx.query.push((param, key));
            }
            other => {
                return Err(PluginError::Rejected(DomainError::validation(format!(
                    "apikey auth plugin 'location' must be 'header' or 'query', got '{other}'"
                ))));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::{mock_credstore, request_context};
    use serde_json::json;

    fn config(pairs: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        pairs.as_object().cloned().unwrap_or_default()
    }

    #[tokio::test]
    async fn injects_into_the_default_header() {
        let plugin = ApiKeyAuthPlugin::new(mock_credstore(vec![("openai-key", "sk-test")]));
        let mut ctx = request_context(config(json!({ "secret_ref": "cred://openai-key" })));
        plugin.authenticate(&mut ctx).await.expect("injects");
        assert_eq!(ctx.headers.get("authorization"), Some("sk-test"));
    }

    #[tokio::test]
    async fn honours_custom_header_and_prefix() {
        let plugin = ApiKeyAuthPlugin::new(mock_credstore(vec![("openai-key", "sk-test")]));
        let mut ctx = request_context(config(json!({
            "secret_ref": "cred://openai-key",
            "name": "X-Api-Key",
            "prefix": "Bearer "
        })));
        plugin.authenticate(&mut ctx).await.expect("injects");
        assert_eq!(ctx.headers.get("x-api-key"), Some("Bearer sk-test"));
    }

    #[tokio::test]
    async fn injects_into_the_query_string() {
        let plugin = ApiKeyAuthPlugin::new(mock_credstore(vec![("openai-key", "sk-test")]));
        let mut ctx = request_context(config(json!({
            "secret_ref": "cred://openai-key",
            "location": "query",
            "name": "key"
        })));
        ctx.query.push(("key".to_owned(), "stale".to_owned()));
        plugin.authenticate(&mut ctx).await.expect("injects");
        assert_eq!(
            ctx.query,
            vec![("key".to_owned(), "sk-test".to_owned())],
            "an existing parameter of the same name is replaced, not duplicated"
        );
    }

    #[tokio::test]
    async fn missing_secret_ref_is_a_401() {
        let plugin = ApiKeyAuthPlugin::new(mock_credstore(Vec::new()));
        let mut ctx = request_context(serde_json::Map::new());
        let err = plugin
            .authenticate(&mut ctx)
            .await
            .expect_err("no secret_ref");
        match err {
            PluginError::Rejected(domain) => assert_eq!(domain.status(), 401),
            PluginError::Internal(msg) => panic!("unexpected internal error: {msg}"),
        }
    }

    #[tokio::test]
    async fn unknown_location_is_a_400() {
        let plugin = ApiKeyAuthPlugin::new(mock_credstore(vec![("openai-key", "sk-test")]));
        let mut ctx = request_context(config(json!({
            "secret_ref": "cred://openai-key",
            "location": "cookie"
        })));
        let err = plugin.authenticate(&mut ctx).await.expect_err("bad location");
        match err {
            PluginError::Rejected(domain) => assert_eq!(domain.status(), 400),
            PluginError::Internal(msg) => panic!("unexpected internal error: {msg}"),
        }
    }

    #[tokio::test]
    async fn the_key_never_appears_in_the_error_path() {
        let plugin = ApiKeyAuthPlugin::new(mock_credstore(Vec::new()));
        let mut ctx = request_context(config(json!({ "secret_ref": "cred://openai-key" })));
        let err = plugin.authenticate(&mut ctx).await.expect_err("absent");
        let rendered = format!("{err:?}");
        assert!(!rendered.contains("sk-"));
    }
}
