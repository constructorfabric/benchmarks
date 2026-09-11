//! `ApiKeyAuthPlugin` — API key injection into a header or the query string.
//!
//! The key itself is never stored in the gateway: the binding names a `cred://`
//! reference resolved through the credential store on the hot path.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use toolkit_security::SecurityContext;

use super::{AuthPlugin, PluginError, RequestContext};
use crate::domain::error::OagwError;

/// API key injection plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Creates the plugin over a credential store client.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

/// Reads the `cred://` reference out of a config value.
fn secret_ref(value: Option<&serde_json::Value>) -> Option<String> {
    value.and_then(serde_json::Value::as_str).map(str::to_owned)
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        crate::gts::auth_plugin::APIKEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let header_name = super::config_string(&ctx.config, "header_name")
            .unwrap_or_else(|| "X-Api-Key".to_owned());
        let query_name = super::config_string(&ctx.config, "query_name");
        let key_ref = secret_ref(
            ctx.config
                .get("key_ref")
                .or_else(|| ctx.config.get("secret_ref")),
        )
        .ok_or_else(|| {
            PluginError::Internal("apikey plugin requires a `key_ref` credential reference".into())
        })?;

        let key = resolve_secret(&self.credstore, &ctx.security_context, &key_ref).await?;
        let header = header_name_from(&header_name);

        if let Some(query_name) = query_name {
            ctx.headers.insert(
                header,
                http::HeaderValue::from_str(&key).map_err(|_| {
                    PluginError::Internal("api key contains non-ASCII characters".into())
                })?,
            );
            // The query component is carried as a synthetic header the proxy
            // layer rewrites into the URL.
            ctx.headers.insert(
                http::HeaderName::from_static("x-oagw-apikey-query"),
                http::HeaderValue::from_str(&format!("{query_name}={key}")).map_err(|_| {
                    PluginError::Internal("api key contains invalid header characters".into())
                })?,
            );
        } else {
            ctx.headers.insert(
                header,
                http::HeaderValue::from_str(&key).map_err(|_| {
                    PluginError::Internal("api key contains non-ASCII characters".into())
                })?,
            );
        }
        Ok(())
    }
}

/// Resolves a `cred://` reference to its value.
pub(crate) async fn resolve_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    key_ref: &str,
) -> Result<String, PluginError> {
    let key = key_ref
        .strip_prefix("cred://")
        .unwrap_or(key_ref)
        .to_owned();
    let secret_ref = credstore_sdk::SecretRef::new(key.clone())
        .map_err(|_| PluginError::Internal(format!("invalid credential reference {key_ref:?}")))?;

    match credstore.get(ctx, &secret_ref).await {
        Ok(Some(response)) => {
            let value = response.value.as_bytes().to_vec();
            String::from_utf8(value)
                .map_err(|_| PluginError::Internal("credential value is not valid UTF-8".into()))
        }
        Ok(None) => Err(PluginError::Reject {
            status: 500,
            type_id: crate::gts::errors::SECRET_NOT_FOUND,
            detail: format!("referenced secret {key_ref:?} was not found"),
        }),
        Err(e) => Err(PluginError::Internal(format!(
            "credential store lookup failed: {e}"
        ))),
    }
}

/// Parses a header name, mapping invalid names to an internal error.
fn header_name_from(name: &str) -> http::HeaderName {
    http::HeaderName::from_bytes(name.as_bytes())
        .unwrap_or(http::HeaderName::from_static("x-api-key"))
}

/// Converts a credential-store failure into the OAGW problem used by the
/// management API when validating a reference.
pub fn secret_error(e: &PluginError) -> OagwError {
    match e {
        PluginError::Reject {
            status,
            type_id,
            detail,
        } => {
            let mut err = OagwError::secret_not_found(detail.clone());
            err = err
                .with_extension("type", *type_id)
                .with_extension("status", *status);
            err
        }
        PluginError::Internal(detail) => OagwError::plugin_not_found(detail.clone()),
    }
}
