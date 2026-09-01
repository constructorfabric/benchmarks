//! The request-id transform plugin (`DESIGN` §3.2, correlation).
//!
//! The gateway stamps one `x-request-id` per request: an inbound value is
//! forwarded untouched, otherwise a UUID is minted. The value is echoed on the
//! response so a caller can correlate a failure with the gateway's logs.

use serde_json::Value;

use crate::domain::dto::ProxyContext;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::REQUEST_ID_HEADER;
use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::TransformPlugin;

/// Ensures every proxied request carries an `x-request-id`.
#[derive(Debug, Clone)]
pub struct RequestIdTransformPlugin {
    /// Existing ids are forwarded when set, minted otherwise.
    forward_inbound: bool,
}

impl RequestIdTransformPlugin {
    /// Build the plugin for one binding.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when `forward_inbound` is not a
    /// boolean.
    pub fn new(config: Option<&Value>) -> Result<Self, DomainError> {
        let config = config.cloned().unwrap_or(Value::Null);
        let forward_inbound = match config.get("forward_inbound") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(value)) => *value,
            Some(other) => {
                return Err(DomainError::validation(format!(
                    "request_id transform config 'forward_inbound' must be a boolean, got {other}"
                )));
            }
        };
        Ok(Self { forward_inbound })
    }

    /// The id this binding puts on `request`: the inbound value when forwarding
    /// is enabled and one is present, a fresh UUID otherwise.
    #[must_use]
    pub fn request_id_for(&self, request: &ProxyContext) -> String {
        if self.forward_inbound
            && let Some(existing) = request.header(REQUEST_ID_HEADER)
            && !existing.trim().is_empty()
        {
            return existing.trim().to_owned();
        }
        uuid::Uuid::now_v7().to_string()
    }
}

#[async_trait::async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn gts_id(&self) -> String {
        REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned()
    }

    async fn transform_request(&self, request: &mut ProxyContext) -> Result<(), DomainError> {
        let request_id = self.request_id_for(request);
        request
            .headers
            .insert(REQUEST_ID_HEADER.to_owned(), request_id.clone());
        if request.trace_id.is_none() {
            request.trace_id = Some(request_id);
        }
        Ok(())
    }
}
