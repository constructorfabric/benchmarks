//! API key auth plugin: injects a credential as a header or a query
//! parameter (`cpt-cf-oagw-fr-auth-injection`).

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;

use crate::domain::error::PluginError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{AuthPlugin, RequestContext};
use crate::infra::plugin::credentials::resolve_secret;

/// Default header when the operator does not name one.
const DEFAULT_HEADER: &str = "Authorization";
/// Default query parameter when injecting into the query string.
const DEFAULT_QUERY_PARAM: &str = "api_key";

/// Where the key is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Header,
    Query,
}

/// `apikey` built-in auth plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Bind the plugin to the credential store.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }

    /// Read the injection location, tolerating the spellings operators reach
    /// for (`in`, `location`, `where`).
    fn location(ctx: &RequestContext) -> Result<Location, PluginError> {
        match ctx.config_str(&["in", "location", "where"]) {
            None => Ok(Location::Header),
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "header" => Ok(Location::Header),
                "query" | "query_param" | "querystring" => Ok(Location::Query),
                other => Err(PluginError::Config(format!(
                    "apikey plugin: '{other}' is not a valid injection location (header|query)"
                ))),
            },
        }
    }

    fn name(ctx: &RequestContext, location: Location) -> String {
        ctx.config_str(&["name", "header_name", "header", "param_name", "query_param"])
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| match location {
                Location::Header => DEFAULT_HEADER.to_owned(),
                Location::Query => DEFAULT_QUERY_PARAM.to_owned(),
            })
    }

    fn compose_value(ctx: &RequestContext, secret: &str) -> String {
        match ctx
            .config_str(&["value_prefix", "prefix", "scheme"])
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(prefix) => format!("{prefix} {secret}"),
            None => secret.to_owned(),
        }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        gts::APIKEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let location = Self::location(ctx)?;
        let name = Self::name(ctx, location);
        let secret_ref = ctx
            .config_str(&["secret_ref", "secret", "cred_ref", "key_ref", "value_ref"])
            .ok_or_else(|| {
                PluginError::Config(
                    "apikey plugin: 'secret_ref' is required and must name a cred:// reference"
                        .to_owned(),
                )
            })?
            .to_owned();
        let secret =
            resolve_secret(self.credstore.as_ref(), &ctx.security_context, &secret_ref).await?;
        let value = Self::compose_value(ctx, secret.expose());
        match location {
            Location::Header => ctx.set_header(&name, &value)?,
            Location::Query => ctx.add_query(&name, &value),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ApiKeyAuthPlugin;
    use crate::domain::error::PluginError;
    use crate::domain::plugin::{AuthPlugin, RequestContext};
    use credstore_sdk::test_util::MockCredStoreClient;
    use http::{HeaderMap, Method};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn plugin() -> ApiKeyAuthPlugin {
        ApiKeyAuthPlugin::new(Arc::new(MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-test-e2e-fake-key".to_owned(),
        )])))
    }

    fn ctx(config: Value) -> RequestContext {
        RequestContext {
            method: Method::POST,
            path: "/v1/chat/completions".to_owned(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            config: config.as_object().cloned().unwrap_or_default(),
            security_context: SecurityContext::builder()
                .subject_id(Uuid::new_v4())
                .subject_tenant_id(Uuid::new_v4())
                .build()
                .expect("context"),
            alias: "api.openai.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            route_id: None,
        }
    }

    #[tokio::test]
    async fn injects_into_a_named_header() {
        let mut c = ctx(json!({
            "in": "header",
            "name": "x-api-key",
            "secret_ref": "cred://openai-key"
        }));
        plugin().authenticate(&mut c).await.expect("injected");
        assert_eq!(c.headers["x-api-key"], "sk-test-e2e-fake-key");
    }

    #[tokio::test]
    async fn applies_a_value_prefix() {
        let mut c = ctx(json!({
            "name": "Authorization",
            "value_prefix": "Bearer",
            "secret_ref": "cred://openai-key"
        }));
        plugin().authenticate(&mut c).await.expect("injected");
        assert_eq!(c.headers["authorization"], "Bearer sk-test-e2e-fake-key");
    }

    #[tokio::test]
    async fn defaults_to_the_authorization_header() {
        let mut c = ctx(json!({ "secret_ref": "openai-key" }));
        plugin().authenticate(&mut c).await.expect("injected");
        assert_eq!(c.headers["authorization"], "sk-test-e2e-fake-key");
    }

    #[tokio::test]
    async fn injects_into_the_query_string() {
        let mut c = ctx(json!({
            "in": "query",
            "name": "key",
            "secret_ref": "openai-key"
        }));
        plugin().authenticate(&mut c).await.expect("injected");
        assert_eq!(
            c.query,
            vec![("key".to_owned(), "sk-test-e2e-fake-key".to_owned())]
        );
        assert!(c.headers.is_empty(), "query injection touches no header");
    }

    #[tokio::test]
    async fn secret_ref_is_required() {
        let mut c = ctx(json!({ "name": "x-api-key" }));
        let err = plugin()
            .authenticate(&mut c)
            .await
            .expect_err("no reference");
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[tokio::test]
    async fn an_unknown_location_is_a_config_error() {
        let mut c = ctx(json!({ "in": "cookie", "secret_ref": "openai-key" }));
        assert!(matches!(
            plugin()
                .authenticate(&mut c)
                .await
                .expect_err("bad location"),
            PluginError::Config(_)
        ));
    }

    #[tokio::test]
    async fn a_missing_credential_surfaces_as_secret_not_found() {
        let mut c = ctx(json!({ "secret_ref": "cred://absent" }));
        assert!(matches!(
            plugin().authenticate(&mut c).await.expect_err("absent"),
            PluginError::SecretNotFound(_)
        ));
    }
}
