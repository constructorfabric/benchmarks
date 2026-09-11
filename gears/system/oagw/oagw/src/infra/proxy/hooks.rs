//! The plugin chain hook points entry 2.5 executes.
//!
//! Feature 2.4 defined the hook points and their inputs; this feature
//! implements the chain behind them. A hook point stays a *seam*: the pipeline
//! hands the hook the merged configuration, the ordered plugin binding list,
//! the request or response facts and the request context, and consumes the
//! outcome. A rejection is mapped onto this feature's error table and stops the
//! pipeline before the upstream call; a continuation hands the mutated request
//! or response back to the pipeline.
//!
//! The chain implementation itself lives in [`crate::infra::proxy::chain`];
//! [`NoPlugins`] is the empty chain the pipeline is wired with when no
//! implementation is installed.

use std::fmt;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::PluginBinding;
use crate::infra::proxy::context::RequestContext;
use crate::infra::proxy::context::ResponseContext;
use crate::infra::proxy::effective::EffectiveUpstream;
use crate::infra::proxy::validate::CorsDecision;

// @cpt-begin:cpt-cf-oagw-dod-plugin-hook-points:p1:inst-full
/// The outcome a hook point returns to the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOutcome {
    /// The chain continues the pipeline.
    Continue,
    /// The chain rejects the request with a mapped error.
    Reject,
}

/// What the request-phase chain produced for the outbound request.
///
/// The chain returns the mutations instead of applying them, so the pipeline
/// stays the one writer of the outbound surface: it applies the headers to the
/// transformed set and the query members to the forwarded query string
/// (`inst-pc-14`).
#[derive(Debug, Clone, Default)]
pub struct RequestEffects {
    /// Request headers to set on the outbound request, in application order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Query members to append to the forwarded query string.
    pub query: Vec<(String, String)>,
    /// The CORS decision of an actual cross-origin request, when the chain
    /// evaluated one. The pipeline uses it for the response headers and skips
    /// its own evaluation when it is present.
    pub cors: Option<CorsDecision>,
}

/// The facts of the proxied request a plugin chain reads.
///
/// The struct is the read-only projection of the request the hook receives: it
/// carries no body and no resolved credential, and the header set it exposes is
/// the inbound one.
pub struct PluginRequest<'a> {
    /// The security context of the authenticated caller.
    pub security: &'a SecurityContext,
    /// The calling tenant, from the security context.
    pub tenant_id: Uuid,
    /// The HTTP method of the request.
    pub method: &'a str,
    /// The query string, without the leading `?`.
    pub query: Option<&'a str>,
    /// The inbound header set.
    pub headers: &'a HeaderMap,
    /// The `Origin` header of the request, when it carried one.
    pub origin: Option<&'a str>,
    /// The peer address of the inbound connection, when the host provided one.
    pub peer_ip: Option<&'a str>,
}

impl fmt::Debug for PluginRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The inbound header set is never rendered: a credential the caller
        // sent must not reach a log line through a debug print.
        formatter
            .debug_struct("PluginRequest")
            .field("tenant_id", &self.tenant_id)
            .field("method", &self.method)
            .field("query", &self.query)
            .field("origin", &self.origin)
            .field("peer_ip", &self.peer_ip)
            .finish_non_exhaustive()
    }
}

/// The plugin chain seam the pipeline calls.
///
/// The pipeline supplies the merged configuration, the ordered binding list and
/// the request facts; the implementation decides what runs. This crate's own
/// implementation is the chain of [`crate::infra::proxy::chain`]; [`NoPlugins`]
/// is the empty one.
#[async_trait]
pub trait PluginChains: Send + Sync {
    /// The request-phase hook point: after the configuration merge and before
    /// endpoint selection.
    ///
    /// # Errors
    ///
    /// Returns the mapped error of the chain's rejection: `401`
    /// AuthenticationFailed, `400` guard validation, `403` CORS enforcement,
    /// `429` RateLimitExceeded, `500` SecretNotFound, `503` PluginNotFound or
    /// `504` a plugin that outlived the request's remaining budget.
    async fn request_phase(
        &self,
        effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        request: &PluginRequest<'_>,
        context: &mut RequestContext,
    ) -> Result<RequestEffects, DomainError>;

    /// The response-phase hook point: after the response is classified or the
    /// error is mapped.
    ///
    /// # Errors
    ///
    /// Returns the mapped error of the chain's rejection, with the same rows as
    /// the request-phase hook.
    async fn response_phase(
        &self,
        effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        response: &ResponseContext,
        headers: &mut HeaderMap,
        context: &mut RequestContext,
    ) -> Result<(), DomainError>;
}

/// The hook points with no plugin behaviour behind them.
///
/// Every call continues the pipeline: no credential is resolved, no guard
/// decides, nothing is mutated and no token bucket is accounted. The resolved
/// effective chain is the ordered binding list the hook receives, which is what
/// a chain implementation executes in reference order.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPlugins;

#[async_trait]
impl PluginChains for NoPlugins {
    async fn request_phase(
        &self,
        _effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        _request: &PluginRequest<'_>,
        _context: &mut RequestContext,
    ) -> Result<RequestEffects, DomainError> {
        // The chain is handed over in reference order and nothing executes: a
        // chain implementation owns the execution, the credential resolution
        // and the deadlines.
        let _ = bindings.len();
        Ok(RequestEffects::default())
    }

