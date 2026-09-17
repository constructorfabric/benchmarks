//! Proxy metrics as OpenTelemetry instruments (DESIGN §4.2).
//!
//! [`OagwMetrics`] is the one handle that owns the instruments the design names
//! for this slice. It is built from a [`Meter`] the caller *hands over*
//! ([`OagwMetrics::new`]), or from the process-global provider under an
//! instrumentation scope named for this gear ([`OagwMetrics::from_global`]); it
//! never looks a meter up on its own in the first case, which is what makes the
//! instrument set assertable against an in-memory exporter.
//!
//! The gear is never a meter provider: the *host* owns the SDK `Pipeline` (and
//! the `/metrics` endpoint, which DESIGN §4.2 puts behind the admin API). With
//! no provider installed the global meter is a no-op, so the default a
//! [`DataPlaneService`](crate::domain::services::data_plane::DataPlaneService)
//! builds costs nothing and is correct in production.
//!
//! # Instruments
//!
//! | instrument | kind | labels |
//! |---|---|---|
//! | `oagw_requests_total` | `u64` counter | `host`, `http.request.method`, `http.route`, `http.response.status_code` |
//! | `oagw_request_duration_seconds` | `f64` histogram (`s`) | `host`, `http.route`, `phase` |
//! | `oagw_requests_in_flight` | `i64` up-down counter | `host` |
//! | `oagw_errors_total` | `u64` counter | `host`, `http.route`, `error_type` |
//! | `oagw_rate_limit_exceeded_total` | `u64` counter | `host`, `path` |
//! | `oagw_rate_limit_usage_ratio` | `f64` gauge | `host`, `path` |
//! | `oagw_routing_target_host_used` | `u64` counter | `upstream_id`, `endpoint_host` |
//! | `oagw_routing_endpoint_selected` | `u64` counter | `upstream_id`, `endpoint_host`, `selection_method` |
//!
//! `oagw_requests_total` is written **once per request, on the final outcome**:
//! a gateway error contributes its own status, an upstream response its own —
//! an upstream `503` is therefore indistinguishable from the gateway's by
//! status alone, which is what `X-OAGW-Error-Source` and
//! [`OagwMetrics::error`] are for.
//!
//! # Label semantics (DESIGN §4.2 "Cardinality management")
//!
//! No label below may carry tenant identity, a raw request path, a header
//! value, a query string or a credential. Each is bounded by configuration:
//!
//! * `host` — the **upstream alias**. OAGW-specific label, shared vocabulary
//!   with the inbound API Gateway. It is the *resolved* alias when the request
//!   reached alias resolution, and the fixed sentinel
//!   [`OagwMetrics::UNRESOLVED_HOST`] (the empty string, the same sentinel
//!   `http.route = ""` uses below) before that. The alias *as the caller
//!   requested it* is deliberately never a label: a caller that sends a
//!   different unknown alias per request would mint a new time series on every
//!   instrument it touches, which is exactly the §4.2 cardinality bug this rule
//!   exists to prevent. Nothing is lost for debugging — the requested alias is
//!   a *log field* (`alias`) of the §4.3 audit record, not a label.
//! * `http.route` — the **matched route's configured path pattern** (the
//!   `path` of the route's `HttpMatch`), never the raw request path. A pattern
//!   is a configured string, so `/v1/models/123` and `/v1/models/456` land on
//!   one label value. When **nothing matched** the record still lands, labelled
//!   `http.route = ""`: a request the router rejected is exactly the one an
//!   operator needs to see counted.
//! * `http.request.method` — a standard verb or `_OTHER`
//!   ([`normalize_method`]), so a caller inventing methods cannot grow the
//!   label set.
//! * `http.response.status_code` — the numeric status, so `5xx` rates stay a
//!   query-time aggregation (DESIGN §4.2).
//! * `phase` — one of `upstream` / `total`; see below.
//! * `error_type` — the error catalogue's GTS type id
//!   ([`OagwErrorKind::gts_type_id`](crate::error::OagwErrorKind)), a fixed
//!   vocabulary.
//! * `path` on the two rate-limit instruments — the **matched route's pattern**,
//!   for the same reason as `http.route`: the raw request path is unbounded
//!   cardinality, which is exactly the bug the §4.2 cardinality rule exists to
//!   prevent. A route is always matched before the rate limit runs, so the
//!   pattern is always available there.
//! * `upstream_id` / `endpoint_host` / `selection_method` — all three come from
//!   configuration, never from the request.
//!
//! # `phase`: an interpretation of §4.2
//!
//! The design names the `phase` label but not its values. Two are recorded, and
//! no others are invented:
//!
//! * `phase = "upstream"` — the leg [`proxy_timeout_secs`](crate::config::OagwConfig)
//!   bounds: connect, send, and time to first byte. Measured around the
//!   outbound call, not around response rendering, so a slow *plugin* or a slow
//!   *guard* cannot be mistaken for a slow upstream.
//! * `phase = "total"` — the whole request, from proxy entry to the final
//!   outcome (gateway error or forwarded response), measured by the wrapper
//!   that also writes the audit record.
//!
//! Both are recorded for every request, so `total - upstream` is the gateway's
//! own overhead on the same request.
//!
//! # Rate-limit usage ratio
//!
//! [`rate_limit_usage_ratio`] turns a decision's numeric capacity and the
//! tokens it has left into the `0.0..=1.0` gauge value §4.2 describes.
//! Decisions that carry no numeric capacity (`limit_value = None`) are not
//! recorded at all rather than guessed at.
//!
//! # Declared subset, and what is out of scope
//!
//! DESIGN §4.2 also names `oagw_circuit_breaker_state`,
//! `oagw_circuit_breaker_transitions_total`, `oagw_upstream_available` and
//! `oagw_upstream_connections`. They are **out of scope here**: there is no
//! circuit breaker in this slice (DESIGN §4.7 "\[Core] Circuit breaker" is
//! listed as future work), and the outbound client exposes no connection-pool
//! introspection to read `oagw_upstream_connections` from. They are not stubbed
//! — an instrument that is declared but never written is a dashboard that lies.
//!
//! # Histogram buckets: a declared deviation
//!
//! §4.2 wants the request-duration boundaries
//! `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]`
//! seconds. In OpenTelemetry 0.32 bucket boundaries are a **view** on the SDK
//! `Pipeline`, i.e. a property of the provider the *host* installs: a gear
//! cannot force a view onto a provider it does not own, and this gear does not
//! build one. The histogram is therefore recorded in seconds
//! (`.with_unit("s")`), its description names the intended boundaries, and
//! [`REQUEST_DURATION_BUCKETS_SECONDS`] is the value a host view should
//! configure. Nothing in this crate sets a view.
//!
//! # Fail-closed posture
//!
//! Metrics are best-effort by construction and cannot fail a request: the
//! instruments are built once, every recording method is infallible, and
//! nothing here reads a request or response body, a header value, a query
//! string or a credential.

