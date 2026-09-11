//! The `request_id` transform plugin.
//!
//! Every proxied request carries an `X-Request-ID`; when the caller supplied
//! one it is propagated unchanged, otherwise a new identifier is minted. The
//! same identifier is echoed on the response so a caller can correlate the two
//! legs of an exchange.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{PluginType, RequestContext, ResponseContext};
use crate::infra::plugin::registry::PluginFactory;

/// The header carrying the correlation identifier.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The configuration key overriding the header name.
pub const HEADER_NAME_KEY: &str = "header_name";

/// Propagates or mints a request identifier.
#[derive(Debug, Clone)]
pub struct RequestIdTransform {
    header_name: String,
}

impl RequestIdTransform {
    /// Build the transform from its binding configuration.
    #[must_use]
    pub fn from_config(config: &serde_json::Map<String, serde_json::Value>) -> Self {
        let header_name = config
            .get(HEADER_NAME_KEY)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(REQUEST_ID_HEADER)
            .to_ascii_lowercase();
        Self { header_name }
    }

    /// The header the identifier travels in.
    #[must_use]
    pub fn header_name(&self) -> &str {
        &self.header_name
    }

    /// The identifier that would be used for `headers`.
    #[must_use]
    pub fn request_id_for(&self, headers: &http::HeaderMap) -> String {
        let carried = http::HeaderName::from_bytes(self.header_name.as_bytes())
            .ok()
            .and_then(|key| headers.get(&key))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty());
        if let Some(value) = carried {
            return value;
        }
        new_request_id()
    }
}

/// Mint a request identifier: 16 random bytes, hex encoded without hyphens.
#[must_use]
pub fn new_request_id() -> String {
    let bytes = uuid::Uuid::new_v4();
    format!("{:032x}", u128::from_be_bytes(*bytes.as_bytes()))
}

#[async_trait]
impl crate::domain::plugin::TransformPlugin for RequestIdTransform {
    fn id(&self) -> &'static str {
        "request_id"
    }

    fn plugin_type(&self) -> &'static str {
        "transform"
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let request_id = self.request_id_for(&ctx.headers);
        ctx.set_header(&self.header_name, &request_id);
        ctx.set_attribute("oagw.request_id", request_id);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), DomainError> {
        let carried = ctx
            .headers
            .get(self.header_name.as_str())
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.trim().is_empty());
        if carried {
            return Ok(());
        }
        let request_id = self.request_id_for(&ctx.headers);
        ctx.set_header(&self.header_name, &request_id);
        Ok(())
    }

    async fn transform_error(
        &self,
        ctx: &mut crate::domain::plugin::ErrorContext,
    ) -> Result<(), DomainError> {
        let request_id = new_request_id();
        ctx.set_header(&self.header_name, &request_id);
        Ok(())
    }
}

/// Builds `request_id` instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformFactory;

impl PluginFactory<dyn crate::domain::plugin::TransformPlugin> for RequestIdTransformFactory {
    fn id(&self) -> &'static str {
        "request_id"
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Transform
    }

    fn description(&self) -> &'static str {
        "Propagates or mints an X-Request-ID and echoes it on the answer"
    }

    fn create(
        &self,
        config: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn crate::domain::plugin::TransformPlugin>, DomainError> {
        Ok(Arc::new(RequestIdTransform::from_config(config)))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod request_id_transform_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::plugin::TransformPlugin;
    use serde_json::json;

    fn config(raw: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        raw.as_object().expect("object").clone()
    }

    fn request_context(headers: &[(&str, &str)]) -> RequestContext {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        RequestContext {
            method: "GET".to_owned(),
            path: "/v1".to_owned(),
            query: String::new(),
            headers: map,
            body_present: false,
            security_context: toolkit_security::SecurityContext::anonymous(),
            tenant_scope: vec![uuid::Uuid::nil()],
            injected_headers: Vec::new(),
            attributes: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn mints_an_identifier_when_none_was_supplied() {
        let transform = RequestIdTransform::from_config(&config(&json!({})));
        let mut ctx = request_context(&[]);
        transform.transform_request(&mut ctx).await.unwrap();
        let id = ctx
            .headers
            .get(REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(id.len(), 32, "hex-encoded uuid");
        assert_eq!(
            ctx.attributes.get("oagw.request_id").map(String::as_str),
            Some(id)
        );
        assert_eq!(ctx.injected_headers, vec![REQUEST_ID_HEADER.to_owned()]);
    }

    #[tokio::test]
    async fn propagates_the_caller_identifier() {
        let transform = RequestIdTransform::from_config(&config(&json!({})));
        let mut ctx = request_context(&[("X-Request-ID", "caller-1")]);
        transform.transform_request(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get(REQUEST_ID_HEADER).unwrap(),
            "caller-1",
            "a supplied identifier is propagated unchanged"
        );
    }

    #[tokio::test]
    async fn a_supplied_identifier_is_stable_across_calls() {
        let transform = RequestIdTransform::from_config(&config(&json!({})));
        let mut first = request_context(&[("X-Request-ID", "stable")]);
        transform.transform_request(&mut first).await.unwrap();
        let mut second = request_context(&[("X-Request-ID", "stable")]);
        transform.transform_request(&mut second).await.unwrap();
        assert_eq!(
            first.headers.get("x-request-id"),
            second.headers.get("x-request-id")
        );
    }

    #[tokio::test]
    async fn responses_carry_the_identifier() {
        let transform = RequestIdTransform::from_config(&config(&json!({})));
        let mut response = ResponseContext {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            injected_headers: Vec::new(),
        };
        transform.transform_response(&mut response).await.unwrap();
        assert!(response.headers.contains_key(REQUEST_ID_HEADER));

        let mut echoed = ResponseContext {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::from_iter([(
                http::HeaderName::from_static("x-request-id"),
                http::HeaderValue::from_static("upstream-id"),
            )]),
            injected_headers: Vec::new(),
        };
        transform.transform_response(&mut echoed).await.unwrap();
        assert_eq!(
            echoed.headers.get(REQUEST_ID_HEADER).unwrap(),
            "upstream-id"
        );
    }

    #[tokio::test]
    async fn errors_carry_an_identifier() {
        let transform = RequestIdTransform::from_config(&config(&json!({})));
        let mut error = crate::domain::plugin::ErrorContext {
            error: crate::domain::error::DomainError::route_not_found("nope"),
            headers: http::HeaderMap::new(),
        };
        transform.transform_error(&mut error).await.unwrap();
        assert!(error.headers.contains_key(REQUEST_ID_HEADER));
    }

    #[test]
    fn the_header_name_is_configurable() {
        let transform =
            RequestIdTransform::from_config(&config(&json!({"header_name": "X-Trace-Id"})));
        assert_eq!(transform.header_name(), "x-trace-id");
    }
}
