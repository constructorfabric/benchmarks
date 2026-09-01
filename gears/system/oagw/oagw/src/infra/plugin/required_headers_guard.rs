//! The required-headers guard plugin (`ADR`-0009).
//!
//! The binding names two comma-separated lists — headers a request must carry
//! and headers an upstream response must carry. Presence only: a value is never
//! inspected. A blank list is fail-open, the comparison is case-insensitive,
//! and a denial names the first missing header only, so the guard never leaks
//! what an upstream sends.

use serde_json::Value;

use crate::domain::error::DomainError;

use crate::domain::dto::ProxyContext;
use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{GuardDecision, GuardPlugin, ResponseContext};

/// Enforces the `required_request_headers` / `required_response_headers` lists.
#[derive(Debug, Clone, Default)]
pub struct RequiredHeadersGuardPlugin {
    request_headers: Vec<String>,
    response_headers: Vec<String>,
}

impl RequiredHeadersGuardPlugin {
    /// Build the plugin for one binding. A blank or absent list is fail-open
    /// (`ADR`-0009), so no binding is malformed.
    #[must_use]
    pub fn new(config: Option<&Value>) -> Self {
        let config = config.cloned().unwrap_or(Value::Null);
        Self {
            request_headers: parse_list(&config, "required_request_headers"),
            response_headers: parse_list(&config, "required_response_headers"),
        }
    }

    /// The configured request-side list, in binding order.
    #[must_use]
    pub fn request_headers(&self) -> &[String] {
        &self.request_headers
    }

    /// The configured response headers, in binding order.
    #[must_use]
    pub fn response_headers(&self) -> &[String] {
        &self.response_headers
    }

    /// The first configured request header that `request` does not carry.
    #[must_use]
    pub fn first_missing_request_header(&self, request: &ProxyContext) -> Option<String> {
        self.request_headers
            .iter()
            .find(|name| request.header(name).is_none())
            .cloned()
    }

    /// The first configured response header that `response` does not carry.
    #[must_use]
    pub fn first_missing_response_header(&self, response: &ResponseContext) -> Option<String> {
        self.response_headers
            .iter()
            .find(|name| !response.headers.contains_key(name.as_str()))
            .cloned()
    }
}

/// Comma-separated, trimmed, lowercased, de-duplicated, empty when absent.
fn parse_list(config: &Value, key: &str) -> Vec<String> {
    let raw = config
        .get(key)
        .and_then(Value::as_str)
        .map_or_else(String::new, |value| value.trim().to_owned());
    let mut seen = std::collections::BTreeSet::new();
    raw.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

#[async_trait::async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn gts_id(&self) -> String {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned()
    }

    async fn guard_request(&self, request: &ProxyContext) -> Result<GuardDecision, DomainError> {
        Ok(match self.first_missing_request_header(request) {
            None => GuardDecision::Allow,
            Some(name) => GuardDecision::Deny(format!("required request header '{name}' missing")),
        })
    }

    async fn guard_response(
        &self,
        response: &ResponseContext,
    ) -> Result<GuardDecision, DomainError> {
        Ok(match self.first_missing_response_header(response) {
            None => GuardDecision::Allow,
            Some(name) => GuardDecision::Deny(format!("required response header '{name}' missing")),
        })
    }
}
