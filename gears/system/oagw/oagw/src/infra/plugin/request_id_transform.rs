//! `RequestIdTransformPlugin` — `X-Request-ID` propagation.
//!
//! Reuses an inbound `X-Request-ID` when present and valid, otherwise mints a
//! fresh UUIDv4. The same value is stamped on the response so callers can
//! correlate a round trip end to end.

use async_trait::async_trait;

use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::{
    ErrorContext, PluginError, RequestContext, ResponseContext, TransformPlugin,
};

/// Header name propagated end to end.
pub const _NAME: &str = "X-Request-ID";
/// Maximum accepted inbound request-id length.
pub const MAX_REQUEST_ID_LEN: usize = 128;

/// `X-Request-ID` propagation transform.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

/// Returns the inbound request id when it is a usable opaque token.
#[must_use]
pub fn reusable_id(value: Option<&str>) -> Option<String> {
    let candidate = value?.trim();
    if candidate.is_empty() || candidate.len() > MAX_REQUEST_ID_LEN {
        return None;
    }
    if !candidate
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(candidate.to_owned())
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let id = reusable_id(ctx.header(_NAME)).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        ctx.set_header(_NAME, id);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        if ctx.header(_NAME).is_none() {
            ctx.headers
                .push((_NAME.to_owned(), uuid::Uuid::new_v4().to_string()));
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        let error = &mut ctx.error;
        *error = error.clone().with_extension(
            "request_id",
            serde_json::json!(uuid::Uuid::new_v4().to_string()),
        );
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reuses_inbound_id_and_mints_when_absent() {
        let plugin = RequestIdTransformPlugin;
        let mut ctx = RequestContext {
            headers: vec![(_NAME.to_owned(), " abc-123 ".to_owned())],
            ..RequestContext::default()
        };
        plugin.transform_request(&mut ctx).await.expect("ok");
        assert_eq!(ctx.header(_NAME), Some("abc-123"));

        let mut fresh = RequestContext::default();
        plugin.transform_request(&mut fresh).await.expect("ok");
        let minted = fresh.header(_NAME).expect("minted");
        assert!(uuid::Uuid::parse_str(minted).is_ok());
    }

    #[test]
    fn rejects_oversized_or_non_ascii_ids() {
        let oversized = "a".repeat(200);
        assert!(reusable_id(Some(oversized.as_str())).is_none());
        assert!(reusable_id(Some("has space")).is_none());
        assert!(reusable_id(Some("")).is_none());
        assert_eq!(reusable_id(Some("ok_id-9")), Some("ok_id-9".to_owned()));
    }
}
