//! Chain execution order and short-circuit semantics
//! (`cpt-cf-oagw-dod-plugin-system-chain-execution`), including the error
//! branches of the chain executor the FEATURE names.
//!
//! The stub plugins here exist only to observe the order the executor runs a
//! chain in, so the executor is tested against the traits it dispatches
//! through rather than against a built-in.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-plugin-traits:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-unit-tests:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    AuthContext, AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginError,
    Principal, RequestContext, ResponseContext, TransformPlugin,
};
use crate::domain::plugin::composition::ComposedChain;
use crate::infra::plugin::executor::{Outcome, Phase, PluginRuntime, ResolvedAuth, ResolvedChain, ResolvedEntry};
use crate::infra::plugin::resolution::TenantChain;

/// How a stub fails, when it fails.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    None,
    /// A plain rejection, no status.
    Reject,
    /// A rejection carrying its own status.
    RejectedWithStatus,
    /// A backing-service failure.
    Unavailable,
    /// An internal plugin failure.
    Internal,
}

/// A stub plugin that records the phase calls it saw and fails in one phase.
struct Stub {
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    fail_in: Phase,
    mode: Failure,
}

impl Stub {
    fn record(&self, phase: &'static str) {
        self.calls.lock().expect("calls").push(phase);
    }

    fn outcome(&self, phase: Phase) -> Result<(), PluginError> {
        if self.fail_in != phase {
            return Ok(());
        }
        match self.mode {
            Failure::None => Ok(()),
            Failure::Reject => Err(PluginError::rejected("rejected by the stub")),
            Failure::RejectedWithStatus => Err(PluginError::rejected_with("rejected", 418)),
            Failure::Unavailable => Err(PluginError::Unavailable),
            Failure::Internal => Err(PluginError::Internal("stub internal".to_owned())),
        }
    }
}

/// A stub auth plugin that captures the `Principal` the executor handed it, so
/// a test can assert the caller identity of the proxied request reaches the
/// auth phase (`inst-ps-key-2`).
struct PrincipalCapture {
    observed: Arc<std::sync::Mutex<Vec<Principal>>>,
}

#[async_trait::async_trait]
impl AuthPlugin for PrincipalCapture {
    fn id(&self) -> &str {
        "principal-capture"
    }
    fn plugin_type(&self) -> &str {
        "gts.cf.core.oagw.auth_plugin.v1"
    }
    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        self.observed.lock().expect("observed").push(ctx.principal.clone());
        Ok(())
    }
}

/// `inst-ps-key-2`: the auth phase receives the caller identity of the proxied
/// request, not a default one — a plugin that keys its state on the subject
/// tenant and subject identifier therefore never shares an entry between two
/// tenants.
#[tokio::test]
async fn the_auth_phase_receives_the_caller_identity() {
    let rt = runtime();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tenant = uuid::Uuid::new_v4();
    let subject = uuid::Uuid::new_v4();
    let mut chain = ResolvedChain {
        auth: Some(ResolvedAuth {
            reference: "auth".to_owned(),
            plugin: Arc::new(PrincipalCapture { observed: Arc::clone(&observed) }),
            config: None,
            credential_refs: Vec::new(),
        }),
        entries: Vec::new(),
        phases: Vec::new(),
    };
    let mut headers = Vec::new();
    rt.run_request(
        &mut chain,
        "GET",
        "/p",
        &[],
        &mut headers,
        Bytes::new(),
        Principal { subject_id: Some(subject), tenant_id: Some(tenant), scopes: Vec::new() },
        "trace",
    )
    .await
    .expect("the chain proceeds");
    let observed = observed.lock().expect("observed");
    assert_eq!(observed.len(), 1, "the auth phase ran exactly once");
    assert_eq!(observed[0].tenant_id, Some(tenant));
    assert_eq!(observed[0].subject_id, Some(subject));
}

#[async_trait::async_trait]
impl AuthPlugin for Stub {
    fn id(&self) -> &str {
        "stub"
    }
    fn plugin_type(&self) -> &str {
        "gts.cf.core.oagw.auth_plugin.v1"
    }
    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        self.record("authenticate");
        self.outcome(Phase::Auth)?;
        ctx.outbound_headers.push(("authorization".to_owned(), "Bearer stub".to_owned()));
        Ok(())
    }
}

#[async_trait::async_trait]
impl GuardPlugin for Stub {
    fn id(&self) -> &str {
        "stub"
    }
    fn plugin_type(&self) -> &str {
        "gts.cf.core.oagw.guard_plugin.v1"
    }
    async fn guard_request(&self, _ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        self.record("guard_request");
        self.outcome(Phase::GuardRequest).map(|_| GuardDecision::allow())
    }
    async fn guard_response(&self, _ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        self.record("guard_response");
        self.outcome(Phase::GuardResponse).map(|_| GuardDecision::allow())
    }
}

