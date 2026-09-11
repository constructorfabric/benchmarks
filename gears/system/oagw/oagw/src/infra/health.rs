//! The health and readiness surface of entry 2.9
//! (`cpt-cf-oagw-flow-observability-and-state-health-readiness`,
//! `cpt-cf-oagw-algo-observability-and-state-health-state-machine`).
//!
//! The gear owns one health state machine and exposes it through the ToolKit
//! `RestApiCapability` healthcheck hook, so the aggregate `/readyz` report the
//! framework serves carries this gear's component report.
//!
//! # The state machine
//!
//! ```text
//! uninitialized -> initializing -> ready -> unhealthy
//!                                     ^---------+
//! ```
//!
//! * `uninitialized`: the gear has not been constructed yet;
//! * `initializing`: the caches and the metric registry are being wired
//!   (`inst-os-deploy-1`);
//! * `ready`: every state component is live — the CP L1 cache, the DP L1
//!   cache, the metrics registry and the audit emitter;
//! * `unhealthy`: a component is missing, and the gear reports itself not
//!   ready until it is wired again (`inst-os-health-4`).
//!
//! The wiring state machine of the gear — `uninitialized`, `initializing`,
//! `ready`, `failed` — is the same sequence with `failed` where the wiring
//! could not be completed; a failed wiring reports unhealthy, because a gear
//! that cannot serve its proxy pipeline must be taken out of rotation.
// @cpt-state:cpt-cf-oagw-state-observability-and-state-health-surface:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-rate-limit-ownership:p1

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use toolkit::{Healthcheck, HealthcheckResult};

// @cpt-begin:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-1
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-2
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-3
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-4
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-5
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-6
/// The health states of the state entry
/// (`cpt-cf-oagw-algo-observability-and-state-health-state-machine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// No component is wired yet.
    Uninitialized,
    /// The wiring is in progress.
    Initializing,
    /// Every state component is wired.
    Ready,
    /// A component is missing or failed.
    Unhealthy,
}
//
// @cpt-end:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-6
// @cpt-end:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-5
// @cpt-end:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-4
// @cpt-end:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-3
// @cpt-end:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-2
// @cpt-end:cpt-cf-oagw-state-observability-and-state-health-surface:p1:inst-os-st-health-1
//

impl HealthState {
    /// The wire value the health surface reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::Initializing => "initializing",
            Self::Ready => "ready",
            Self::Unhealthy => "unhealthy",
        }
    }

    /// Whether the state admits traffic.
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Whether the machine may move from `self` to `next`
    /// (`cpt-cf-oagw-algo-observability-and-state-health-state-machine`).
    #[must_use]
    const fn admits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Uninitialized, Self::Initializing)
                | (Self::Initializing, Self::Ready)
                | (Self::Ready, Self::Unhealthy)
                | (Self::Unhealthy, Self::Ready)
        )
    }

    fn ordinal(self) -> u8 {
        match self {
            Self::Uninitialized => 0,
            Self::Initializing => 1,
            Self::Ready => 2,
            Self::Unhealthy => 3,
        }
    }

    fn from_ordinal(value: u8) -> Self {
        match value {
            1 => Self::Initializing,
            2 => Self::Ready,
            3 => Self::Unhealthy,
            _ => Self::Uninitialized,
        }
    }
}

/// The gear wiring state, which differs from the health state only in its
/// failure value (`cpt-cf-oagw-algo-observability-and-state-health-state-machine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GearWiringState {
    /// Not constructed.
    Uninitialized,
    /// Constructing the caches, the metrics registry and the audit emitter.
    Initializing,
    /// Wired and serving.
    Ready,
    /// A wiring step failed.
    Failed,
}

impl GearWiringState {
    /// The health state the wiring state reports.
    #[must_use]
    pub const fn as_health(self) -> HealthState {
        match self {
            Self::Uninitialized => HealthState::Uninitialized,
            Self::Initializing => HealthState::Initializing,
            Self::Ready => HealthState::Ready,
            Self::Failed => HealthState::Unhealthy,
        }
    }
}

