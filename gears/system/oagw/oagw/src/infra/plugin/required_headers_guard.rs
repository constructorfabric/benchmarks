//! Built-in `required_headers` guard plugin (ADR-0009).
//!
//! Rejects requests that do not carry every configured header. Values are
//! never echoed into error messages: only the header *name* is reported.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{GuardPlugin, PluginContext, PluginRequest, PluginResponse};

/// Configuration of the `required_headers` plugin.
///
/// Both spellings from the documentation are accepted: the array form
/// (`required` / `required_non_empty` / `forbidden`) and the ADR-0009
/// comma-separated form (`required_request_headers` /
/// `required_response_headers`).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct RequiredHeadersConfig {
    /// Header names that must be present.
    required: Vec<String>,
    /// Header names that must be present and non-empty.
    required_non_empty: Vec<String>,
    /// Header names that must NOT be present.
    forbidden: Vec<String>,
    /// ADR-0009 comma-separated list of mandatory request headers.
    required_request_headers: Option<String>,
    /// ADR-0009 comma-separated list of mandatory response headers, checked
    /// against the upstream's response (ADR-0009 response phase).
    required_response_headers: Option<String>,
}

/// Split an ADR-0009 comma-separated header list.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

impl RequiredHeadersConfig {
    /// The request headers that must be present, both spellings merged.
    fn required_request_headers(&self) -> Vec<String> {
        let mut names = self.required.clone();
        if let Some(list) = &self.required_request_headers {
            names.extend(split_list(list));
        }
        names
    }

    /// The response headers that must be present (ADR-0009).
    fn required_response_headers(&self) -> Vec<String> {
        self.required_response_headers
            .as_deref()
            .map(split_list)
            .unwrap_or_default()
    }
}

/// Guard plugin enforcing mandatory inbound headers.
pub struct RequiredHeadersGuardPlugin;

impl std::fmt::Debug for RequiredHeadersGuardPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequiredHeadersGuardPlugin")
    }
}

impl RequiredHeadersGuardPlugin {
    /// Build the plugin.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for RequiredHeadersGuardPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn name(&self) -> &'static str {
        "required_headers"
    }

    fn validate_config(&self, config: &serde_json::Value) -> Result<(), DomainError> {
        let parsed: RequiredHeadersConfig = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid required_headers config: {e}")))?;
        let required = parsed.required_request_headers();
        if required.is_empty()
            && parsed.required_non_empty.is_empty()
            && parsed.forbidden.is_empty()
        {
            return Err(DomainError::Validation(
                "required_headers requires at least one of 'required', 'required_non_empty' or 'forbidden'"
                    .into(),
            ));
        }
        for name in required
            .iter()
            .chain(parsed.required_non_empty.iter())
            .chain(parsed.forbidden.iter())
            .chain(
                parsed
                    .required_response_headers
                    .as_deref()
                    .map(split_list)
                    .unwrap_or_default()
                    .iter(),
            )
        {
            if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(DomainError::Validation(format!(
                    "'{name}' is not a valid HTTP header name"
                )));
            }
        }
        Ok(())
    }

    async fn guard(
        &self,
        _ctx: &PluginContext,
        config: &serde_json::Value,
        request: &PluginRequest,
    ) -> Result<(), DomainError> {
        let parsed: RequiredHeadersConfig = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid required_headers config: {e}")))?;
        for name in parsed.required_request_headers() {
            if request.headers.get(name.as_str()).is_none() {
                return Err(DomainError::RequiredHeaderMissing(name));
            }
        }
        for name in &parsed.required_non_empty {
            let empty = request
                .headers
                .get(name.as_str())
                .map(|v| v.as_bytes().is_empty())
                .unwrap_or(true);
            if empty {
                return Err(DomainError::RequiredHeaderMissing(name.clone()));
            }
        }
        for name in &parsed.forbidden {
            if request.headers.get(name.as_str()).is_some() {
                return Err(DomainError::RequiredHeaderMissing(format!(
                    "{name} must not be present"
                )));
            }
        }
        Ok(())
    }

    async fn guard_response(
        &self,
        _ctx: &PluginContext,
        config: &serde_json::Value,
        response: &PluginResponse,
    ) -> Result<(), DomainError> {
        let parsed: RequiredHeadersConfig = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid required_headers config: {e}")))?;
        for name in parsed.required_response_headers() {
            if response.headers.get(name.as_str()).is_none() {
                // ADR-0009 response phase: the upstream misbehaved, so the
                // client sees a gateway 502, not the upstream's response.
                return Err(DomainError::ResponseHeaderMissing(name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)]) -> PluginRequest {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                http::HeaderValue::from_str(value).expect("header value"),
            );
        }
        PluginRequest {
            headers: map,
            ..PluginRequest::default()
        }
    }

    #[tokio::test]
    async fn missing_header_is_rejected_by_name_only() {
        let plugin = RequiredHeadersGuardPlugin;
        let config = serde_json::json!({ "required": ["x-request-signature"] });
        plugin
            .guard(&PluginContext::default(), &config, &request(&[]))
            .await
            .expect_err("missing header");
        let err = plugin
            .guard(&PluginContext::default(), &config, &request(&[("x-request-signature", "v")]))
            .await;
        assert!(err.is_ok());
    }

    #[tokio::test]
    async fn empty_values_are_rejected_when_non_empty_is_required() {
        let plugin = RequiredHeadersGuardPlugin;
        let config = serde_json::json!({ "required_non_empty": ["x-trace"] });
        let err = plugin
            .guard(
                &PluginContext::default(),
                &config,
                &request(&[("x-trace", "")]),
            )
            .await
            .expect_err("empty header");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn invalid_header_names_are_rejected() {
        let plugin = RequiredHeadersGuardPlugin;
        assert!(
            plugin
                .validate_config(&serde_json::json!({ "required": ["bad header"] }))
                .is_err()
        );
        assert!(
            plugin
                .validate_config(&serde_json::json!({}))
                .is_err()
        );
        assert!(
            plugin
                .validate_config(&serde_json::json!({ "forbidden": ["x-internal-only"] }))
                .is_ok()
        );
    }
}
