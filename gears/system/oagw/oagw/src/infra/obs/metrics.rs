//! The metric registry of the OAGW gear (entry 2.7).
//!
//! The registry holds the twelve metric families DESIGN §4.2 fixes — their
//! names, kinds, label keys and the request-duration buckets — and turns one
//! closed request context, one in-flight change or one breaker transition into
//! an atomic update on exactly one collector
//! (`cpt-cf-oagw-algo-metric-emit`). Two surfaces are kept for every family:
//!
//! * the host surface: the instruments registered on the OpenTelemetry global
//!   meter provider the host installed, which the host's `GET /metrics`
//!   exposition collects (DECOMPOSITION assumption 9 — the gear owns no
//!   `/metrics` endpoint of its own);
//! * the in-process collector this registry holds, which is what a process
//!   without a configured metrics provider still reports and what the in-crate
//!   tests assert on.
//!
//! A label set is bounded by construction: the label keys are the family's
//! recorded keys, the values are normalized onto the recorded vocabulary, and a
//! value the registry cannot place is dropped and counted rather than
//! materialized, because an unbounded label set is the one failure mode here
//! that degrades the host process rather than a single request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::metrics::{AsyncInstrument, Counter, Gauge, Histogram, Meter};
use opentelemetry::KeyValue;
use parking_lot::Mutex;

use super::RequestObservation;
use crate::infra::proxy::breaker::{BreakerState, Transition};
use crate::infra::proxy::context::{RateLimitObservation, RequestContext, SelectionMethod};
use crate::infra::proxy::rate_limit;

/// `oagw_requests_total{host, http.request.method, http.route,
/// http.response.status_code}` — counter.
pub const REQUESTS_TOTAL: &str = "oagw_requests_total";
/// `oagw_request_duration_seconds{host, http.route, phase}` — histogram.
pub const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
/// `oagw_requests_in_flight{host}` — gauge.
pub const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
/// `oagw_errors_total{host, http.route, error_type}` — counter.
pub const ERRORS_TOTAL: &str = "oagw_errors_total";
/// `oagw_circuit_breaker_state{host}` — gauge.
pub const BREAKER_STATE: &str = "oagw_circuit_breaker_state";
/// `oagw_rate_limit_exceeded_total{host, path}` — counter.
pub const RATE_LIMIT_EXCEEDED: &str = "oagw_rate_limit_exceeded_total";
/// `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` —
/// counter.
pub const BREAKER_TRANSITIONS: &str = "oagw_circuit_breaker_transitions_total";
/// `oagw_rate_limit_usage_ratio{host, path}` — gauge bounded to 0.0 to 1.0.
pub const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
/// `oagw_routing_target_host_used{upstream_id, endpoint_host}` — counter.
pub const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
/// `oagw_routing_endpoint_selected{upstream_id, endpoint_host,
/// selection_method}` — counter.
pub const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
/// `oagw_upstream_available{host, endpoint}` — gauge valued 0 or 1.
pub const UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";
/// `oagw_upstream_connections{host, state}` — gauge.
pub const UPSTREAM_CONNECTIONS: &str = "oagw_upstream_connections";

/// The `host` label key, which carries the resolved upstream alias.
pub const LABEL_HOST: &str = "host";
/// The `endpoint` label of the upstream-health families.
pub const LABEL_ENDPOINT: &str = "endpoint";
/// The `http.request.method` label key, per the OTel HTTP semantic conventions.
pub const LABEL_METHOD: &str = "http.request.method";
/// The `http.route` label key: the normalized route match pattern.
pub const LABEL_ROUTE: &str = "http.route";
/// The `http.response.status_code` label key, per the OTel HTTP conventions.
pub const LABEL_STATUS: &str = "http.response.status_code";
/// The `phase` label of the request-duration histogram.
pub const LABEL_PHASE: &str = "phase";
/// The `error_type` label of `oagw_errors_total`, carrying the GTS type.
pub const LABEL_ERROR_TYPE: &str = "error_type";
/// The `path` label of the two rate-limit families.
pub const LABEL_PATH: &str = "path";
/// The `from_state` label of the transition counter.
pub const LABEL_FROM_STATE: &str = "from_state";
/// The `to_state` label of the transition counter.
pub const LABEL_TO_STATE: &str = "to_state";
/// The `upstream_id` label of the two routing families.
pub const LABEL_UPSTREAM_ID: &str = "upstream_id";
/// The `endpoint_host` label of the two routing families.
pub const LABEL_ENDPOINT_HOST: &str = "endpoint_host";
/// The `selection_method` label of the endpoint-selection counter.
pub const LABEL_SELECTION_METHOD: &str = "selection_method";
/// The `state` label of the connection gauge.
pub const LABEL_STATE: &str = "state";

/// The `gateway_added` value of the request-duration `phase` label.
pub const PHASE_GATEWAY_ADDED: &str = "gateway_added";
/// The `upstream` value of the request-duration `phase` label.
pub const PHASE_UPSTREAM: &str = "upstream";

/// The value an unrecognized method label is normalized onto.
pub const METHOD_OTHER: &str = "_OTHER";
/// The value a label the family's vocabulary does not name falls back to.
pub const LABEL_OTHER: &str = "_OTHER";

/// The `closed` value of the breaker state vocabulary.
pub const STATE_CLOSED: &str = "closed";
/// The `open` value of the breaker state vocabulary.
pub const STATE_OPEN: &str = "open";
/// The `half_open` value of the breaker state vocabulary.
pub const STATE_HALF_OPEN: &str = "half_open";

/// The `idle` value of the connection-state vocabulary.
pub const CONNECTION_IDLE: &str = "idle";
/// The `active` value of the connection-state vocabulary.
pub const CONNECTION_ACTIVE: &str = "active";
/// The `max` value of the connection-state vocabulary.
pub const CONNECTION_MAX: &str = "max";

/// The twelve request-duration buckets of DESIGN §4.2, in seconds.
pub const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The kind a recorded family is registered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A monotonically increasing counter.
    Counter,
    /// A gauge of the current value.
    Gauge,
    /// A distribution of observations over recorded buckets.
    Histogram,
}

impl MetricKind {
    /// The name the recorded kind is reported with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// The descriptor one family is registered with.
///
/// The descriptor is the compatibility surface an operator builds a dashboard
/// on: the name, the kind, the label keys and, for a histogram, the buckets.
/// It is recorded verbatim at registration, which is what the in-crate tests
/// assert on and what the host surface serves as the family metadata.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FamilyDescriptor {
    /// The metric name.
    pub name: &'static str,
    /// The kind the family is registered as.
    pub kind: MetricKind,
    /// The label keys the family carries, in the recorded order.
    pub label_keys: &'static [&'static str],
    /// The bucket boundaries, for a histogram; empty for the other kinds.
    pub buckets: &'static [f64],
}

impl FamilyDescriptor {
    // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-05
    /// Whether `labels` carries exactly the recorded keys, in order.
    ///
    /// A label set that does not match the recorded keys is refused here, so no
    /// update can materialize a key the family does not document.
    #[must_use]
    pub fn accepts(&self, labels: &[(&'static str, String)]) -> bool {
        if labels.len() != self.label_keys.len() {
            return false;
        }
        labels
            .iter()
            .zip(self.label_keys.iter())
            .all(|(carried, recorded)| carried.0 == *recorded)
    }
    // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-05

    /// Whether the descriptor can be registered: a non-empty name, a label key
    /// per recorded key and, for a histogram, strictly increasing buckets.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if self.name.is_empty() || self.label_keys.is_empty() {
            return false;
        }
        if self.kind == MetricKind::Histogram && self.buckets.is_empty() {
            return false;
        }
        self.buckets.windows(2).all(|window| window[0] < window[1])
    }
}

