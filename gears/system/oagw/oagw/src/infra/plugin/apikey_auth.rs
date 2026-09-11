//! Built-in `apikey` auth plugin: injects a credential-store-resolved secret as a header or query
//! parameter.

use serde_json::Value;

use crate::domain::gts_helpers;
use crate::domain::plugin::{AuthPlugin, PluginError, ProxyRequest};
use crate::infra::credentials::{resolve_secret, strip_cred_prefix};

/// [`AuthPlugin`] injecting an API key.
pub struct ApiKeyAuthPlugin {
    resolver: crate::infra::credentials::SecretResolver,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
    }
}

impl Default for ApiKeyAuthPlugin {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ApiKeyAuthPlugin {
    /// Plugin with the given credential store (or none, in which case references fail).
    #[must_use]
    pub fn new(resolver: Option<crate::infra::credentials::SecretResolver>) -> Self {
        Self {
            resolver: resolver.unwrap_or_else(|| {
                std::sync::Arc::new(crate::infra::credentials::MissingResolver)
            }),
        }
    }
}

#[async_trait::async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::AUTH_APIKEY
    }

    fn plugin_type(&self) -> &'static str {
        "apikey"
    }

    async fn authenticate(
        &self,
        request: &mut ProxyRequest,
        config: &Value,
    ) -> Result<(), PluginError> {
        let reference = config
            .get("secret_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::Config("apikey requires a 'secret_ref'".to_string()))?;
        let secret = resolve_secret(&self.resolver, request.security.clone(), reference).await?;
        let value = secret.expose();

        let location = config.get("in").and_then(Value::as_str).unwrap_or("header");
        match location {
            "query" => {
                let param = config
                    .get("query_param")
                    .and_then(Value::as_str)
                    .unwrap_or("api_key");
                let mut pairs = form_urlencoded::Serializer::new(String::new());
                if !request.query.is_empty() {
                    for (k, v) in form_urlencoded::parse(request.query.as_bytes()) {
                        pairs.append_pair(&k, &v);
                    }
                }
                pairs.append_pair(param, value);
                request.query = pairs.finish();
            }
            _ => {
                let header = config
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or("X-API-Key");
                request.set_header(header, value);
            }
        }
        Ok(())
    }
}

/// The `cred://` prefix is not part of the credential store's key space.
#[must_use]
pub fn normalize_secret_ref(reference: &str) -> &str {
    strip_cred_prefix(reference)
}

#[cfg(test)]
#[path = "apikey_auth_tests.rs"]
mod tests;
