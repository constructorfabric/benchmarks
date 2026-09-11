//! The `noop` and `apikey` built-in authentication plugins.
//!
//! `NoopAuthPlugin` injects nothing. `ApiKeyAuthPlugin` resolves the credential
//! material from the credential store at request time and injects it into a
//! header or a query parameter; the value is never echoed to the client.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{AUTH_PLUGIN_APIKEY, AUTH_PLUGIN_NOOP};
use crate::domain::plugin::{AuthPlugin, PluginContext, SecretResolver};

/// Injects nothing; used when an upstream opts out of credential injection.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_PLUGIN_NOOP
    }

    async fn apply(
        &self,
        _context: &PluginContext,
        _config: &serde_json::Value,
        _headers: &mut http::HeaderMap,
        _query: &mut Vec<(String, String)>,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Where the credential is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialLocation {
    Header,
    Query,
}

/// Injects an API key resolved from the credential store.
pub struct ApiKeyAuthPlugin {
    secrets: std::sync::Arc<dyn SecretResolver>,
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin over a credential resolver.
    #[must_use]
    pub fn new(secrets: std::sync::Arc<dyn SecretResolver>) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_PLUGIN_APIKEY
    }

    async fn apply(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        headers: &mut http::HeaderMap,
        query: &mut Vec<(String, String)>,
    ) -> Result<(), DomainError> {
        let reference = config
            .get("secret_ref")
            .or_else(|| config.get("value_ref"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                DomainError::AuthenticationFailed(
                    "apikey auth config requires `secret_ref`".into(),
                )
            })?;
        let name = config
            .get("name")
            .or_else(|| config.get("header_name"))
            .or_else(|| config.get("query_name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("x-api-key")
            .to_owned();
        let location = config
            .get("in")
            .or_else(|| config.get("location"))
            .and_then(serde_json::Value::as_str)
            .map_or(CredentialLocation::Header, |value| {
                if value.eq_ignore_ascii_case("query") {
                    CredentialLocation::Query
                } else {
                    CredentialLocation::Header
                }
            });

        let Some(secret) = self.secrets.resolve(context, reference).await? else {
            return Err(DomainError::AuthenticationFailed(format!(
                "credential reference `{reference}` could not be resolved"
            )));
        };

        match location {
            CredentialLocation::Header => {
                let value = http::HeaderValue::from_str(&secret).map_err(|_| {
                    DomainError::AuthenticationFailed(
                        "resolved credential is not a valid header value".into(),
                    )
                })?;
                headers.insert(
                    http::HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| {
                            DomainError::AuthenticationFailed(format!(
                                "`{name}` is not a valid header name"
                            ))
                        })?,
                    value,
                );
            }
            CredentialLocation::Query => {
                replace_or_push(query, &name, &secret);
            }
        }
        Ok(())
    }
}

/// Replaces an existing query parameter of the same name, or appends one.
fn replace_or_push(query: &mut Vec<(String, String)>, name: &str, value: &str) {
    let mut replaced = false;
    for (existing_name, existing_value) in query.iter_mut() {
        if existing_name.eq_ignore_ascii_case(name) {
            existing_value.clear();
            existing_value.push_str(value);
            replaced = true;
        }
    }
    if !replaced {
        query.push((name.to_owned(), value.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// Resolver backed by a fixed table, for tests.
    #[derive(Default)]
    struct MapSecretResolver {
        values: Mutex<BTreeMap<String, String>>,
    }

    impl MapSecretResolver {
        fn with(values: &[(&str, &str)]) -> std::sync::Arc<Self> {
            let map: BTreeMap<String, String> = values
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            std::sync::Arc::new(Self {
                values: Mutex::new(map),
            })
        }
    }

    #[async_trait]
    impl SecretResolver for MapSecretResolver {
        async fn resolve(
            &self,
            _context: &PluginContext,
            reference: &str,
        ) -> Result<Option<String>, DomainError> {
            Ok(self
                .values
                .lock()
                .map_or(None, |values| values.get(reference).cloned()))
        }
    }

    fn context() -> PluginContext {
        PluginContext {
            tenant_id: uuid::Uuid::nil(),
            subject_id: uuid::Uuid::nil(),
            upstream_id: uuid::Uuid::nil(),
            route_id: None,
            alias: "api.openai.com".into(),
            bearer_token: None,
            request_id: None,
        }
    }

    #[tokio::test]
    async fn apikey_is_injected_into_a_header() {
        let plugin = ApiKeyAuthPlugin::new(MapSecretResolver::with(&[(
            "cred://sk",
            "sk-123",
        )]));
        let mut headers = http::HeaderMap::new();
        let mut query = Vec::new();
        plugin
            .apply(
                &context(),
                &serde_json::json!({"name": "x-api-key", "secret_ref": "cred://sk"}),
                &mut headers,
                &mut query,
            )
            .await
            .expect("injected");
        assert_eq!(headers.get("x-api-key").and_then(|value| value.to_str().ok()), Some("sk-123"));
        assert!(query.is_empty());
    }

    #[tokio::test]
    async fn apikey_can_be_injected_into_the_query() {
        let plugin = ApiKeyAuthPlugin::new(MapSecretResolver::with(&[("cred://sk", "sk-123")]));
        let mut headers = http::HeaderMap::new();
        let mut query = Vec::new();
        plugin
            .apply(
                &context(),
                &serde_json::json!({"in": "query", "name": "api_key", "secret_ref": "cred://sk"}),
                &mut headers,
                &mut query,
            )
            .await
            .expect("injected");
        assert!(headers.is_empty());
        assert_eq!(query, vec![("api_key".to_owned(), "sk-123".to_owned())]);
    }

    #[tokio::test]
    async fn unresolvable_secret_is_an_authentication_failure() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(MapSecretResolver::default()));
        let mut headers = http::HeaderMap::new();
        let mut query = Vec::new();
        let error = plugin
            .apply(
                &context(),
                &serde_json::json!({"name": "x-api-key", "secret_ref": "cred://missing"}),
                &mut headers,
                &mut query,
            )
            .await
            .expect_err("unresolvable");
        assert_eq!(error.status(), 401);
    }

    #[tokio::test]
    async fn missing_secret_ref_is_an_authentication_failure() {
        let plugin = ApiKeyAuthPlugin::new(Arc::new(MapSecretResolver::default()));
        let mut headers = http::HeaderMap::new();
        let mut query = Vec::new();
        let error = plugin
            .apply(&context(), &serde_json::json!({}), &mut headers, &mut query)
            .await
            .expect_err("misconfigured");
        assert_eq!(error.status(), 401);
    }

    #[tokio::test]
    async fn noop_injects_nothing() {
        let plugin = NoopAuthPlugin;
        let mut headers = http::HeaderMap::new();
        let mut query = Vec::new();
        plugin
            .apply(&context(), &serde_json::json!({}), &mut headers, &mut query)
            .await
            .expect("no-op");
        assert!(headers.is_empty());
    }
}