/// The four components the health state machine tracks
/// (`cpt-cf-oagw-dod-observability-and-state-cp-cache`,
/// `cpt-cf-oagw-dod-observability-and-state-dp-cache`,
/// `cpt-cf-oagw-dod-observability-and-state-metrics`,
/// `cpt-cf-oagw-dod-observability-and-state-audit-log`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateComponent {
    /// The Control Plane L1 cache.
    CpCache,
    /// The Data Plane L1 cache.
    DpCache,
    /// The metrics registry.
    Metrics,
    /// The audit emitter.
    Audit,
}

impl StateComponent {
    /// The check name the aggregate report carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CpCache => "cp-l1-cache",
            Self::DpCache => "dp-l1-cache",
            Self::Metrics => "metrics-registry",
            Self::Audit => "audit-emitter",
        }
    }
}

/// The health state of the entry's own state surface, observed by the
/// readiness hook.
pub struct HealthStateHolder {
    state: AtomicU8,
}

impl Default for HealthStateHolder {
    fn default() -> Self {
        Self { state: AtomicU8::new(HealthState::Uninitialized.ordinal()) }
    }
}

impl std::fmt::Debug for HealthStateHolder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HealthStateHolder(")?;
        formatter.write_str(self.state().as_str())?;
        formatter.write_str(")")
    }
}

impl HealthStateHolder {
    /// The state the holder currently reports.
    #[must_use]
    pub fn state(&self) -> HealthState {
        HealthState::from_ordinal(self.state.load(Ordering::SeqCst))
    }

    /// Move the state machine forward
    /// (`cpt-cf-oagw-algo-observability-and-state-health-state-machine`): only
    /// the documented transitions are accepted — `uninitialized ->
    /// initializing -> ready -> unhealthy -> ready` — so a state cannot be
    /// rolled back to `uninitialized` once the gear serves.
    pub fn transition(&self, next: HealthState) {
        if !self.state().admits(next) {
            return;
        }
        self.state.store(next.ordinal(), Ordering::SeqCst);
    }

    /// Enter `initializing`.
    pub fn initializing(&self) {
        self.transition(HealthState::Initializing);
    }

    /// Enter `ready`.
    pub fn ready(&self) {
        self.transition(HealthState::Ready);
    }

    /// Enter `unhealthy`.
    pub fn unhealthy(&self) {
        self.transition(HealthState::Unhealthy);
    }
}

// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-2
/// The readiness hook the gear registers through the ToolKit
/// `RestApiCapability::healthcheck` contract: the aggregate `/readyz` and
/// `/health` reports carry one component per state surface
/// (`cpt-cf-oagw-flow-observability-and-state-health-readiness`).
#[derive(Debug)]
pub struct GearHealth {
    state: Arc<HealthStateHolder>,
}

impl GearHealth {
    /// The hook over one health state.
    #[must_use]
    pub fn new(state: Arc<HealthStateHolder>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl Healthcheck for GearHealth {
    fn name(&self) -> &'static str {
        "oagw-observability-state"
    }

    async fn check(&self) -> HealthcheckResult {
        match self.state.state() {
            HealthState::Ready => HealthcheckResult::healthy(),
            HealthState::Uninitialized | HealthState::Initializing => {
                HealthcheckResult::degraded("observability and state surface is still wiring")
            }
            HealthState::Unhealthy => HealthcheckResult::unhealthy("observability and state surface is not wired")
                .with_code("oagw_state_unwired"),
        }
    }
}
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-2

/// The health state a fresh gear reports before its readiness hook is asked.
#[must_use]
pub fn initial_state() -> Arc<HealthStateHolder> {
    Arc::new(HealthStateHolder::default())
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod health_tests;
