//! The plugin traits and the per-request context value objects
//! (`cpt-cf-oagw-dod-plugin-traits`).
//!
//! The three traits carry the exact signatures of
//! `cpt-cf-oagw-adr-plugin-system`: [`AuthPlugin`] injects a credential,
//! [`GuardPlugin`] validates and can reject, [`TransformPlugin`] modifies
//! request, response and error data. The mutable [`RequestContext`] is the one
//! context the proxy pipeline prepares and hands to every phase, so a plugin
//! reaches its own binding `config` through
//! [`RequestContext::config`]; ADR 0008's prose name `AuthContext` maps onto
//! this type and no second context type exists.
//!
//! The surface deliberately exposes no hook that executes plugin source and no
//! dynamic-loading or runtime-installation path, so the sandbox invariant of
//! `cpt-cf-oagw-nfr-starlark-sandbox` holds by absence in this release.
// @cpt-begin:cpt-cf-oagw-dod-plugin-traits:p1:inst-full

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;
use toolkit_security::SecurityContext;

pub use crate::domain::error::ErrorContext;
use crate::domain::error::OagwError;

/// The `{type}_plugin` family prefix of the auth plugins, the first member of
/// the closed [`PLUGIN_TYPE_IDS`] set of the domain model.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// The `{type}_plugin` family prefix of the guard plugins.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// The `{type}_plugin` family prefix of the transform plugins.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// `error_code` of a `required_headers` guard rejection (ADR 0009).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";
/// HTTP status of a guard rejection in the request phase (ADR 0009).
pub const GUARD_REQUEST_PHASE_STATUS: u16 = 400;
/// HTTP status of a guard rejection in the response phase (ADR 0009).
pub const GUARD_RESPONSE_PHASE_STATUS: u16 = 502;

/// The mutable per-request context the proxy pipeline prepares and hands to
/// every plugin (`cpt-cf-oagw-flow-auth-phase`,
/// `cpt-cf-oagw-flow-guard-transform-phase`).
///
/// `config` is the binding `config` object of the plugin currently being
/// invoked: the caller that drives the phase sets it before each invocation,
/// so `authenticate(&mut RequestContext)` reaches its own binding's keys.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Headers of the request, as the proxy pipeline forwards them.
    pub headers: http::HeaderMap,
    /// Query parameters in order; may hold duplicates.
    pub query: Vec<(String, String)>,
    /// The authenticated subject the request belongs to.
    pub security_context: SecurityContext,
    /// The binding `config` of the plugin currently being invoked.
    pub config: BTreeMap<String, Value>,
}

impl RequestContext {
    /// Prepares an empty context for one authenticated subject: no header, no
    /// query parameter and no binding `config` yet.
    #[must_use]
    pub fn new(security_context: SecurityContext) -> Self {
        Self {
            headers: http::HeaderMap::new(),
            query: Vec::new(),
            security_context,
            config: BTreeMap::new(),
        }
    }
}

/// The upstream response context of the response phase, carrying the request's
/// established identifier so a transform can propagate it.
///
/// `config` mirrors [`RequestContext::config`] for the response phase: the
/// caller that drives the phase sets it to the binding `config` of the plugin
/// currently being invoked, which is how a [`GuardPlugin`] reaches its
/// `required_response_headers` key through the immutable
/// `guard_response(&ResponseContext)` signature.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    /// Headers of the upstream response.
    pub headers: http::HeaderMap,
    /// The `X-Request-ID` the request phase established, if any.
    pub request_id: Option<String>,
    /// The binding `config` of the plugin currently being invoked.
    pub config: BTreeMap<String, Value>,
}

/// The outcome of one guard invocation: a plugin either allows the request or
/// rejects it.
///
/// A rejection is a plugin decision and deliberately not an [`OagwError`]: it
/// carries the ADR 0009 phase status and the ADR `error_code` directly and is
/// rendered through the problem+json contract by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The phase continues.
    Allow,
    /// The phase stops with the carried rejection.
    Reject(GuardRejection),
}

/// One guard rejection: the phase status, the ADR `error_code` and the detail
/// naming the first missing header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRejection {
    /// HTTP status the rejection is rendered with: 400 in the request phase,
    /// 502 in the response phase.
    pub status: u16,
    /// The ADR 0009 machine-readable code of the rejection.
    pub error_code: &'static str,
    /// The first missing header name, and none of the remaining ones.
    pub detail: String,
}

