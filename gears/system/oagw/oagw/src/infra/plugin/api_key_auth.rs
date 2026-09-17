//! The API-key auth plugin (ADR-0002 "Built-in Plugins": `ApiKeyAuthPlugin`).
//!
//! An upstream that is reached with a shared secret declares the plugin and
//! names the header it wants the key in and where the key lives:
//!
//! ```json
//! { "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
//!   "config": { "header": "x-api-key", "key": "cred://vendor/api-key" } }
//! ```
//!
//! `header` defaults to `x-api-key`; `key` is either a literal (a dev upstream,
//! a non-secret identifier) or a `cred://` reference resolved through the host's
//! credential store with the *caller's* security context (DESIGN §2.1
//! "Credential Isolation"). Both keys are the only configuration the plugin
//! reads.
//!
//! # Documented deviation
//!
//! A binding that names no `key` at all fails the request with
//! `cf.oagw.secret.not_found.v1` rather than forwarding unauthenticated: an
//! upstream that asked for the plugin has asked for a credential, and a gateway
//! that quietly forwards without one would be a bypass. The configuration error
//! is therefore reported as the credential it prevents from being produced.
//!
//! The plugin implements the `header` placement only. PRD §5.2 also names a
//! `query` placement ("API Key (header/query)"), and this slice does not
//! provide it: the `AuthPlugin` trait (ADR-0002) is deliberately header-only —
//! a plugin "never sees a body" and receives only the header map, so a
//! query-parameter placement would need a request-URI mutation surface, which is
//! a data-plane feature rather than a plugin. Widening the trait to carry query
//! mutations is a design change, not a fix, and is deferred to the slice owner:
//! revisit it if a tenant actually needs query-placed keys.

use http::HeaderMap;
use http::header::HeaderName;

use super::registry::API_KEY_AUTH_PLUGIN_REF;
use super::secret::SecretResolver;
use super::traits::{AuthPlugin, PluginContext};
use crate::error::OagwError;

/// The header the key is injected into when the binding names none.
pub const DEFAULT_KEY_HEADER: &str = "x-api-key";

/// The configuration key that names the credential.
const KEY: &str = "key";
/// The configuration key that names the header the credential goes into.
const HEADER: &str = "header";

/// Injects the configured API key into the outbound request.
#[derive(Debug, Clone, Default)]
pub struct ApiKeyAuthPlugin {
    resolver: SecretResolver,
}

impl ApiKeyAuthPlugin {
    /// A plugin that resolves no `cred://` reference (every key must be a
    /// literal).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A plugin that resolves `cred://` references through `client`.
    #[must_use]
    pub fn with_client(
        client: Option<std::sync::Arc<dyn credstore_sdk::api::CredStoreClientV1>>,
    ) -> Self {
        Self {
            resolver: SecretResolver::new(client),
        }
    }
}

#[async_trait::async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn plugin_ref(&self) -> &str {
        API_KEY_AUTH_PLUGIN_REF
    }

    async fn authenticate(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        let Some(named) = context.string(KEY) else {
            return Err(OagwError::secret_not_found(format!(
                "the api-key auth plugin is bound without a '{KEY}' in its configuration, so no \
                 credential can be injected"
            )));
        };
        let key = self
            .resolver
            .resolve(&context.request.security, named)
            .await?;

        let header = context
            .string(HEADER)
            .unwrap_or(DEFAULT_KEY_HEADER)
            .to_ascii_lowercase();
        let name = HeaderName::try_from(header.as_str()).map_err(|error| {
            OagwError::validation(format!(
                "the api-key auth plugin is bound to an unusable header name '{header}': {error}"
            ))
        })?;

        // Overwrite: a header the caller sent under the same name is the
        // caller's, not the upstream's, and the credential is the gateway's to
        // choose.
        headers.insert(name, credential_value(&key)?);

        Ok(())
    }
}

/// The credential as a header value.
///
/// # Errors
/// A credential that is not a valid header value cannot be injected; forwarding
/// without it would be an unauthenticated request.
fn credential_value(key: &str) -> Result<http::HeaderValue, OagwError> {
    http::HeaderValue::try_from(key).map_err(|error| {
        OagwError::secret_not_found(format!(
            "the resolved credential is not a usable header value: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_context;
    use serde_json::json;

    #[tokio::test]
    async fn the_key_is_injected_into_the_default_header() {
        let config = json!({"key": "s3cr3t"});
        let mut headers = HeaderMap::new();

        ApiKeyAuthPlugin::new()
            .authenticate(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &mut headers,
            )
            .await
            .expect("a literal key needs no store");

        assert_eq!(
            headers
                .get(DEFAULT_KEY_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("s3cr3t")
        );
    }

    #[tokio::test]
    async fn the_header_is_configurable_and_a_caller_value_is_replaced() {
        let config = json!({"header": "Authorization", "key": "Bearer s3cr3t"});
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer caller".parse().expect("valid"));

        ApiKeyAuthPlugin::new()
            .authenticate(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &mut headers,
            )
            .await
            .expect("a literal key needs no store");

        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer s3cr3t"),
            "the caller's credential is not forwarded"
        );
        assert_eq!(headers.len(), 1);
    }

    #[tokio::test]
    async fn a_binding_without_a_key_fails_closed() {
        let config = json!({"header": "x-api-key"});
        let mut headers = HeaderMap::new();

        let error = ApiKeyAuthPlugin::new()
            .authenticate(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &mut headers,
            )
            .await
            .expect_err("no key configured");

        assert_eq!(error.status().as_u16(), 500);
        assert!(headers.is_empty(), "nothing is forwarded unauthenticated");
    }

    #[tokio::test]
    async fn a_cred_reference_without_a_store_fails_closed() {
        let config = json!({"key": "cred://vendor/api-key"});
        let mut headers = HeaderMap::new();

        let error = ApiKeyAuthPlugin::with_client(None)
            .authenticate(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &mut headers,
            )
            .await
            .expect_err("no credential store");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn an_unusable_header_name_is_a_configuration_error() {
        let config = json!({"header": "not a header name", "key": "s3cr3t"});
        let mut headers = HeaderMap::new();

        let error = ApiKeyAuthPlugin::new()
            .authenticate(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &mut headers,
            )
            .await
            .expect_err("not a header name");

        assert_eq!(error.status().as_u16(), 400);
        assert!(headers.is_empty());
    }

    #[test]
    fn the_plugin_is_registered_under_the_apikey_identifier() {
        assert_eq!(
            ApiKeyAuthPlugin::new().plugin_ref(),
            API_KEY_AUTH_PLUGIN_REF
        );
    }
}