use std::time::Duration;

use http::Method;
use opentelemetry::InstrumentationScope;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, UpDownCounter};
use uuid::Uuid;

/// Instrumentation scope of the meter a default-constructed data plane uses.
///
/// With no provider installed the global meter is a no-op, so this default
/// exists so that [`DataPlaneService::new`](crate::domain::services::data_plane::DataPlaneService::new)
/// is always usable. It names the gear, as platform convention does
/// (`credstore` names its meter after the gear too), so a host that constructs
/// the service itself reports the instruments under the *same* scope the gear
/// installs — one instrument name, one scope.
pub const DEFAULT_METER_SCOPE: &str = "oagw";

// --- Instrument names: literal Prometheus names, as §4.2 spells them. --------
const REQUESTS_TOTAL: &str = "oagw_requests_total";
const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
const ERRORS_TOTAL: &str = "oagw_errors_total";
const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_rate_limit_exceeded_total";
const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";

// --- Label keys, exactly as §4.2 spells them. --------------------------------
const ATTR_HOST: &str = "host";
const ATTR_METHOD: &str = "http.request.method";
const ATTR_ROUTE: &str = "http.route";
const ATTR_STATUS: &str = "http.response.status_code";
const ATTR_PHASE: &str = "phase";
const ATTR_ERROR_TYPE: &str = "error_type";
const ATTR_PATH: &str = "path";
const ATTR_UPSTREAM_ID: &str = "upstream_id";
const ATTR_ENDPOINT_HOST: &str = "endpoint_host";
const ATTR_SELECTION_METHOD: &str = "selection_method";

