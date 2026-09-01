// Created: 2026-08-31 by Constructor Tech
//! OpenTelemetry instruments of the proxy data plane (DESIGN §4.2).
//!
//! The instruments are pulled from the **process-global meter provider the
//! host installs**; the gear builds no exporter and serves no scrape endpoint.
//! DESIGN §4.2 opens with "Prometheus metrics at `/metrics` (admin-only)", and
//! in this platform that route is the *host's* concern: credstore's
//! `infra/metrics.rs` is the platform precedent — it emits instruments against
//! the global provider and builds no route — and account-management does the
//! same. Emitting the instruments without the route is therefore the
//! implementation of §4.2 here, not an omission of it.
//!
//! Names are full literal Prometheus names with the suffix baked in — counters
//! end in `_total`, the duration histogram in `_seconds` — and no instrument
//! carries a unit (`add_metric_suffixes: false` is the collector posture, so
//! the suffix has to be part of the name; see credstore).
//!
//! Every emit is fire-and-forget: a meter that is not wired, or that fails, is
//! a lost data point and never a failed request.
//!
//! # Cardinality (DESIGN §4.2 "Cardinality management")
//!
//! * No tenant label anywhere: a tenant is unbounded and a label per tenant is
//!   a metric explosion the collector cannot prune.
//! * `http.route` is the **matched route's path prefix**, never the raw
//!   request path, whose segments are client input.
//! * `http.request.method` is normalized to a standard verb or `_OTHER`, the
//!   exact mapping the inbound API gateway uses, so both gateways share
//!   dashboards.
//! * `http.response.status_code` is the numeric status the *upstream* answered
//!   with. On the failure path there is no upstream answer, so the counter
//!   carries the status the gateway answered instead — the only status that
//!   exists — and `oagw_errors_total` is what identifies the request as a
//!   gateway refusal.
//! * `host` is the upstream alias, a value the control plane validated when it
//!   was written. A request the data plane cannot attribute to an upstream and
//!   a route is **not** recorded, and there are two classes of those:
//!   - the alias never resolved to an upstream (an unknown or foreign alias),
//!     where the alias the request names is client input and fabricating a
//!     host for it would both lie about an upstream and hand the label an
//!     unbounded cardinality;
//!   - the upstream resolved but no route matched (`select_route` finds
//!     nothing, every `HEAD` or `OPTIONS` among them, since the route model
//!     names no such method), which leaves the request without the
//!     `http.route` every family here is labelled with. An unregistered
//!     method — `CONNECT`, `TRACE`, an extension method — is refused by the
//!     router's fallback before it reaches the data plane and lands in the
//!     same class. Both cost nothing here; what an operator reads instead is
//!     the audit record's 404, which carries the alias, the path and the
//!     correlation id.
//! * `path` of the two rate-limit families is the matched route's prefix, for
//!   the same reason `http.route` is: the raw path is unbounded.
//!
//! # The `phase` label of the duration histogram
//!
//! DESIGN names no vocabulary. The one the data plane can honestly report is
//! **`total`** — the whole `proxy` call, from the moment the request enters it
//! to the moment its answer leaves, the guards included: a rate-limit refusal
//! is a request the data plane served, only with a shorter answer. A finer
//! phase has to be attributable without double counting, and the dial has two
//! call sites of different meaning (an ordinary request and a protocol
//! switch), so a second phase would be a judgement call per request; it is
//! deferred rather than invented.
//!
//! # What `oagw_requests_in_flight` covers
//!
//! A request is counted from the moment it is attributed to a host and a route
//! to the moment `proxy` returns its answer. What the data plane serves after
//! that — a body it keeps streaming, a bridged websocket session — is *not*
//! inside the gauge: the guard lives in the call that returns the answer,
//! because that is the one scope the pipeline can drop on every exit. The
//! instrument's description says the same thing in fewer words.
//!
//! # The two circuit-breaker families
//!
//! `oagw_circuit_breaker_state{host}` and
//! `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` are
//! emitted by [`crate::domain::proxy::breaker`], which is their only producer,
//! and they are emitted **at a transition and nowhere else**. An upstream whose
//! breaker has never moved therefore has no state series at all, rather than a
//! `closed` one: the gauge is a report of *movement*, and a dashboard that
//! needs a row per upstream should read `oagw_requests_total`, which every
//! request produces. The state values are `0` closed, `1` half-open, `2` open —
//! open is the largest because it is the state an operator alerts on.
//!
//! # Families DESIGN §4.2 declares that this module does not
//!
//! * `oagw_upstream_available{host, endpoint}` and
//!   `oagw_upstream_connections{host, state}` have no producer in this gear:
//!   there is no health probe, and the outbound client exposes no connection
//!   pool telemetry. Emitting them would be a stream of zeros, which hides a
//!   missing signal instead of reporting one.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, UpDownCounter};
use uuid::Uuid;