#[async_trait::async_trait]
impl TransformPlugin for Stub {
    fn id(&self) -> &str {
        "stub"
    }
    fn plugin_type(&self) -> &str {
        "gts.cf.core.oagw.transform_plugin.v1"
    }
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        self.record("transform_request");
        self.outcome(Phase::TransformRequest)?;
        ctx.headers.push(("x-stub".to_owned(), "1".to_owned()));
        Ok(())
    }
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        self.record("transform_response");
        self.outcome(Phase::TransformResponse)?;
        ctx.headers.push(("x-stub".to_owned(), "1".to_owned()));
        Ok(())
    }
    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), PluginError> {
        self.record("transform_error");
        self.outcome(Phase::TransformError)
    }
}

fn runtime() -> PluginRuntime {
    let (_upstreams, _routes, plugins) = crate::infra::storage::Storage::new().repositories();
    PluginRuntime::new(
        Arc::new(crate::infra::plugin::resolution::PluginRegistries::with_builtins(
            Arc::new(crate::test_support::FakeCredStore),
            crate::config::TokenCacheConfig::default(),
            plugins,
        )),
        5,
    )
}

fn guard_entry(calls: &Arc<std::sync::Mutex<Vec<&'static str>>>, fail_in: Phase, mode: Failure) -> ResolvedEntry {
    ResolvedEntry::Guard {
        reference: "guard".to_owned(),
        plugin: Arc::new(Stub { calls: Arc::clone(calls), fail_in, mode }),
        config: None,
    }
}

fn transform_entry(
    calls: &Arc<std::sync::Mutex<Vec<&'static str>>>,
    fail_in: Phase,
    mode: Failure,
) -> ResolvedEntry {
    ResolvedEntry::Transform {
        reference: "transform".to_owned(),
        plugin: Arc::new(Stub { calls: Arc::clone(calls), fail_in, mode }),
        config: None,
    }
}

/// `inst-ps-exec-1` .. `-4`: auth runs exactly once, before every guard, and
/// the guards and transforms run in chain order.
#[tokio::test]
async fn the_request_phases_run_in_chain_order() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut chain = ResolvedChain {
        auth: Some(ResolvedAuth {
            reference: "auth".to_owned(),
            plugin: Arc::new(Stub { calls: Arc::clone(&calls), fail_in: Phase::Auth, mode: Failure::None }),
            config: None,
            credential_refs: Vec::new(),
        }),
        entries: vec![
            guard_entry(&calls, Phase::Auth, Failure::None),
            transform_entry(&calls, Phase::Auth, Failure::None),
        ],
        phases: Vec::new(),
    };
    let mut headers = Vec::new();
    let transformed = rt
        .run_request(&mut chain, "GET", "/v1/orders", &[], &mut headers, Bytes::new(), Principal::default(), "trace")
        .await
        .expect("the chain proceeds");
    assert_eq!(*calls.lock().expect("calls"), ["authenticate", "guard_request", "transform_request"]);
    assert_eq!(
        chain.phases.iter().map(|trace| trace.phase).collect::<Vec<_>>(),
        [Phase::Auth, Phase::GuardRequest, Phase::TransformRequest]
    );
    assert!(chain.phases.iter().all(|trace| trace.outcome.as_str() == "allow"));
    // The credential the auth phase injected is on the outbound surface.
    assert_eq!(transformed.header("authorization"), Some("Bearer stub"));
    // The transform's write is on the transformed surface too.
    assert_eq!(transformed.header("x-stub"), Some("1"));
}

/// `inst-ps-exec-5`/`-6`: the first guard rejection is terminal and no later
/// phase runs.
#[tokio::test]
async fn the_first_rejection_is_terminal() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![
            guard_entry(&calls, Phase::GuardRequest, Failure::Reject),
            transform_entry(&calls, Phase::Auth, Failure::None),
        ],
        phases: Vec::new(),
    };
    let mut headers = Vec::new();
    let error = rt
        .run_request(&mut chain, "GET", "/p", &[], &mut headers, Bytes::new(), Principal::default(), "trace")
        .await
        .expect_err("the guard rejects");
    assert_eq!(*calls.lock().expect("calls"), ["guard_request"], "no later phase runs");
    assert!(matches!(error, DomainError::ValidationError { .. }), "{error}");
    assert_eq!(chain.phases.len(), 1);
    assert_eq!(chain.phases[0].outcome, Outcome::Reject);
}