/// The request-duration histogram boundaries §4.2 asks for, in seconds.
///
/// Not installed from inside the gear — see the module documentation for why
/// the host's view owns them. [`OagwMetrics`] carries the list so the value the
/// host should configure is written down next to the instrument it belongs to.
pub const REQUEST_DURATION_BUCKETS_SECONDS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The label value of [`REQUEST_DURATION`]'s intended boundaries.
const REQUEST_DURATION_DESCRIPTION: &str = "Duration of proxied requests, by phase. Intended \
     histogram boundaries (DESIGN §4.2; configured as a view on the host's meter provider): \
     [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0] seconds";

/// The two phases [`OagwMetrics::request_duration`] distinguishes.
///
/// An interpretation of §4.2's `phase` label — see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPhase {
    /// The leg `proxy_timeout_secs` bounds, measured around the outbound call in
    /// `DataPlaneService::forward`: connect + send + time to the upstream's
    /// first byte. Plugin, guard and routing time is recorded in
    /// `phase = "total"`, never here.
    Upstream,
    /// The whole request, in the `proxy`/`proxy_upgrade` wrappers.
    Total,
}

impl RequestPhase {
    /// The label value the phase is recorded under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Total => "total",
        }
    }
}

/// How the proxy picked the endpoint it forwarded to.
///
/// The vocabulary of `oagw_routing_endpoint_selected`'s `selection_method`
/// label, exactly as §4.2 lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSelectionMethod {
    /// The caller's `X-OAGW-Target-Host` decided.
    ExplicitHeader,
    /// The upstream has several endpoints and the process-wide round-robin
    /// counter decided.
    RoundRobin,
    /// The upstream has exactly one endpoint, so there was nothing to choose.
    Default,
}

impl EndpointSelectionMethod {
    /// The label value the method is recorded under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// The OpenTelemetry instruments of the proxy (DESIGN §4.2).
///
/// `Clone` because [`DataPlaneService`](crate::domain::services::data_plane::DataPlaneService)
/// is a cheap `Arc`-heavy handle: the instruments are reference-counted handles
/// on the meter, so cloning is as cheap as the service is.
#[derive(Clone)]
pub struct OagwMetrics {
    requests: Counter<u64>,
    duration: Histogram<f64>,
    in_flight: UpDownCounter<i64>,
    errors: Counter<u64>,
    rate_limit_exceeded: Counter<u64>,
    rate_limit_usage: Gauge<f64>,
    routing_target_host_used: Counter<u64>,
    routing_endpoint_selected: Counter<u64>,
}

impl std::fmt::Debug for OagwMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The instruments are opaque handles; their names are the interesting
        // part, and they are documented on the module.
        formatter
            .debug_struct("OagwMetrics")
            .finish_non_exhaustive()
    }
}

impl OagwMetrics {
    /// The `host` label of a request that has not reached alias resolution.
    ///
    /// A fixed value rather than the alias the caller asked for: the alias on
    /// the proxy path is request data, so it is unbounded cardinality and would
    /// let one caller mint a new time series per request (DESIGN §4.2
    /// "Cardinality management"). It is the same sentinel `http.route` uses for
    /// a request nothing matched. The requested alias is still written down —
    /// as the §4.3 audit record's `alias` *log field*, never as a label.
    pub const UNRESOLVED_HOST: &str = "";

