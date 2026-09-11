//! The enforced per-phase bound and its phase-error classification
//! (`cpt-cf-oagw-dod-plugin-system-phase-bound`).
//!
//! The bound is derived from the configured `proxy_timeout_secs` — no new
//! configuration key is introduced for it — and a phase that exceeds it is a
//! *phase error* that renders as `504`, never a reject decision.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    AuthContext, Principal, AuthPlugin, GuardDecision, GuardPlugin, PluginError, PluginKind,
    RequestContext, ResponseContext, TransformPlugin,
};
use crate::infra::plugin::executor::{Outcome, Phase, PluginRuntime, ResolvedAuth, ResolvedChain, ResolvedEntry};
use crate::infra::plugin::resolution::PluginRegistries;
use crate::test_support::FakeCredStore;

fn runtime(secs: u64) -> PluginRuntime {
    let (_upstreams, _routes, plugins) = crate::infra::storage::Storage::new().repositories();
    PluginRuntime::new(
        Arc::new(PluginRegistries::with_builtins(
            Arc::new(FakeCredStore),
            crate::config::TokenCacheConfig::default(),
            plugins,
        )),
        secs,
    )
}

/// A stub auth plugin that always exceeds whatever bound it is given.
struct Slow;

#[async_trait::async_trait]
impl AuthPlugin for Slow {
    fn id(&self) -> &str {
        "slow"
    }
    fn plugin_type(&self) -> &str {
        "gts.cf.core.oagw.auth_plugin.v1"
    }
    async fn authenticate(&self, _ctx: &mut AuthContext) -> Result<(), PluginError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl GuardPlugin for Slow {
    fn id(&self) -> &str {
        "slow"
    }
    fn plugin_type(&self) -> &str {
        "gts.cf.core.oagw.guard_plugin.v1"
    }
    async fn guard_request(&self, _ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(GuardDecision::allow())
    }
    async fn guard_response(&self, _ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(GuardDecision::allow())
    }
}

#[async_trait::async_trait]
impl TransformPlugin for Slow {
    fn id(&self) -> &str {
        "slow"
    }
    fn plugin_type(&self) -> &str {
        "gts.cf.core.oagw.transform_plugin.v1"
    }
    async fn transform_request(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }
    async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), PluginError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }
    async fn transform_error(&self, _ctx: &mut crate::domain::plugin::ErrorContext) -> Result<(), PluginError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }
}

fn slow_chain() -> ResolvedChain {
    ResolvedChain {
        auth: Some(ResolvedAuth {
            reference: "slow".to_owned(),
            plugin: Arc::new(Slow),
            config: None,
            credential_refs: Vec::new(),
        }),
        entries: vec![
            ResolvedEntry::Guard { reference: "slow".to_owned(), plugin: Arc::new(Slow), config: None },
            ResolvedEntry::Transform { reference: "slow".to_owned(), plugin: Arc::new(Slow), config: None },
        ],
        phases: Vec::new(),
    }
}

/// The bound is the configured `proxy_timeout_secs`, and nothing else: no new
/// configuration key is introduced for the per-plugin bound.
#[test]
fn the_bound_is_derived_from_proxy_timeout_secs() {
    assert_eq!(runtime(2).budget(), Duration::from_secs(2));
    assert_eq!(runtime(5).budget(), Duration::from_secs(5));
    assert_eq!(runtime(0).budget(), Duration::ZERO);
    let config = crate::OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, 2);
    assert_eq!(runtime(config.proxy_timeout_secs).budget(), Duration::from_secs(2));
}