/// Meter / instrumentation scope name (credstore's `METER_NAME` pattern).
pub(crate) const METER_NAME: &str = "oagw";

// ── Metric names (literal Prometheus form; `add_metric_suffixes: false`) ─────
const REQUESTS: &str = "oagw_requests_total";
const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
const ERRORS: &str = "oagw_errors_total";
const RATE_LIMIT_EXCEEDED: &str = "oagw_rate_limit_exceeded_total";
const RATE_LIMIT_USAGE: &str = "oagw_rate_limit_usage_ratio";
const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
const BREAKER_STATE: &str = "oagw_circuit_breaker_state";
const BREAKER_TRANSITIONS: &str = "oagw_circuit_breaker_transitions_total";

/// Buckets of the request-duration histogram, in seconds (DESIGN §4.2).
const REQUEST_DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The only phase the duration histogram records (see the module docs).
const PHASE: &str = "total";

/// Every instrument the proxy data plane emits.
///
/// `Clone` because a forwarded body outlives the request that dialled it and
/// still has to report to the circuit breaker: the handle the body carries is a
/// clone of these. An instrument handle is cheap and holds no state, so a clone
/// is a handle, not a copy of anything measured.
#[derive(Clone)]
pub(crate) struct ProxyMetrics {
    requests: Counter<u64>,
    duration: Histogram<f64>,
    in_flight: UpDownCounter<i64>,
    errors: Counter<u64>,
    rate_limit_exceeded: Counter<u64>,
    rate_limit_usage: Gauge<f64>,
    target_host_used: Counter<u64>,
    endpoint_selected: Counter<u64>,
    breaker_state: Gauge<u64>,
    breaker_transitions: Counter<u64>,
}

impl std::fmt::Debug for ProxyMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyMetrics").finish_non_exhaustive()
    }
}

impl ProxyMetrics {
    /// Build the instrument set on `meter`.
    fn new(meter: &Meter) -> Self {
        Self {
            requests: meter
                .u64_counter(REQUESTS)
                .with_description("Proxied requests, by upstream alias, method, route and status")
                .build(),
            duration: meter
                .f64_histogram(REQUEST_DURATION)
                .with_description("Duration of a proxied request, by upstream alias and route")
                .with_boundaries(REQUEST_DURATION_BUCKETS.to_vec())
                .build(),
            in_flight: meter
                .i64_up_down_counter(REQUESTS_IN_FLIGHT)
                .with_description("Requests the data plane holds, from attribution to its answer")
                .build(),
            errors: meter
                .u64_counter(ERRORS)
                .with_description("Failed requests, by upstream alias, route and problem type")
                .build(),
            rate_limit_exceeded: meter
                .u64_counter(RATE_LIMIT_EXCEEDED)
                .with_description("Requests a rate limit refused, by upstream alias and route")
                .build(),
            rate_limit_usage: meter
                .f64_gauge(RATE_LIMIT_USAGE)
                .with_description("Bucket usage of the last rate-limit refusal, 0.0 to 1.0")
                .build(),
            target_host_used: meter
                .u64_counter(ROUTING_TARGET_HOST_USED)
                .with_description("Requests that pinned an endpoint with the target-host header")
                .build(),
            endpoint_selected: meter
                .u64_counter(ROUTING_ENDPOINT_SELECTED)
                .with_description("Endpoints dialled, by upstream and selection method")
                .build(),
            breaker_state: meter
                .u64_gauge(BREAKER_STATE)
                .with_description(
                    "State of an upstream's circuit breaker, 0 closed, 1 half-open, 2 open",
                )
                .build(),
            breaker_transitions: meter
                .u64_counter(BREAKER_TRANSITIONS)
                .with_description(
                    "Circuit breaker transitions, by upstream alias and the two states",
                )
                .build(),
        }
    }

    /// Build a handle bound to the process-global meter provider.
    #[must_use]
    pub(crate) fn from_global() -> Self {
        let scope = opentelemetry::InstrumentationScope::builder(METER_NAME).build();
        Self::new(&opentelemetry::global::meter_with_scope(scope))
    }

    /// Count one request that resolved to an upstream and a route.
    ///
    /// `method` is [`normalize_method`]'s output, whose `&'static str` the
    /// attribute borrows instead of copying.
    pub(crate) fn request(&self, host: &str, method: &'static str, route: &str, status: u16) {
        self.requests.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.request.method", method),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
    }