/// The twelve families of DESIGN §4.2, in the order the design lists them.
pub const FAMILIES: [FamilyDescriptor; 12] = [
    FamilyDescriptor {
        name: REQUESTS_TOTAL,
        kind: MetricKind::Counter,
        label_keys: &[LABEL_HOST, LABEL_METHOD, LABEL_ROUTE, LABEL_STATUS],
        buckets: &[],
    },
    FamilyDescriptor {
        name: REQUEST_DURATION,
        kind: MetricKind::Histogram,
        label_keys: &[LABEL_HOST, LABEL_ROUTE, LABEL_PHASE],
        buckets: DURATION_BUCKETS,
    },
    FamilyDescriptor {
        name: REQUESTS_IN_FLIGHT,
        kind: MetricKind::Gauge,
        label_keys: &[LABEL_HOST],
        buckets: &[],
    },
    FamilyDescriptor {
        name: ERRORS_TOTAL,
        kind: MetricKind::Counter,
        label_keys: &[LABEL_HOST, LABEL_ROUTE, LABEL_ERROR_TYPE],
        buckets: &[],
    },
    FamilyDescriptor {
        name: BREAKER_STATE,
        kind: MetricKind::Gauge,
        label_keys: &[LABEL_HOST],
        buckets: &[],
    },
    FamilyDescriptor {
        name: RATE_LIMIT_EXCEEDED,
        kind: MetricKind::Counter,
        label_keys: &[LABEL_HOST, LABEL_PATH],
        buckets: &[],
    },
    FamilyDescriptor {
        name: BREAKER_TRANSITIONS,
        kind: MetricKind::Counter,
        label_keys: &[LABEL_HOST, LABEL_FROM_STATE, LABEL_TO_STATE],
        buckets: &[],
    },
    FamilyDescriptor {
        name: RATE_LIMIT_USAGE_RATIO,
        kind: MetricKind::Gauge,
        label_keys: &[LABEL_HOST, LABEL_PATH],
        buckets: &[],
    },
    FamilyDescriptor {
        name: ROUTING_TARGET_HOST_USED,
        kind: MetricKind::Counter,
        label_keys: &[LABEL_UPSTREAM_ID, LABEL_ENDPOINT_HOST],
        buckets: &[],
    },
    FamilyDescriptor {
        name: ROUTING_ENDPOINT_SELECTED,
        kind: MetricKind::Counter,
        label_keys: &[LABEL_UPSTREAM_ID, LABEL_ENDPOINT_HOST, LABEL_SELECTION_METHOD],
        buckets: &[],
    },
    FamilyDescriptor {
        name: UPSTREAM_AVAILABLE,
        kind: MetricKind::Gauge,
        label_keys: &[LABEL_HOST, LABEL_ENDPOINT],
        buckets: &[],
    },
    FamilyDescriptor {
        name: UPSTREAM_CONNECTIONS,
        kind: MetricKind::Gauge,
        label_keys: &[LABEL_HOST, LABEL_STATE],
        buckets: &[],
    },
];

/// The states of `cpt-cf-oagw-state-metric-registry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryState {
    /// No family is registered; the registry emits nothing.
    Unregistered,
    /// The families are being registered on the host surface.
    Registering,
    /// Every family of DESIGN §4.2 is registered.
    Registered,
}

impl RegistryState {
    const UNREGISTERED: u8 = 0;
    const REGISTERING: u8 = 1;
    const REGISTERED: u8 = 2;

    /// The state a stored code names.
    const fn from_code(code: u8) -> Self {
        match code {
            Self::REGISTERING => Self::Registering,
            Self::REGISTERED => Self::Registered,
            _ => Self::Unregistered,
        }
    }

    /// The name the state is reported with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unregistered => "unregistered",
            Self::Registering => "registering",
            Self::Registered => "registered",
        }
    }
}

/// Why a registration did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationFailure {
    /// A descriptor the host surface rejected, named by its family name.
    Rejected(&'static str),
    /// The registry is already registered; the call changed nothing.
    AlreadyRegistered,
}

/// The live state the observable gauges read at scrape time.
///
/// The breaker of `cpt-cf-oagw-state-circuit-breaker` and the upstream
/// configuration are the pipeline's state, not this registry's, so the
/// collectors read them through this source and never hold a copy of them.
pub trait RuntimeState: Send + Sync {
    /// The state of the breaker keyed by `host`.
    fn breaker_state(&self, host: &str) -> BreakerState;
    /// The transitions the breaker recorded since the last call, oldest first.
    ///
    /// Draining, not reading: the source hands each transition over exactly
    /// once, so the registry's counter stays correct without re-reading a log
    /// that grows with the process.
    fn breaker_drain_transitions(&self) -> Vec<Transition>;
}

/// One label set of one family, in the family's recorded key order.
type LabelSet = Vec<(&'static str, String)>;

/// The key one series is collected under: the family and its label set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Series {
    family: &'static str,
    labels: LabelSet,
}

/// The recorded buckets, sum and count of one histogram series.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistogramSeries {
    /// Cumulative count per recorded bucket boundary.
    pub buckets: Vec<u64>,
    /// Sum of the observed values.
    pub sum: f64,
    /// Total number of observations.
    pub count: u64,
}

/// The instruments the host surface serves, created once at registration.
///
/// The handles are shared clones over the host's instrument state, so the
/// request path reads the set it was registered with without holding the
/// registry lock across a host-surface call.
#[derive(Clone)]
struct Instruments {
    requests_total: Counter<u64>,
    duration: Histogram<f64>,
    in_flight: Gauge<u64>,
    errors_total: Counter<u64>,
    rate_limit_exceeded: Counter<u64>,
    transitions: Counter<u64>,
    usage_ratio: Gauge<f64>,
    target_host_used: Counter<u64>,
    endpoint_selected: Counter<u64>,
}

/// The state the registry holds between registrations.
#[derive(Default)]
struct Registry {
    /// The descriptors recorded at registration, in registration order.
    families: Vec<FamilyDescriptor>,
    counters: HashMap<Series, u64>,
    gauges: HashMap<Series, f64>,
    histograms: HashMap<Series, HistogramSeries>,
    /// The endpoint hosts a breaker was observed for, in first-seen order.
    breaker_hosts: Vec<String>,
    /// The resolved upstream alias and the endpoint host it forwarded to, in
    /// first-seen order.
    endpoints: Vec<(String, String)>,
}

/// The in-flight bookkeeping of the request-context lifecycle.
#[derive(Default)]
struct InFlight {
    /// Contexts opened whose host is not resolved yet.
    pending: AtomicI64,
    /// Open contexts per resolved upstream alias.
    per_host: Mutex<HashMap<String, i64>>,
}

/// The metric registry of the gear (`cpt-cf-oagw-state-metric-registry`).
///
/// One instance per gear process, held for the lifetime of the process: the
/// state is process-local and is dropped with it, so every counter restarts
/// from zero and every gauge from its initial value on the next start
/// (DECOMPOSITION assumption 3).
pub struct MetricRegistry {
    state: AtomicU8,
    registry: Mutex<Registry>,
    instruments: Mutex<Option<Arc<Instruments>>>,
    dropped_updates: AtomicU64,
    applied_updates: AtomicU64,
    in_flight: InFlight,
    source: Mutex<Option<Arc<dyn RuntimeState>>>,
}

impl std::fmt::Debug for MetricRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricRegistry")
            .field("state", &self.state().as_str())
            .field("families", &self.families().len())
            .field("dropped_updates", &self.dropped_updates())
            .finish_non_exhaustive()
    }
}

impl Default for MetricRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The in-flight guard one open request context holds.
///
/// The guard is created when the pipeline opens the request context and is
/// dropped when the pipeline closes it, whichever exit the request takes, so
/// the gauge cannot leak an increment on a rejected request. The host it
/// reports under is recorded when the pipeline resolves the upstream, which is
/// after the open: until then the context is held in the registry's pending
/// bucket, which is not a labelled series.
#[derive(Debug)]
pub struct InFlightGuard {
    registry: Arc<MetricRegistry>,
    resolved: Mutex<Option<Resolved>>,
}

/// The labels one open context is accounted under.
#[derive(Debug, Clone)]
struct Resolved {
    host: String,
    endpoint: Option<String>,
}

impl InFlightGuard {
    /// Record the upstream alias the request resolved to.
    ///
    /// Called once per request, when the alias walk selected an upstream; a
    /// request that resolves none stays pending and is returned to zero at the
    /// close without ever producing a series.
    pub fn resolve_host(&self, host: &str) {
        let mut resolved = self.resolved.lock();
        let entry = resolved.get_or_insert_with(|| Resolved {
            host: host.to_owned(),
            endpoint: None,
        });
        entry.host = host.to_owned();
        self.registry.resolve_in_flight(host);
    }

    /// Record the endpoint host the request selected.
    pub fn resolve_endpoint(&self, endpoint: &str) {
        let mut resolved = self.resolved.lock();
        let Some(entry) = resolved.as_mut() else {
            return;
        };
        if entry.endpoint.is_some() {
            return;
        }
        entry.endpoint = Some(endpoint.to_owned());
        // The endpoint the request selected is noted for the availability
        // gauge the scrape reads; the in-flight accounting itself is per
        // upstream alias, which is the `host` label of every family.
        self.registry.note_endpoint(&entry.host, endpoint);
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a
        // The close of the request context returns the in-flight accounting to
        // its previous value, for a proxied request and for a gateway-rejected
        // request alike: a rejected request opens and closes the same context.
        let resolved = self.resolved.lock().clone();
        self.registry.release_in_flight(resolved.as_ref());
        // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a
    }
}

