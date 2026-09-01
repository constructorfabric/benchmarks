//! The API-key auth plugin (`DESIGN` §3.2, `API key injection (header/query)`).
//!
//! The upstream names one credential reference and where the key goes; the
//! plugin resolves the reference per request and injects the value. The value
//! is never logged, echoed in an error, or carried into a problem body
//! (`DESIGN` §"Credential isolation").

use std::sync::Arc;

use crate::domain::dto::ProxyContext;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::API_KEY_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthOutcome, AuthPlugin};
use crate::infra::plugin::secrets::SecretResolver;

/// Where an injected API key goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeyLocation {
    /// A request header.
    Header(String),
    /// A query parameter.
    Query(String),
}

impl ApiKeyLocation {
    /// Parse the config's `location`/`name` pair, defaulting to the
    /// `x-api-key` header.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] for an unknown `location`.
    pub fn parse(config: &serde_json::Value) -> Result<Self, DomainError> {
        let name = |key: &str, fallback: &str| -> String {
            config
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| fallback.to_owned())
        };
        match config.get("location").and_then(serde_json::Value::as_str) {
            None | Some("header") => Ok(Self::Header(name("name", "x-api-key"))),
            Some("query") => Ok(Self::Query(name("name", "api-key"))),
            Some(other) => Err(DomainError::validation(format!(
                "auth config location must be 'header' or 'query', got '{other}'"
            ))),
        }
    }
}

/// Injects an API key resolved from the credential store.
#[derive(Debug)]
pub struct ApiKeyAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
    location: ApiKeyLocation,
    reference: String,
}

impl ApiKeyAuthPlugin {
    /// Build the plugin for one upstream binding.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when the config is malformed and
    /// [`DomainError::PluginNotFound`] when the credential reference is absent.
    pub fn new(
        resolver: Arc<dyn SecretResolver>,
        config: Option<&serde_json::Value>,
    ) -> Result<Self, DomainError> {
        let config = config.cloned().unwrap_or(serde_json::Value::Null);
        let location = ApiKeyLocation::parse(&config)?;
        let reference = config
            .get("value_ref")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                DomainError::validation(
                    "apikey auth config requires a 'value_ref' credential reference",
                )
            })?;
        Ok(Self {
            resolver,
            location,
            reference: reference.to_owned(),
        })
    }
}

#[async_trait::async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn gts_id(&self) -> String {
        API_KEY_AUTH_PLUGIN_ID.to_owned()
    }

    async fn authenticate(&self, request: &mut ProxyContext) -> Result<AuthOutcome, DomainError> {
        let value = self
            .resolver
            .resolve(request.tenant, request.subject, &self.reference)
            .await?;
        let mut forwarded = std::collections::BTreeMap::new();
        match &self.location {
            ApiKeyLocation::Header(name) => {
                request.headers.insert(name.clone(), value.clone());
                forwarded.insert(name.clone(), value);
            }
            ApiKeyLocation::Query(name) => {
                request.query.push((name.clone(), value.clone()));
                forwarded.insert(format!("query:{name}"), value);
            }
        }
        Ok(AuthOutcome {
            subject: Some(request.subject.to_string()),
            forwarded_headers: forwarded,
        })
    }
}
