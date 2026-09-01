//! `ApiKeyAuthPlugin` — injects an API key stored in the CredStore.
//!
//! Config keys (plugin-defined; the contract documents no schema):
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `key_ref` | required | `cred://` reference holding the API key |
//! | `header` | `X-API-Key` | header receiving the key |
//! | `query` | *(absent)* | query parameter receiving the key as well |
//!
//! The key is injected as an outbound credential; it is never logged.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::gts::AUTH_PLUGIN_APIKEY_INSTANCE;
use crate::domain::plugin::{AuthPlugin, PluginContext};
use crate::infra::plugin::SecretResolver;

/// Default header receiving the API key.
pub const DEFAULT_API_KEY_HEADER: &str = "x-api-key";

/// Resolves the configured CredStore reference and injects the key.
#[derive(Clone)]
pub struct ApiKeyAuthPlugin {
    secrets: Arc<SecretResolver>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ApiKeyAuthPlugin").finish()
    }
}

impl ApiKeyAuthPlugin {
    /// Creates the plugin bound to a CredStore-backed resolver.
    #[must_use]
    pub fn new(secrets: Arc<SecretResolver>) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_PLUGIN_APIKEY_INSTANCE
    }

    async fn authenticate(
        &self,
        _ctx: &PluginContext,
        security_context: &SecurityContext,
        config: &serde_json::Value,
        parts: &mut http::request::Parts,
    ) -> Result<(), DomainError> {
        let reference = super::config_str(config, "key_ref").ok_or_else(|| {
            DomainError::Validation(
                "apikey auth plugin requires the 'key_ref' config key".to_owned(),
            )
        })?;
        let header_name = super::config_str(config, "header")
            .unwrap_or_else(|| DEFAULT_API_KEY_HEADER.to_owned());
        let key = self.secrets.resolve(security_context, &reference).await?;
        let value = http::HeaderValue::from_str(&key).map_err(|_| {
            DomainError::AuthenticationFailed(
                "the configured API key is not a valid header value".to_owned(),
            )
        })?;
        let name = http::HeaderName::from_bytes(header_name.as_bytes()).map_err(|_| {
            DomainError::Validation(format!("invalid apikey header '{header_name}'"))
        })?;
        parts.headers.insert(name, value);

        if let Some(query_param) = super::config_str(config, "query") {
            let path = parts.uri.path().to_owned();
            let existing = parts.uri.query().unwrap_or_default();
            let merged = with_query_param(existing, &query_param, &key);
            let mut builder = http::Uri::builder().path_and_query(format!("{path}?{merged}"));
            if let Some(scheme) = parts.uri.scheme_str() {
                builder = builder.scheme(scheme);
            }
            if let Some(authority) = parts.uri.authority() {
                builder = builder.authority(authority.as_str());
            }
            parts.uri = builder
                .build()
                .map_err(|_| DomainError::Validation("cannot append the api key".to_owned()))?;
        }
        Ok(())
    }
}

