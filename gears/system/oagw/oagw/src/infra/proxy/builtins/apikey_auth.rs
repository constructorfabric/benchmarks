//! The `apikey` auth plugin (ADR-0008, DESIGN.md §3.1).
//!
//! Configuration (all keys optional except one value reference):
//!
//! ```json
//! { "header_name": "X-API-Key", "header_value_ref": "cred://...",
//!   "query_name": "api_key", "query_value_ref": "cred://..." }
//! ```
//!
//! The key is resolved through [`SecretResolver`] and injected either as a
//! request header (default) or as a query parameter.

use async_trait::async_trait;
use axum::http::header::HeaderName;
use axum::http::header::HeaderValue;
use serde_json::Value;

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext, SecretResolver};
use std::sync::Arc;

const INSTANCE: &str = "cf.core.oagw.apikey.v1";
const PLUGIN_TYPE: &str = "cf.core.oagw.auth_plugin.v1";
const DEFAULT_HEADER: &str = "x-api-key";

/// Auth plugin that injects a static API key.
pub struct ApiKeyAuthPlugin {
    secrets: Arc<dyn SecretResolver>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin over `secrets`.
    #[must_use]
    pub fn new(secrets: Arc<dyn SecretResolver>) -> Self {
        Self { secrets }
    }
}

fn config_str<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        INSTANCE
    }

    fn plugin_type(&self) -> &str {
        PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let reference = config_str(&ctx.config, "header_value_ref")
            .or_else(|| config_str(&ctx.config, "query_value_ref"))
            .ok_or_else(|| {
                PluginError::invalid(
                    "CONFIG_INVALID",
                    "apikey auth plugin requires `header_value_ref` or `query_value_ref`",
                )
            })?;
        let key = self.secrets.resolve(reference).await?;
        if key.expose().is_empty() {
            return Err(PluginError::unauthorized("the configured api key is empty"));
        }
        if let Some(query_name) = config_str(&ctx.config, "query_name") {
            ctx.query = append_query(&ctx.query, query_name, key.expose());
            ctx.set_attribute("oagw.auth.plugin", INSTANCE);
            return Ok(());
        }
        let name = config_str(&ctx.config, "header_name").unwrap_or(DEFAULT_HEADER);
        let parsed =
            HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).map_err(|_| {
                PluginError::invalid("CONFIG_INVALID", format!("invalid header name {name:?}"))
            })?;
        let value = HeaderValue::from_str(key.expose()).map_err(|_| {
            PluginError::invalid("CONFIG_INVALID", "api key is not a valid header value")
        })?;
        ctx.headers.insert(parsed, value);
        ctx.set_attribute("oagw.auth.plugin", INSTANCE);
        Ok(())
    }
}

/// Appends `name=value` to a query string.
fn append_query(query: &str, name: &str, value: &str) -> String {
    let pair = form_urlencoded::Serializer::new(String::new())
        .append_pair(name, value)
        .finish();
    if query.is_empty() {
        pair
    } else {
        format!("{query}&{pair}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::{LiteralSecretResolver, Secret};
    use bytes::Bytes;
    use uuid::Uuid;

    fn ctx(config: Value) -> RequestContext {
        RequestContext {
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            query: String::new(),
            headers: axum::http::HeaderMap::new(),
            body: Bytes::new(),
            config,
            attributes: Default::default(),
        }
    }

    #[tokio::test]
    async fn injects_the_default_header() {
        let mut ctx = ctx(serde_json::json!({ "header_value_ref": "sk-live-123" }));
        ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
            .authenticate(&mut ctx)
            .await
            .expect("injected");
        assert_eq!(
            ctx.headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("sk-live-123")
        );
    }

    #[tokio::test]
    async fn injects_a_custom_header_and_a_query_parameter() {
        let mut header_ctx = ctx(serde_json::json!({
            "header_name": "X-Key", "header_value_ref": "secret-1"
        }));
        ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
            .authenticate(&mut header_ctx)
            .await
            .expect("injected");
        assert!(header_ctx.headers.get("x-key").is_some());
        assert!(header_ctx.query.is_empty());

        let mut query_ctx = ctx(serde_json::json!({
            "query_name": "key", "query_value_ref": "secret 2"
        }));
        ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
            .authenticate(&mut query_ctx)
            .await
            .expect("injected");
        assert_eq!(query_ctx.query, "key=secret+2");
        assert!(query_ctx.headers.is_empty());
    }

    #[tokio::test]
    async fn a_missing_value_reference_is_a_configuration_error() {
        let mut ctx = ctx(serde_json::json!({ "header_name": "X-Key" }));
        let error = ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
            .authenticate(&mut ctx)
            .await
            .expect_err("no value reference");
        assert_eq!(error.status, 400);
        assert_eq!(error.error_code, "CONFIG_INVALID");
    }

    #[tokio::test]
    async fn an_unresolvable_reference_is_not_found() {
        let mut ctx = ctx(serde_json::json!({ "header_value_ref": "cred://missing" }));
        let error = ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
            .authenticate(&mut ctx)
            .await
            .expect_err("no credential store");
        assert_eq!(error.status, 500);
        assert_eq!(error.error_code, "SECRET_NOT_FOUND");
    }

    #[tokio::test]
    async fn secrets_are_not_debug_printed() {
        let secret = Secret::new("sk-live-123");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
    }
}
