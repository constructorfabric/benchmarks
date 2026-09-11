//! The built-in `request_id` transform plugin
//! (`cpt-cf-oagw-dod-plugin-system-request-id-transform`).
//!
//! `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1` is the
//! **only** writer of the `X-Request-ID` header. The correlation identifier
//! itself is minted once at proxy entry by entry 2.4 and propagated by the
//! trace-propagation flow of the observability entry, so this plugin *adopts*
//! that value from the request context rather than minting a second one, and
//! mints a fresh lowercase hyphenated UUID only when no propagated value is
//! present or the propagated value is invalid.
//!
//! The value is held in the request context through the response phase and is
//! never added to the response headers returned to the caller: caller-visible
//! correlation is the `trace_id` extension member of the shared error
//! contract, owned by the error-handling entry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::plugin::{ErrorContext, PluginError, RequestContext, ResponseContext, TransformPlugin};

/// `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1`
pub const REQUEST_ID_PLUGIN_TYPE: &str =
    crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;

/// The only header name this plugin writes.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// The request-context extension the minted or adopted value is held under
/// through the response phase.
pub const REQUEST_ID_EXTENSION: &str = "oagw.request_id";
/// The proxy-entry correlation identifier, minted by entry 2.4.
pub const CORRELATION_EXTENSION: &str = "request_id";
/// The longest inbound value that is reused rather than replaced.
pub const MAX_INBOUND_LENGTH: usize = 128;

/// The built-in request-id transform.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformPlugin;

impl RequestIdTransformPlugin {
    /// Validate an inbound value against the 1..=128-character window over the
    /// RFC 3986 unreserved set plus `-._~`, and return the value to reuse, or
    /// `None` when the freshly minted identifier must replace it.
    #[must_use]
    pub fn validate_inbound(value: &str) -> Option<&str> {
        let ok = !value.is_empty()
            && value.len() <= MAX_INBOUND_LENGTH
            && value.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '-' | '.' | '_' | '~')
            });
        ok.then_some(value)
    }

    /// The identifier the request phase settles on: the validated proxy-entry
    /// correlation identifier when one was propagated, otherwise a freshly
    /// minted lowercase hyphenated UUID.
    #[must_use]
    pub fn resolve(ctx: &RequestContext) -> String {
        let propagated = ctx
            .extensions
            .iter()
            .find(|(name, _)| name == CORRELATION_EXTENSION)
            .map(|(_, value)| value.as_str())
            .or_else(|| {
                ctx.headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(REQUEST_ID_HEADER))
                    .map(|(_, value)| value.as_str())
            });
        match propagated.and_then(Self::validate_inbound) {
            Some(valid) => valid.to_owned(),
            None => Uuid::new_v4().to_string(),
        }
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        REQUEST_ID_PLUGIN_TYPE
    }

    // @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-3
    // `inst-ps-seq-3`: the request phase is where the only `X-Request-ID`
    // write happens; the value is then held in the request context through the
    // response phase.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let resolved = Self::resolve(ctx);
        // An invalid inbound header value is replaced rather than forwarded.
        retain_header(&mut ctx.headers, REQUEST_ID_HEADER);
        ctx.headers.push((REQUEST_ID_HEADER.to_owned(), resolved.clone()));
        upsert(&mut ctx.extensions, REQUEST_ID_EXTENSION, &resolved);
        Ok(())
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-3

    // `inst-ps-exec-11`: the response phase runs the transform again, and this
    // plugin's contribution to it is deliberately nothing: the value is never
    // added to the response headers returned to the caller.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        // The request-phase value is held in the request context and is not
        // copied into the response; a header an upstream echoed back is
        // dropped so the caller-visible correlation surface stays `trace_id`.
        retain_header(&mut ctx.headers, REQUEST_ID_HEADER);
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), PluginError> {
        Ok(())
    }
}

// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-1
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-10
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-2
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-4
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-5
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-6
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-7
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-8
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-9
/// Remove every entry named `name`, case-insensitively.
fn retain_header(headers: &mut Vec<(String, String)>, name: &str) {
    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
}
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-9
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-8
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-7
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-6
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-5
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-4
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-2
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-10
// @cpt-end:cpt-cf-oagw-algo-plugin-system-execution-order:p1:inst-ps-seq-1
//

/// Set `name` to `value`, replacing any earlier entry.
fn upsert(extensions: &mut Vec<(String, String)>, name: &str, value: &str) {
    extensions.retain(|(existing, _)| existing != name);
    extensions.push((name.to_owned(), value.to_owned()));
}

#[cfg(test)]
#[path = "request_id_transform_tests.rs"]
mod request_id_transform_tests;