impl MetricRegistry {
    /// A registry in the `unregistered` state, holding no family.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(RegistryState::UNREGISTERED),
            registry: Mutex::new(Registry::default()),
            instruments: Mutex::new(None),
            dropped_updates: AtomicU64::new(0),
            applied_updates: AtomicU64::new(0),
            in_flight: InFlight::default(),
            source: Mutex::new(None),
        }
    }

    /// The state machine's current state.
    #[must_use]
    pub fn state(&self) -> RegistryState {
        RegistryState::from_code(self.state.load(Ordering::Acquire))
    }

    /// The descriptors recorded at registration, in registration order.
    #[must_use]
    pub fn families(&self) -> Vec<FamilyDescriptor> {
        self.registry.lock().families.clone()
    }

    /// The descriptor of one family, when it is registered.
    #[must_use]
    pub fn family(&self, name: &str) -> Option<FamilyDescriptor> {
        self.registry
            .lock()
            .families
            .iter()
            .find(|family| family.name == name)
            .copied()
    }

    /// The number of updates the registry dropped instead of applying.
    #[must_use]
    pub fn dropped_updates(&self) -> u64 {
        self.dropped_updates.load(Ordering::Relaxed)
    }

    /// The number of updates the registry applied to a collector.
    #[must_use]
    pub fn applied_updates(&self) -> u64 {
        self.applied_updates.load(Ordering::Relaxed)
    }

    /// Attach the live state the observable gauges read at scrape time.
    pub fn attach_state_source(&self, source: Arc<dyn RuntimeState>) {
        *self.source.lock() = Some(source);
    }

    /// The recorded value of one counter series.
    #[must_use]
    pub fn counter(&self, family: &str, labels: &[(&str, &str)]) -> Option<u64> {
        let key = self.key(family, labels)?;
        self.registry.lock().counters.get(&key).copied()
    }

    /// The recorded value of one gauge series.
    #[must_use]
    pub fn gauge(&self, family: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let key = self.key(family, labels)?;
        self.registry.lock().gauges.get(&key).copied()
    }

    /// The recorded buckets, sum and count of one histogram series.
    #[must_use]
    pub fn histogram(&self, family: &str, labels: &[(&str, &str)]) -> Option<HistogramSeries> {
        let key = self.key(family, labels)?;
        self.registry.lock().histograms.get(&key).cloned()
    }

    /// The host keys the breaker gauges report, in first-seen order.
    #[must_use]
    pub fn breaker_hosts(&self) -> Vec<String> {
        self.registry.lock().breaker_hosts.clone()
    }

    /// The alias and endpoint-host pairs the health gauge reports.
    #[must_use]
    pub fn endpoint_pairs(&self) -> Vec<(String, String)> {
        self.registry.lock().endpoints.clone()
    }

    /// The number of contexts opened whose upstream alias is not resolved yet.
    #[must_use]
    pub fn pending_in_flight(&self) -> i64 {
        self.in_flight.pending.load(Ordering::Relaxed)
    }

    /// The number of contexts currently open per resolved upstream alias.
    #[must_use]
    pub fn in_flight_by_host(&self) -> HashMap<String, i64> {
        self.in_flight.per_host.lock().clone()
    }

    /// The label set of one series, checked against the recorded keys.
    // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-04
    // @cpt-begin:cpt-cf-oagw-dod-metric-cardinality:p1:inst-full
    // Every recorded value's label set is normalized against the family's
    // recorded keys before the update reaches a collector: a key the family
    // does not document is refused here, so no update can materialize one and
    // the set stays bounded by the recorded roster.
    fn key(&self, family: &str, labels: &[(&str, &str)]) -> Option<Series> {
        let registry = self.registry.lock();
        let descriptor = registry
            .families
            .iter()
            .find(|candidate| candidate.name == family)?;
        let mut carried: LabelSet = Vec::with_capacity(labels.len());
        for (key, value) in labels {
            let recorded = descriptor
                .label_keys
                .iter()
                .find(|recorded| **recorded == *key)?;
            carried.push((*recorded, (*value).to_owned()));
        }
        if !descriptor.accepts(&carried) {
            return None;
        }
        Some(Series {
            family: descriptor.name,
            labels: carried,
        })
    }
    // @cpt-end:cpt-cf-oagw-dod-metric-cardinality:p1:inst-full
    // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-04

    /// Register the twelve families of DESIGN §4.2
    /// (`cpt-cf-oagw-state-metric-registry`).
    ///
    /// # Errors
    ///
    /// Returns the family the host surface rejected, leaving no partial family
    /// behind: the registry returns to `unregistered` and emits nothing, and
    /// traffic continues to be served.
    // @cpt-begin:cpt-cf-oagw-dod-metric-families:p1:inst-full
    // The roster this gear registers is the twelve families of DESIGN §4.2,
    // each with the exact name, kind, label keys and histogram buckets the
    // design records, so the exposition document carries no other name.
    pub fn register(self: &Arc<Self>) -> Result<(), RegistrationFailure> {
        self.register_roster(&FAMILIES)
    }
    // @cpt-end:cpt-cf-oagw-dod-metric-families:p1:inst-full

    // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-07
    // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-06
    // Every family of the roster is declared before any series exists, so a
    // family that has recorded nothing since the process started is exposed
    // with its name, kind, unit and description and zero series rather than
    // being absent from the document.
    // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-06
    /// Register an explicit roster, the seam the registration-failure path is
    /// exercised through.
    ///
    /// # Errors
    ///
    /// Returns the first family the host surface rejected.
    pub fn register_roster(
        self: &Arc<Self>,
        roster: &[FamilyDescriptor],
    ) -> Result<(), RegistrationFailure> {
        if self.state() == RegistryState::Registered {
            return Err(RegistrationFailure::AlreadyRegistered);
        }
        // @cpt-begin:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-01
        // `unregistered` -> `registering`: the gear's initialization begins
        // registering its families on the host-provided metrics surface.
        if self
            .state
            .compare_exchange(
                RegistryState::UNREGISTERED,
                RegistryState::REGISTERING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(RegistrationFailure::AlreadyRegistered);
        }
        // @cpt-end:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-01

        // @cpt-begin:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-02a
        // A descriptor the host surface rejects leaves no partial family
        // behind: the instruments are dropped, the registry returns to
        // `unregistered` and emits nothing, and traffic continues to be
        // served.
        if let Some(rejected) = roster.iter().find(|family| !family.is_valid()) {
            tracing::error!(
                family = rejected.name,
                "oagw metric family rejected by the host metrics surface; the gear serves \
                 traffic with no metric emission"
            );
            *self.registry.lock() = Registry::default();
            self.state
                .store(RegistryState::UNREGISTERED, Ordering::Release);
            return Err(RegistrationFailure::Rejected(rejected.name));
        }
        // @cpt-end:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-02a

        let meter = opentelemetry::global::meter("oagw");
        let instruments = self.instruments(&meter);
        // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-04
        // Each registered family that reports its value at scrape time is
        // handed to the host surface as an observable instrument, so the
        // document carries every family the roster holds.
        for family in roster {
            if family.kind == MetricKind::Gauge {
                self.register_observable_gauge(&meter, family, self);
            }
        }
        // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-04

        // @cpt-begin:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-02
        // `registering` -> `registered`: every family of DESIGN §4.2 is
        // registered with its name, type, label keys and histogram buckets,
        // which the recorded descriptor set is.
        {
            let mut registry = self.registry.lock();
            registry.families = roster.to_vec();
        }
        *self.instruments.lock() = Some(Arc::new(instruments));
        self.state
            .store(RegistryState::REGISTERED, Ordering::Release);
        tracing::debug!(families = roster.len(), "oagw metric families registered");
        Ok(())
        // @cpt-end:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-02
    }
    // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-07

    /// Tear the registry down with the gear: the in-memory counters and gauges
    /// are dropped with it, so the next start begins from empty series.
    pub fn teardown(&self) {
        *self.registry.lock() = Registry::default();
        *self.instruments.lock() = None;
        self.state
            .store(RegistryState::UNREGISTERED, Ordering::Release);
    }

    // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-05
    /// Create the synchronous instruments of the roster on the host meter.
    ///
    /// The host meter is a no-op meter when the host process configured no
    /// metrics provider, so registration never fails and never blocks. The
    /// instrument kind of every family is the kind its descriptor records, and
    /// the histogram carries the twelve boundaries of DESIGN §4.2, so the
    /// exposition document reports a counter's accumulated value, a gauge's
    /// current value and the buckets with their cumulative counts.
    fn instruments(&self, meter: &Meter) -> Instruments {
        Instruments {
            requests_total: meter
                .u64_counter(REQUESTS_TOTAL)
                .with_description("Proxy requests served, by upstream alias, method, route and status.")
                .with_unit("{request}")
                .build(),
            duration: meter
                .f64_histogram(REQUEST_DURATION)
                .with_description("Request duration, by phase.")
                .with_unit("s")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            in_flight: meter
                .u64_gauge(REQUESTS_IN_FLIGHT)
                .with_description("Proxy request contexts the pipeline currently holds.")
                .with_unit("{request}")
                .build(),
            errors_total: meter
                .u64_counter(ERRORS_TOTAL)
                .with_description("Gateway failures, by upstream alias, route and error type.")
                .with_unit("{error}")
                .build(),
            rate_limit_exceeded: meter
                .u64_counter(RATE_LIMIT_EXCEEDED)
                .with_description("Requests a rate limit throttled or degraded, by route match path.")
                .with_unit("{request}")
                .build(),
            transitions: meter
                .u64_counter(BREAKER_TRANSITIONS)
                .with_description("Circuit-breaker state transitions, by host and states.")
                .with_unit("{transition}")
                .build(),
            usage_ratio: meter
                .f64_gauge(RATE_LIMIT_USAGE_RATIO)
                .with_description("Rate-limit bucket usage, from 0.0 to 1.0.")
                .with_unit("1")
                .build(),
            target_host_used: meter
                .u64_counter(ROUTING_TARGET_HOST_USED)
                .with_description("Requests that named an endpoint with the routing header.")
                .with_unit("{request}")
                .build(),
            endpoint_selected: meter
                .u64_counter(ROUTING_ENDPOINT_SELECTED)
                .with_description("Endpoint selections, by selection method.")
                .with_unit("{request}")
                .build(),
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-05

    // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-08
    /// Register the observable gauges of the roster on the host meter.
    ///
    /// The callback reads the live values through this registry, so a scrape
    /// reports the current state and no OAGW code runs on the scrape path
    /// beyond the collectors. The registry is held weakly, so a dropped
    /// registry leaves no callback holding it alive.
    fn register_observable_gauge(
        &self,
        meter: &Meter,
        family: &FamilyDescriptor,
        registry: &Arc<MetricRegistry>,
    ) {
        let weak = Arc::downgrade(registry);
        let name = family.name;
        let builder = meter
            .u64_observable_gauge(name)
            .with_description(description_of(name));
        // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-01
        // @cpt-begin:cpt-cf-oagw-dod-breaker-metrics:p1:inst-full
        // The resilience series the operator reads are these observable
        // instruments: the per-host breaker state, the availability of each
        // alias and endpoint pair and the connection distribution, each read
        // from the live pipeline state at scrape time. The transition counter
        // and the in-flight gauge are the synchronous companions of the same
        // families.
        let _ = match name {
            BREAKER_STATE => builder
                .with_callback(move |observer: &dyn AsyncInstrument<u64>| {
                    if let Some(registry) = weak.upgrade() {
                        registry.observe_breaker_state(observer);
                    }
                })
                .build(),
            UPSTREAM_AVAILABLE => builder
                .with_callback(move |observer: &dyn AsyncInstrument<u64>| {
                    if let Some(registry) = weak.upgrade() {
                        registry.observe_upstream_available(observer);
                    }
                })
                .build(),
            UPSTREAM_CONNECTIONS => builder
                .with_callback(move |observer: &dyn AsyncInstrument<u64>| {
                    if let Some(registry) = weak.upgrade() {
                        registry.observe_upstream_connections(observer);
                    }
                })
                .build(),
            _ => builder.build(),
        };
        // @cpt-end:cpt-cf-oagw-dod-breaker-metrics:p1:inst-full
        // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-01
        // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-09
        // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-08
        // The document the host serves is built from these collectors: the gear
        // hands the exposition to the host, answers no scrape itself and runs
        // no code on the scrape path beyond the callbacks registered here, so
        // the series are visible on the next scrape.
        // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-08
        // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-09
    }
    // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-08

    /// The breaker state of every breaker the registry observed.
    fn observe_breaker_state(&self, observer: &dyn AsyncInstrument<u64>) {
        let state = self.state_source();
        for host in self.breaker_hosts() {
            let value = u64::from(state.as_ref().map_or(BreakerState::Closed, |state| {
                state.breaker_state(&host)
            })
            .as_code());
            observer.observe(value, &[KeyValue::new(LABEL_HOST, host)]);
        }
    }

    /// The availability of every observed alias and endpoint pair.
    fn observe_upstream_available(&self, observer: &dyn AsyncInstrument<u64>) {
        let state = self.state_source();
        let samples = self.endpoint_pairs();
        for (alias, endpoint) in samples {
            let open = state
                .as_ref()
                .is_some_and(|state| state.breaker_state(&endpoint) == BreakerState::Open);
            let value = u64::from(!open);
            observer.observe(
                value,
                &[
                    KeyValue::new(LABEL_HOST, alias),
                    KeyValue::new(LABEL_ENDPOINT, endpoint),
                ],
            );
        }
    }

    /// The connection distribution of every host an exchange was opened for.
    ///
    /// The `host` label is the upstream alias, as on every family of the
    /// roster. The gear holds no connection pool of its own, so the only
    /// measured state is `active`, the per-host count of open exchanges this
    /// registry maintains itself. `idle` and `max` are the derived values the
    /// vocabulary records — the zero value, because the gear's configuration
    /// set holds no pool cap to derive a maximum from — and no pool data is
    /// fabricated for them.
    fn observe_upstream_connections(&self, observer: &dyn AsyncInstrument<u64>) {
        let connections = self.in_flight_by_host();
        let mut hosts = connections.keys().cloned().collect::<Vec<_>>();
        hosts.sort();
        for host in hosts {
            let active = connections.get(&host).copied().unwrap_or_default();
            for (state, value) in [
                (CONNECTION_IDLE, 0),
                (CONNECTION_ACTIVE, active.max(0)),
                (CONNECTION_MAX, 0),
            ] {
                observer.observe(
                    to_u64(value),
                    &[
                        KeyValue::new(LABEL_HOST, host.clone()),
                        KeyValue::new(LABEL_STATE, state),
                    ],
                );
            }
        }
    }

    /// The attached live state, when the pipeline provided one.
    fn state_source(&self) -> Option<Arc<dyn RuntimeState>> {
        self.source.lock().clone()
    }

    /// The breaker state the attached source reports for `host`.
    #[must_use]
    pub fn breaker_state_of(&self, host: &str) -> Option<BreakerState> {
        self.state_source().map(|source| source.breaker_state(host))
    }

    /// Move one opened context into the per-host accounting.
    fn resolve_in_flight(&self, host: &str) {
        {
            let mut per_host = self.in_flight.per_host.lock();
            *per_host.entry(host.to_owned()).or_default() += 1;
        }
        self.in_flight.pending.fetch_sub(1, Ordering::AcqRel);
        self.record_in_flight_gauge(host);
    }

    /// Return one closed context out of the accounting.
    fn release_in_flight(&self, resolved: Option<&Resolved>) {
        let Some(resolved) = resolved else {
            self.in_flight.pending.fetch_sub(1, Ordering::AcqRel);
            return;
        };
        // The decrement, the decision over the emptied entry and the value the
        // gauge publishes are one critical section on the per-host lock: a
        // context that resolved into the host between the decrement and the
        // removal would be counted and then deleted with the entry.
        let remaining = {
            let mut per_host = self.in_flight.per_host.lock();
            let entry = per_host.entry(resolved.host.clone()).or_default();
            *entry -= 1;
            if *entry <= 0 {
                per_host.remove(&resolved.host);
                0
            } else {
                *entry
            }
        };
        // The per-host lock is released before the host surface is told.
        self.publish_in_flight(&resolved.host, remaining);
    }

    /// Publish the current in-flight value of `host` to the collector.
    fn record_in_flight_gauge(&self, host: &str) {
        let value = self
            .in_flight
            .per_host
            .lock()
            .get(host)
            .copied()
            .unwrap_or_default();
        self.publish_in_flight(host, value);
    }

    /// Publish one in-flight reading of `host` to the collector.
    fn publish_in_flight(&self, host: &str, value: i64) {
        self.set_gauge(
            REQUESTS_IN_FLIGHT,
            &[LABEL_HOST],
            &[host],
            to_f64(to_u64(value)),
        );
    }

    /// Open one request context for the in-flight accounting.
    #[must_use]
    pub fn request_opened(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.pending.fetch_add(1, Ordering::AcqRel);
        InFlightGuard {
            registry: Arc::clone(self),
            resolved: Mutex::new(None),
        }
    }

    /// Record the outcome of one closed request
    /// (`cpt-cf-oagw-algo-metric-emit`).
    pub fn record_request(&self, observation: &RequestObservation<'_>) {
        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-01
        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-03
        // The label set is normalized before any update reaches a collector:
        // the method onto a standard verb or `_OTHER`, the status onto the
        // numeric status the client received, the route onto the match pattern
        // and never the raw request path, the host onto the resolved upstream
        // alias, and no tenant, subject, peer address or request identifier on
        // any family.
        let context = observation.context;
        let host = host_label(context.alias.as_deref());
        let route = route_label(context.matched_route.as_deref());
        let method = method_label(&context.method);
        let status = observation.status.to_string();
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-03
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-01

        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-07
        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-07a
        // The request counter is incremented by one for a successfully proxied
        // request and for a gateway-rejected request alike.
        self.increment(
            REQUESTS_TOTAL,
            &[LABEL_HOST, LABEL_METHOD, LABEL_ROUTE, LABEL_STATUS],
            &[&host, method, &route, &status],
        );
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-07a
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-07

        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-03a
        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-04
        // One gateway-added observation per closed request, over the twelve
        // recorded buckets. A request on which the pipeline made no upstream
        // call adds no `upstream` observation.
        self.observe_duration(
            &host,
            &route,
            PHASE_GATEWAY_ADDED,
            observation.duration.as_secs_f64(),
        );
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-04
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-03a

        // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07a
        // The same closed outcome updates the request families: the error
        // counter is incremented only for a gateway failure, with the GTS type
        // identifier the audit line carries for the same request.
        if let Some(error_type) = observation.error_type() {
            self.increment(
                ERRORS_TOTAL,
                &[LABEL_HOST, LABEL_ROUTE, LABEL_ERROR_TYPE],
                &[&host, &route, error_type],
            );
        }
        // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07a

        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-02
        // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-02
        // @cpt-begin:cpt-cf-oagw-dod-routing-metrics:p1:inst-full
        // The routing and rate-limit series and their vocabularies are owned
        // here: `selection_method` is restricted to `explicit_header`,
        // `round_robin` and `default`, the target-host counter records the
        // `X-OAGW-Target-Host` usage, and the two rate-limit families carry the
        // route's configured match path and a usage ratio bounded to 0.0-1.0.
        // The routing series are recorded from the selection method the
        // entry-2.4 endpoint-selection step put on the request context; this
        // feature owns the family names, the label keys and the
        // `explicit_header`|`round_robin`|`default` vocabulary, not the
        // selection.
        if let (Some(upstream_id), Some(endpoint_host), Some(selection)) = (
            &context.upstream_id,
            &context.endpoint_host,
            context.selection,
        ) {
            self.note_endpoint(&host, endpoint_host);
            self.increment(
                ROUTING_ENDPOINT_SELECTED,
                &[
                    LABEL_UPSTREAM_ID,
                    LABEL_ENDPOINT_HOST,
                    LABEL_SELECTION_METHOD,
                ],
                &[upstream_id, endpoint_host, selection.as_str()],
            );
            if selection == SelectionMethod::ExplicitHeader {
                self.increment(
                    ROUTING_TARGET_HOST_USED,
                    &[LABEL_UPSTREAM_ID, LABEL_ENDPOINT_HOST],
                    &[upstream_id, endpoint_host],
                );
            }
        }
        // @cpt-end:cpt-cf-oagw-dod-routing-metrics:p1:inst-full
        // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-02
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-02

        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-13
        // The update result is returned to the emitting step: a value that
        // could not be recorded was dropped and counted in-process, and no
        // error reaches the caller, because a metric that cannot be recorded
        // never fails a request.
        self.record_rate_limit(context, &host);
        self.record_breaker_transitions();
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-13
    }

    /// Record one upstream-call duration observation (`upstream` phase).
    pub fn observe_upstream_duration(&self, host: &str, route: &str, duration: Duration) {
        self.observe_duration(
            &host_label(Some(host)),
            &route_label(Some(route)),
            PHASE_UPSTREAM,
            duration.as_secs_f64(),
        );
    }

    /// Record the rate-limit series of one closed request.
    // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-02b
    // The rate-limit series are recorded from the decision and the degradation
    // flag the entry-2.5 rate-limit step put on the request context; this
    // feature owns the two family names, their label keys and the 0.0 to 1.0
    // bound of the usage ratio, not the decision.
    fn record_rate_limit(&self, context: &RequestContext, host: &str) {
        let Some(observation) = context.rate_limit.as_ref() else {
            return;
        };
        // The `path` label is the route's configured match path, never the raw
        // request path and never the query string.
        let path = route_label(context.matched_route.as_deref());
        if observation.decision == rate_limit::REJECTED_DECISION || context.degraded {
            self.increment(
                RATE_LIMIT_EXCEEDED,
                &[LABEL_HOST, LABEL_PATH],
                &[host, &path],
            );
        }
        self.set_gauge(
            RATE_LIMIT_USAGE_RATIO,
            &[LABEL_HOST, LABEL_PATH],
            &[host, &path],
            usage_ratio(observation),
        );
    }
    // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-02b

    /// Record the transitions the breaker took since the last request.
    ///
    /// The breaker of `cpt-cf-oagw-state-circuit-breaker` records every
    /// transition without emitting a metric itself; this registry owns the
    /// transition counter and reads them here, once per transition and never
    /// per rejected request.
    // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03
    // The breaker of `cpt-cf-oagw-state-circuit-breaker` records every
    // transition without emitting a metric itself; this registry owns the gauge
    // and the transition counter with their `host`, `from_state` and `to_state`
    // labels, and reads the transitions here.
    fn record_breaker_transitions(&self) {
        let Some(source) = self.state_source() else {
            return;
        };
        // The source drains: each transition is handed over exactly once, so
        // this scan covers only what the breaker recorded since the last closed
        // request instead of a log that grows with the process.
        for transition in &source.breaker_drain_transitions() {
            self.increment(
                BREAKER_TRANSITIONS,
                &[LABEL_HOST, LABEL_FROM_STATE, LABEL_TO_STATE],
                &[
                    &transition.host,
                    transition.from.as_str(),
                    transition.to.as_str(),
                ],
            );
            self.note_breaker_host(&transition.host);
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03

    /// Increment one counter series by one.
    // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-09
    // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-10
    // The update is one atomic operation on the collector: the registry lock is
    // taken for the single entry mutation and is released before the host
    // surface is told, so no lock is held across any I/O and the label set is
    // the only allocation the request path pays for.
    fn increment(&self, family: &'static str, keys: &[&'static str], values: &[&str]) {
        let labels = label_set(keys, values);
        // The registry lock covers the recorded check and the single entry
        // mutation together: one acquisition, no lock held across a host-surface
        // call, and the label set moves into the series key instead of being
        // cloned for it.
        {
            let mut registry = self.registry.lock();
            if !accepts(&registry, family, &labels) {
                drop(registry);
                // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-11
                // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-12
                // An unregistered family or a label set outside the recorded keys
                // is dropped, counted in-process and raised to nobody: a metric
                // that cannot be recorded never fails a request.
                self.drop_update();
                return;
                // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-12
                // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-11
            }
            // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-07
            // A label set the family accepts is applied to the collector as one
            // atomic operation and the update returns: nothing else is read, so
            // no partial series is left behind.
            *registry
                .counters
                .entry(Series {
                    family,
                    labels: labels.clone(),
                })
                .or_default() += 1;
            // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-07
        }
        self.applied_updates.fetch_add(1, Ordering::Relaxed);
        self.export_counter(family, &labels, 1);
    }
    // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-10
    // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-09

    /// Add one observation to one histogram series.
    fn observe_duration(&self, host: &str, route: &str, phase: &str, seconds: f64) {
        let labels: LabelSet = vec![
            (LABEL_HOST, host.to_owned()),
            (LABEL_ROUTE, route.to_owned()),
            (LABEL_PHASE, phase.to_owned()),
        ];
        {
            let mut registry = self.registry.lock();
            if !accepts(&registry, REQUEST_DURATION, &labels) {
                drop(registry);
                self.drop_update();
                return;
            }
            let series = registry.histograms.entry(Series {
                family: REQUEST_DURATION,
                labels: labels.clone(),
            })
            .or_default();
            // A series the in-process registry just opened carries no bucket
            // slots yet; the slots are grown to the recorded bucket count once
            // and every observation then raises the boundaries it falls under.
            series.buckets.resize(DURATION_BUCKETS.len(), 0);
            // The buckets accumulate: `or_default` starts a new series at zeros
            // and every observation raises the boundaries it falls under, so a
            // series with more than one observation reports the distribution the
            // host histogram instrument reports. An observation above the last
            // recorded boundary raises `sum` and `count` only, as the DESIGN
            // bucket list ends at `10.0` and carries no `+Inf` row.
            // Cumulative: a bucket counts the observations at or below its
            // boundary, so an observation raises the boundary it falls under
            // and every larger one, exactly as the host histogram instrument
            // records it.
            for counted in series.buckets.iter_mut().skip(bucket_of(seconds)) {
                *counted += 1;
            }
            series.sum += seconds;
            series.count += 1;
        }
        self.applied_updates.fetch_add(1, Ordering::Relaxed);
        if let Some(instruments) = self.instruments.lock().clone() {
            instruments
                .duration
                .record(seconds, &label_values(&labels));
        }
    }

    // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-08
    /// Set one gauge series to a value.
    ///
    /// A family that is neither a ranged gauge nor a counter is set to its
    /// current value: the in-flight, breaker-state and connection gauges are
    /// published as the value the state holds now, never as an accumulation of
    /// the updates that reached them.
    fn set_gauge(
        &self,
        family: &'static str,
        keys: &[&'static str],
        values: &[&str],
        value: f64,
    ) {
        let labels = label_set(keys, values);
        {
            let mut registry = self.registry.lock();
            if !accepts(&registry, family, &labels) {
                drop(registry);
                self.drop_update();
                return;
            }
            registry.gauges.insert(
                Series {
                    family,
                    labels: labels.clone(),
                },
                value,
            );
        }
        self.applied_updates.fetch_add(1, Ordering::Relaxed);
        self.export_gauge(family, &labels, value);
    }
    // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-08

    /// Export one counter increment to the host surface.
    fn export_counter(&self, family: &'static str, labels: &LabelSet, delta: u64) {
        // The instrument set is read out of its own slot, so the registry lock
        // stays free while the host surface is called.
        let Some(instruments) = self.instruments.lock().clone() else {
            return;
        };
        let attributes = label_values(labels);
        match family {
            REQUESTS_TOTAL => instruments.requests_total.add(delta, &attributes),
            ERRORS_TOTAL => instruments.errors_total.add(delta, &attributes),
            RATE_LIMIT_EXCEEDED => instruments.rate_limit_exceeded.add(delta, &attributes),
            BREAKER_TRANSITIONS => instruments.transitions.add(delta, &attributes),
            ROUTING_TARGET_HOST_USED => instruments.target_host_used.add(delta, &attributes),
            ROUTING_ENDPOINT_SELECTED => instruments.endpoint_selected.add(delta, &attributes),
            _ => {}
        }
    }

    /// Export one gauge reading to the host surface.
    fn export_gauge(&self, family: &'static str, labels: &LabelSet, value: f64) {
        let Some(instruments) = self.instruments.lock().clone() else {
            return;
        };
        let attributes = label_values(labels);
        match family {
            REQUESTS_IN_FLIGHT => instruments
                .in_flight
                .record(to_u64(to_i64(value)), &attributes),
            RATE_LIMIT_USAGE_RATIO => instruments.usage_ratio.record(value, &attributes),
            _ => {}
        }
    }

    /// Count one update the registry could not apply.
    fn drop_update(&self) {
        self.dropped_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an alias and endpoint pair the health gauge reports.
    ///
    /// The endpoint host is also a key the breaker gauges report, so a breaker
    /// that never left `closed` exposes a state series of its initial value and
    /// is distinguishable from a breaker that is absent because the upstream has
    /// no configuration.
    fn note_endpoint(&self, alias: &str, endpoint: &str) {
        let mut registry = self.registry.lock();
        if !registry
            .endpoints
            .iter()
            .any(|(recorded, host)| recorded == alias && host == endpoint)
        {
            registry
                .endpoints
                .push((alias.to_owned(), endpoint.to_owned()));
        }
        if !registry.breaker_hosts.iter().any(|recorded| recorded == endpoint) {
            registry.breaker_hosts.push(endpoint.to_owned());
        }
    }

    /// Record a breaker key the state gauge reports.
    fn note_breaker_host(&self, host: &str) {
        let mut registry = self.registry.lock();
        if !registry.breaker_hosts.iter().any(|recorded| recorded == host) {
            registry.breaker_hosts.push(host.to_owned());
        }
    }
}

impl BreakerState {
    /// The numeric value the state gauge reports the state with.
    #[must_use]
    pub const fn as_code(self) -> u8 {
        match self {
            Self::Closed => 0,
            Self::Open => 1,
            Self::HalfOpen => 2,
        }
    }
}

/// The help text one family is registered with.
fn description_of(name: &str) -> &'static str {
    match name {
        BREAKER_STATE => "Circuit-breaker state per host, 0 closed, 1 open, 2 half open.",
        UPSTREAM_AVAILABLE => "Whether an upstream endpoint admits traffic, 0 down, 1 up.",
        UPSTREAM_CONNECTIONS => "Open exchanges per host and connection state.",
        _ => "OAGW metric family.",
    }
}

/// Whether the family is recorded and the label set is in its recorded keys.
///
/// Read over an already-locked registry, so an update validates and applies in
/// the one acquisition the entry mutation needs.
fn accepts(registry: &Registry, family: &str, labels: &LabelSet) -> bool {
    registry
        .families
        .iter()
        .find(|candidate| candidate.name == family)
        .is_some_and(|descriptor| descriptor.accepts(labels))
}

/// Build one label set in the family's recorded key order.
fn label_set(keys: &[&'static str], values: &[&str]) -> LabelSet {
    keys.iter()
        .zip(values.iter())
        .map(|(key, value)| (*key, (*value).to_owned()))
        .collect()
}

/// The OpenTelemetry attributes of one label set.
fn label_values(labels: &LabelSet) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|(key, value)| KeyValue::new(*key, value.clone()))
        .collect()
}

/// The `host` label value of a request family: the resolved upstream alias.
///
/// An alias the pipeline did not resolve is not a host value, so it is
/// normalized onto the recorded fallback rather than replaced by the requested
/// alias, the path or the authority.
#[must_use]
pub fn host_label(alias: Option<&str>) -> String {
    alias
        .filter(|alias| !alias.is_empty())
        .map_or_else(|| LABEL_OTHER.to_owned(), str::to_owned)
}

/// The `http.route` label value: the normalized route match pattern.
#[must_use]
pub fn route_label(route: Option<&str>) -> String {
    route
        .filter(|route| !route.is_empty())
        .map_or_else(|| LABEL_OTHER.to_owned(), str::to_owned)
}

// @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-06
/// The `http.request.method` label value: a standard verb or `_OTHER`.
/// An HTTP method that is not one of the recorded standard verbs is mapped onto
/// `_OTHER` and never invented as a new label value at runtime.
pub fn method_label(method: &str) -> &'static str {
    match method.to_ascii_uppercase().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "CONNECT" => "CONNECT",
        _ => METHOD_OTHER,
    }
}
// @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-06

