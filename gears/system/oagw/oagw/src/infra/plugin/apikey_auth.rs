//! Built-in API-key auth plugin.
//!
//! Resolves the key from the credential store and injects it into the
//! configured header or query parameter.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::plugin::{
    AuthPlugin, PluginError, PluginErrorKind, PluginPhase, RequestContext,
};

/// Default header receiving the API key.
const DEFAULT_HEADER: &str = "authorization";
/// Default credential-store key when the config omits `secret_ref`.
const DEFAULT_SECRET_REF: &str = "api-key";

/// API-key injection plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Build the plugin over a credential-store client.
    #[must_use]
    pub fn new(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

/// Strip an optional `cred://` prefix from a secret reference.
///
/// The credential store's [`credstore_sdk::SecretRef`] only admits
/// `[a-zA-Z0-9_-]`, so the scheme prefix is not part of the stored key.
#[must_use]
pub fn strip_cred_scheme(reference: &str) -> String {
    reference
        .trim()
        .strip_prefix("cred://")
        .unwrap_or_else(|| reference.trim())
        .trim_matches('/')
        .to_owned()
}

/// Plugin configuration as read from the binding's `config` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyConfig {
    /// Credential-store key of the secret.
    pub secret_ref: String,
    /// Header receiving the key; ignored when `query_name` is set.
    pub header_name: String,
    /// Query parameter receiving the key, when configured.
    pub query_name: Option<String>,
}

impl ApiKeyConfig {
    /// Parse the plugin configuration.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Self {
        let secret_ref = value
            .get("secret_ref")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| DEFAULT_SECRET_REF.to_owned(), strip_cred_scheme);
        let header_name = value
            .get("header_name")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| DEFAULT_HEADER.to_owned(), str::to_ascii_lowercase);
        let query_name = value
            .get("query_name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Self {
            secret_ref,
            header_name,
            query_name,
        }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        "apikey"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = ApiKeyConfig::from_value(&ctx.config);
        let security = Arc::clone(&ctx.security);
        let key = SecretRef(security, config.secret_ref.clone());
        let resolved = key.resolve(self.credstore.as_ref()).await;
        let Some(value) = resolved else {
            ctx.record(self.id(), PluginPhase::Request);
            return Err(PluginError::new(
                PluginErrorKind::Authentication,
                format!("secret '{}' could not be resolved", config.secret_ref),
            ));
        };

        if let Some(name) = config.query_name {
            ctx.headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    PluginError::new(PluginErrorKind::BadRequest, "invalid query_name")
                })?,
                http::HeaderValue::from_str(&value).map_err(|_| {
                    PluginError::new(PluginErrorKind::BadRequest, "invalid api key value")
                })?,
            );
        } else if let Ok(name) = http::HeaderName::from_bytes(config.header_name.as_bytes())
            && let Ok(header) = http::HeaderValue::from_str(&value)
        {
            ctx.headers.insert(name, header);
        }
        ctx.record(self.id(), PluginPhase::Request);
        Ok(())
    }
}

/// A credential-store lookup bound to the caller's security context.
struct SecretRef(Arc<toolkit_security::SecurityContext>, String);

impl SecretRef {
    async fn resolve(&self, credstore: &dyn credstore_sdk::CredStoreClientV1) -> Option<String> {
        let reference = credstore_sdk::SecretRef::new(self.1.as_str()).ok()?;
        let response = credstore.get(&self.0, &reference).await.ok()??;
        String::from_utf8(response.value.as_bytes().to_vec()).ok()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use credstore_sdk::test_util::MockCredStoreClient;
    use serde_json::json;

    use super::*;
    use crate::infra::plugin::test_support::{recorded, request_context};

    fn plugin(secrets: &[(&str, &str)]) -> ApiKeyAuthPlugin {
        ApiKeyAuthPlugin::new(Arc::new(MockCredStoreClient::with_secrets(
            secrets
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )))
    }

    #[test]
    fn strip_cred_scheme_removes_only_the_scheme() {
        assert_eq!(strip_cred_scheme("cred://api-key"), "api-key");
        assert_eq!(strip_cred_scheme("api-key"), "api-key");
        assert_eq!(strip_cred_scheme("  cred://tenant/key "), "tenant/key");
        assert_eq!(strip_cred_scheme("cred://"), "");
    }

    #[test]
    fn config_defaults_match_the_adr() {
        let config = ApiKeyConfig::from_value(&json!({}));
        assert_eq!(
            config,
            ApiKeyConfig {
                secret_ref: "api-key".to_owned(),
                header_name: "authorization".to_owned(),
                query_name: None,
            }
        );
    }

    #[test]
    fn config_reads_the_wire_keys() {
        let config = ApiKeyConfig::from_value(&json!({
            "secret_ref": "cred://vendor-key",
            "header_name": "X-Api-Key",
            "query_name": "apiKey"
        }));
        assert_eq!(config.secret_ref, "vendor-key");
        assert_eq!(config.header_name, "x-api-key");
        assert_eq!(config.query_name.as_deref(), Some("apiKey"));
    }

    #[tokio::test]
    async fn the_key_is_injected_into_the_configured_header() {
        let mut ctx = request_context("local");
        ctx.config = json!({ "secret_ref": "vendor-key", "header_name": "X-Api-Key" });

        plugin(&[("vendor-key", "s3cret")])
            .authenticate(&mut ctx)
            .await
            .expect("the key resolves");
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "s3cret");
        assert!(ctx.headers.get("authorization").is_none());
        assert_eq!(recorded(&ctx), vec!["apikey:Request".to_owned()]);
    }

    #[tokio::test]
    async fn a_query_parameter_replaces_the_header() {
        let mut ctx = request_context("local");
        ctx.config = json!({ "secret_ref": "vendor-key", "query_name": "api_key" });

        plugin(&[("vendor-key", "s3cret")])
            .authenticate(&mut ctx)
            .await
            .expect("the key resolves");
        assert_eq!(ctx.headers.get("api_key").unwrap(), "s3cret");
    }

    #[tokio::test]
    async fn an_unresolvable_secret_is_an_authentication_failure() {
        let mut ctx = request_context("local");
        ctx.config = json!({ "secret_ref": "absent" });

        let err = plugin(&[("other", "x")])
            .authenticate(&mut ctx)
            .await
            .expect_err("the secret is missing");
        assert_eq!(err.kind, PluginErrorKind::Authentication);
        assert!(err.detail.contains("'absent'"));
        assert!(ctx.headers.is_empty());
        assert_eq!(recorded(&ctx), vec!["apikey:Request".to_owned()]);
    }

    #[tokio::test]
    async fn a_failing_store_is_an_authentication_failure() {
        let mut ctx = request_context("local");
        let plugin = ApiKeyAuthPlugin::new(Arc::new(MockCredStoreClient::always_failing()));

        let err = plugin
            .authenticate(&mut ctx)
            .await
            .expect_err("the store fails");
        assert_eq!(err.kind, PluginErrorKind::Authentication);
    }
}