/// Injects the credential of one proxied request.
///
/// Executed once per request, before the guards. `authenticate` receives the
/// mutable request context and returns success with the credential injected,
/// or the typed failure mapped per §1.5 of
/// `cpt-cf-oagw-feature-plugin-chain`.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The full GTS identifier the plugin is registered under.
    fn id(&self) -> &str;
    /// The `{type}_plugin` family the plugin belongs to.
    fn plugin_type(&self) -> &str;
    /// Injects the credential into the context, or fails with the typed error.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
}

/// Validates one proxied request or its upstream response, and can reject.
///
/// Executed after the auth phase and before the transforms, symmetrically in
/// the response phase after the caller's upstream call.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The full GTS identifier the plugin is registered under.
    fn id(&self) -> &str;
    /// The `{type}_plugin` family the plugin belongs to.
    fn plugin_type(&self) -> &str;
    /// Decides on the request phase of the context.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError>;
    /// Decides on the response phase of the context.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError>;
}

/// Modifies request, response and error data of one proxied request.
///
/// Executed on the request before the caller's upstream call and on the
/// response — or on the error context of a failed call — after it.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The full GTS identifier the plugin is registered under.
    fn id(&self) -> &str;
    /// The `{type}_plugin` family the plugin belongs to.
    fn plugin_type(&self) -> &str;
    /// Modifies the request before the caller's upstream call.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
    /// Modifies the upstream response before it is returned.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError>;
    /// Modifies the error context of a failed call or phase.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PLUGIN_TYPE_IDS;

    fn assert_send_sync<T: Send + Sync + ?Sized>() {}

    /// The test plugin that witnesses the ADR 0002 signatures: the impl blocks
    /// below carry the exact parameter and return types of the ADR listing.
    struct WitnessPlugin;

    #[async_trait]
    impl AuthPlugin for WitnessPlugin {
        fn id(&self) -> &str {
            "gts.cf.core.oagw.auth_plugin.v1~test.witness.v1"
        }
        fn plugin_type(&self) -> &str {
            AUTH_PLUGIN_TYPE
        }
        async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
            Ok(())
        }
    }

    #[async_trait]
    impl GuardPlugin for WitnessPlugin {
        fn id(&self) -> &str {
            "gts.cf.core.oagw.guard_plugin.v1~test.witness.v1"
        }
        fn plugin_type(&self) -> &str {
            GUARD_PLUGIN_TYPE
        }
        async fn guard_request(&self, _ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
            Ok(GuardDecision::Allow)
        }
        async fn guard_response(&self, _ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
            Ok(GuardDecision::Allow)
        }
    }

    #[async_trait]
    impl TransformPlugin for WitnessPlugin {
        fn id(&self) -> &str {
            "gts.cf.core.oagw.transform_plugin.v1~test.witness.v1"
        }
        fn plugin_type(&self) -> &str {
            TRANSFORM_PLUGIN_TYPE
        }
        async fn transform_request(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
            Ok(())
        }
        async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), OagwError> {
            Ok(())
        }
        async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
            Ok(())
        }
    }

    /// The ADR 0002 signature of `AuthPlugin::authenticate`, as a function
    /// witness the compiler checks against the trait: the mutable request
    /// context in, `Result<(), OagwError>` out.
    async fn authenticate_signature<T: AuthPlugin + ?Sized>(
        plugin: &T,
        ctx: &mut RequestContext,
    ) -> Result<(), OagwError> {
        AuthPlugin::authenticate(plugin, ctx).await
    }

    /// The ADR 0002 signature of both `GuardPlugin` guard methods: the context
    /// by shared reference, `Result<GuardDecision, OagwError>` out.
    async fn guard_signature<T: GuardPlugin + ?Sized>(
        plugin: &T,
        request: &RequestContext,
        response: &ResponseContext,
    ) -> (
        Result<GuardDecision, OagwError>,
        Result<GuardDecision, OagwError>,
    ) {
        (
            GuardPlugin::guard_request(plugin, request).await,
            GuardPlugin::guard_response(plugin, response).await,
        )
    }

    /// The ADR 0002 signature of the three `TransformPlugin` methods, each over
    /// its own mutable context.
    #[allow(clippy::type_complexity)]
    async fn transform_signature<T: TransformPlugin + ?Sized>(
        plugin: &T,
        request: &mut RequestContext,
        response: &mut ResponseContext,
        error: &mut ErrorContext,
    ) -> (
        Result<(), OagwError>,
        Result<(), OagwError>,
        Result<(), OagwError>,
    ) {
        (
            TransformPlugin::transform_request(plugin, request).await,
            TransformPlugin::transform_response(plugin, response).await,
            TransformPlugin::transform_error(plugin, error).await,
        )
    }

    #[tokio::test]
    async fn the_trait_signatures_match_adr_0002_verbatim() {
        assert_send_sync::<dyn AuthPlugin>();
        assert_send_sync::<dyn GuardPlugin>();
        assert_send_sync::<dyn TransformPlugin>();

        let security = SecurityContext::builder()
            .subject_id(uuid::Uuid::nil())
            .subject_tenant_id(uuid::Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        let mut request = RequestContext::new(security);
        let mut response = ResponseContext::default();
        let mut error = ErrorContext::new();

        let plugin = WitnessPlugin;
        assert!(authenticate_signature(&plugin, &mut request).await.is_ok());
        let (request_decision, response_decision) =
            guard_signature(&plugin, &request, &response).await;
        assert_eq!(request_decision, Ok(GuardDecision::Allow));
        assert_eq!(response_decision, Ok(GuardDecision::Allow));
        let (request_phase, response_phase, error_phase) =
            transform_signature(&plugin, &mut request, &mut response, &mut error).await;
        assert!(request_phase.is_ok());
        assert!(response_phase.is_ok());
        assert!(error_phase.is_ok());

        // The same witnesses hold through the trait objects the registries
        // store, which is the form the proxy pipeline invokes.
        let auth: &dyn AuthPlugin = &plugin;
        let guard: &dyn GuardPlugin = &plugin;
        let transform: &dyn TransformPlugin = &plugin;
        assert!(authenticate_signature(auth, &mut request).await.is_ok());
        let (request_decision, response_decision) =
            guard_signature(guard, &request, &response).await;
        assert_eq!(request_decision.unwrap(), GuardDecision::Allow);
        assert_eq!(response_decision.unwrap(), GuardDecision::Allow);
        let _ = transform_signature(transform, &mut request, &mut response, &mut error).await;
    }

    #[test]
    fn the_declared_plugin_types_are_the_domain_model_families() {
        assert_eq!(
            PLUGIN_TYPE_IDS,
            &[AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE][..],
            "the plugin_type values are the closed family set"
        );
    }

    #[test]
    fn the_guard_statuses_are_the_adr_0009_phase_statuses() {
        assert_eq!(GUARD_REQUEST_PHASE_STATUS, 400);
        assert_eq!(GUARD_RESPONSE_PHASE_STATUS, 502);
        assert_eq!(REQUIRED_HEADER_MISSING, "REQUIRED_HEADER_MISSING");
    }

    #[test]
    fn a_rejection_is_a_decision_and_not_an_error() {
        let decision = GuardDecision::Reject(GuardRejection {
            status: GUARD_REQUEST_PHASE_STATUS,
            error_code: REQUIRED_HEADER_MISSING,
            detail: "x-request-id".to_owned(),
        });
        match decision {
            GuardDecision::Reject(rejection) => {
                assert_eq!(rejection.status, 400);
                assert_eq!(rejection.error_code, "REQUIRED_HEADER_MISSING");
                assert_eq!(rejection.detail, "x-request-id");
            }
            GuardDecision::Allow => panic!("the rejection was lost"),
        }
    }

    #[test]
    fn a_prepared_request_context_starts_empty() {
        let security = SecurityContext::builder()
            .subject_id(uuid::Uuid::nil())
            .subject_tenant_id(uuid::Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        let context = RequestContext::new(security);
        assert!(context.headers.is_empty());
        assert!(context.query.is_empty());
        assert!(context.config.is_empty());
        assert_eq!(context.security_context.subject_id(), uuid::Uuid::nil());
    }
}

// @cpt-end:cpt-cf-oagw-dod-plugin-traits:p1:inst-full