    /// Build the instrument set from the supplied meter.
    ///
    /// Handing the meter in (rather than looking it up here) is what makes this
    /// assertable: a test builds its own provider, hands its meter over, and
    /// reads the aggregated data back.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests: meter
                .u64_counter(REQUESTS_TOTAL)
                .with_description(
                    "Proxied requests by upstream alias, method, matched route pattern and final \
                     status",
                )
                .build(),
            duration: meter
                .f64_histogram(REQUEST_DURATION)
                .with_description(REQUEST_DURATION_DESCRIPTION)
                .with_unit("s")
                .build(),
            in_flight: meter
                .i64_up_down_counter(REQUESTS_IN_FLIGHT)
                .with_description(
                    "Proxied requests currently in flight, by upstream alias. Counted from \
                     proxy entry until the response is handed back; a streamed body or a \
                     spliced connection is not counted",
                )
                .build(),
            errors: meter
                .u64_counter(ERRORS_TOTAL)
                .with_description(
                    "Gateway-side rejections, by upstream alias, route pattern and \
                     error type",
                )
                .build(),
            rate_limit_exceeded: meter
                .u64_counter(RATE_LIMIT_EXCEEDED_TOTAL)
                .with_description(
                    "Requests a rate limit rejected, by upstream alias and route \
                     pattern",
                )
                .build(),
            rate_limit_usage: meter
                .f64_gauge(RATE_LIMIT_USAGE_RATIO)
                .with_description(
                    "Fraction of the rate-limit bucket a request consumed (0.0 to 1.0), by \
                     upstream alias and route pattern",
                )
                .build(),
            routing_target_host_used: meter
                .u64_counter(ROUTING_TARGET_HOST_USED)
                .with_description("Requests that pinned an endpoint with X-OAGW-Target-Host")
                .build(),
            routing_endpoint_selected: meter
                .u64_counter(ROUTING_ENDPOINT_SELECTED)
                .with_description(
                    "Endpoint selections, by upstream, endpoint and how it was \
                     chosen",
                )
                .build(),
        }
    }

    /// Build a handle bound to the process-global meter provider, under an
    /// instrumentation scope named `scope`.
    ///
    /// Without a provider installed this is a no-op handle that costs nothing.
    #[must_use]
    pub fn from_global(scope: &str) -> Self {
        let scope = InstrumentationScope::builder(scope.to_owned()).build();
        Self::new(&opentelemetry::global::meter_with_scope(scope))
    }

    /// `oagw_requests_total`: one request reached its final outcome.
    pub fn request_outcome(&self, host: &str, method: &str, route: &str, status: u16) {
        self.requests.add(
            1,
            &[
                KeyValue::new(ATTR_HOST, host.to_owned()),
                KeyValue::new(ATTR_METHOD, normalize_method_str(method)),
                KeyValue::new(ATTR_ROUTE, route.to_owned()),
                KeyValue::new(ATTR_STATUS, i64::from(status)),
            ],
        );
    }

    /// `oagw_request_duration_seconds`: one phase of one request.
    pub fn request_duration(
        &self,
        host: &str,
        route: &str,
        phase: RequestPhase,
        elapsed: Duration,
    ) {
        self.duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new(ATTR_HOST, host.to_owned()),
                KeyValue::new(ATTR_ROUTE, route.to_owned()),
                KeyValue::new(ATTR_PHASE, phase.as_str()),
            ],
        );
    }

    /// `oagw_requests_in_flight`: take one unit of the gauge.
    ///
    /// The gauge covers a request from proxy entry until the response is handed
    /// back; a streamed body or a spliced connection is not counted (see
    /// [`crate::streaming`]).
    ///
    /// Returns the guard that gives the unit back: hold it for the request, and
    /// a panic anywhere in it still cannot leave the gauge above where it
    /// started (the decrement is in [`Drop`]).
    #[must_use = "the returned guard is what returns the in-flight unit; discarding it unbound \
         drops it immediately and the gauge never rises"]
    pub fn in_flight(&self, host: &str) -> InFlightGuard {
        let attribute = KeyValue::new(ATTR_HOST, host.to_owned());
        self.in_flight.add(1, std::slice::from_ref(&attribute));
        InFlightGuard {
            counter: self.in_flight.clone(),
            attribute,
        }
    }

    /// `oagw_errors_total`: one gateway-side rejection.
    ///
    /// `error_type` is a GTS type id, which is a `&'static str` — the one
    /// bounded vocabulary in the label set, and the reason no allocation is
    /// needed for it.
    pub fn error(&self, host: &str, route: &str, error_type: &'static str) {
        self.errors.add(
            1,
            &[
                KeyValue::new(ATTR_HOST, host.to_owned()),
                KeyValue::new(ATTR_ROUTE, route.to_owned()),
                KeyValue::new(ATTR_ERROR_TYPE, error_type),
            ],
        );
    }

    /// `oagw_rate_limit_exceeded_total`: one rejected rate-limit decision.
    pub fn rate_limit_exceeded(&self, host: &str, path: &str) {
        self.rate_limit_exceeded.add(
            1,
            &[
                KeyValue::new(ATTR_HOST, host.to_owned()),
                KeyValue::new(ATTR_PATH, path.to_owned()),
            ],
        );
    }

    /// `oagw_rate_limit_usage_ratio`: how full the caller's bucket was.
    ///
    /// `ratio` must be in `0.0..=1.0`; [`rate_limit_usage_ratio`] is how a
    /// decision is turned into one.
    pub fn rate_limit_usage(&self, host: &str, path: &str, ratio: f64) {
        self.rate_limit_usage.record(
            ratio,
            &[
                KeyValue::new(ATTR_HOST, host.to_owned()),
                KeyValue::new(ATTR_PATH, path.to_owned()),
            ],
        );
    }

    /// `oagw_routing_target_host_used`: the caller pinned the endpoint.
    ///
    /// Recorded only once the pin has *succeeded*, so `endpoint_host` is always
    /// a configured host and never a caller-supplied string (an unparsable
    /// `X-OAGW-Target-Host` is counted by [`Self::error`] instead, under its
    /// own error type).
    pub fn routing_target_host_used(&self, upstream_id: Uuid, endpoint_host: &str) {
        self.routing_target_host_used.add(
            1,
            &[
                KeyValue::new(ATTR_UPSTREAM_ID, upstream_id.to_string()),
                KeyValue::new(ATTR_ENDPOINT_HOST, endpoint_host.to_owned()),
            ],
        );
    }

    /// `oagw_routing_endpoint_selected`: one endpoint was chosen.
    pub fn routing_endpoint_selected(
        &self,
        upstream_id: Uuid,
        endpoint_host: &str,
        method: EndpointSelectionMethod,
    ) {
        self.routing_endpoint_selected.add(
            1,
            &[
                KeyValue::new(ATTR_UPSTREAM_ID, upstream_id.to_string()),
                KeyValue::new(ATTR_ENDPOINT_HOST, endpoint_host.to_owned()),
                KeyValue::new(ATTR_SELECTION_METHOD, method.as_str()),
            ],
        );
    }
}

