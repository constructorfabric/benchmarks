//! `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` — API key
//! injection into a header or a query parameter.
//!
//! Config keys:
//!
//! | Key | Required | Description |
//! |---|---|---|
//! | `secret_ref` | yes | `cred://` reference to the key material |
//! | `in` | no | `header` (default) or `query` |
//! | `name` | no | header / parameter name (`X-API-Key` / `api_key`) |
//! | `prefix` | no | literal prefix, e.g. `Bearer ` |

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use http::HeaderName;
use http::header::HeaderValue;

use crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginError, PluginResult, RequestContext};
use crate::infra::plugin::secret::resolve_secret;

pub const DEFAULT_HEADER_NAME: &str = "X-API-Key";
pub const DEFAULT_QUERY_NAME: &str = "api_key";

pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

/// Where the key is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Header,
    Query,
}

/// Parse the placement, defaulting to `header`.
///
/// # Errors
/// Returns [`PluginError::Config`] for an unrecognised value.
pub fn parse_placement(raw: Option<&str>) -> PluginResult<Placement> {
    match raw.map(str::trim).unwrap_or("header") {
        "header" => Ok(Placement::Header),
        "query" => Ok(Placement::Query),
        other => Err(PluginError::Config(format!(
            "apikey auth plugin: `in` must be 'header' or 'query', got '{other}'"
        ))),
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

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let reference = ctx
            .config_str("secret_ref")
            .or_else(|| ctx.config_str("secret"))
            .ok_or_else(|| {
                PluginError::Config(
                    "apikey auth plugin: missing required config key 'secret_ref'".to_owned(),
                )
            })?
            .to_owned();

        let placement = parse_placement(ctx.config_str("in"))?;
        let prefix = ctx
            .config_str("prefix")
            .or_else(|| ctx.config_str("value_prefix"))
            .unwrap_or("")
            .to_owned();
        let name = ctx
            .config_str("name")
            .or_else(|| ctx.config_str("header_name"))
            .or_else(|| ctx.config_str("query_param"))
            .unwrap_or(match placement {
                Placement::Header => DEFAULT_HEADER_NAME,
                Placement::Query => DEFAULT_QUERY_NAME,
            })
            .to_owned();

        let value = resolve_secret(&self.credstore, &ctx.security_context, &reference).await?;
        let rendered = format!("{prefix}{value}");

        match placement {
            Placement::Header => {
                let header_name = HeaderName::try_from(name.as_str()).map_err(|_| {
                    PluginError::Config(format!("apikey auth plugin: invalid header name '{name}'"))
                })?;
                let header_value = HeaderValue::from_str(&rendered).map_err(|_| {
                    // Never echo the value — it is secret material.
                    PluginError::Config(
                        "apikey auth plugin: resolved secret is not a valid header value"
                            .to_owned(),
                    )
                })?;
                ctx.headers.insert(header_name, header_value);
            }
            Placement::Query => {
                ctx.query.retain(|(k, _)| k != &name);
                ctx.query.push((name, rendered));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_defaults_to_header() {
        assert_eq!(parse_placement(None).expect("default"), Placement::Header);
        assert_eq!(
            parse_placement(Some("query")).expect("query"),
            Placement::Query
        );
        assert!(parse_placement(Some("cookie")).is_err());
    }
}
