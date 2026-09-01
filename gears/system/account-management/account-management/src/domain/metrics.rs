//! AM observability metric catalog.
//!
//! Declares the AM metric families from PRD §5.9 / FEATURE §5 "Metric
//! Catalog". Metric constants and [`MetricKind`] live in the runtime
//! crate so it is self-contained and peer SDKs do not expose metric
//! constants (see `resource-group-sdk`, `tenant-resolver-sdk`).
//!
//! ## Emission pipeline
//!
//! Existing call sites emit through the stringly-typed helpers
//! [`emit_metric`], [`emit_gauge_value`], and [`emit_histogram_value`].
//! When the infra adapter has installed a [`MetricsFacadeBridge`] via
//! [`install_facade_bridge`] (done in [`crate::gear`] init), emissions
//! are forwarded to OpenTelemetry instruments. Before installation —
//! and in tests that do not wire the adapter — the helpers are silent
//! no-ops, preserving the pre-init posture.
//!
//! The bridge is a **transitional surface**: typed port traits in
//! [`crate::domain::ports::metrics`] are the long-term API. Per-subdomain
//! call-site migration onto the typed ports proceeds family-by-family;
//! the bridge and the [`emit_*`] helpers are removed in the final
//! cleanup PR once every call site has moved.
//!
//! ## Metric naming
//!
//! The `AM_*` constants are the **full, literal Prometheus names** —
//! the exact series names that appear in Prometheus / `VictoriaMetrics`.
//! They bake in the suffix that the OTel→Prometheus translation would
//! otherwise add: counters carry `_total`, and gauges / histograms that
//! measure a quantity carry the unit word (`_seconds`, `_milliseconds`);
//! gauges that are bare counts carry no suffix. No `.with_unit()` hint
//! is set on any instrument. Because the name is already literal and
//! unit-free at the instrument level, the rendered Prometheus name is
//! identical whether the collector has `add_metric_suffixes` on or off
//! (the exporter dedups an existing `_total` and adds no unit suffix
//! when none is configured). The const VALUES equal the rendered name
//! under the default `"am"` prefix, so the facade-bridge `match family`
//! dispatch routes correctly.

use std::sync::Arc;
use std::sync::LazyLock;

use arc_swap::ArcSwap;
use toolkit_macros::domain_model;

// @cpt-begin:cpt-cf-account-management-dod-errors-observability-metric-catalog:p1:inst-dod-metric-catalog-constants
/// Dependency-call health: `IdP` / Resource Group / GTS / `AuthZ` outbound calls.
pub const AM_DEPENDENCY_HEALTH: &str = "am_dependency_health_total";

/// Tenant-metadata resolution operations and inheritance policy outcomes.
pub const AM_METADATA_RESOLUTION: &str = "am_metadata_resolution_total";

/// Root-tenant bootstrap lifecycle (phase transitions, IdP-wait timeouts).
pub const AM_BOOTSTRAP_LIFECYCLE: &str = "am_bootstrap_lifecycle_total";

/// Provisioning reaper / hard-delete / deprovision background job telemetry.
pub const AM_TENANT_RETENTION: &str = "am_tenant_retention_total";

/// Invalid retention-window configuration encountered while evaluating due-ness.
pub const AM_RETENTION_INVALID_WINDOW: &str = "am_retention_invalid_window_total";

/// Mode-conversion request transitions and outcomes.
pub const AM_CONVERSION_LIFECYCLE: &str = "am_conversion_lifecycle_total";

/// Hierarchy-depth threshold exceedance (warning-band + hard-limit rejects).
pub const AM_HIERARCHY_DEPTH_EXCEEDANCE: &str = "am_hierarchy_depth_exceedance_total";

/// Cross-tenant denial counter (security-alert candidate family).
pub const AM_CROSS_TENANT_DENIAL: &str = "am_cross_tenant_denial_total";

/// Hierarchy-integrity violation telemetry (one per integrity category).
pub const AM_HIERARCHY_INTEGRITY_VIOLATIONS: &str = "am_hierarchy_integrity_violations";

/// Periodic integrity-check job tick outcome (`outcome` ∈ `completed` |
/// `skipped_in_progress` | `failed`). Distinguishes a clean tick from a
/// never-ran job, which [`AM_HIERARCHY_INTEGRITY_VIOLATIONS`] alone cannot.
pub const AM_HIERARCHY_INTEGRITY_RUNS: &str = "am_hierarchy_integrity_runs_total";

