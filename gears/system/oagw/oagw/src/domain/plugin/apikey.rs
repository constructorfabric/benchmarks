//! API-key auth plugin.
//!
//! Resolves a `cred://` reference and injects the key into a header or into
//! the query string of the outbound request.

use super::{AuthDecision, AuthPlugin, CredentialResolver, PluginRequestContext, config_string};
use crate::domain::error::OagwError;
use crate::domain::model::AuthConfig;
use crate::gts_helpers;
use async_trait::async_trait;

/// Built-in auth plugin injecting a static API key.
#[derive(Debug, Default)]
pub struct ApiKeyAuthPlugin;

/// Configuration keys understood by the plugin.
pub mod keys {
    /// Header the key is injected into (takes precedence over the query).
    pub const HEADER_NAME: &str = "header_name";
    /// Query parameter the key is injected into.
    pub const QUERY_PARAM: &str = "query_param";
    /// `cred://` reference holding the key.
    pub const CREDENTIAL_REF: &str = "credential_ref";
    /// `cred://` reference holding the key, as DESIGN.md names it.
    pub const SECRET_REF: &str = "secret_ref";
}

/// Reads the credential reference, accepting either spelling of the key.
fn credential_reference(config: &AuthConfig) -> Option<String> {
    let value = config_value(config);
    config_string(&value, keys::CREDENTIAL_REF).or_else(|| config_string(&value, keys::SECRET_REF))
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        gts_helpers::AUTH_APIKEY
    }

    async fn authenticate(
        &self,
        context: &mut PluginRequestContext,
        config: &AuthConfig,
        credentials: &dyn CredentialResolver,
    ) -> Result<AuthDecision, OagwError> {
        let reference = credential_reference(config).ok_or_else(|| {
            OagwError::ValidationError("auth.config.credential_ref is required".to_owned())
        })?;
        let credential = credentials.resolve(context.tenant_id, &reference).await?;
        let value = credential.as_text().ok_or_else(|| {
            OagwError::AuthenticationFailed("api key credential is not valid UTF-8".to_owned())
        })?;
        if let Some(header) = config_string(&config_value(config), keys::HEADER_NAME) {
            let name = http::HeaderName::try_from(header.as_str()).map_err(|_| {
                OagwError::ValidationError(format!("auth.config.header_name '{header}' is invalid"))
            })?;
            let parsed = http::HeaderValue::try_from(value).map_err(|_| {
                OagwError::AuthenticationFailed(
                    "api key credential is not a valid header value".to_owned(),
                )
            })?;
            context.headers.insert(name, parsed);
            context.credential = Some(credential);
            return Ok(AuthDecision::Injected);
        }
        if let Some(param) = config_string(&config_value(config), keys::QUERY_PARAM) {
            let mut pairs = if context.query.is_empty() {
                Vec::new()
            } else {
                form_urlencoded::parse(context.query.as_bytes())
                    .map(|(name, value)| (name.into_owned(), value.into_owned()))
                    .collect::<Vec<_>>()
            };
            pairs.retain(|(name, _)| name.as_str() != param);
            pairs.push((param.clone(), value.to_owned()));
            context.query = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs)
                .finish();
            context.credential = Some(credential);
            return Ok(AuthDecision::Injected);
        }
        Err(OagwError::ValidationError(
            "auth.config requires header_name or query_param".to_owned(),
        ))
    }
}

/// The plugin's configuration map.
fn config_value(config: &AuthConfig) -> serde_json::Value {
    serde_json::Value::Object(
        config
            .config
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

#[cfg(test)]
#[path = "apikey_tests.rs"]
mod tests;