/// `inst-ps-exec-14`/`-15`: a phase that exceeds its bound is a phase error,
/// not a reject decision, and it renders as the request-timeout class (`504`).
#[tokio::test]
async fn a_phase_that_exceeds_its_bound_is_a_phase_error() {
    let rt = runtime(0);
    let mut chain = slow_chain();
    let error = rt
        .run_request(&mut chain, "GET", "/p", &[], &mut Vec::new(), Bytes::new(), Principal::default(), "trace")
        .await
        .expect_err("the auth phase exceeds its bound");
    assert!(matches!(error, DomainError::RequestTimeout { .. }), "{error}");
    // The mapped status is the gateway-timeout status, not a client
    // rejection, and the error type is the request-timeout type.
    let rendered = crate::api::rest::error::ApiError::from(error);
    assert_eq!(rendered.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(rendered.gts_type(), crate::gts::ERR_REQUEST_TIMEOUT);
    // The trace records the failure of the chain, not a refusal of the caller.
    assert_eq!(chain.phases.len(), 1);
    assert_eq!(chain.phases[0].phase, Phase::Auth);
    assert_eq!(chain.phases[0].outcome, Outcome::Error);
}

/// Every phase is bounded individually: a guard phase that exceeds its bound
/// is stopped too, and the chain never reaches the upstream call.
#[tokio::test]
async fn a_guard_phase_is_bounded_as_well() {
    let rt = runtime(0);
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![ResolvedEntry::Guard {
            reference: "slow".to_owned(),
            plugin: Arc::new(Slow),
            config: None,
        }],
        phases: Vec::new(),
    };
    let error = rt
        .run_request(&mut chain, "GET", "/p", &[], &mut Vec::new(), Bytes::new(), Principal::default(), "trace")
        .await
        .expect_err("the guard phase exceeds its bound");
    assert!(matches!(error, DomainError::RequestTimeout { .. }), "{error}");
    assert_eq!(chain.phases[0].phase, Phase::GuardRequest);
    assert_eq!(chain.phases[0].outcome, Outcome::Error);
}

/// A transform phase is bounded in the response half as well, and the failure
/// discards the upstream response rather than surfacing the plugin error.
#[tokio::test]
async fn a_response_phase_is_bounded_as_well() {
    let rt = runtime(0);
    let mut chain = ResolvedChain {
        auth: None,
        entries: vec![ResolvedEntry::Transform {
            reference: "slow".to_owned(),
            plugin: Arc::new(Slow),
            config: None,
        }],
        phases: Vec::new(),
    };
    let error = rt
        .run_response(&mut chain, 200, &mut Vec::new(), None, "trace")
        .await
        .expect_err("the response phase exceeds its bound");
    assert!(matches!(error, DomainError::RequestTimeout { .. }), "{error}");
    assert_eq!(chain.phases[0].phase, Phase::TransformResponse);
}

/// A phase that completes inside its bound is an `allow`, which is the
/// ordinary posture the other modules assert.
#[tokio::test]
async fn a_phase_inside_its_bound_allows() {
    let rt = runtime(30);
    let mut chain = slow_chain();
    rt.run_request(&mut chain, "GET", "/p", &[], &mut Vec::new(), Bytes::new(), Principal::default(), "trace")
        .await
        .expect("every phase completes");
    assert_eq!(chain.phases.len(), 3);
    assert!(chain.phases.iter().all(|trace| trace.outcome == Outcome::Allow));
    assert_eq!(
        chain.phases.iter().map(|trace| trace.phase).collect::<Vec<_>>(),
        [Phase::Auth, Phase::GuardRequest, Phase::TransformRequest]
    );
}

/// The phase names the trace carries are the names the observability entry
/// emits.
#[test]
fn the_phase_names_are_the_observability_names() {
    assert_eq!(Phase::Auth.as_str(), "auth");
    assert_eq!(Phase::GuardRequest.as_str(), "guard_request");
    assert_eq!(Phase::TransformRequest.as_str(), "transform_request");
    assert_eq!(Phase::GuardResponse.as_str(), "guard_response");
    assert_eq!(Phase::TransformResponse.as_str(), "transform_response");
    assert_eq!(Phase::TransformError.as_str(), "transform_error");
    assert_eq!(Outcome::Allow.as_str(), "allow");
    assert_eq!(Outcome::Reject.as_str(), "reject");
    assert_eq!(Outcome::Error.as_str(), "error");
    assert_eq!(Outcome::ResolveFailure.as_str(), "resolve_failure");
    assert_eq!(std::mem::discriminant(&PluginKind::Auth), std::mem::discriminant(&PluginKind::Auth));
}