/// Periodic auto-repair tick outcome — separate family from
/// [`AM_HIERARCHY_INTEGRITY_RUNS`] so its fixed-label set is not widened.
pub const AM_HIERARCHY_INTEGRITY_REPAIR_RUNS: &str = "am_hierarchy_integrity_repair_runs_total";

/// Periodic integrity-check tick wall-clock duration in milliseconds.
/// The `phase` label disaggregates the check phase (`phase = "check"`)
/// from the chained auto-repair phase (`phase = "repair"`) so
/// dashboards can tell a slow check from a slow check + repair.
/// Drives capacity-planning alerts ("p95 > 60s"), distinct from
/// [`AM_HIERARCHY_INTEGRITY_RUNS`] which is a tick-outcome counter.
pub const AM_HIERARCHY_INTEGRITY_DURATION: &str = "am_hierarchy_integrity_duration_milliseconds";

/// Unix-epoch seconds of the last successful integrity-check tick.
/// Used for a freshness watchdog (alert when `last_success` is older
/// than twice the configured interval) that the violation gauge
/// cannot satisfy on its own — a stuck job and a perfectly-clean tree
/// look identical at the violation-gauge level until this gauge stops
/// advancing.
pub const AM_HIERARCHY_INTEGRITY_LAST_SUCCESS: &str = "am_hierarchy_integrity_last_success_seconds";

/// Unix-epoch seconds of the last failed integrity-check tick — paired
/// with [`AM_HIERARCHY_INTEGRITY_LAST_SUCCESS`] so operators can tell
/// "sustained failure" from "never ran" (the success gauge keeps the last
/// good timestamp indefinitely).
pub const AM_HIERARCHY_INTEGRITY_LAST_FAILURE: &str = "am_hierarchy_integrity_last_failure_seconds";

/// Lock-lifecycle event counter for `integrity_check_runs`. Emitted
/// from [`crate::infra::storage::integrity::lock::release`] when the
/// release DELETE affects zero rows — the row this worker inserted
/// was reclaimed by a contender's stale-lock sweep, which means the
/// check or repair exceeded
/// [`crate::infra::storage::integrity::lock::MAX_LOCK_AGE`] AND a
/// peer raced in. Distinct from
/// [`AM_HIERARCHY_INTEGRITY_RUNS`] (which documents a fixed
/// scheduler-tick outcome set) so dashboards keyed on
/// `RUNS{outcome=*}` stay stable; this counter exists for
/// lock-health alerting.
pub const AM_INTEGRITY_LOCK_EVENTS: &str = "am_integrity_lock_events_total";

/// Hierarchy-integrity repair telemetry. Emits one gauge sample per
/// run with `category` ∈ all 10
/// [`IntegrityCategory`](crate::domain::tenant::integrity::IntegrityCategory)
/// values and `bucket` ∈ {`repaired`, `deferred`} so dashboards see a
/// stable shape across runs (zero-valued samples for categories that
/// did not appear). The five derivable categories carry counts only
/// in `bucket = repaired`; the five operator-triage categories carry
/// counts only in `bucket = deferred`.
pub const AM_HIERARCHY_INTEGRITY_REPAIRED: &str = "am_hierarchy_integrity_repaired";

/// SERIALIZABLE-isolation retry telemetry for the AM repo's
/// `with_serializable_retry` helper.
pub const AM_SERIALIZABLE_RETRY: &str = "am_serializable_retry_total";
// @cpt-end:cpt-cf-account-management-dod-errors-observability-metric-catalog:p1:inst-dod-metric-catalog-constants

/// Live tenant inventory gauge: current tenant row count, broken down
/// by `status` (provisioning | active | suspended | deleted) and
/// `self_managed` (true | false). A bare-count gauge, so it carries no
/// unit suffix. Refreshed each reaper tick.
pub const AM_TENANTS: &str = "am_tenants";

/// Live `tenant_closure` table size gauge: total ancestor-descendant
/// edge count. A bare-count gauge (no unit suffix), refreshed each
/// reaper tick alongside [`AM_TENANTS`]. Grows ~O(tenants × depth);
/// a divergence from that expectation flags closure bloat / stale edges.
pub const AM_TENANT_CLOSURE_ROWS: &str = "am_tenant_closure_rows";