    /// Record how long the data plane held one request.
    pub(crate) fn duration(&self, host: &str, route: &str, seconds: f64) {
        self.duration.record(
            seconds,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", PHASE),
            ],
        );
    }

    /// Count a request as in flight until the returned guard is dropped.
    ///
    /// The guard is what keeps the gauge honest on every exit, the error paths
    /// included: a request that panics or is refused still leaves the pool
    /// exactly once.
    pub(crate) fn in_flight(&self, host: &str) -> InFlightGuard {
        let host = KeyValue::new("host", host.to_owned());
        self.in_flight.add(1, std::slice::from_ref(&host));
        InFlightGuard {
            counter: self.in_flight.clone(),
            host,
        }
    }

    /// Count one failure, by the problem type the client was answered with.
    pub(crate) fn error(&self, host: &str, route: &str, error_type: &str) {
        self.errors.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("error_type", error_type.to_owned()),
            ],
        );
    }

    /// Count one request a rate limit refused.
    pub(crate) fn rate_limit_exceeded(&self, host: &str, path: &str) {
        self.rate_limit_exceeded.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    /// Record the bucket usage a refusal saw, `0.0` to `1.0`.
    pub(crate) fn rate_limit_usage(&self, host: &str, path: &str, ratio: f64) {
        self.rate_limit_usage.record(
            ratio,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    /// Count the endpoint a request dials, and how it was chosen.
    pub(crate) fn endpoint_selected(
        &self,
        upstream: Uuid,
        endpoint_host: &str,
        method: &'static str,
    ) {
        self.endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream.to_string()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", method),
            ],
        );
    }

    /// Publish the state an upstream's breaker moved to (DESIGN §4.2).
    ///
    /// Only a transition publishes: a breaker that never moved has no series,
    /// which the module docs note.
    pub(crate) fn breaker_state(&self, host: &str, value: u64) {
        self.breaker_state.record(
            value,
            std::slice::from_ref(&KeyValue::new("host", host.to_owned())),
        );
    }

    /// Count one transition of an upstream's breaker (DESIGN §4.2).
    pub(crate) fn breaker_transition(&self, host: &str, from_state: &str, to_state: &str) {
        self.breaker_transitions.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("from_state", from_state.to_owned()),
                KeyValue::new("to_state", to_state.to_owned()),
            ],
        );
    }

    /// Count a request that pinned its endpoint with the target-host header.
    pub(crate) fn target_host_used(&self, upstream: Uuid, endpoint_host: &str) {
        self.target_host_used.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream.to_string()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
            ],
        );
    }
}

/// Drop guard of the in-flight gauge.
///
/// `+1` is taken when the request is attributed to a host and `-1` happens
/// here, which is the only way to cover every exit of a pipeline that can
/// return, be refused, panic or be cancelled.
pub(crate) struct InFlightGuard {
    counter: UpDownCounter<i64>,
    host: KeyValue,
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard").finish_non_exhaustive()
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.add(-1, std::slice::from_ref(&self.host));
    }
}

/// Normalize the HTTP method per the `OTel` semantic conventions.
///
/// Unknown methods become `_OTHER`: a method string is client input, and a
/// label per spelling is a metric explosion. The mapping is the inbound API
/// gateway's `normalize_method`, so both gateways share dashboards.
#[must_use]
pub(crate) fn normalize_method(method: &http::Method) -> &'static str {
    match *method {
        http::Method::GET => "GET",
        http::Method::POST => "POST",
        http::Method::PUT => "PUT",
        http::Method::DELETE => "DELETE",
        http::Method::PATCH => "PATCH",
        http::Method::HEAD => "HEAD",
        http::Method::OPTIONS => "OPTIONS",
        http::Method::CONNECT => "CONNECT",
        http::Method::TRACE => "TRACE",
        _ => "_OTHER",
    }
}

#[cfg(test)]
mod tests {
    use super::{METER_NAME, normalize_method};

    #[test]
    fn the_meter_scope_is_the_gear_name() {
        assert_eq!(METER_NAME, "oagw");
    }

    #[test]
    fn standard_verbs_keep_their_spelling() {
        for (method, expected) in [
            ("GET", "GET"),
            ("POST", "POST"),
            ("PUT", "PUT"),
            ("DELETE", "DELETE"),
            ("PATCH", "PATCH"),
            ("HEAD", "HEAD"),
            ("OPTIONS", "OPTIONS"),
            ("CONNECT", "CONNECT"),
            ("TRACE", "TRACE"),
        ] {
            assert_eq!(
                normalize_method(&method.parse::<http::Method>().unwrap()),
                expected
            );
        }
    }

    #[test]
    fn an_unknown_verb_becomes_other() {
        assert_eq!(
            normalize_method(&"BREW".parse::<http::Method>().unwrap()),
            "_OTHER"
        );
    }
}
