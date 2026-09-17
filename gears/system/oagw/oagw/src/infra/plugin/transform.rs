//! Built-in transform plugin — `request_id` (DoD
//! `cpt-cf-oagw-dod-plugin-system-builtins`).
//!
//! Propagates `X-Request-ID` across the proxy lifecycle (algorithm
//! `cpt-cf-oagw-algo-plugin-system-execute-chain`): the request phase mints a
//! fresh correlation id when the inbound request carries none and assigns it to
//! the outbound request; the response/error phases are identity transforms
//! (header propagation into clients is owned by the Observability feature,
//! per FEATURE.md §28).

use async_trait::async_trait;

use crate::domain::DomainError;
use crate::domain::plugin::{ErrorContext, RequestContext, ResponseContext, TransformPlugin};

/// The correlation header propagated by this plugin.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// `request_id` — mints and propagates `X-Request-ID` on the outbound request.
///
/// The request phase reuses an inbound `X-Request-ID` when present and mints a
/// fresh one otherwise; the response and error phases leave the contexts
/// unchanged (downstream correlation is an Observability-feature concern).
#[derive(Debug, Clone, Default)]
pub struct RequestIdTransformPlugin;

impl RequestIdTransformPlugin {
    /// Generates a fresh correlation id for this request.
    #[must_use]
    fn new_request_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::ids::REQUEST_ID_TRANSFORM
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        // Propagate an inbound id or mint a fresh one (step
        // `inst-ps-chain-transform-req`).
        if !ctx.headers.contains(REQUEST_ID_HEADER) {
            ctx.headers
                .insert(REQUEST_ID_HEADER, Self::new_request_id());
        }
        Ok(())
    }

    async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), DomainError> {
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), DomainError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::Headers;
    use crate::domain::plugin::ids::REQUEST_ID_TRANSFORM;

    #[tokio::test]
    async fn request_phase_mints_a_fresh_id_when_absent() {
        let p = RequestIdTransformPlugin;
        let mut ctx = RequestContext {
            headers: Headers::new(),
            config: serde_json::json!({}),
            ..RequestContext::default()
        };
        p.transform_request(&mut ctx).await.expect("never fails");
        let id = ctx.headers.get(REQUEST_ID_HEADER).expect("id injected");
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn request_phase_reuses_an_inbound_id() {
        let p = RequestIdTransformPlugin;
        let mut ctx = RequestContext {
            headers: Headers::new(),
            config: serde_json::json!({}),
            ..RequestContext::default()
        };
        ctx.headers.insert("X-Request-ID", "inbound-42");
        p.transform_request(&mut ctx).await.expect("never fails");
        assert_eq!(ctx.headers.get(REQUEST_ID_HEADER), Some("inbound-42"));
    }

    #[tokio::test]
    async fn response_and_error_phases_are_identity() {
        let p = RequestIdTransformPlugin;
        let mut resp = ResponseContext {
            status: 200,
            headers: Headers::new(),
            config: serde_json::json!({}),
        };
        p.transform_response(&mut resp).await.expect("identity");
        assert!(resp.headers.is_empty());

        let mut err = ErrorContext {
            status: 502,
            headers: Headers::new(),
            detail: "boom".to_owned(),
            config: serde_json::json!({}),
        };
        p.transform_error(&mut err).await.expect("identity");
        assert!(err.headers.is_empty());
    }

    #[tokio::test]
    async fn id_and_plugin_type_reflect_registration() {
        let p = RequestIdTransformPlugin;
        assert_eq!(p.id(), "request_id");
        assert_eq!(p.plugin_type(), REQUEST_ID_TRANSFORM);
    }
}
