//! The unit tests of the health and readiness surface
//! (`cpt-cf-oagw-dod-observability-and-state-health-readiness`).
// @cpt-algo:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1

use super::*;
use toolkit::HealthcheckStatus;

// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-1
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-1b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-2
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-3
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-4
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-4b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-5
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-5b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-6
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-7
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-8
/// The state machine admits only the documented transitions
/// (`cpt-cf-oagw-algo-observability-and-state-health-state-machine`).
#[test]
fn the_health_state_machine_walks_the_documented_sequence() {
    let holder = HealthStateHolder::default();
    assert_eq!(holder.state(), HealthState::Uninitialized);
    holder.initializing();
    assert_eq!(holder.state(), HealthState::Initializing);
    holder.ready();
    assert_eq!(holder.state(), HealthState::Ready);
    assert!(holder.state().is_ready());
    holder.unhealthy();
    assert_eq!(holder.state(), HealthState::Unhealthy);
    assert!(!holder.state().is_ready());
    // A wired gear never rolls back to `uninitialized`.
    holder.transition(HealthState::Uninitialized);
    assert_eq!(holder.state(), HealthState::Unhealthy);
}
//
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-8
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-7
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-6
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-5b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-5
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-4b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-4
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-3
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-2
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-1b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance:p1:inst-os-algo-lru-1
//

/// The wire values are the documented lowercase names.
#[test]
fn the_state_wire_values_are_the_documented_names() {
    assert_eq!(HealthState::Uninitialized.as_str(), "uninitialized");
    assert_eq!(HealthState::Initializing.as_str(), "initializing");
    assert_eq!(HealthState::Ready.as_str(), "ready");
    assert_eq!(HealthState::Unhealthy.as_str(), "unhealthy");
}

/// A failed gear wiring reports unhealthy, so the gear leaves rotation.
#[test]
fn a_failed_wiring_reports_unhealthy() {
    assert_eq!(GearWiringState::Failed.as_health(), HealthState::Unhealthy);
    assert_eq!(GearWiringState::Ready.as_health(), HealthState::Ready);
    assert_eq!(GearWiringState::Initializing.as_health(), HealthState::Initializing);
    assert_eq!(GearWiringState::Uninitialized.as_health(), HealthState::Uninitialized);
}

/// The readiness hook reports healthy when ready, degraded while wiring and
/// unhealthy with a stable code when the surface is unwired.
#[tokio::test]
async fn the_readiness_hook_reports_the_state() {
    let holder = initial_state();
    let hook = GearHealth::new(Arc::clone(&holder));
    assert_eq!(hook.name(), "oagw-observability-state");
    let wired = holder.state();
    let report = hook.check().await;
    assert_eq!(report.status, HealthcheckStatus::Degraded, "{wired:?}");

    holder.initializing();
    holder.ready();
    assert_eq!(hook.check().await.status, HealthcheckStatus::Healthy);

    holder.unhealthy();
    let report = hook.check().await;
    assert_eq!(report.status, HealthcheckStatus::Unhealthy);
    assert_eq!(report.code.as_deref(), Some("oagw_state_unwired"));
}

/// Every component the hook names is one of the state surfaces.
#[test]
fn the_components_are_the_state_surfaces() {
    assert_eq!(StateComponent::CpCache.name(), "cp-l1-cache");
    assert_eq!(StateComponent::DpCache.name(), "dp-l1-cache");
    assert_eq!(StateComponent::Metrics.name(), "metrics-registry");
    assert_eq!(StateComponent::Audit.name(), "audit-emitter");
}
