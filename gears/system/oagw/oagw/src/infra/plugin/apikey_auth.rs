//! `cf.core.oagw.apikey.v1` — API key injection into a header or a query
//! parameter.
//!
//! The key itself never appears in configuration: `secret_ref` is a
//! `cred://` reference resolved through CredStore at request time
//! (`cpt-cf-oagw-nfr-credential-isolation`).

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue};
use credstore_sdk::CredStoreClientV1;

use crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;
use crate::domain::model::{PluginConfig, config_str};
use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError};

use super::secret::resolve_secret_ref;

/// Where the key is placed on the outbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Header,
    Query,
}

/// Parsed `apikey` plugin configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiKeyPluginConfig {
    secret_ref: String,
    location: Location,
    name: String,
    prefix: String,
}

/// Read the first present key from a list of accepted spellings.
fn first_of(config: &PluginConfig, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| config_str(config, key))
}

impl ApiKeyPluginConfig {
    fn parse(config: &PluginConfig) -> Result<Self, PluginError> {
        let secret_ref =
            first_of(config, &["secret_ref", "key_ref", "api_key_ref"]).ok_or_else(|| {
                PluginError::InvalidConfig(
                    "apikey auth plugin requires a `secret_ref` (cred://…) config key".to_owned(),
                )
            })?;
        let location = match first_of(config, &["in", "location"])
            .unwrap_or_else(|| "header".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "header" => Location::Header,
            "query" | "query_param" => Location::Query,
            other => {
                return Err(PluginError::InvalidConfig(format!(
                    "apikey auth plugin `in` must be `header` or `query`, got {other:?}"
                )));
            }
        };
        let name = first_of(
            config,
            &["name", "header", "header_name", "query_param", "param"],
        )
        .unwrap_or_else(|| match location {
            Location::Header => "authorization".to_owned(),
            Location::Query => "api_key".to_owned(),
        });
        let prefix = first_of(config, &["prefix", "value_prefix"]).unwrap_or_default();
        Ok(Self {
            secret_ref,
            location,
            name,
            prefix,
        })
    }
}

/// API-key auth plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Bind the plugin to a CredStore client.
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

    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        let config = ApiKeyPluginConfig::parse(&ctx.config)?;
        let key = resolve_secret_ref(
            self.credstore.as_ref(),
            &ctx.security_context,
            &config.secret_ref,
        )
        .await?;
        let value = format!("{}{}", config.prefix, key.expose());
        match config.location {
            Location::Header => {
                let name =
                    HeaderName::try_from(config.name.to_ascii_lowercase()).map_err(|_| {
                        PluginError::InvalidConfig(format!(
                            "apikey auth plugin `name` is not a valid header name: {:?}",
                            config.name
                        ))
                    })?;
                // A credential must never end up in a log line, so the error
                // deliberately does not echo the value.
                let header = HeaderValue::from_str(&value).map_err(|_| {
                    PluginError::Internal(
                        "resolved API key is not a valid HTTP header value".to_owned(),
                    )
                })?;
                ctx.headers.insert(name, header);
            }
            Location::Query => {
                ctx.query.retain(|(k, _)| *k != config.name);
                ctx.query.push((config.name.clone(), value));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PluginConfig;
    use axum::http::HeaderMap;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn store_with_key() -> MockCredStoreClient {
        MockCredStoreClient::with_secrets(vec![("openai-key".to_owned(), "sk-test".to_owned())])
    }

    fn config(pairs: &[(&str, &str)]) -> PluginConfig {
        let mut config = PluginConfig::new();
        for (key, value) in pairs {
            config.insert((*key).to_owned(), serde_json::json!(value));
        }
        config
    }

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("context")
    }

    fn ctx(cfg: PluginConfig) -> AuthContext {
        AuthContext {
            security_context: security(),
            config: cfg,
            headers: HeaderMap::new(),
            query: vec![],
            upstream_alias: "api.openai.com".to_owned(),
        }
    }

    #[tokio::test]
    async fn injects_a_prefixed_header() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(store_with_key()));
        let mut context = ctx(config(&[
            ("secret_ref", "cred://openai-key"),
            ("in", "header"),
            ("name", "Authorization"),
            ("prefix", "Bearer "),
        ]));

        plugin.authenticate(&mut context).await.expect("injected");
        assert_eq!(
            context.headers.get("authorization").unwrap(),
            "Bearer sk-test"
        );
    }

    #[tokio::test]
    async fn injects_a_query_parameter() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(store_with_key()));
        let mut context = ctx(config(&[
            ("secret_ref", "cred://openai-key"),
            ("in", "query"),
            ("name", "api_key"),
        ]));

        plugin.authenticate(&mut context).await.expect("injected");
        assert_eq!(
            context.query,
            vec![("api_key".to_owned(), "sk-test".to_owned())]
        );
        assert!(context.headers.is_empty());
    }

    #[tokio::test]
    async fn missing_secret_is_a_secret_not_found() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(MockCredStoreClient::empty()));
        let mut context = ctx(config(&[("secret_ref", "cred://absent")]));
        let err = plugin
            .authenticate(&mut context)
            .await
            .expect_err("no secret");
        assert!(matches!(err, PluginError::SecretNotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn missing_secret_ref_is_a_config_error() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(MockCredStoreClient::empty()));
        let mut context = ctx(PluginConfig::new());
        let err = plugin
            .authenticate(&mut context)
            .await
            .expect_err("no config");
        assert!(matches!(err, PluginError::InvalidConfig(_)), "{err:?}");
    }

    #[test]
    fn config_accepts_alternate_spellings() {
        let parsed = ApiKeyPluginConfig::parse(&config(&[
            ("key_ref", "cred://k"),
            ("location", "QUERY"),
            ("param", "token"),
        ]))
        .expect("parses");
        assert_eq!(parsed.location, Location::Query);
        assert_eq!(parsed.name, "token");
        assert_eq!(parsed.secret_ref, "cred://k");
    }

    #[test]
    fn unknown_location_is_rejected() {
        assert!(
            ApiKeyPluginConfig::parse(&config(&[("secret_ref", "cred://k"), ("in", "cookie")]))
                .is_err()
        );
    }
}
