//! The `apikey` auth plugin: API-key injection into a configured header
//! ([PRD.md](../../../../../docs/PRD.md) `cpt-cf-oagw-fr-auth-injection`).
//!
//! The key is resolved from the credential store at request time and written
//! into the header the binding names ([`APIKEY_DEFAULT_HEADER`] by default),
//! replacing whatever the caller supplied under that name.

use std::sync::Arc;

use async_trait::async_trait;
use http::HeaderName;
use serde_json::Value;

use crate::domain::error::OagwError;
use crate::domain::model::AuthType;
use crate::infra::plugins::{PluginType, RequestContext, SecretResolver};

use super::{AuthPlugin, config_of, credential_text, header_value_of, optional_str, required_ref};

/// `apikey` — `cred://` reference of the API key (required).
pub const APIKEY_SECRET_REF: &str = "secret_ref";
/// `apikey` — header the key is injected into.
pub const APIKEY_HEADER: &str = "header";
/// Default value of [`APIKEY_HEADER`].
pub const APIKEY_DEFAULT_HEADER: &str = "x-api-key";
/// Scratch-space key the injected header name is published under.
pub const APIKEY_HEADER_ATTRIBUTE: &str = "auth.apikey.header";

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` — API key injection.
///
/// Configuration: `secret_ref` (required, `cred://` reference of the key) and
/// `header` (optional, [`APIKEY_DEFAULT_HEADER`] by default).
pub struct ApiKeyAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin, resolving keys through `resolver`.
    #[must_use]
    pub fn new(resolver: Arc<dyn SecretResolver>) -> Self {
        Self { resolver }
    }

    /// The header the key is injected into.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the configured header is not a valid
    /// header name.
    fn header_of(config: &Value) -> Result<HeaderName, OagwError> {
        let name = optional_str(config, APIKEY_HEADER).unwrap_or(APIKEY_DEFAULT_HEADER);
        HeaderName::from_lowercase(name.to_lowercase().as_bytes()).map_err(|_| {
            OagwError::Validation {
                message: format!("'{APIKEY_HEADER}' is not a valid header name"),
            }
        })
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        AuthType::APIKEY
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Auth
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let config = config_of(ctx)?;
        let reference = required_ref(&config, APIKEY_SECRET_REF)?;
        let key = self.resolver.resolve_cref(ctx, reference).await?;
        let name = Self::header_of(&config)?;
        // `insert` replaces every value the caller supplied under that name: the
        // injected credential is the only one that may reach the upstream.
        ctx.request_headers
            .insert(name.clone(), header_value_of(&credential_text(&key))?);
        ctx.set_attribute(APIKEY_HEADER_ATTRIBUTE, name.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use http::{HeaderMap, HeaderValue};

    use super::{
        APIKEY_DEFAULT_HEADER, APIKEY_HEADER, APIKEY_HEADER_ATTRIBUTE, APIKEY_SECRET_REF,
        ApiKeyAuthPlugin,
    };
    use crate::domain::error::{OagwError, SECRET_NOT_FOUND_GTS_ID};
    use crate::domain::model::AuthType;
    use crate::infra::plugins::{AuthPlugin, PluginType};
    use crate::infra::test_support::{context_with, secret_store};

    const CLIENT_KEY: &str = "client-supplied-key";
    const STORED_KEY: &str = "sk-123";

    fn plugin(secrets: &[(&str, &str)]) -> ApiKeyAuthPlugin {
        ApiKeyAuthPlugin::new(secret_store(secrets.to_vec()))
    }

    #[tokio::test]
    async fn injects_the_resolved_key_into_the_default_header() {
        let plugin = plugin(&[("partner-key", STORED_KEY)]);
        let mut context =
            context_with(serde_json::json!({ APIKEY_SECRET_REF: "cred://partner-key" }));

        plugin.authenticate(&mut context).await.expect("injected");

        assert_eq!(
            context
                .request_headers
                .get(APIKEY_DEFAULT_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(STORED_KEY)
        );
        assert_eq!(
            context.attribute(APIKEY_HEADER_ATTRIBUTE),
            Some("x-api-key")
        );
        assert_eq!(plugin.id(), AuthType::APIKEY);
        assert_eq!(plugin.plugin_type(), PluginType::Auth);
    }

    #[tokio::test]
    async fn honours_a_configured_header() {
        let plugin = plugin(&[("partner-key", STORED_KEY)]);
        let mut context = context_with(serde_json::json!({
            APIKEY_SECRET_REF: "partner-key",
            APIKEY_HEADER: "X-Api-Token",
        }));

        plugin.authenticate(&mut context).await.expect("injected");

        assert_eq!(
            context
                .request_headers
                .get("x-api-token")
                .and_then(|value| value.to_str().ok()),
            Some(STORED_KEY)
        );
        assert!(!context.request_headers.contains_key(APIKEY_DEFAULT_HEADER));
    }

    #[tokio::test]
    async fn replaces_a_client_supplied_key() {
        let plugin = plugin(&[("partner-key", STORED_KEY)]);
        let mut context =
            context_with(serde_json::json!({ APIKEY_SECRET_REF: "cred://partner-key" }));
        let mut headers = HeaderMap::new();
        headers.insert(APIKEY_DEFAULT_HEADER, HeaderValue::from_static(CLIENT_KEY));
        context.request_headers = headers;

        plugin.authenticate(&mut context).await.expect("injected");

        let injected: Vec<_> = context
            .request_headers
            .get_all(APIKEY_DEFAULT_HEADER)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(injected, [STORED_KEY], "the client key must be replaced");
    }

    #[tokio::test]
    async fn a_missing_key_is_a_500_secret_not_found() {
        let plugin = plugin(&[]);
        let mut context = context_with(serde_json::json!({ APIKEY_SECRET_REF: "cred://unknown" }));

        let error = plugin.authenticate(&mut context).await.unwrap_err();

        assert!(matches!(error, OagwError::SecretNotFound), "{error}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_NOT_FOUND_GTS_ID);
    }

    #[tokio::test]
    async fn a_missing_secret_ref_is_a_400() {
        let plugin = plugin(&[]);
        let mut context = context_with(serde_json::json!({}));

        let error = plugin.authenticate(&mut context).await.unwrap_err();

        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
        assert_eq!(error.http_status(), 400);
    }

    #[test]
    fn an_invalid_header_name_is_rejected() {
        let config = serde_json::json!({ APIKEY_HEADER: "not a header" });

        let error = ApiKeyAuthPlugin::header_of(&config).expect_err("invalid header name");

        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
    }

    #[test]
    fn the_plugin_debug_output_carries_no_credential() {
        let plugin = plugin(&[("partner-key", STORED_KEY)]);

        assert!(
            !format!("{plugin:?}").contains(STORED_KEY),
            "Debug must not reveal the resolved key"
        );
    }
}