// @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-05
// @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-06
/// The rate-limit bucket usage of one observation, clamped into 0.0 to 1.0.
///
/// The usage ratio is clamped into its recorded 0.0 to 1.0 range and the
/// availability gauge is published as `0` or `1` only, so no update can place a
/// value outside the range its family documents.
#[must_use]
pub fn usage_ratio(observation: &RateLimitObservation) -> f64 {
    if observation.limit == 0 {
        return 1.0;
    }
    let ratio = 1.0 - (to_f64(observation.remaining) / to_f64(observation.limit));
    value_clamped(ratio, 0.0, 1.0)
}
// @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-06
// @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-05

/// The `f64` a token count is measured with.
fn to_f64(value: u64) -> f64 {
    value as f64
}

/// The `u64` an integer gauge reading is published with.
fn to_u64(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or_default()
}

/// The `i64` a gauge reading is narrowed to before it is published.
fn to_i64(value: f64) -> i64 {
    value.max(0.0) as i64
}

/// Clamp a ranged gauge value into its recorded range.
#[must_use]
pub fn value_clamped(value: f64, lowest: f64, highest: f64) -> f64 {
    if value.is_nan() {
        return lowest;
    }
    value.max(lowest).min(highest)
}

/// The `oagw_upstream_available` value of an endpoint whose breaker is `open`:
/// an endpoint whose breaker is open is down, so it is available as `0`.
// @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-06
#[must_use]
pub fn available_value(open: bool) -> f64 {
    f64::from(u8::from(!open))
}
// @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-06

