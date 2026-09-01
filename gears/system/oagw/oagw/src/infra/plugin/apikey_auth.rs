//! `apikey` auth plugin — API key injection into outbound requests.
//!
//! Configuration (`ctx.config`):
//! - `header`: header name to inject the key into (mutually exclusive with
//!   `param`)
//! - `param`: query parameter name to inject the key into (mutually
//!   exclusive with `header`)
//! - `secret_ref`: `cred://...` reference resolved from the credential store
//!   at request time

use std::sync::Arc;

use serde::Deserialize;

use crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginError, PluginResult, RequestContext, async_trait};

use super::resolve_secret;

/// Parsed configuration for the apikey plugin.
#[derive(Debug, Deserialize, Default)]
struct ApiKeyConfig {
    header: Option<String>,
    param: Option<String>,
    secret_ref: Option<String>,
}

/// The `apikey` auth plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Create the plugin.
    #[must_use]
    pub fn new(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        APIKEY_AUTH_PLUGIN_ID
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait declares `&str`; returns a literal
    fn plugin_type(&self) -> &str {
        "apikey"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let cfg: ApiKeyConfig =
            serde_json::from_value(serde_json::Value::Object(ctx.config.clone()))
                .map_err(|e| PluginError::config(format!("invalid apikey config: {e}")))?;

        let header = cfg.header.filter(|h| !h.trim().is_empty());
        let param = cfg.param.filter(|p| !p.trim().is_empty());
        if header.is_some() && param.is_some() {
            return Err(PluginError::config(
                "apikey config must set exactly one of 'header' or 'param'",
            ));
        }
        let Some(secret_ref) = cfg.secret_ref.filter(|s| !s.trim().is_empty()) else {
            return Err(PluginError::config("apikey config requires 'secret_ref'"));
        };

        let key = resolve_secret(&self.credstore, &ctx.security_context, &secret_ref)
            .await
            .map_err(|e| PluginError::Internal {
                detail: format!("credential store lookup failed: {e}"),
            })?
            .ok_or_else(|| PluginError::SecretNotFound {
                detail: "referenced api key secret not found".to_owned(),
            })?;
        let key = String::from_utf8_lossy(&key).into_owned();

        if let Some(header_name) = header {
            let header_name = http::HeaderName::try_from(header_name.as_str())
                .map_err(|_| PluginError::config("invalid api key header name in plugin config"))?;
            let header_value = http::HeaderValue::from_str(&key)
                .map_err(|_| PluginError::config("api key contains invalid header value bytes"))?;
            ctx.headers.insert(header_name, header_value);
        } else if let Some(param_name) = param {
            let pair = format!("{param_name}={}", encode_query_value(&key));
            ctx.query = if ctx.query.is_empty() {
                pair
            } else {
                format!("{}&{pair}", ctx.query)
            };
        }
        Ok(())
    }
}

fn encode_query_value(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn query_value_is_encoded() {
        assert_eq!(encode_query_value("a b&c"), "a+b%26c");
    }
}
