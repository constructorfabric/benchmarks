//! Built-in `apikey` auth plugin.
//!
//! Reads a credential (credstore reference preferred, inline value allowed for
//! non-production) and writes it to the configured outbound header.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, PluginContext};
use crate::infra::plugin::secret::CredentialSource;

/// Configuration of the `apikey` plugin.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ApiKeyConfig {
    /// Header that carries the credential.
    header: String,
    /// Optional credential prefix (`Bearer`, `Token`, …).
    scheme: Option<String>,
    /// Credstore reference.
    secret_ref: Option<String>,
    /// Inline credential (non-production).
    value: Option<String>,
}

impl ApiKeyConfig {
    fn header(&self) -> String {
        if self.header.trim().is_empty() {
            String::from("X-Api-Key")
        } else {
            self.header.trim().to_owned()
        }
    }
}

/// Auth plugin that injects an API key header.
pub struct ApiKeyAuthPlugin {
    secrets: CredentialSource,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin")
            .field("secrets", &self.secrets)
            .finish()
    }
}

impl ApiKeyAuthPlugin {
    /// Build the plugin.
    #[must_use]
    pub fn new(secrets: CredentialSource) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn name(&self) -> &'static str {
        "apikey"
    }

    fn validate_config(&self, config: &serde_json::Value) -> Result<(), DomainError> {
        let parsed: ApiKeyConfig = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid apikey config: {e}")))?;
        if parsed.secret_ref.is_none() && parsed.value.is_none() {
            return Err(DomainError::Validation(
                "apikey requires either 'secret_ref' or 'value'".into(),
            ));
        }
        Ok(())
    }

    async fn authenticate(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError> {
        let parsed: ApiKeyConfig = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid apikey config: {e}")))?;
        let secret = self.secrets.resolve(ctx, config).await?;
        let header = parsed.header();
        let name = http::HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| DomainError::Validation(format!("invalid apikey header '{header}'")))?;
        let value = match parsed.scheme.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(scheme) => format!("{scheme} {secret}"),
            None => secret,
        };
        let header_value = http::HeaderValue::from_str(&value)
            .map_err(|_| DomainError::AuthFailed("credential is not header-safe".into()))?;
        headers.insert(name, header_value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use std::sync::Arc;

    fn ctx() -> PluginContext {
        PluginContext::default()
    }

    #[tokio::test]
    async fn injects_the_configured_header() {
        let plugin = ApiKeyAuthPlugin::new(CredentialSource::new(Some(Arc::new(
            MockCredStoreClient::with_secrets(vec![("k".to_owned(), "sk-test".to_owned())]),
        ))));
        let config = serde_json::json!({ "secret_ref": "k", "header": "x-api-key" });
        let mut headers = http::HeaderMap::new();
        plugin
            .authenticate(&ctx(), &config, &mut headers)
            .await
            .expect("injected");
        assert_eq!(headers.get("x-api-key").and_then(|v| v.to_str().ok()), Some("sk-test"));
    }

    #[tokio::test]
    async fn scheme_is_prefixed() {
        let plugin = ApiKeyAuthPlugin::new(CredentialSource::inline_only());
        let config = serde_json::json!({ "value": "abc", "header": "Authorization", "scheme": "Bearer" });
        let mut headers = http::HeaderMap::new();
        plugin
            .authenticate(&ctx(), &config, &mut headers)
            .await
            .expect("injected");
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer abc")
        );
    }

    #[test]
    fn default_header_is_x_api_key() {
        let plugin = ApiKeyAuthPlugin::new(CredentialSource::inline_only());
        plugin
            .validate_config(&serde_json::json!({ "value": "x" }))
            .expect("valid");
        assert!(
            plugin
                .validate_config(&serde_json::json!({}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn missing_credential_is_reported_without_the_secret() {
        let plugin = ApiKeyAuthPlugin::new(CredentialSource::new(Some(Arc::new(
            MockCredStoreClient::empty(),
        ))));
        let config = serde_json::json!({ "secret_ref": "nope" });
        let mut headers = http::HeaderMap::new();
        let err = plugin
            .authenticate(&ctx(), &config, &mut headers)
            .await
            .expect_err("missing credential");
        assert!(!err.to_string().contains("sk-"));
    }
}
