//! `request_id` transform plugin — propagates or generates `X-Request-ID`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{PluginConfig, RequestContext, TransformPlugin};

/// Stateless request-correlation transform.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        gts::TRANSFORM_REQUEST_ID
    }

    async fn on_request(
        &self,
        ctx: &mut RequestContext,
        _config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let existing = ctx
            .headers
            .get(gts::HEADER_REQUEST_ID)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let id = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
        let value = http::HeaderValue::from_str(&id).map_err(|_| {
            DomainError::Validation("x-request-id header value is invalid".to_owned())
        })?;
        ctx.headers.insert(gts::request_id_header(), value);
        ctx.attributes.set("oagw.request_id", id);
        Ok(())
    }

    async fn on_response(
        &self,
        ctx: &mut crate::domain::plugin::ResponseContext<'_>,
        _config: &PluginConfig,
    ) -> Result<(), DomainError> {
        if let Some(id) = ctx.request.attributes.get("oagw.request_id")
            && let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(gts::HEADER_REQUEST_ID.as_bytes()),
                http::HeaderValue::from_str(id),
            )
        {
            ctx.headers.insert(name, value);
        }
        Ok(())
    }
}
