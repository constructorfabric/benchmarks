//! Built-in API key auth plugin
//! (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
//!
//! Injects a statically configured API key into the outbound request, either as
//! a header (default `x-api-key`) or as a query parameter. The key is sourced
//! from the credential store through a `cred://` reference (preferred, ADR-0008
//! and the DESIGN "Secret Access Control" flow) or, for tests and non-sensitive
//! upstreams, from an inline literal.
//!
//! ## Configuration keys
//!
//! | Key | Required | Meaning |
//! |---|---|---|
//! | `header_name` | no | Header the key is injected into; default `x-api-key`. |
//! | `query_name` | no | Query parameter the key is injected into; takes precedence over `header_name`. |
//! | `key` | one of | Inline API key. |
//! | `secret_ref` | one of | `cred://` reference resolved at request time. |
//!
//! ## Failure mapping
//!
//! A missing credential source, an unresolvable reference or a credential that
//! resolves to an empty value all reject the request with `401`
//! `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`
//! ([`OagwError::AuthenticationFailed`]), matching the DESIGN secret-resolution
//! flow ("If not accessible -> return error, OAGW returns 401 Unauthorized").

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue};
use serde::Deserialize;

use crate::domain::error::OagwError;
use crate::domain::plugin::{AUTH_PLUGIN_TYPE_ID, AuthPlugin, RequestContext, builtin};
use crate::infra::plugin::secret::{SecretResolver, strip_secret_scheme};

/// Header the API key is injected into when the configuration names none.
pub const DEFAULT_API_KEY_HEADER: &str = "x-api-key";

/// Configuration payload of the API key plugin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ApiKeyPluginConfig {
    /// Header the key is injected into.
    header_name: String,
    /// Query parameter the key is injected into; wins over `header_name`.
    query_name: Option<String>,
    /// Inline API key.
    key: Option<String>,
    /// `cred://` reference resolved at request time.
    secret_ref: Option<String>,
}

impl Default for ApiKeyPluginConfig {
    fn default() -> Self {
        Self {
            header_name: DEFAULT_API_KEY_HEADER.to_owned(),
            query_name: None,
            key: None,
            secret_ref: None,
        }
    }
}

/// Where the credential comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApiKeySource {
    /// A literal value from the plugin configuration.
    Literal(String),
    /// A `cred://` reference resolved per request.
    Reference(String),
}

/// API key injection plugin.
pub struct ApiKeyAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
    header_name: String,
    query_name: Option<String>,
    source: ApiKeySource,
}

// The resolver port is not `Debug` (its implementations may wrap the credential
// store), so the plugin reports only its identity.
impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiKeyAuthPlugin")
            .field("id", &Self::PLUGIN_ID)
            .field("header_name", &self.header_name)
            .field("query_name", &self.query_name)
            .finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    /// Registry key of this plugin.
    pub const PLUGIN_ID: &'static str = builtin::APIKEY_AUTH;

    /// GTS base type of this plugin.
    pub const PLUGIN_TYPE: &'static str = AUTH_PLUGIN_TYPE_ID;

    /// Builds the plugin from a binding configuration payload.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the payload is not an object with
    /// the documented keys, or when neither `key` nor `secret_ref` is set.
    pub fn new(
        resolver: Arc<dyn SecretResolver>,
        config: &serde_json::Value,
    ) -> Result<Self, OagwError> {
        let parsed = parse_config(config)?;
        let source = match (parsed.key.as_ref(), parsed.secret_ref.as_ref()) {
            (Some(key), _) => ApiKeySource::Literal(key.clone()),
            (None, Some(reference)) => ApiKeySource::Reference(reference.clone()),
            (None, None) => {
                return Err(OagwError::validation(
                    "api key plugin requires either `key` or `secret_ref`",
                ));
            }
        };
        Ok(Self {
            resolver,
            header_name: parsed.header_name,
            query_name: parsed.query_name,
            source,
        })
    }

    /// Resolves and injects the credential into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::AuthenticationFailed`] when the credential is
    /// missing, cannot be resolved, or resolves to an empty value.
    async fn credential(&self, ctx: &RequestContext) -> Result<String, OagwError> {
        let value = match &self.source {
            ApiKeySource::Literal(key) => key.clone(),
            ApiKeySource::Reference(reference) => self.resolve(ctx, reference).await?,
        };
        if value.trim().is_empty() {
            return Err(OagwError::authentication_failed(
                "the configured api key credential is empty",
            ));
        }
        Ok(value)
    }

    async fn resolve(&self, ctx: &RequestContext, reference: &str) -> Result<String, OagwError> {
        let Some(security) = ctx.security.clone() else {
            return Err(OagwError::authentication_failed(
                "no security context is available to resolve the api key credential",
            ));
        };
        self.resolver
            .resolve(&security, reference)
            .await?
            .ok_or_else(|| {
                OagwError::authentication_failed(format!(
                    "api key credential '{reference}' is not accessible to this tenant"
                ))
            })
    }
}

fn parse_config(config: &serde_json::Value) -> Result<ApiKeyPluginConfig, OagwError> {
    let payload = match config {
        serde_json::Value::Null => &serde_json::Value::Object(serde_json::Map::new()),
        value => value,
    };
    serde_json::from_value(payload.clone())
        .map_err(|error| OagwError::validation(format!("invalid api key plugin config: {error}")))
}

fn inject(ctx: &mut RequestContext, name: &str, value: &str) -> Result<(), OagwError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| OagwError::validation(format!("invalid api key header name '{name}'")))?;
    let header_value = HeaderValue::from_str(value)
        .map_err(|_| OagwError::validation("api key credential is not a valid header value"))?;
    ctx.headers
        .insert(header_name.clone(), header_value.clone());
    ctx.inject_header(header_name, header_value);
    Ok(())
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        builtin::APIKEY_AUTH
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_TYPE_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let value = self.credential(ctx).await?;
        match &self.query_name {
            Some(name) => {
                ctx.inject_query(name.clone(), value);
            }
            None => inject(ctx, &self.header_name, &value)?,
        }
        Ok(())
    }
}

/// Bare key of a `cred://` reference, for diagnostics and tests.
#[must_use]
pub fn api_key_reference_key(reference: &str) -> &str {
    strip_secret_scheme(reference)
}

#[cfg(test)]
#[path = "apikey_tests.rs"]
mod tests;
