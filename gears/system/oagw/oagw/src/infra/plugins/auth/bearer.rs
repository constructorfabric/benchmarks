//! The `bearer` auth plugin: static bearer token injection.
//!
//! The token is resolved from the credential store at request time and written
//! as `Authorization: Bearer <token>`, replacing any `Authorization` header the
//! caller supplied. Unlike [`OAuth2ClientCredAuthPlugin`](super::oauth2) it
//! holds no token cache: the credential store owns the lifetime of the value.

use std::sync::Arc;

use async_trait::async_trait;
use http::header::AUTHORIZATION;

use crate::domain::error::OagwError;
use crate::domain::model::AuthType;
use crate::infra::plugins::{PluginType, RequestContext, SecretResolver};

use super::{AuthPlugin, bearer_value_of, config_of, credential_text, required_ref};

/// `bearer` — `cred://` reference of the token.
pub const BEARER_SECRET_REF: &str = "secret_ref";

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1` — Bearer token
/// injection.
///
/// Configuration: `secret_ref` (required, `cred://` reference of the token).
pub struct BearerAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
}

impl std::fmt::Debug for BearerAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerAuthPlugin").finish_non_exhaustive()
    }
}

impl BearerAuthPlugin {
    /// Builds the plugin, resolving the token through `resolver`.
    #[must_use]
    pub fn new(resolver: Arc<dyn SecretResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AuthPlugin for BearerAuthPlugin {
    fn id(&self) -> &str {
        AuthType::BEARER
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Auth
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let config = config_of(ctx)?;
        let reference = required_ref(&config, BEARER_SECRET_REF)?;
        let token = self.resolver.resolve_cref(ctx, reference).await?;
        // Replaces any client-supplied Authorization header.
        ctx.request_headers
            .insert(AUTHORIZATION, bearer_value_of(&credential_text(&token))?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use http::{HeaderMap, HeaderValue};

    use super::{BEARER_SECRET_REF, BearerAuthPlugin};
    use crate::domain::error::{OagwError, SECRET_NOT_FOUND_GTS_ID};
    use crate::domain::model::AuthType;
    use crate::infra::plugins::{AuthPlugin, PluginType};
    use crate::infra::test_support::{context_with, secret_store};

    const STORED_TOKEN: &str = "rotating-token-value";

    fn plugin(secrets: &[(&str, &str)]) -> BearerAuthPlugin {
        BearerAuthPlugin::new(secret_store(secrets.to_vec()))
    }

    #[tokio::test]
    async fn injects_the_resolved_token_as_a_bearer_value() {
        let plugin = plugin(&[("rotating", STORED_TOKEN)]);
        let mut context = context_with(serde_json::json!({ BEARER_SECRET_REF: "rotating" }));

        plugin.authenticate(&mut context).await.expect("injected");

        assert_eq!(
            context
                .request_headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {STORED_TOKEN}").as_str())
        );
        assert_eq!(plugin.id(), AuthType::BEARER);
        assert_eq!(plugin.plugin_type(), PluginType::Auth);
    }

    #[tokio::test]
    async fn replaces_a_client_supplied_authorization_header() {
        let plugin = plugin(&[("rotating", STORED_TOKEN)]);
        let mut context = context_with(serde_json::json!({ BEARER_SECRET_REF: "rotating" }));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Basic stale"));
        context.request_headers = headers;

        plugin.authenticate(&mut context).await.expect("injected");

        let injected: Vec<_> = context
            .request_headers
            .get_all("authorization")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(
            injected,
            [format!("Bearer {STORED_TOKEN}")],
            "the client header must be replaced"
        );
    }

    #[tokio::test]
    async fn a_missing_token_is_a_500_secret_not_found() {
        let plugin = plugin(&[]);
        let mut context = context_with(serde_json::json!({ BEARER_SECRET_REF: "cred://unknown" }));

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
    fn the_plugin_debug_output_carries_no_credential() {
        let plugin = plugin(&[("rotating", STORED_TOKEN)]);

        assert!(
            !format!("{plugin:?}").contains(STORED_TOKEN),
            "Debug must not reveal the resolved token"
        );
    }
}