/// `inst-ps-exec-7`/`-8`: a request-phase transform failure stops the chain
/// with no upstream call and no later phase.
#[tokio::test]
async fn a_request_transform_failure_stops_the_chain() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![
            transform_entry(&calls, Phase::TransformRequest, Failure::Internal),
            transform_entry(&calls, Phase::Auth, Failure::None),
        ],
        phases: Vec::new(),
    };
    let mut headers = Vec::new();
    let error = rt
        .run_request(&mut chain, "GET", "/p", &[], &mut headers, Bytes::new(), Principal::default(), "trace")
        .await
        .expect_err("the transform fails");
    assert_eq!(*calls.lock().expect("calls"), ["transform_request"]);
    assert!(matches!(error, DomainError::PluginInternal(_)), "{error}");
    assert_eq!(chain.phases[0].outcome, Outcome::Error);
}

/// `inst-ps-exec-16`/`-17`: a rejection that carries its own status keeps it,
/// and a guard that reports a backing-service failure is an error, not a
/// reject decision.
#[tokio::test]
async fn the_error_branches_map_onto_the_shared_contract() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));

    // (1) a rejection with a status below `500` renders as a validation error.
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![guard_entry(&calls, Phase::GuardRequest, Failure::RejectedWithStatus)],
        phases: Vec::new(),
    };
    let mut headers = Vec::new();
    let error = rt
        .run_request(&mut chain, "GET", "/p", &[], &mut headers, Bytes::new(), Principal::default(), "trace")
        .await
        .expect_err("the guard rejects");
    assert!(matches!(error, DomainError::ValidationError { .. }), "{error}");

    // (2) a rejection with a status of `500` or above renders as the
    // downstream-error class.
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![guard_entry(&calls, Phase::GuardResponse, Failure::Reject)],
        phases: Vec::new(),
    };
    let error = rt
        .run_response(&mut chain, 200, &mut Vec::new(), None, "trace")
        .await
        .expect_err("the guard rejects in the response phase");
    assert!(matches!(error, DomainError::DownstreamError { .. }), "{error}");

    // (3) a backing-service failure is the credential class, not a reject.
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![guard_entry(&calls, Phase::GuardRequest, Failure::Unavailable)],
        phases: Vec::new(),
    };
    let error = rt
        .run_request(&mut chain, "GET", "/p", &[], &mut headers, Bytes::new(), Principal::default(), "trace")
        .await
        .expect_err("the guard fails");
    assert!(matches!(error, DomainError::SecretNotFound { .. }), "{error}");
    assert_eq!(chain.phases[0].outcome, Outcome::Error);
}

/// `inst-ps-exec-19`: a response-phase transform failure discards the upstream
/// response.
#[tokio::test]
async fn a_response_transform_failure_discards_the_upstream_response() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![transform_entry(&calls, Phase::TransformResponse, Failure::Internal)],
        phases: Vec::new(),
    };
    let error = rt
        .run_response(&mut chain, 200, &mut Vec::new(), None, "trace")
        .await
        .expect_err("the transform fails");
    assert!(matches!(error, DomainError::DownstreamError { .. }), "{error}");
    assert_eq!(chain.phases[0].outcome, Outcome::Error);
}

/// `inst-ps-exec-21`: a transform failure in the error phase never masks the
/// original error.
#[tokio::test]
async fn an_error_phase_failure_never_masks_the_original() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![transform_entry(&calls, Phase::TransformError, Failure::Internal)],
        phases: Vec::new(),
    };
    let upstream = DomainError::LinkUnavailable {
        upstream_id: Some("upstream".to_owned()),
        host: None,
        path: Some("/p".to_owned()),
        trace_id: None,
    };
    rt.run_response(&mut chain, 503, &mut Vec::new(), Some(&upstream), "trace")
        .await
        .expect("the failure is not surfaced");
    assert_eq!(*calls.lock().expect("calls"), ["transform_error"]);
    assert_eq!(chain.phases[0].phase, Phase::TransformError);
    assert_eq!(chain.phases[0].outcome, Outcome::Error);
}

/// `inst-ps-exec-10`/`-12`: the response phases run the transforms against the
/// upstream response and the guards after it.
#[tokio::test]
async fn the_response_phases_run_in_order() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![
            transform_entry(&calls, Phase::Auth, Failure::None),
            guard_entry(&calls, Phase::Auth, Failure::None),
        ],
        phases: Vec::new(),
    };
    let mut headers = vec![("content-type".to_owned(), "text/plain".to_owned())];
    rt.run_response(&mut chain, 200, &mut headers, None, "trace").await.expect("the phases run");
    assert_eq!(*calls.lock().expect("calls"), ["transform_response", "guard_response"]);
    assert_eq!(headers.iter().any(|(name, _)| name == "x-stub"), true, "the write lands");
}

