//! Built-in `request_id` transform plugin.
//!
//! Guarantees that every proxied request carries a correlation identifier,
//! honouring an inbound header when configured to do so.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::plugin::{PluginContext, TransformPlugin};

/// Configuration of the `request_id` plugin.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct RequestIdConfig {
    /// Header that carries the identifier.
    header: String,
    /// `true` to reuse an inbound identifier when present.
    forward_incoming: bool,
    /// Prefix prepended to the generated identifier.
    prefix: Option<String>,
}

/// Transform plugin that injects a correlation identifier.
pub struct RequestIdTransformPlugin;

impl std::fmt::Debug for RequestIdTransformPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestIdTransformPlugin")
    }
}

impl RequestIdTransformPlugin {
    /// Build the plugin.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for RequestIdTransformPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn name(&self) -> &'static str {
        "request_id"
    }

    fn validate_config(&self, config: &serde_json::Value) -> Result<(), DomainError> {
        let parsed: RequestIdConfig = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid request_id config: {e}")))?;
        let header = if parsed.header.trim().is_empty() {
            String::from("X-Request-Id")
        } else {
            parsed.header.trim().to_owned()
        };
        if http::HeaderName::from_bytes(header.as_bytes()).is_err() {
            return Err(DomainError::Validation(format!(
                "'{header}' is not a valid HTTP header name"
            )));
        }
        Ok(())
    }

    async fn transform_request(
        &self,
        ctx: &mut PluginContext,
        config: &serde_json::Value,
        request: &mut http::Request<()>,
    ) {
        let Ok(parsed) = serde_json::from_value::<RequestIdConfig>(config.clone()) else {
            return;
        };
        let header = if parsed.header.trim().is_empty() {
            String::from("X-Request-Id")
        } else {
            parsed.header.trim().to_owned()
        };
        let Ok(name) = http::HeaderName::from_bytes(header.as_bytes()) else {
            return;
        };

        let existing = parsed
            .forward_incoming
            .then(|| request.headers().get(&name))
            .flatten()
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned);

        let value = existing.unwrap_or_else(|| {
            let id = Uuid::now_v7().to_string();
            match parsed.prefix.as_deref().filter(|p| !p.is_empty()) {
                Some(prefix) => format!("{prefix}-{id}"),
                None => id,
            }
        });

        // Recorded so the response half can echo the same correlation id.
        ctx.attributes
            .insert(String::from("request_id"), value.clone());

        if let Ok(header_value) = http::HeaderValue::from_str(&value) {
            request.headers_mut().insert(name, header_value);
        }
    }

    async fn transform_response(
        &self,
        ctx: &mut PluginContext,
        config: &serde_json::Value,
        response: &mut http::Response<()>,
    ) {
        let Ok(parsed) = serde_json::from_value::<RequestIdConfig>(config.clone()) else {
            return;
        };
        let header = if parsed.header.trim().is_empty() {
            String::from("X-Request-Id")
        } else {
            parsed.header.trim().to_owned()
        };
        let Ok(name) = http::HeaderName::from_bytes(header.as_bytes()) else {
            return;
        };
        // Echo the identifier stamped on the request; generate one when the
        // request half never ran (e.g. a re-used context).
        let value = ctx
            .attributes
            .get("request_id")
            .cloned()
            .unwrap_or_else(|| {
                let id = Uuid::now_v7().to_string();
                match parsed.prefix.as_deref().filter(|p| !p.is_empty()) {
                    Some(prefix) => format!("{prefix}-{id}"),
                    None => id,
                }
            });
        if let Ok(header_value) = http::HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, header_value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generates_an_identifier_when_absent() {
        let plugin = RequestIdTransformPlugin;
        let mut request = http::Request::builder().method("GET").uri("/v1").body(()).unwrap();
        plugin
            .transform_request(
                &mut PluginContext::default(),
                &serde_json::json!({}),
                &mut request,
            )
            .await;
        assert!(request.headers().get("x-request-id").is_some());
    }

    #[tokio::test]
    async fn forward_incoming_reuses_the_inbound_value() {
        let plugin = RequestIdTransformPlugin;
        let mut request = http::Request::builder()
            .method("GET")
            .uri("/v1")
            .header("x-request-id", "incoming-1")
            .body(())
            .unwrap();
        plugin
            .transform_request(
                &mut PluginContext::default(),
                &serde_json::json!({ "forward_incoming": true }),
                &mut request,
            )
            .await;
        assert_eq!(
            request.headers().get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("incoming-1")
        );
    }

    #[tokio::test]
    async fn forward_incoming_false_overwrites() {
        let plugin = RequestIdTransformPlugin;
        let mut request = http::Request::builder()
            .method("GET")
            .uri("/v1")
            .header("x-request-id", "incoming-1")
            .body(())
            .unwrap();
        plugin
            .transform_request(
                &mut PluginContext::default(),
                &serde_json::json!({ "forward_incoming": false }),
                &mut request,
            )
            .await;
        assert_ne!(
            request.headers().get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("incoming-1")
        );
    }

    #[test]
    fn invalid_header_name_is_rejected() {
        let plugin = RequestIdTransformPlugin;
        assert!(
            plugin
                .validate_config(&serde_json::json!({ "header": "bad header" }))
                .is_err()
        );
        assert!(plugin.validate_config(&serde_json::json!({})).is_ok());
    }
}
