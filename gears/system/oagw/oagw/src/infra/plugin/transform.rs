//! The `request_id` transform plugin
//! (`cpt-cf-oagw-dod-builtin-plugin-behaviors`,
//! `cpt-cf-oagw-algo-request-id-transform`).
//!
//! A stateless transform that establishes the `X-Request-ID` of a proxied
//! request in the request phase, propagates it onto the upstream response in
//! the response phase and mutates nothing in the error phase. An incoming
//! identifier is never rewritten and an upstream identifier is never
//! overwritten.
// @cpt-begin:cpt-cf-oagw-dod-builtin-plugin-behaviors:p1:inst-transform-full

use http::HeaderMap;
use http::header::HeaderName;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::plugin::{
    ErrorContext, RequestContext, ResponseContext, TRANSFORM_PLUGIN_TYPE, TransformPlugin,
};
use crate::infra::plugin::plan::PlanEntry;
use crate::infra::plugin::registry::REQUEST_ID_TRANSFORM_PLUGIN_ID;

/// The header name the transform establishes and propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The transform plugin establishing the `X-Request-ID` of a proxied request.
///
/// Stateless: no cache, no security-sensitive material and no per-request
/// state, and no configuration — the plugin reads no `config` key, so a binding
/// with an empty `config` object has the same behaviour as a populated one.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformPlugin;

/// Reads the request's established identifier, if the request carries one.
#[must_use]
pub fn request_id_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Sets the identifier header on a phase's headers, replacing any prior value.
fn set_request_id(headers: &mut HeaderMap, value: &str) {
    let Ok(name) = HeaderName::from_bytes(REQUEST_ID_HEADER.as_bytes()) else {
        return;
    };
    let Ok(value) = http::HeaderValue::from_str(value) else {
        return;
    };
    headers.insert(name, value);
}

impl RequestIdTransformPlugin {
    /// The request phase of the algorithm: an absent identifier is generated, a
    /// present one is left untouched.
    fn on_request(ctx: &mut RequestContext) {
        // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-01
        // In the request phase, check whether the incoming request already
        // carries `X-Request-ID`.
        if request_id_of(&ctx.headers).is_none() {
            // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-02
            // An absent identifier is generated as a UUID v4 and set as the
            // request's `X-Request-ID`.
            set_request_id(&mut ctx.headers, &Uuid::new_v4().to_string());
            // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-02
        }
        // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-03
        // A present identifier is left untouched, so an incoming
        // `X-Request-ID` is never rewritten.
        // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-03
        // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-01
    }

    /// The response phase of the algorithm: the request's identifier is copied
    /// onto a response the upstream did not tag.
    fn on_response(ctx: &mut ResponseContext) {
        // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-04
        // In the response phase, check whether the upstream set
        // `X-Request-ID` on the response.
        if request_id_of(&ctx.headers).is_none()
            // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-05
            // The upstream did not set it, so the request's value is copied
            // onto the response.
            && let Some(request_id) = ctx.request_id.clone()
        {
            set_request_id(&mut ctx.headers, &request_id);
            // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-05
        }
        // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-06
        // The upstream set it, so its value stays the response's value and is
        // never overwritten.
        // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-06
        // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-04
    }
}

#[async_trait::async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        TRANSFORM_PLUGIN_TYPE
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        Self::on_request(ctx);
        // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-08
        // The transform returns success, the identifier value never being
        // logged with any credential material attached to it.
        Ok(())
        // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-08
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        Self::on_response(ctx);
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError> {
        // @cpt-begin:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-07
        // In the error phase, mutate nothing: the DESIGN phase table declares
        // the request and response phases for this identifier, so
        // `transform_error` is a no-op that changes no field of the
        // `ErrorContext`.
        let _ = ctx;
        Ok(())
        // @cpt-end:cpt-cf-oagw-algo-request-id-transform:p1:inst-ri-07
    }
}

/// Drives the request phase of the transform tier, in plan order.
///
/// Each transform is invoked with the request context whose `config` is that
/// transform's own binding config; a transform failure stops the tier.
///
/// # Errors
/// Returns the typed failure of a transform invocation, mapped per §1.5.
pub async fn transform_request_phase(
    transforms: &[PlanEntry<dyn TransformPlugin>],
    ctx: &mut RequestContext,
) -> Result<(), OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-01
    // FOR EACH transform in the transform tier, in plan order, invoke
    // transform_request(&mut RequestContext), applying the request-id
    // algorithm above for the request_id plugin.
    for entry in transforms {
        ctx.config = entry.binding.config.clone().unwrap_or_default();
        entry.plugin.transform_request(ctx).await?;
    }
    // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-01
    Ok(())
}

/// Drives the response phase of the transform tier, in plan order.
///
/// The symmetric counterpart of [`transform_request_phase`] over the upstream
/// response.
///
/// # Errors
/// Returns the typed failure of a transform invocation, mapped per §1.5.
pub async fn transform_response_phase(
    transforms: &[PlanEntry<dyn TransformPlugin>],
    ctx: &mut ResponseContext,
) -> Result<(), OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-03
    // The caller re-entered the flow with the upstream `ResponseContext`:
    // invoke, for each transform of the transform tier in plan order,
    // transform_response(&mut ResponseContext), applying the propagation rule
    // of the request-id algorithm above.
    for entry in transforms {
        ctx.config = entry.binding.config.clone().unwrap_or_default();
        entry.plugin.transform_response(ctx).await?;
    }
    // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-03
    Ok(())
}

