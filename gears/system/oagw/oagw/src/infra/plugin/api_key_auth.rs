//! The `apikey` auth plugin: injects an API key from CredStore.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::SecretRef;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::BUILTIN_AUTH_APIKEY;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// Header the API key is injected into when the config does not name one.
pub const DEFAULT_HEADER: &str = "x-api-key";

/// Injects a CredStore-resolved API key into a header.
#[derive(Clone)]
pub struct ApiKeyAuth {
    credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
}

impl std::fmt::Debug for ApiKeyAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuth").finish_non_exhaustive()
    }
}

impl Default for ApiKeyAuth {
    fn default() -> Self {
        Self { credstore: None }
    }
}

impl ApiKeyAuth {
    /// Builds the plugin against a CredStore client.
    pub fn new(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { credstore: Some(credstore) }
    }
}

/// The plugin's declared configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyConfig {
    /// `cred://` reference of the key.
    pub key_ref: String,
    /// Header the key is written to.
    pub header: String,
    /// Query parameter the key is written to, when configured.
    pub query: Option<String>,
}

impl ApiKeyConfig {
    /// Reads the plugin configuration, failing loud when it is unusable.
    pub fn from_value(value: Option<&serde_json::Value>) -> Result<Self, PluginError> {
        let value = value.cloned().unwrap_or(serde_json::Value::Null);
        let key_ref = value
            .get("key_ref")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                PluginError::failure(
                    BUILTIN_AUTH_APIKEY,
                    "config requires `key_ref`, a cred:// reference of the API key",
                )
            })?;
        let header = value
            .get("header")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_HEADER.to_string());
        let query = value
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(Self { key_ref: key_ref.to_string(), header, query })
    }
}

/// Strips the `cred://` scheme from a reference.
pub fn strip_scheme(reference: &str) -> &str {
    reference
        .strip_prefix("cred://")
        .unwrap_or(reference)
        .trim_start_matches('/')
}

#[async_trait]
impl AuthPlugin for ApiKeyAuth {
    fn id(&self) -> &str {
        BUILTIN_AUTH_APIKEY
    }

    fn plugin_type(&self) -> &str {
        crate::domain::gts_helpers::AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = ApiKeyConfig::from_value(ctx.plugin_config.as_ref())?;
        let key = self.resolve(&config, ctx).await?;
        ctx.set_header(config.header.clone(), key.clone());
        if let Some(parameter) = config.query.as_deref() {
            let appended = match ctx.query.as_deref().filter(|q| !q.is_empty()) {
                Some(existing) => format!("{existing}&{parameter}={key}"),
                None => format!("{parameter}={key}"),
            };
            ctx.query = Some(appended);
        }
        Ok(())
    }
}

impl ApiKeyAuth {
    async fn resolve(&self, config: &ApiKeyConfig, ctx: &RequestContext) -> Result<String, PluginError> {
        let client = self.credstore.as_ref().ok_or_else(|| {
            PluginError::failure(BUILTIN_AUTH_APIKEY, "no CredStore client is wired to the gear")
        })?;
        let reference = SecretRef::new(strip_scheme(&config.key_ref)).map_err(|err| {
            PluginError::failure(BUILTIN_AUTH_APIKEY, format!("invalid credential reference: {err}"))
        })?;
        let security = super::security_context::service_context(ctx);
        match client.get(&security, &reference).await {
            Ok(Some(response)) => {
                let value = String::from_utf8_lossy(response.value.as_bytes()).to_string();
                if value.is_empty() {
                    Err(PluginError::failure(
                        BUILTIN_AUTH_APIKEY,
                        format!("credential `{}` resolved to an empty value", config.key_ref),
                    ))
                } else {
                    Ok(value)
                }
            }
            Ok(None) => Err(PluginError::Reject(DomainError::SecretNotFound {
                detail: format!("credential `{}` does not exist", config.key_ref),
                plugin_id: Some(BUILTIN_AUTH_APIKEY.to_string()),
            })),
            Err(err) => Err(PluginError::Reject(DomainError::AuthenticationFailed {
                detail: format!("credential lookup failed: {err}"),
                plugin_id: Some(BUILTIN_AUTH_APIKEY.to_string()),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_requires_a_key_reference() {
        assert!(ApiKeyConfig::from_value(None).is_err());
        assert!(ApiKeyConfig::from_value(Some(&serde_json::json!({}))).is_err());
    }

    #[test]
    fn the_config_defaults_to_the_x_api_key_header() {
        let config =
            ApiKeyConfig::from_value(Some(&serde_json::json!({ "key_ref": "cred://alpha" })))
                .unwrap();
        assert_eq!(config.key_ref, "cred://alpha");
        assert_eq!(config.header, DEFAULT_HEADER);
        assert_eq!(config.query, None);
    }

    #[test]
    fn the_config_accepts_an_explicit_header_and_query() {
        let config = ApiKeyConfig::from_value(Some(&serde_json::json!({
            "key_ref": "alpha",
            "header": "x-vendor-key",
            "query": "api_key"
        })))
        .unwrap();
        assert_eq!(config.header, "x-vendor-key");
        assert_eq!(config.query.as_deref(), Some("api_key"));
    }

    #[test]
    fn the_scheme_is_stripped_from_references() {
        assert_eq!(strip_scheme("cred://alpha"), "alpha");
        assert_eq!(strip_scheme("alpha"), "alpha");
    }

    #[tokio::test]
    async fn an_unwired_plugin_fails_loud() {
        let mut ctx = RequestContext::default();
        ctx.plugin_config = Some(serde_json::json!({ "key_ref": "cred://alpha" }));
        let err = ApiKeyAuth::default().authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::Failure { .. }), "{err:?}");
        assert_eq!(ctx.header("x-api-key"), None);
    }
}
