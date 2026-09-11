//! The `apikey` auth plugin: injects a credential resolved from the store.
//!
//! The binding names the header to set and the `secret_ref` to resolve; the
//! value is fetched per request by the data plane's credential resolver and
//! placed straight into the outbound header map. It is never logged, never
//! returned to a caller and never present in an error document.

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, PluginType, RequestContext};
use crate::infra::plugin::registry::PluginFactory;

/// The configuration key holding the credential reference.
pub const SECRET_REF_KEY: &str = "secret_ref";
/// The configuration key holding the header or query parameter name.
pub const HEADER_NAME_KEY: &str = "header_name";

/// The attribute carrying the reference the data plane has to resolve.
pub const SECRET_REF_ATTRIBUTE: &str = "oagw.auth.secret_ref";
/// The attribute carrying the header the resolved key is injected into.
pub const HEADER_NAME_ATTRIBUTE: &str = "oagw.auth.header_name";

/// Injects an API key as a request header.
#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    header_name: String,
    secret_ref: String,
}

impl ApiKeyAuth {
    /// Build a plugin from its binding configuration.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when `secret_ref` is missing.
    pub fn from_config(
        config: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, DomainError> {
        let secret_ref = config
            .get(SECRET_REF_KEY)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                DomainError::validation(format!("apikey requires a '{SECRET_REF_KEY}'"))
            })?
            .to_owned();
        let header_name = config
            .get(HEADER_NAME_KEY)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Authorization")
            .to_owned();
        Ok(Self {
            header_name,
            secret_ref,
        })
    }

    /// The header the key is injected into.
    #[must_use]
    pub fn header_name(&self) -> &str {
        &self.header_name
    }

    /// The credential reference, `cred://` scheme included.
    #[must_use]
    pub fn secret_ref(&self) -> &str {
        &self.secret_ref
    }

    /// Place a resolved key into the context's headers.
    pub fn inject(&self, ctx: &mut RequestContext, key: &SecretString) {
        ctx.set_header(&self.header_name, key.expose_secret());
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuth {
    fn id(&self) -> &'static str {
        "apikey"
    }

    fn plugin_type(&self) -> &'static str {
        "auth"
    }

    /// Declares what must be resolved; the data plane performs the resolution
    /// and calls [`Self::inject`], because a plugin cannot await a resolver it
    /// was not handed.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when the configuration is malformed.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        ctx.set_attribute(SECRET_REF_ATTRIBUTE, self.secret_ref.clone());
        ctx.set_attribute(HEADER_NAME_ATTRIBUTE, self.header_name.clone());
        Ok(())
    }
}

/// Builds `apikey` instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApiKeyAuthFactory;

impl PluginFactory<dyn AuthPlugin> for ApiKeyAuthFactory {
    fn id(&self) -> &'static str {
        "apikey"
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Auth
    }

    fn description(&self) -> &'static str {
        "Reads an API key from the configured request header"
    }

    fn create(
        &self,
        config: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn AuthPlugin>, DomainError> {
        Ok(Arc::new(ApiKeyAuth::from_config(config)?))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod apikey_auth_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::error::ErrorKind;
    use serde_json::json;

    fn config(raw: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        raw.as_object().expect("object").clone()
    }

    fn context() -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            path: "/v1".to_owned(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            body_present: false,
            security_context: toolkit_security::SecurityContext::anonymous(),
            tenant_scope: vec![uuid::Uuid::nil()],
            injected_headers: Vec::new(),
            attributes: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn configuration_requires_a_secret_ref() {
        let err = ApiKeyAuth::from_config(&config(&json!({}))).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        let err = ApiKeyAuth::from_config(&config(&json!({"secret_ref": " "}))).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
    }

    #[test]
    fn header_name_defaults_to_authorization() -> Result<(), DomainError> {
        let plugin = ApiKeyAuth::from_config(&config(&json!({"secret_ref": "cred://k"})))?;
        assert_eq!(plugin.header_name(), "Authorization");
        let plugin = ApiKeyAuth::from_config(&config(
            &json!({"secret_ref": "cred://k", "header_name": "x-key"}),
        ))?;
        assert_eq!(plugin.header_name(), "x-key");
        Ok(())
    }

    #[test]
    fn injection_sets_the_header_and_records_it() -> Result<(), DomainError> {
        let mut ctx = context();
        let key = SecretString::from("sk-super-secret");
        ApiKeyAuth::from_config(&config(
            &json!({"secret_ref": "cred://k", "header_name": "x-api-key"}),
        ))?
        .inject(&mut ctx, &key);
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-super-secret");
        assert_eq!(ctx.injected_headers, vec!["x-api-key".to_owned()]);
        assert!(
            !format!(
                "{:?}",
                ApiKeyAuth::from_config(&config(&json!({
                    "secret_ref": "cred://k"
                })))?
            )
            .contains("sk-super-secret"),
            "the plugin itself never holds a key"
        );
        Ok(())
    }

    #[tokio::test]
    async fn authenticate_records_the_reference_it_needs() -> Result<(), DomainError> {
        let plugin = ApiKeyAuth::from_config(&config(&json!({"secret_ref": "cred://k"}))).unwrap();
        let mut ctx = context();
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.attributes
                .get("oagw.auth.secret_ref")
                .map(String::as_str),
            Some("cred://k")
        );
        assert!(
            ctx.injected_headers.is_empty(),
            "no header without a resolved key"
        );
        Ok(())
    }

    #[test]
    fn factory_builds_the_apikey_plugin() {
        let registry = crate::infra::plugin::registry::AuthPluginRegistry::with_builtins();
        assert!(registry.has("apikey"));
        assert!(registry.has("noop"));
    }
}