/// The histogram boundary a duration observation falls under.
///
/// The boundary is the first one at or above the observation, and the slot past
/// the last one is returned for an observation above every recorded boundary,
/// which leaves the finite buckets untouched — the roster records no `+Inf` row
/// that such an observation would raise.
#[must_use]
pub fn bucket_of(seconds: f64) -> usize {
    DURATION_BUCKETS
        .iter()
        .position(|boundary| seconds <= *boundary)
        .unwrap_or(DURATION_BUCKETS.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The string one attribute value is carried with.
    fn value_string(value: &opentelemetry::Value) -> String {
        if let opentelemetry::Value::String(text) = value {
            text.to_string()
        } else {
            String::new()
        }
    }

    fn registry() -> Arc<MetricRegistry> {
        let registry = Arc::new(MetricRegistry::new());
        registry.register().expect("the roster registers");
        registry
    }

    #[test]
    fn the_roster_is_the_twelve_families_of_the_design() {
        assert_eq!(FAMILIES.len(), 12);
        let names = FAMILIES.iter().map(|family| family.name).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                REQUESTS_TOTAL,
                REQUEST_DURATION,
                REQUESTS_IN_FLIGHT,
                ERRORS_TOTAL,
                BREAKER_STATE,
                RATE_LIMIT_EXCEEDED,
                BREAKER_TRANSITIONS,
                RATE_LIMIT_USAGE_RATIO,
                ROUTING_TARGET_HOST_USED,
                ROUTING_ENDPOINT_SELECTED,
                UPSTREAM_AVAILABLE,
                UPSTREAM_CONNECTIONS,
            ]
        );
        for family in FAMILIES {
            assert!(family.is_valid(), "{} is a valid descriptor", family.name);
        }
    }

    #[test]
    fn the_request_duration_histogram_presents_the_twelve_buckets() {
        let descriptor = FAMILIES
            .iter()
            .find(|family| family.name == REQUEST_DURATION)
            .expect("the histogram is in the roster");
        assert_eq!(descriptor.kind, MetricKind::Histogram);
        assert_eq!(descriptor.buckets, DURATION_BUCKETS);
        assert_eq!(DURATION_BUCKETS.len(), 12);
        assert_eq!(
            DURATION_BUCKETS,
            &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
        );
        assert_eq!(descriptor.label_keys, &["host", "http.route", "phase"]);
    }

    #[test]
    fn every_family_records_its_label_keys() {
        let expected = [
            (REQUESTS_TOTAL, vec![LABEL_HOST, LABEL_METHOD, LABEL_ROUTE, LABEL_STATUS]),
            (REQUEST_DURATION, vec![LABEL_HOST, LABEL_ROUTE, LABEL_PHASE]),
            (REQUESTS_IN_FLIGHT, vec![LABEL_HOST]),
            (ERRORS_TOTAL, vec![LABEL_HOST, LABEL_ROUTE, LABEL_ERROR_TYPE]),
            (BREAKER_STATE, vec![LABEL_HOST]),
            (RATE_LIMIT_EXCEEDED, vec![LABEL_HOST, LABEL_PATH]),
            (BREAKER_TRANSITIONS, vec![LABEL_HOST, LABEL_FROM_STATE, LABEL_TO_STATE]),
            (RATE_LIMIT_USAGE_RATIO, vec![LABEL_HOST, LABEL_PATH]),
            (ROUTING_TARGET_HOST_USED, vec![LABEL_UPSTREAM_ID, LABEL_ENDPOINT_HOST]),
            (
                ROUTING_ENDPOINT_SELECTED,
                vec![LABEL_UPSTREAM_ID, LABEL_ENDPOINT_HOST, LABEL_SELECTION_METHOD],
            ),
            (UPSTREAM_AVAILABLE, vec![LABEL_HOST, LABEL_ENDPOINT]),
            (UPSTREAM_CONNECTIONS, vec![LABEL_HOST, LABEL_STATE]),
        ];
        for (name, keys) in expected {
            let descriptor = FAMILIES
                .iter()
                .find(|family| family.name == name)
                .unwrap_or_else(|| panic!("{name} is in the roster"));
            assert_eq!(descriptor.label_keys, keys.as_slice(), "{name}");
        }
    }

    #[test]
    fn every_family_is_registered_with_its_recorded_kind() {
        // The type of each family of DESIGN §4.2: a counter accumulates, a gauge
        // carries a current value and the request duration is the one histogram.
        // The kind is the rest of the compatibility surface the host exposition
        // serves as the family metadata, next to the name and the label keys.
        let expected = [
            (REQUESTS_TOTAL, MetricKind::Counter),
            (REQUEST_DURATION, MetricKind::Histogram),
            (REQUESTS_IN_FLIGHT, MetricKind::Gauge),
            (ERRORS_TOTAL, MetricKind::Counter),
            (BREAKER_STATE, MetricKind::Gauge),
            (RATE_LIMIT_EXCEEDED, MetricKind::Counter),
            (BREAKER_TRANSITIONS, MetricKind::Counter),
            (RATE_LIMIT_USAGE_RATIO, MetricKind::Gauge),
            (ROUTING_TARGET_HOST_USED, MetricKind::Counter),
            (ROUTING_ENDPOINT_SELECTED, MetricKind::Counter),
            (UPSTREAM_AVAILABLE, MetricKind::Gauge),
            (UPSTREAM_CONNECTIONS, MetricKind::Gauge),
        ];
        for (name, kind) in expected {
            let descriptor = FAMILIES
                .iter()
                .find(|family| family.name == name)
                .unwrap_or_else(|| panic!("{name} is in the roster"));
            assert_eq!(descriptor.kind, kind, "{name}");
        }
        // The kind the descriptor records is the name the host surface reports.
        for family in FAMILIES {
            let reported = family.kind.as_str();
            assert!(
                matches!(reported, "counter" | "gauge" | "histogram"),
                "{reported} is the type of {}",
                family.name
            );
        }
    }

    #[test]
    fn a_rejected_family_leaves_no_partial_family_behind() {
        let registry = Arc::new(MetricRegistry::new());
        let mut broken = FAMILIES.to_vec();
        broken[1] = FamilyDescriptor {
            name: REQUEST_DURATION,
            kind: MetricKind::Histogram,
            label_keys: &[LABEL_HOST, LABEL_ROUTE, LABEL_PHASE],
            // Not strictly increasing, which the host surface rejects.
            buckets: &[1.0, 0.5],
        };
        let failure = registry
            .register_roster(&broken)
            .expect_err("a rejected family fails the registration");
        assert_eq!(failure, RegistrationFailure::Rejected(REQUEST_DURATION));
        assert_eq!(registry.state(), RegistryState::Unregistered);
        assert!(registry.families().is_empty());
    }

    #[test]
    fn a_registered_registry_reports_the_registered_state() {
        let registry = registry();
        assert_eq!(registry.state(), RegistryState::Registered);
        assert_eq!(registry.families().len(), 12);
        let failure = registry.register().expect_err("a second registration is refused");
        assert_eq!(failure, RegistrationFailure::AlreadyRegistered);
    }

    #[test]
    fn the_method_label_is_a_standard_verb_or_other() {
        for (method, expected) in [
            ("GET", "GET"),
            ("post", "POST"),
            ("DELETE", "DELETE"),
            ("PATCH", "PATCH"),
            ("HEAD", "HEAD"),
            ("OPTIONS", "OPTIONS"),
            ("TRACE", "TRACE"),
            ("CONNECT", "CONNECT"),
            ("PROPFIND", METHOD_OTHER),
            ("", METHOD_OTHER),
        ] {
            assert_eq!(method_label(method), expected, "{method}");
        }
        assert_eq!(method_label("get"), "GET");
    }

    /// One observed gauge series: its label pairs and the value it reported.
    type Observed = (Vec<(String, String)>, u64);

    /// A collector the gauge callbacks are driven with directly, so the value a
    /// scrape reads is asserted without an OpenTelemetry meter provider.
    #[derive(Default)]
    struct Collected {
        series: std::sync::Mutex<Vec<Observed>>,
    }

    impl AsyncInstrument<u64> for Collected {
        fn observe(&self, value: u64, attributes: &[KeyValue]) {
            let labels = attributes
                .iter()
                .map(|attribute| (attribute.key.as_str().to_owned(), value_string(&attribute.value)))
                .collect();
            self.series.lock().unwrap().push((labels, value));
        }
    }

    #[test]
    fn the_connection_gauge_reports_the_open_exchanges_per_host() {
        let registry = registry();
        let collector = Collected::default();

        // No exchange is open, so the callback reports nothing at all.
        registry.observe_upstream_connections(&collector);
        assert!(collector.series.lock().unwrap().is_empty());

        // One open exchange on a resolved alias, and one still pending, are
        // reported as the `active` value of their host and as nothing else:
        // the vocabulary's other states are the zero values the family
        // documents, and no pool data is fabricated for them.
        let pending = registry.request_opened();
        let open = registry.request_opened();
        open.resolve_host("api.vendor.com");
        registry.observe_upstream_connections(&collector);
        let series = collector.series.lock().unwrap().clone();
        let active: Vec<(&(String, String), u64)> = series
            .iter()
            .flat_map(|(labels, value)| {
                labels
                    .iter()
                    .find(|(key, _)| key == LABEL_STATE)
                    .map(|label| (label, *value))
            })
            .collect();
        assert_eq!(
            active,
            vec![
                (&(LABEL_STATE.to_owned(), CONNECTION_IDLE.to_owned()), 0),
                (&(LABEL_STATE.to_owned(), CONNECTION_ACTIVE.to_owned()), 1),
                (&(LABEL_STATE.to_owned(), CONNECTION_MAX.to_owned()), 0),
            ],
            "the vocabulary's three states are reported, the open exchange as `active`: {series:?}"
        );
        for (labels, _) in &series {
            assert_eq!(
                labels.iter().find(|(key, _)| key == LABEL_HOST),
                Some(&(LABEL_HOST.to_owned(), "api.vendor.com".to_owned())),
                "the host label is the resolved alias"
            );
        }
        drop(open);
        drop(pending);
    }

    #[test]
    fn the_health_gauge_reports_the_noted_endpoints() {
        let registry = registry();
        let collector = Collected::default();
        registry.observe_upstream_available(&collector);
        assert!(collector.series.lock().unwrap().is_empty(), "nothing noted, nothing reported");

        let open = registry.request_opened();
        open.resolve_host("api.vendor.com");
        open.resolve_endpoint("10.0.0.1");
        registry.observe_upstream_available(&collector);
        let series = collector.series.lock().unwrap().clone();
        assert_eq!(
            series,
            vec![(
                vec![
                    (LABEL_HOST.to_owned(), "api.vendor.com".to_owned()),
                    (LABEL_ENDPOINT.to_owned(), "10.0.0.1".to_owned()),
                ],
                1,
            )],
            "a reachable endpoint whose breaker is closed is up"
        );
        drop(open);
    }

    #[test]
    fn the_route_label_is_never_the_raw_request_path() {
        assert_eq!(route_label(Some("/v1/things")), "/v1/things");
        assert_eq!(route_label(None), LABEL_OTHER);
        assert_eq!(route_label(Some("")), LABEL_OTHER);
    }

    #[test]
    fn the_host_label_is_the_resolved_upstream_alias() {
        assert_eq!(host_label(Some("api.vendor.com")), "api.vendor.com");
        assert_eq!(host_label(None), LABEL_OTHER);
    }

    #[test]
    fn the_duration_buckets_are_cumulative_over_the_recorded_boundaries() {
        let registry = registry();
        let seconds = 0.02;
        registry.observe_duration("api.vendor.com", "/v1", PHASE_GATEWAY_ADDED, seconds);
        let series = registry
            .histogram(
                REQUEST_DURATION,
                &[(LABEL_HOST, "api.vendor.com"), (LABEL_ROUTE, "/v1"), (LABEL_PHASE, PHASE_GATEWAY_ADDED)],
            )
            .expect("the series exists");
        assert_eq!(series.count, 1);
        // The boundary the observation falls under and every larger one carry
        // it; the smaller ones do not.
        let under = DURATION_BUCKETS
            .iter()
            .position(|boundary| seconds <= *boundary)
            .expect("the observation falls inside the recorded range");
        for (index, bucket) in series.buckets.iter().enumerate() {
            assert_eq!(
                *bucket,
                u64::from(index >= under),
                "bucket {index} of a cumulative histogram"
            );
        }
        // An observation above the largest recorded boundary raises none of the
        // finite buckets, which is what a roster without an `+Inf` row reports.
        registry.observe_duration("api.vendor.com", "/v1", PHASE_GATEWAY_ADDED, 60.0);
        let series = registry
            .histogram(
                REQUEST_DURATION,
                &[(LABEL_HOST, "api.vendor.com"), (LABEL_ROUTE, "/v1"), (LABEL_PHASE, PHASE_GATEWAY_ADDED)],
            )
            .expect("the series exists");
        assert_eq!(series.count, 2);
        assert_eq!(
            series.buckets,
            DURATION_BUCKETS
                .iter()
                .map(|boundary| u64::from(seconds <= *boundary))
                .collect::<Vec<_>>(),
            "the above-range observation raised no finite bucket"
        );
    }

    #[test]
    fn the_usage_ratio_is_clamped_into_its_recorded_range() {
        let observation = |limit: u64, remaining: u64| RateLimitObservation {
            scope: "tenant".to_owned(),
            scope_fallback: false,
            decision: rate_limit::REJECTED_DECISION.to_owned(),
            limit,
            remaining,
            reset: None,
            retry_after: None,
            response_headers: true,
        };
        let mid = usage_ratio(&observation(10, 5));
        assert!(
            (mid - 0.5).abs() < 1e-9,
            "half the bucket is a 0.5 usage ratio"
        );
        assert!((usage_ratio(&observation(10, 0)) - 1.0).abs() < 1e-9);
        assert!((usage_ratio(&observation(10, 10)) - 0.0).abs() < 1e-9);
        assert!(
            (usage_ratio(&observation(10, 99)) - 0.0).abs() < 1e-9,
            "a remaining above the limit is not a full bucket"
        );
        assert!((usage_ratio(&observation(0, 0)) - 1.0).abs() < 1e-9);
        assert!((value_clamped(2.0, 0.0, 1.0) - 1.0).abs() < 1e-9);
        assert!((value_clamped(-1.0, 0.0, 1.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn the_upstream_available_value_is_zero_or_one() {
        assert!(
            (available_value(true) - 0.0).abs() < 1e-9,
            "an endpoint whose breaker is open is down"
        );
        assert!((available_value(false) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_duration_lands_in_the_recorded_buckets() {
        assert_eq!(bucket_of(0.000_5), 0);
        assert_eq!(bucket_of(0.001), 0);
        assert_eq!(bucket_of(0.006), 2);
        assert_eq!(bucket_of(0.25), 6);
        assert_eq!(bucket_of(10.0), 11);
        assert_eq!(bucket_of(60.0), 12);
    }

    #[test]
    fn an_open_request_moves_the_in_flight_gauge_and_returns_it() {
        let registry = registry();
        let before = registry.gauge(REQUESTS_IN_FLIGHT, &[(LABEL_HOST, "api.vendor.com")]);
        let guard = registry.request_opened();
        assert_eq!(registry.pending_in_flight(), 1);
        guard.resolve_host("api.vendor.com");
        assert_eq!(registry.pending_in_flight(), 0);
        let open = registry
            .gauge(REQUESTS_IN_FLIGHT, &[(LABEL_HOST, "api.vendor.com")])
            .expect("the host series exists");
        assert!((open - 1.0).abs() < 1e-9, "one context is open");
        drop(guard);
        let after = registry
            .gauge(REQUESTS_IN_FLIGHT, &[(LABEL_HOST, "api.vendor.com")])
            .expect("the series survives the close");
        assert!(
            (after - before.unwrap_or_default()).abs() < 1e-9,
            "the gauge returns to its previous value"
        );
    }

    #[test]
    fn an_unresolved_request_never_produces_a_host_series() {
        let registry = registry();
        let guard = registry.request_opened();
        assert_eq!(registry.pending_in_flight(), 1);
        drop(guard);
        assert_eq!(registry.pending_in_flight(), 0);
        assert!(registry.gauge(REQUESTS_IN_FLIGHT, &[(LABEL_HOST, LABEL_OTHER)]).is_none());
    }

    #[test]
    fn an_update_for_an_unregistered_family_is_dropped_and_counted() {
        let registry = Arc::new(MetricRegistry::new());
        assert_eq!(registry.state(), RegistryState::Unregistered);
        registry.increment(REQUESTS_TOTAL, &[], &[]);
        assert_eq!(registry.dropped_updates(), 1);
        assert_eq!(registry.applied_updates(), 0);
    }

    #[test]
    fn a_label_set_outside_the_recorded_keys_is_dropped() {
        let registry = registry();
        registry.increment(REQUESTS_TOTAL, &[LABEL_HOST, "tenant_id"], &["a", "t"]);
        assert_eq!(registry.dropped_updates(), 1);
        assert!(
            registry
                .counter(REQUESTS_TOTAL, &[(LABEL_HOST, "a"), ("tenant_id", "t")])
                .is_none(),
            "no series is materialized for an unrecorded label key"
        );
    }

    #[test]
    fn teardown_drops_the_in_memory_collectors() {
        let registry = registry();
        registry.increment(
            REQUESTS_TOTAL,
            &[LABEL_HOST, LABEL_METHOD, LABEL_ROUTE, LABEL_STATUS],
            &["api.vendor.com", "GET", "/v1", "200"],
        );
        assert!(
            registry
                .counter(
                    REQUESTS_TOTAL,
                    &[
                        (LABEL_HOST, "api.vendor.com"),
                        (LABEL_METHOD, "GET"),
                        (LABEL_ROUTE, "/v1"),
                        (LABEL_STATUS, "200"),
                    ],
                )
                .is_some()
        );
        registry.teardown();
        assert_eq!(registry.state(), RegistryState::Unregistered);
        assert!(registry.families().is_empty());
        assert!(
            registry
                .counter(
                    REQUESTS_TOTAL,
                    &[
                        (LABEL_HOST, "api.vendor.com"),
                        (LABEL_METHOD, "GET"),
                        (LABEL_ROUTE, "/v1"),
                        (LABEL_STATUS, "200"),
                    ],
                )
                .is_none()
        );
    }

    #[test]
    fn the_breaker_states_report_their_recorded_codes() {
        assert_eq!(BreakerState::Closed.as_code(), 0);
        assert_eq!(BreakerState::Open.as_code(), 1);
        assert_eq!(BreakerState::HalfOpen.as_code(), 2);
        assert_eq!(BreakerState::Closed.as_str(), STATE_CLOSED);
        assert_eq!(BreakerState::Open.as_str(), STATE_OPEN);
        assert_eq!(BreakerState::HalfOpen.as_str(), STATE_HALF_OPEN);
    }

    #[test]
    fn the_connection_states_are_the_recorded_vocabulary() {
        assert_eq!(CONNECTION_IDLE, "idle");
        assert_eq!(CONNECTION_ACTIVE, "active");
        assert_eq!(CONNECTION_MAX, "max");
        assert_eq!(PHASE_GATEWAY_ADDED, "gateway_added");
        assert_eq!(PHASE_UPSTREAM, "upstream");
        assert_eq!(SelectionMethod::ExplicitHeader.as_str(), "explicit_header");
    }
}