/// Rebuilds `query` with `name=value` percent-encoded, replacing any parameter
/// the client already supplied under `name`.
///
/// Both sides are encoded with [`form_urlencoded::Serializer`], so neither the
/// configured parameter name nor the secret can splice extra pairs or fragments
/// into the query. A same-named parameter is *replaced in place*, never
/// duplicated and never moved: a backend that honours the first occurrence
/// would otherwise trust a client-supplied value over the injected credential.
/// The configured value wins by design; a client cannot opt out of the
/// injection.
#[must_use]
fn with_query_param(query: &str, name: &str, value: &str) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    let mut injected = false;
    for (key, existing) in form_urlencoded::parse(query.as_bytes()) {
        if key == name {
            if injected {
                continue;
            }
            serializer.append_pair(name, value);
            injected = true;
        } else {
            serializer.append_pair(&key, &existing);
        }
    }
    if !injected {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::domain::plugin::PluginContext;

    fn ctx() -> PluginContext {
        PluginContext {
            security_context: SecurityContext::anonymous(),
            upstream_id: uuid::Uuid::nil(),
            host: "vendor.com".to_owned(),
            route_id: None,
            endpoint_host: "api.vendor.com:443".to_owned(),
            request_id: PluginContext::default_request_id(),
        }
    }

    fn resolver() -> Arc<SecretResolver> {
        Arc::new(SecretResolver::new(Some(Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![(
                "partner-key".to_owned(),
                "sk-live-123".to_owned(),
            )]),
        ))))
    }

    #[tokio::test]
    async fn injects_the_default_header() {
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        ApiKeyAuthPlugin::new(resolver())
            .authenticate(
                &ctx(),
                &SecurityContext::anonymous(),
                &serde_json::json!({"key_ref": "cred://partner-key"}),
                &mut parts,
            )
            .await
            .unwrap();
        assert_eq!(parts.headers.get("x-api-key").unwrap(), "sk-live-123");
    }

    #[tokio::test]
    async fn missing_key_ref_is_a_validation_error() {
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        let error = ApiKeyAuthPlugin::new(resolver())
            .authenticate(
                &ctx(),
                &SecurityContext::anonymous(),
                &serde_json::json!({}),
                &mut parts,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, DomainError::Validation(_)));
    }

    #[tokio::test]
    async fn unknown_secret_is_rejected_without_leaking_the_value() {
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        let error = ApiKeyAuthPlugin::new(resolver())
            .authenticate(
                &ctx(),
                &SecurityContext::anonymous(),
                &serde_json::json!({"key_ref": "cred://nope"}),
                &mut parts,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, DomainError::SecretNotFound(_)));
        assert!(parts.headers.is_empty());
    }

    #[tokio::test]
    async fn can_mirror_the_key_into_the_query() {
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        ApiKeyAuthPlugin::new(resolver())
            .authenticate(
                &ctx(),
                &SecurityContext::anonymous(),
                &serde_json::json!({"key_ref": "partner-key", "query": "api_key"}),
                &mut parts,
            )
            .await
            .unwrap();
        assert_eq!(parts.uri.query(), Some("api_key=sk-live-123"));
    }

    #[test]
    fn the_query_parameter_is_percent_encoded() {
        // A key with reserved characters cannot splice a second pair or a
        // fragment into the query.
        assert_eq!(
            with_query_param("a=1", "api_key", "sk&live=123#frag"),
            "a=1&api_key=sk%26live%3D123%23frag"
        );
        assert_eq!(with_query_param("", "k", "v 1+2"), "k=v+1%2B2");
        assert_eq!(with_query_param("", "na me", "v"), "na+me=v");
    }

    #[test]
    fn an_existing_parameter_is_replaced_not_duplicated() {
        // The injected pair takes the place the client's own value occupied, so
        // the parameter order the caller signed or cached is preserved.
        assert_eq!(
            with_query_param("api_key=attacker&a=1", "api_key", "sk-live-123"),
            "api_key=sk-live-123&a=1"
        );
        assert_eq!(
            with_query_param("api_key=one&api_key=two", "api_key", "sk-live-123"),
            "api_key=sk-live-123"
        );
        assert_eq!(
            with_query_param("api_key=sk-live-123", "api_key", "sk-live-123"),
            "api_key=sk-live-123"
        );
    }

    #[tokio::test]
    async fn an_existing_query_parameter_is_replaced_in_place() {
        let request = http::Request::builder()
            .uri("/v1/items?api_key=attacker&api-version=1")
            .body(())
            .unwrap();
        let (mut parts, _) = request.into_parts();
        ApiKeyAuthPlugin::new(resolver())
            .authenticate(
                &ctx(),
                &SecurityContext::anonymous(),
                &serde_json::json!({"key_ref": "partner-key", "query": "api_key"}),
                &mut parts,
            )
            .await
            .unwrap();
        assert_eq!(parts.uri.query(), Some("api_key=sk-live-123&api-version=1"));
    }
}
