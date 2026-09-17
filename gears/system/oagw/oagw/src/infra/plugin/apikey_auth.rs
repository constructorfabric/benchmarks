//! `ApiKeyAuthPlugin` — static API key injection (ADR-0002).
//!
//! Configuration (`ctx.config` keys):
//!
//! | Key | Required | Description |
//! |---|---|---|
//! | `key_ref` | one of `key_ref`/`key` | `cred://` reference resolved via CredStore |
//! | `key` | one of `key_ref`/`key` | literal key (single-tenant/dev use only) |
//! | `in` | no | `header` (default) or `query` |
//! | `header` | no | Header name, default `x-api-key` |
//! | `query_param` | no | Query parameter name, default `api_key` |

use async_trait::async_trait;
use toolkit_utils::SecretString;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, RequestContext};

/// Injects an API key as a header or query parameter.
#[derive(Clone)]
pub struct ApiKeyAuthPlugin {
    credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
}

use std::sync::Arc;

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin")
            .field("has_credstore", &self.credstore.is_some())
            .finish()
    }
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin; credential lookups need a CredStore client.
    #[must_use]
    pub fn new(credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>) -> Self {
        Self { credstore }
    }

    fn config_string(config: &serde_json::Value, keys: &[&str]) -> Option<String> {
        config.as_object().map(|obj| {
            keys.iter().find_map(|key| {
                obj.get(*key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
        }).flatten()
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        APIKEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> OagwResult<()> {
        let config = ctx.config.clone();
        let location = Self::config_string(&config, &["in", "location"])
            .unwrap_or_else(|| "header".to_owned())
            .to_ascii_lowercase();
        let header_name = Self::config_string(&config, &["header", "header_name"])
            .unwrap_or_else(|| "x-api-key".to_owned());
        let query_param = Self::config_string(&config, &["query_param", "query_param_name"])
            .unwrap_or_else(|| "api_key".to_owned());

        let key = if let Some(literal) = Self::config_string(&config, &["key"]) {
            SecretString::new(literal)
        } else if let Some(key_ref) = Self::config_string(&config, &["key_ref", "secret_ref"]) {
            let credstore = self.credstore.as_ref().ok_or_else(|| {
                OagwError::SecretNotFound(format!(
                    "credential '{key_ref}' cannot be resolved: credstore unavailable"
                ))
            })?;
            let reference = credstore_sdk::SecretRef::new(key_ref.clone()).map_err(|err| {
                OagwError::Validation(format!("invalid credential reference: {err}"))
            })?;
            let resolved = credstore
                .get(&ctx.security_context, &reference)
                .await
                .map_err(|err| {
                    OagwError::SecretNotFound(format!("credential '{key_ref}' lookup failed: {err}"))
                })?;
            match resolved {
                Some(response) => {
                    SecretString::new(String::from_utf8_lossy(response.value.as_bytes()).to_string())
                }
                None => {
                    return Err(OagwError::SecretNotFound(format!(
                        "credential '{key_ref}' not found"
                    )));
                }
            }
        } else {
            return Err(OagwError::AuthenticationFailed(
                "apikey plugin requires 'key_ref' or 'key' in its configuration".to_owned(),
            ));
        };

        match location.as_str() {
            "query" => {
                ctx.query.retain(|(name, _)| name != &query_param);
                ctx.query.push((query_param, key.expose().to_owned()));
            }
            _ => {
                if let Ok(value) = http::HeaderValue::from_str(key.expose()) {
                    if let Ok(name) = http::HeaderName::try_from(header_name.as_str()) {
                        ctx.headers.insert(name, value);
                    } else {
                        return Err(OagwError::Validation(format!(
                            "apikey plugin header name '{header_name}' is invalid"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::request_context;

    #[tokio::test]
    async fn injects_literal_key_header() {
        let mut ctx = request_context();
        ctx.config = serde_json::json!({ "key": "sk-test", "header": "x-api-key" });
        ApiKeyAuthPlugin::new(None)
            .authenticate(&mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-test");
    }

    #[tokio::test]
    async fn injects_query_parameter() {
        let mut ctx = request_context();
        ctx.config = serde_json::json!({ "key": "sk-test", "in": "query" });
        ApiKeyAuthPlugin::new(None)
            .authenticate(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            ctx.query,
            vec![("api_key".to_owned(), "sk-test".to_owned())]
        );
    }

    #[tokio::test]
    async fn missing_config_fails_authentication() {
        let mut ctx = request_context();
        let err = ApiKeyAuthPlugin::new(None)
            .authenticate(&mut ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, OagwError::AuthenticationFailed(_)));
    }
}