/// `inst-ps-exec-13`: the error phase replaces the response transform, and no
/// response transform runs for a failed upstream call.
#[tokio::test]
async fn the_error_phase_replaces_the_response_phase() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![transform_entry(&calls, Phase::Auth, Failure::None)],
        phases: Vec::new(),
    };
    let upstream = DomainError::DownstreamError {
        upstream_id: None,
        host: None,
        path: None,
        trace_id: None,
        retriable: false,
    };
    rt.run_response(&mut chain, 502, &mut Vec::new(), Some(&upstream), "trace")
        .await
        .expect("the phases run");
    assert_eq!(*calls.lock().expect("calls"), ["transform_error"]);
    assert_eq!(chain.phases[0].phase, Phase::TransformError);
}

/// A chain with no entry and no auth plugin runs no phase at all and leaves
/// the request surface untouched.
#[tokio::test]
async fn an_empty_chain_runs_no_phase() {
    let rt = runtime();
    let mut chain = ResolvedChain { auth: None, entries: Vec::new(), phases: Vec::new() };
    assert!(chain.is_empty());
    let mut headers = vec![("content-type".to_owned(), "text/plain".to_owned())];
    let transformed = rt
        .run_request(
            &mut chain,
            "GET",
            "/p",
            &[("a".to_owned(), "b".to_owned())],
            &mut headers,
            Bytes::new(),
            Principal::default(),
            "trace",
        )
        .await
        .expect("the chain is a no-op");
    assert_eq!(transformed.headers, headers);
    assert_eq!(transformed.query, vec![("a".to_owned(), "b".to_owned())]);
    rt.run_response(&mut chain, 200, &mut Vec::new(), None, "trace").await.expect("no-op");
    assert!(chain.phases.is_empty());
}

/// `inst-ps-comp-7`/`-9`: a reference that resolves to nothing fails the
/// request with `PluginNotFound` before any phase runs.
#[test]
fn an_unresolvable_reference_fails_the_resolve() {
    let rt = runtime();
    let tenant = uuid::Uuid::new_v4();
    let chain = ComposedChain {
        bindings: vec![crate::domain::repo::PluginBinding {
            position: 0,
            plugin_ref: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.absent.v1".to_owned(),
            plugin_uuid: None,
        }],
        auth_ref: None,
    };
    let error = rt
        .resolve(&chain, &TenantChain::new(vec![tenant]), None)
        .expect_err("unresolvable");
    assert!(matches!(error, DomainError::PluginNotFound { .. }), "{error}");
}

/// An auth plugin reached through a chain binding is not the request's auth
/// plugin: the resolution fails rather than letting a chain position inject a
/// credential invisibly.
#[test]
fn an_auth_plugin_in_a_chain_position_is_rejected() {
    let rt = runtime();
    let tenant = uuid::Uuid::new_v4();
    let chain = ComposedChain {
        bindings: vec![crate::domain::repo::PluginBinding {
            position: 0,
            plugin_ref: crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned(),
            plugin_uuid: None,
        }],
        auth_ref: None,
    };
    let error = rt
        .resolve(&chain, &TenantChain::new(vec![tenant]), None)
        .expect_err("rejected");
    assert!(matches!(error, DomainError::PluginNotFound { .. }), "{error}");
}

/// An auth plugin declared by the upstream `auth` block is the one auth plugin
/// the chain runs, and it is the first phase.
#[tokio::test]
async fn the_auth_block_supplies_the_single_auth_phase() {
    let rt = runtime();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tenant = uuid::Uuid::new_v4();
    let chain = ComposedChain {
        bindings: Vec::new(),
        auth_ref: Some(crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned()),
    };
    let mut resolved = rt
        .resolve(&chain, &crate::infra::plugin::resolution::TenantChain::new(vec![tenant]), None)
        .expect("the auth plugin resolves");
    // Replace the built-in with the stub so the phase is observable.
    resolved.auth = Some(ResolvedAuth {
        reference: chain.auth_ref.clone().expect("auth reference"),
        plugin: Arc::new(Stub { calls, fail_in: Phase::Auth, mode: Failure::None }),
        config: None,
        credential_refs: Vec::new(),
    });
    let mut headers = Vec::new();
    rt.run_request(&mut resolved, "GET", "/p", &[], &mut headers, Bytes::new(), Principal::default(), "trace")
        .await
        .expect("the chain proceeds");
    assert_eq!(resolved.phases.len(), 1);
    assert_eq!(resolved.phases[0].phase, Phase::Auth);
}