/// Drives the error phase of the transform tier, in plan order.
///
/// Invoked on the `ErrorContext` of a failed upstream call or of an earlier
/// failed phase.
///
/// # Errors
/// Returns the typed failure of a transform invocation, mapped per §1.5.
pub async fn transform_error_phase(
    transforms: &[PlanEntry<dyn TransformPlugin>],
    ctx: &mut ErrorContext,
) -> Result<(), OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-04
    // The caller re-entered the flow with the `ErrorContext` of a failed call
    // or of an earlier phase: invoke, for each transform of the transform tier
    // in plan order, transform_error(&mut ErrorContext); for the request_id
    // plugin this is the no-op its declared phase set implies.
    for entry in transforms {
        entry.plugin.transform_error(ctx).await?;
    }
    // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-04
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolkit_security::SecurityContext;

    fn plugin() -> RequestIdTransformPlugin {
        RequestIdTransformPlugin
    }

    fn request() -> RequestContext {
        let security = SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        RequestContext::new(security)
    }

    fn response() -> ResponseContext {
        ResponseContext::default()
    }

    #[tokio::test]
    async fn an_absent_incoming_identifier_is_generated() {
        let mut ctx = request();
        assert!(request_id_of(&ctx.headers).is_none());

        plugin().transform_request(&mut ctx).await.unwrap();

        let id = request_id_of(&ctx.headers).expect("an identifier is established");
        assert!(
            Uuid::parse_str(&id).is_ok(),
            "'{id}' is a UUID v4 and not any other shape"
        );
    }

    #[tokio::test]
    async fn two_generated_identifiers_differ() {
        let mut first = request();
        let mut second = request();
        plugin().transform_request(&mut first).await.unwrap();
        plugin().transform_request(&mut second).await.unwrap();

        assert_ne!(
            request_id_of(&first.headers),
            request_id_of(&second.headers),
            "each request carries its own identifier"
        );
    }

    #[tokio::test]
    async fn an_incoming_identifier_is_never_rewritten() {
        let mut ctx = request();
        ctx.headers
            .insert(REQUEST_ID_HEADER, "incoming-id".parse().unwrap());

        plugin().transform_request(&mut ctx).await.unwrap();
        let id = request_id_of(&ctx.headers).expect("the identifier is present");

        assert_eq!(id, "incoming-id", "the incoming value is left untouched");
        assert_eq!(ctx.headers.len(), 1, "no second identifier value is added");
    }

    #[tokio::test]
    async fn the_request_identifier_is_copied_onto_an_untagged_response() {
        let mut ctx = request();
        plugin().transform_request(&mut ctx).await.unwrap();
        let request_id = request_id_of(&ctx.headers).expect("the request id");

        let mut upstream = response();
        upstream.request_id = Some(request_id.clone());
        plugin().transform_response(&mut upstream).await.unwrap();

        assert_eq!(
            request_id_of(&upstream.headers).as_deref(),
            Some(request_id.as_str()),
            "the request's value is copied onto the response"
        );
    }

    #[tokio::test]
    async fn an_upstream_identifier_is_never_overwritten() {
        let mut upstream = response();
        upstream
            .headers
            .insert(REQUEST_ID_HEADER, "upstream-id".parse().unwrap());
        upstream.request_id = Some("request-id".to_owned());

        plugin().transform_response(&mut upstream).await.unwrap();

        assert_eq!(
            request_id_of(&upstream.headers).as_deref(),
            Some("upstream-id"),
            "the upstream's value stays the response's value"
        );
    }

    #[tokio::test]
    async fn a_response_without_a_request_identifier_gains_none() {
        let mut upstream = response();
        assert!(upstream.request_id.is_none());

        plugin().transform_response(&mut upstream).await.unwrap();

        assert!(
            request_id_of(&upstream.headers).is_none(),
            "nothing is invented in the response phase"
        );
    }

    #[tokio::test]
    async fn the_error_phase_mutates_nothing() {
        let mut error = ErrorContext::default();
        let before = format!("{error:?}");

        plugin().transform_error(&mut error).await.unwrap();

        assert_eq!(
            format!("{error:?}"),
            before,
            "no field of the error context changes"
        );
    }

    #[tokio::test]
    async fn the_transform_declares_its_identifier_and_family() {
        assert_eq!(
            plugin().id(),
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        );
        assert_eq!(plugin().plugin_type(), TRANSFORM_PLUGIN_TYPE);
    }

    #[test]
    fn the_plugin_holds_no_state() {
        assert!(RequestIdTransformPlugin.id().ends_with("request_id.v1"));
        assert_eq!(REQUEST_ID_HEADER, "x-request-id");
    }
}

// @cpt-end:cpt-cf-oagw-dod-builtin-plugin-behaviors:p1:inst-transform-full