/// Returns the in-flight unit to the gauge ([`OagwMetrics::in_flight`]).
///
/// A drop guard rather than a plain `add(-1, …)` on every exit path: a panic
/// unwinding through the proxy must not leak a unit, because a gauge that only
/// ever ratchets up is worse than no gauge at all.
#[derive(Debug)]
pub struct InFlightGuard {
    counter: UpDownCounter<i64>,
    attribute: KeyValue,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.add(-1, std::slice::from_ref(&self.attribute));
    }
}

/// Normalize an HTTP method per the OTel HTTP semantic conventions.
///
/// Unknown methods map to `_OTHER` to bound the label set: a caller that
/// invents a method per request cannot grow `oagw_requests_total` without
/// bound, which is the §4.2 cardinality rule applied to the one label a caller
/// can influence.
#[must_use]
pub fn normalize_method(method: &Method) -> &'static str {
    normalize_method_str(method.as_str())
}

/// [`normalize_method`] over a method *token*.
///
/// [`http::Method::as_str`] is the canonical uppercase token, so comparing
/// against those tokens is exact; this is the form the recorded label needs,
/// because the wrapper has already copied the method out of the request.
#[must_use]
pub fn normalize_method_str(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "CONNECT" => "CONNECT",
        "TRACE" => "TRACE",
        _ => "_OTHER",
    }
}

