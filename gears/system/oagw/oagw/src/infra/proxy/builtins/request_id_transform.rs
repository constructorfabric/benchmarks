//! The `request_id` transform plugin (DESIGN.md §3.3 “Error Response Format”).
//!
//! Propagates `X-Request-ID`/`X-Correlation-ID` end to end: an inbound id is
//! reused, otherwise a new UUID is minted. The id is written back onto the
//! response (and onto gateway errors) so a caller can always correlate a
//! response with its request.

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::plugin::{
    ErrorContext, PluginError, RequestContext, ResponseContext, TransformPlugin,
};

const INSTANCE: &str = "cf.core.oagw.request_id.v1";
const PLUGIN_TYPE: &str = "cf.core.oagw.transform_plugin.v1";
pub(crate) const REQUEST_ID: &str = "x-request-id";
pub(crate) const CORRELATION_ID: &str = "x-correlation-id";

/// Transform plugin that propagates the request id.
///
/// The instance is created per request, so the id assigned on the way out is
/// remembered for the way back.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin {
    assigned: Mutex<Option<String>>,
}

fn inbound_id(ctx: &RequestContext) -> Option<String> {
    for name in [REQUEST_ID, CORRELATION_ID] {
        if let Some(value) = ctx
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_owned());
        }
    }
    None
}

/// Inserts `name: value` when both parse as header components.
fn set_header(headers: &mut axum::http::HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        axum::http::header::HeaderName::try_from(name),
        axum::http::header::HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        INSTANCE
    }

    fn plugin_type(&self) -> &str {
        PLUGIN_TYPE
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let id = inbound_id(ctx).unwrap_or_else(|| Uuid::new_v4().to_string());
        *self.assigned.lock() = Some(id.clone());
        ctx.set_attribute("oagw.request_id", id.clone());
        set_header(&mut ctx.headers, REQUEST_ID, &id);
        if !ctx.headers.contains_key(CORRELATION_ID) {
            set_header(&mut ctx.headers, CORRELATION_ID, &id);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let id = ctx
            .headers
            .get(REQUEST_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| self.assigned.lock().clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        set_header(&mut ctx.headers, REQUEST_ID, &id);
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        let id = self
            .assigned
            .lock()
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        ctx.headers.retain(|(name, _)| name != REQUEST_ID);
        ctx.headers.push((REQUEST_ID.to_owned(), id));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use uuid::Uuid;

    fn ctx() -> RequestContext {
        RequestContext {
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            query: String::new(),
            headers: axum::http::HeaderMap::new(),
            body: Bytes::new(),
            config: serde_json::Value::Null,
            attributes: Default::default(),
        }
    }

    fn header(ctx: &RequestContext, name: &str) -> Option<String> {
        ctx.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    #[tokio::test]
    async fn reuses_the_inbound_request_id() {
        let plugin = RequestIdTransformPlugin::default();
        let mut ctx = ctx();
        ctx.headers.insert(
            axum::http::header::HeaderName::from_static(REQUEST_ID),
            axum::http::header::HeaderValue::from_static("req-42"),
        );
        plugin
            .transform_request(&mut ctx)
            .await
            .expect("transformed");
        assert_eq!(header(&ctx, REQUEST_ID).as_deref(), Some("req-42"));
        assert_eq!(header(&ctx, CORRELATION_ID).as_deref(), Some("req-42"));

        let mut response = ResponseContext {
            status: 200,
            headers: axum::http::HeaderMap::new(),
            body: Bytes::new(),
            config: serde_json::Value::Null,
        };
        plugin
            .transform_response(&mut response)
            .await
            .expect("transformed");
        assert_eq!(
            response
                .headers
                .get(REQUEST_ID)
                .and_then(|value| value.to_str().ok()),
            Some("req-42")
        );

        let mut error = ErrorContext {
            kind: crate::domain::error::ErrorKind::Internal,
            detail: "boom".to_owned(),
            headers: Vec::new(),
            config: serde_json::Value::Null,
        };
        plugin
            .transform_error(&mut error)
            .await
            .expect("transformed");
        assert_eq!(
            error.headers,
            vec![(REQUEST_ID.to_owned(), "req-42".to_owned())]
        );
    }

    #[tokio::test]
    async fn mints_an_id_when_none_is_present() {
        let plugin = RequestIdTransformPlugin::default();
        let mut ctx = ctx();
        plugin
            .transform_request(&mut ctx)
            .await
            .expect("transformed");
        let id = header(&ctx, REQUEST_ID).expect("id");
        assert!(
            Uuid::parse_str(&id).is_ok(),
            "generated id must be a uuid: {id}"
        );
        assert_eq!(ctx.attribute("oagw.request_id"), Some(id.as_str()));
    }
}
