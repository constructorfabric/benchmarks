//! `cf.core.oagw.apikey.v1` — API key injection into a header or a query
//! parameter.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use http::{HeaderName, HeaderValue};

use crate::domain::gts;
use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError, config_nonblank};

use super::credref::resolve_secret;

/// Default header when the binding does not name one.
const DEFAULT_HEADER: &str = "x-api-key";

/// Where the key material is placed on the outbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Header,
    Query,
}

/// Injects a stored API key.
///
/// Config keys (all optional except the reference):
///
/// | Key | Default | Meaning |
/// |---|---|---|
/// | `secret_ref` (`credential_ref`, `cred_ref`, `key_ref`) | — | `cred://` reference to the key |
/// | `in` (`location`) | `header` | `header` or `query` |
/// | `name` (`header_name`, `param_name`, `query_param`) | `x-api-key` | Header or parameter name |
/// | `prefix` (`value_prefix`, `scheme`) | *(none)* | Text prepended to the key, e.g. `Bearer ` |
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
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

    async fn authenticate(&self, ctx: &mut AuthContext<'_>) -> Result<(), PluginError> {
        let config = ctx.config;
        let reference = ["secret_ref", "credential_ref", "cred_ref", "key_ref"]
            .into_iter()
            .find_map(|key| config_nonblank(config, key))
            .ok_or_else(|| {
                PluginError::InvalidConfig(
                    "apikey auth plugin requires a 'secret_ref' config key".to_owned(),
                )
            })?;

        let placement = match ["in", "location"]
            .into_iter()
            .find_map(|key| config_nonblank(config, key))
            .unwrap_or_else(|| "header".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "header" => Placement::Header,
            "query" | "query_param" | "querystring" => Placement::Query,
            other => {
                return Err(PluginError::InvalidConfig(format!(
                    "apikey auth plugin: unsupported 'in' value '{other}' (expected 'header' or 'query')"
                )));
            }
        };

        let name = ["name", "header_name", "param_name", "query_param"]
            .into_iter()
            .find_map(|key| config_nonblank(config, key))
            .unwrap_or_else(|| DEFAULT_HEADER.to_owned());

        let prefix = ["prefix", "value_prefix", "scheme"]
            .into_iter()
            .find_map(|key| config_nonblank(config, key))
            .unwrap_or_default();

        let secret =
            resolve_secret(&self.credstore, ctx.security_context(), &reference).await?;

        // `prefix` is a config value, not secret material; the concatenation
        // is scoped to this request and never logged.
        let value = if prefix.is_empty() {
            secret.expose().to_owned()
        } else if prefix.ends_with(' ') {
            format!("{prefix}{}", secret.expose())
        } else {
            format!("{prefix} {}", secret.expose())
        };

        match placement {
            Placement::Header => {
                let header_name = HeaderName::try_from(name.to_ascii_lowercase())
                    .map_err(|_| {
                        PluginError::InvalidConfig(format!("invalid header name '{name}'"))
                    })?;
                let mut header_value = HeaderValue::from_str(&value).map_err(|_| {
                    PluginError::InvalidConfig(
                        "resolved API key is not a valid header value".to_owned(),
                    )
                })?;
                header_value.set_sensitive(true);
                ctx.headers.insert(header_name, header_value);
            }
            Placement::Query => {
                ctx.query.retain(|(key, _)| key != &name);
                ctx.query.push((name, value));
            }
        }
        Ok(())
    }
}