/// The `oagw_rate_limit_usage_ratio` value of one rate-limit decision.
///
/// `limit_value` is the bucket's numeric capacity (`None` = unknown: nothing is
/// recorded rather than a guess), `remaining` the tokens the decision reports
/// left in it. The ratio is of the bucket **consumed**, clamped into
/// `0.0..=1.0` so a decision that races a refill can never push the gauge out
/// of the range §4.2 promises.
///
/// A capacity of zero is reported as exhausted (`1.0`): a bucket that can hold
/// nothing serves nothing. A `cost` above the bucket's capacity reads as `0.0`:
/// the bucket is full and nothing is ever consumed; check
/// `oagw_rate_limit_exceeded_total` for that case.
#[must_use]
pub fn rate_limit_usage_ratio(limit_value: Option<u64>, remaining: u64) -> Option<f64> {
    let capacity = limit_value?;
    if capacity == 0 {
        return Some(1.0);
    }
    let used = capacity.saturating_sub(remaining);
    Some((used as f64 / capacity as f64).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_standard_verb_is_normalized_to_itself() {
        for (verb, expected) in [
            (Method::GET, "GET"),
            (Method::POST, "POST"),
            (Method::PUT, "PUT"),
            (Method::DELETE, "DELETE"),
            (Method::PATCH, "PATCH"),
            (Method::HEAD, "HEAD"),
            (Method::OPTIONS, "OPTIONS"),
            (Method::CONNECT, "CONNECT"),
            (Method::TRACE, "TRACE"),
        ] {
            assert_eq!(normalize_method(&verb), expected, "{verb}");
        }
    }

    #[test]
    fn an_unknown_method_becomes_other_rather_than_growing_the_label_set() {
        let invented = Method::from_bytes(b"PROPFIND").expect("a token is a method");
        assert_eq!(normalize_method(&invented), "_OTHER");
        assert_eq!(
            normalize_method(&Method::from_bytes(b"X-CUSTOM").expect("a token")),
            "_OTHER"
        );
    }

    #[test]
    fn normalizing_a_method_and_its_token_agree() {
        for verb in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::HEAD,
            Method::OPTIONS,
            Method::CONNECT,
            Method::TRACE,
        ] {
            assert_eq!(normalize_method(&verb), normalize_method_str(verb.as_str()));
        }
        let invented = Method::from_bytes(b"PROPFIND").expect("a token is a method");
        assert_eq!(
            normalize_method(&invented),
            normalize_method_str(invented.as_str())
        );
    }

    #[test]
    fn the_duration_buckets_are_the_ones_the_design_names() {
        assert_eq!(
            REQUEST_DURATION_BUCKETS_SECONDS,
            [
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0
            ]
        );
        // The instrument's description names them, because the gear cannot
        // install the view itself.
        for bucket in REQUEST_DURATION_BUCKETS_SECONDS {
            assert!(
                REQUEST_DURATION_DESCRIPTION.contains(&format!("{bucket}")),
                "the description must name {bucket}"
            );
        }
    }

    #[test]
    fn the_usage_ratio_is_the_consumed_share_of_the_bucket() {
        assert_eq!(rate_limit_usage_ratio(Some(10), 10), Some(0.0));
        assert_eq!(rate_limit_usage_ratio(Some(10), 5), Some(0.5));
        assert_eq!(rate_limit_usage_ratio(Some(10), 0), Some(1.0));
        assert_eq!(rate_limit_usage_ratio(Some(1), 0), Some(1.0));
        assert_eq!(rate_limit_usage_ratio(Some(0), 0), Some(1.0));
    }

    #[test]
    fn a_decision_without_a_numeric_capacity_is_not_recorded() {
        assert_eq!(rate_limit_usage_ratio(None, 0), None);
    }

    #[test]
    fn a_decision_that_races_a_refill_is_clamped_into_the_gauge_range() {
        let ratio = rate_limit_usage_ratio(Some(4), 9).expect("capacity is known");
        assert!((0.0..=1.0).contains(&ratio), "{ratio}");
    }

    #[test]
    fn the_phase_labels_are_the_two_values_the_module_documents() {
        assert_eq!(RequestPhase::Upstream.as_str(), "upstream");
        assert_eq!(RequestPhase::Total.as_str(), "total");
    }

    #[test]
    fn the_unresolved_host_is_the_empty_string_the_route_sentinel_uses() {
        // The sentinel is a label value a caller can observe in the exported
        // series, so it is pinned to the one documented value: the same empty
        // string `http.route` uses for a request nothing matched.
        assert_eq!(OagwMetrics::UNRESOLVED_HOST, "");
    }

    #[test]
    fn the_selection_method_labels_are_the_three_values_the_design_lists() {
        assert_eq!(
            EndpointSelectionMethod::ExplicitHeader.as_str(),
            "explicit_header"
        );
        assert_eq!(EndpointSelectionMethod::RoundRobin.as_str(), "round_robin");
        assert_eq!(EndpointSelectionMethod::Default.as_str(), "default");
    }
}
