//! Integration tests of the health and readiness surface of entry 2.9
//! (`cpt-cf-oagw-dod-observability-and-state-health-readiness`,
//! `cpt-cf-oagw-flow-observability-and-state-health-readiness`,
//! `cpt-cf-oagw-algo-observability-and-state-health-state-machine`).
//!
//! The tests drive the real `OagwGear`, so the reported readiness is the one
//! the `RestApiCapability::healthcheck` hook serves on the path the framework
//! provides — no gear-specific health route is registered.
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-health-readiness:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use toolkit::{Gear, HealthcheckStatus};
use toolkit::contracts::RestApiCapability as _;

use oagw::test_support::{permissive_surface, test_context, test_context_with_registry, failing_registry, unimplemented};

/// A healthy gear reports `ready` and serves a healthy check over the hook the
/// `RestApiCapability` contract publishes
/// (`inst-os-health-1`, `inst-os-health-3`).
#[tokio::test]
async fn an_initialized_gear_reports_ready() {
    let surface = permissive_surface(None).await;
    let health = surface.gear.health().expect("the health state is published");
    assert_eq!(health.state(), oagw::infra::health::HealthState::Ready);

    let hook = surface.gear.healthcheck(&surface.ctx).expect("the hook is published");
    assert_eq!(hook.name(), "oagw-observability-state", "the name the framework serves");
    let result = hook.check().await;
    assert_eq!(result.status, HealthcheckStatus::Healthy, "{result:?}");
    assert!(result.message.is_none(), "a healthy check carries no message: {result:?}");
}

/// A gear whose initialization failed is reportable and is **not** ready: the
/// state holder is published before the wiring runs, so a failure leaves the
/// state it stopped in and the hook serves `degraded`
/// (`cpt-cf-oagw-dod-observability-and-state-health-readiness`).
#[tokio::test]
async fn a_failed_initialization_is_not_ready() {
    for ctx in [
        test_context(Some(serde_json::json!({ "proxy_timeout_secs": 0 }))),
        test_context_with_registry(None, failing_registry(unimplemented())),
    ] {
        let gear = oagw::OagwGear::default();
        assert!(gear.init(&ctx).await.is_err(), "the initialization fails");
        let health = gear.health().expect("the state holder was published before the wiring ran");
        assert_ne!(
            health.state(),
            oagw::infra::health::HealthState::Ready,
            "a failed initialization never reports ready"
        );
        let hook = gear.healthcheck(&ctx).expect("the hook is published");
        let result = hook.check().await;
        assert_eq!(result.status, HealthcheckStatus::Degraded, "{result:?}");
    }
}

/// A gear that lacks one of its owned state components is reported alive but
/// not ready, with the unavailable state named, and returns to ready when the
/// component becomes available again (`inst-os-health-4`, `inst-os-health-5`).
#[tokio::test]
async fn an_unavailable_component_is_reported_alive_but_not_ready() {
    let surface = permissive_surface(None).await;
    let health = surface.gear.health().expect("the health state is published");
    let hook = surface.gear.healthcheck(&surface.ctx).expect("the hook is published");

    health.unhealthy();
    let result = hook.check().await;
    assert_eq!(result.status, HealthcheckStatus::Unhealthy, "{result:?}");
    assert_eq!(result.code.as_deref(), Some("oagw_state_unwired"), "{result:?}");

    // The machine returns to ready, and the check with it.
    health.ready();
    let result = hook.check().await;
    assert_eq!(result.status, HealthcheckStatus::Healthy, "{result:?}");
}

/// The four state components the gear owns are the four the state machine
/// tracks, and the machine accepts only the documented transitions
/// (`cpt-cf-oagw-algo-observability-and-state-health-state-machine`).
#[test]
fn the_state_machine_admits_only_the_documented_transitions() {
    use oagw::infra::health::{HealthState, StateComponent};

    assert_eq!(StateComponent::CpCache.name(), "cp-l1-cache");
    assert_eq!(StateComponent::DpCache.name(), "dp-l1-cache");
    assert_eq!(StateComponent::Metrics.name(), "metrics-registry");
    assert_eq!(StateComponent::Audit.name(), "audit-emitter");

    let health = oagw::infra::health::HealthStateHolder::default();
    assert_eq!(health.state(), HealthState::Uninitialized);
    // No transition skips a step: `uninitialized` cannot go straight to ready.
    health.ready();
    assert_eq!(health.state(), HealthState::Uninitialized, "the transition was refused");
    health.initializing();
    assert_eq!(health.state(), HealthState::Initializing);
    health.ready();
    assert_eq!(health.state(), HealthState::Ready);
    assert!(health.state().is_ready());
    // Ready cannot roll back to uninitialized.
    health.initializing();
    assert_eq!(health.state(), HealthState::Ready, "a serving gear is never rewound");
    health.unhealthy();
    assert_eq!(health.state(), HealthState::Unhealthy);
    health.ready();
    assert_eq!(health.state(), HealthState::Ready, "the recovery transition");
}