/// Kinds of metric samples the emitter supports.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    /// Stable string tag used in emitted samples.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

// ════════════════════════════════════════════════════════════════════
//  Transitional facade-bridge
// ════════════════════════════════════════════════════════════════════
//
// The bridge plugs the stringly-typed [`emit_metric`] /
// [`emit_gauge_value`] / [`emit_histogram_value`] helpers into the
// OpenTelemetry-backed adapter without requiring every call site to
// migrate at once. The adapter installs an implementation during
// gear init; calls before installation are silent no-ops.

/// Forwarder used by the [`emit_*`] helpers to reach a real metrics
/// adapter without the domain layer depending on infra. The infra
/// adapter implements this trait; [`install_facade_bridge`] installs
/// the implementation at gear-init time.
pub trait MetricsFacadeBridge: Send + Sync + 'static {
    /// Forward an [`emit_metric`] call (counter-only today; the helper
    /// rejects gauge / histogram kinds at the call site).
    fn emit(&self, family: &'static str, kind: MetricKind, labels: &[(&'static str, &str)]);

    /// Forward an [`emit_gauge_value`] call.
    fn emit_gauge(&self, family: &'static str, value: i64, labels: &[(&'static str, &str)]);

    /// Forward an [`emit_histogram_value`] call.
    fn emit_histogram(&self, family: &'static str, value: f64, labels: &[(&'static str, &str)]);
}

/// `Arc`-wrapped `dyn MetricsFacadeBridge`. Aliased to keep the
/// `ArcSwap<Option<_>>` parametrisation readable — `arc_swap`'s
/// `RefCnt` impl requires the inner type to be `Sized`, which a bare
/// `dyn Trait` is not, so we wrap it in an `Arc` *first* (Sized) and
/// then wrap that `Option` in `ArcSwap`.
type BridgeArc = Arc<dyn MetricsFacadeBridge>;

/// Process-wide bridge slot. `ArcSwap` gives lock-free reads on the
/// emit hot path and lets [`install_facade_bridge`] *replace* the
/// active bridge — needed when a test harness swaps the global meter
/// provider between AM gear inits (the new adapter's instruments
/// are bound to the new provider; an unconditionally first-wins
/// `OnceLock` would freeze emissions on the stale instruments).
static FACADE_BRIDGE: LazyLock<ArcSwap<Option<BridgeArc>>> =
    LazyLock::new(|| ArcSwap::from(Arc::new(None)));

/// Install (or replace) the process-wide facade bridge. Called once
/// during AM gear init; idempotent across re-inits — the most
/// recent installation wins. The bridge stays installed for the
/// lifetime of the process unless overwritten, matching the
/// `opentelemetry::global::set_meter_provider` posture which itself
/// supports overwrite.
///
/// Returns `true` if this call installed the *first* bridge, `false`
/// if a prior bridge was replaced. The boolean is informational —
/// callers can log on the rare "already installed" branch (parallel
/// gear init in test harnesses, meter-provider hot-swap) but should
/// not treat it as an error.
pub fn install_facade_bridge(bridge: BridgeArc) -> bool {
    let prev = FACADE_BRIDGE.swap(Arc::new(Some(bridge)));
    prev.is_none()
}

/// Emit a metric sample (fire-and-forget).
///
/// Forwards to the installed [`MetricsFacadeBridge`] when one is
/// present; otherwise a silent no-op. Counter-only — gauge and
/// histogram families use [`emit_gauge_value`] / [`emit_histogram_value`].
#[inline]
pub fn emit_metric(family: &'static str, kind: MetricKind, labels: &[(&'static str, &str)]) {
    if let Some(bridge) = FACADE_BRIDGE.load().as_ref() {
        bridge.emit(family, kind, labels);
    }
}

/// Emit a value-carrying gauge sample (fire-and-forget).
#[inline]
pub fn emit_gauge_value(family: &'static str, value: i64, labels: &[(&'static str, &str)]) {
    if let Some(bridge) = FACADE_BRIDGE.load().as_ref() {
        bridge.emit_gauge(family, value, labels);
    }
}

/// Emit a value-carrying histogram sample (fire-and-forget).
#[inline]
pub fn emit_histogram_value(family: &'static str, value: f64, labels: &[(&'static str, &str)]) {
    if let Some(bridge) = FACADE_BRIDGE.load().as_ref() {
        bridge.emit_histogram(family, value, labels);
    }
}
