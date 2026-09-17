//! Builtin transform plugins (ADR 0002).
//!
//! [`RequestIdTransformPlugin`] injects a fresh `X-Request-Id` (or a
//! configured alternate header) into the outbound request when the client
//! did not supply one, and passes it through on responses.

use async_trait::async_trait;
use http::header::HeaderMap;
use uuid::Uuid;

use crate::domain::plugin::{PluginContext, PluginError, TransformPlugin};
use crate::gts_helpers;

/// Transform — request-ID injection / propagation.
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::TRANSFORM_PLUGIN_REQUEST_ID
    }

    fn plugin_type(&self) -> &'static str {
        "transform"
    }

    async fn transform_request(
        &self,
        ctx: &PluginContext,
        headers: &mut HeaderMap,
    ) -> Result<(), PluginError> {
        let name = ctx
            .config
            .get("header_name")
            .and_then(|v| v.as_str())
            .unwrap_or("X-Request-Id");
        let name = http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            PluginError::Internal(format!("request_id plugin: invalid header name '{name}': {e}"))
        })?;
        if !headers.contains_key(&name) {
            let value = http::header::HeaderValue::from_str(&Uuid::new_v4().to_string())
                .map_err(|e| PluginError::Internal(format!("request_id plugin: {e}")))?;
            headers.insert(name, value);
        }
        Ok(())
    }

    async fn transform_response(
        &self,
        _ctx: &PluginContext,
        _headers: &mut HeaderMap,
    ) -> Result<(), PluginError> {
        // The injected request id rides upstream and back; no mutation here.
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use toolkit_security::SecurityContext;

    fn ctx(config: serde_json::Value) -> PluginContext {
        static CLIENT: std::sync::OnceLock<toolkit_http::HttpClient> = std::sync::OnceLock::new();
        let http = CLIENT
            .get_or_init(|| toolkit_http::HttpClient::new().expect("test http client"))
            .clone();
        PluginContext {
            security_context: SecurityContext::anonymous(),
            cred_store: Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
            http,
            config,
        }
    }

    #[tokio::test]
    async fn injects_request_id_when_absent() {
        let mut headers = HeaderMap::new();
        RequestIdTransformPlugin
            .transform_request(&ctx(json!({})), &mut headers)
            .await
            .unwrap();
        assert!(headers.contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn keeps_client_supplied_request_id() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-request-id",
            http::header::HeaderValue::from_static("client-rid-1"),
        );
        RequestIdTransformPlugin
            .transform_request(&ctx(json!({})), &mut headers)
            .await
            .unwrap();
        assert_eq!(
            headers.get("x-request-id").map(|v| v.to_str().unwrap()),
            Some("client-rid-1")
        );
    }

    #[tokio::test]
    async fn honors_custom_header_name() {
        let mut headers = HeaderMap::new();
        RequestIdTransformPlugin
            .transform_request(&ctx(json!({ "header_name": "X-Correlation-Id" })), &mut headers)
            .await
            .unwrap();
        assert!(headers.contains_key("x-correlation-id"));
        assert!(!headers.contains_key("x-request-id"));
    }
}