    async fn response_phase(
        &self,
        _effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        _response: &ResponseContext,
        _headers: &mut HeaderMap,
        _context: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let _ = bindings.len();
        Ok(())
    }
}

/// Map a hook rejection onto the error table and record it on the context.
///
/// The pipeline consumes the outcome in one place, so a hook cannot bypass the
/// error table and no upstream call happens behind a rejection.
pub fn consume_rejection(error: &DomainError, context: &mut RequestContext) -> DomainError {
    // The mapped rows the hook points raise: 400, 401, 403, 429, 500, 503 and
    // 504.
    context.fail(error);
    error.clone()
}
// @cpt-end:cpt-cf-oagw-dod-plugin-hook-points:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::proxy::call::HttpVersion;
    use std::sync::Arc;

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_type("user")
            .subject_tenant_id(Uuid::new_v4())
            .token_scopes(Vec::new())
            .build()
            .expect("context builds")
    }

    fn context() -> RequestContext {
        RequestContext::new(
            Uuid::new_v4().to_string(),
            "/oagw/v1/proxy/api.vendor.com".to_owned(),
            "GET".to_owned(),
        )
    }

    fn request<'a>(security: &'a SecurityContext, headers: &'a HeaderMap) -> PluginRequest<'a> {
        PluginRequest {
            security,
            tenant_id: security.subject_tenant_id(),
            method: "GET",
            query: None,
            headers,
            origin: None,
            peer_ip: None,
        }
    }

    fn bindings() -> Vec<PluginBinding> {
        vec![PluginBinding {
            position: 0,
            reference: "gts.cf.core.oagw.auth_plugin.v1~00000000-0000-0000-0000-000000000000"
                .to_owned(),
            plugin_uuid: None,
            config: None,
        }]
    }

    #[tokio::test]
    async fn the_default_chain_continues_the_pipeline() {
        let chains = NoPlugins;
        let security = security();
        let headers = HeaderMap::new();
        let effects = chains
            .request_phase(
                &EffectiveUpstream::default(),
                &bindings(),
                &request(&security, &headers),
                &mut context(),
            )
            .await
            .expect("no plugin behaviour runs");
        assert_eq!(effects.headers.len(), 0);
        assert_eq!(effects.query.len(), 0);
        assert!(effects.cors.is_none(), "the empty chain evaluates no CORS decision");
        let response = ResponseContext {
            status: 200,
            streamed: false,
            handed_off: false,
            http_version: HttpVersion::Http11,
            error_source: "upstream",
        };
        let mut headers = HeaderMap::new();
        chains
            .response_phase(
                &EffectiveUpstream::default(),
                &bindings(),
                &response,
                &mut headers,
                &mut context(),
            )
            .await
            .expect("no plugin behaviour runs");
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn a_chain_rejection_is_mapped_onto_the_error_table() {
        struct Rejecting;
        #[async_trait]
        impl PluginChains for Rejecting {
            async fn request_phase(
                &self,
                _effective: &EffectiveUpstream,
                _bindings: &[PluginBinding],
                _request: &PluginRequest<'_>,
                _context: &mut RequestContext,
            ) -> Result<RequestEffects, DomainError> {
                Err(DomainError::RateLimitExceeded {
                    detail: "the bucket is empty".to_owned(),
                    retry_after_seconds: Some(1),
                })
            }

            async fn response_phase(
                &self,
                _effective: &EffectiveUpstream,
                _bindings: &[PluginBinding],
                _response: &ResponseContext,
                _headers: &mut HeaderMap,
                _context: &mut RequestContext,
            ) -> Result<(), DomainError> {
                Ok(())
            }
        }
        let security = security();
        let headers = HeaderMap::new();
        let error = Rejecting
            .request_phase(
                &EffectiveUpstream::default(),
                &bindings(),
                &request(&security, &headers),
                &mut context(),
            )
            .await
            .expect_err("the chain rejects");
        assert_eq!(error.status(), 429, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
    }

    #[test]
    fn a_rejection_is_recorded_on_the_request_context() {
        let mut context = context();
        let error = DomainError::AuthenticationFailed {
            detail: "the token was refused".to_owned(),
        };
        let mapped = consume_rejection(&error, &mut context);
        assert_eq!(mapped.status(), 401);
        assert_eq!(context.phase, crate::infra::proxy::context::RequestPhase::Failed);
        assert_eq!(context.error_type, Some(error.gts_id()));
    }

    #[test]
    fn the_hook_points_carry_no_credential_material() {
        // The hook receives the merged configuration and the binding list, both
        // of which reference plugins and never resolve a secret.
        let effective = EffectiveUpstream::default();
        let rendered = format!("{effective:?}{:?}", bindings()).to_lowercase();
        assert!(!rendered.contains("bearer"));
        assert!(!rendered.contains("password"));
        // The request projection is rendered without the inbound header set.
        let security = security();
        let rendered = format!("{:?}", request(&security, &HeaderMap::new())).to_lowercase();
        assert!(!rendered.contains("authorization"));
        assert!(!rendered.contains("secret"));
        // A chain never holds a credential: the trait object is bound over the
        // hook seam, which is `Send + Sync` and carries no state of its own.
        let _chains: Arc<dyn PluginChains> = Arc::new(NoPlugins);
    }
}
