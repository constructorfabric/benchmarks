//! Prometheus metrics for the data plane (DESIGN §4.2 "Metrics and
//! Observability").
//!
//! The gear carries no `prometheus` dependency, so the registry keeps plain
//! atomic state and renders the Prometheus 004 exposition format itself. All
//! label keys follow the `OTel` HTTP semantic conventions named in the design
//! and no tenant identifier is ever used as a label: cardinality is bounded by
//! the number of aliases and routes configured in the calling tenants.
//!
//! Review evidence (privilege boundary — observability):
//! * Guardrail: DESIGN §4.2 "Cardinality management" — no tenant labels,
//!   `http.route` is the matched pattern, method is normalised to a standard
//!   verb or `_OTHER`.
//! * Rationale: a raw path, a query string or a header value as a label would
//!   let a caller blow up the metric cardinality and would leak request data
//!   into a monitoring system.
//! * Validation performed: `metrics_*` tests assert the rendered series names,
//!   the label sets and the histogram bucket boundaries.

use std::collections::HashMap;
use std::sync::Mutex;

/// Metric exposed at `/api/oagw/v1/metrics`.
pub const REQUESTS_TOTAL: &str = "oagw_requests_total";
/// Total time spent serving a proxied request, by phase.
pub const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
/// Requests currently being proxied.
pub const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
/// Gateway-generated failures, by error type.
pub const ERRORS_TOTAL: &str = "oagw_errors_total";
/// Target-host selections, by selection method.
pub const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
/// Requests rejected by a rate limiter.
pub const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_rate_limit_exceeded_total";
/// Requests still counted by a limiter of this alias.
pub const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";

/// Request-duration histogram buckets in seconds (DESIGN §4.2).
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// One counter/gauge series identified by its metric name and ordered labels.
#[derive(Debug, Default)]
struct Series {
    values: Mutex<HashMap<String, u64>>,
}

impl Series {
    fn add(&self, labels: &str, delta: u64) {
        let mut values = self
            .values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *values.entry(labels.to_owned()).or_insert(0) += delta;
    }

}

/// One histogram series, bucketed by [`DURATION_BUCKETS`].
#[derive(Debug, Default)]
struct Histogram {
    counts: Mutex<HashMap<String, [u64; DURATION_BUCKETS.len()]>>,
    sums: Mutex<HashMap<String, f64>>,
    totals: Mutex<HashMap<String, u64>>,
}

impl Histogram {
    /// Records one observation against its label set.
    fn observe(&self, labels: &str, seconds: f64) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = counts
            .entry(labels.to_owned())
            .or_insert([0; DURATION_BUCKETS.len()]);
        for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                entry[index] += 1;
            }
        }
        drop(counts);

        let mut sums = self
            .sums
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *sums.entry(labels.to_owned()).or_insert(0.0) += seconds;

        let mut totals = self
            .totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *totals.entry(labels.to_owned()).or_insert(0) += 1;
    }
}

/// Registry of the data-plane metrics of one process.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    requests_total: Series,
    durations: Histogram,
    in_flight: Mutex<u64>,
    errors_total: Series,
    routing_selected: Series,
    rate_limited: Series,
    rate_usage: Mutex<HashMap<String, f64>>,
}

impl MetricsRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one completed proxied request.
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16) {
        self.requests_total.add(
            &format!(
                "{{host=\"{host}\",http_request_method=\"{method}\",http_route=\"{route}\",http_response_status_code=\"{status}\"}}"
            ),
            1,
        );
    }

    /// Records the duration of one phase of a proxied request.
    pub fn observe_duration(&self, host: &str, route: &str, phase: &str, seconds: f64) {
        let labels = format!("{{host=\"{host}\",http_route=\"{route}\",phase=\"{phase}\"}}");
        self.durations.observe(&labels, seconds);
    }

    /// Marks a request as in flight.
    pub fn begin_request(&self) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight += 1;
    }

    /// Marks a proxied request as complete.
    pub fn end_request(&self) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight = in_flight.saturating_sub(1);
    }

    /// Records a gateway-generated failure.
    pub fn record_error(&self, host: &str, route: &str, error_type: &str) {
        self.errors_total.add(
            &format!("{{host=\"{host}\",http_route=\"{route}\",error_type=\"{error_type}\"}}"),
            1,
        );
    }

    /// Records which endpoint of an upstream pool served the request.
    pub fn record_endpoint_selection(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection_method: &str,
    ) {
        self.routing_selected.add(
            &format!(
                "{{upstream_id=\"{upstream_id}\",endpoint_host=\"{endpoint_host}\",selection_method=\"{selection_method}\"}}"
            ),
            1,
        );
    }

    /// Records a rate-limit rejection.
    pub fn record_rate_limit_exceeded(&self, host: &str, route: &str) {
        self.rate_limited
            .add(&format!("{{host=\"{host}\",path=\"{route}\"}}"), 1);
    }

    /// Publishes the remaining quota of a limiter as a ratio in `0.0..=1.0`.
    pub fn record_rate_usage(&self, host: &str, route: &str, ratio: f64) {
        let clamped = ratio.clamp(0.0, 1.0);
        let mut usage = self
            .rate_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        usage.insert(format!("{{host=\"{host}\",path=\"{route}\"}}"), clamped);
    }

    /// Renders the Prometheus 004 exposition.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        Self::render_counter(
            &mut out,
            REQUESTS_TOTAL,
            "Total proxied requests",
            &self.requests_total,
        );
        self.render_in_flight(&mut out);
        self.render_errors(&mut out);
        self.render_routing(&mut out);
        self.render_rate_limit(&mut out);
        self.render_durations(&mut out);
        out
    }

    fn render_counter(out: &mut String, name: &str, help: &str, series: &Series) {
        let values = series
            .values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if values.is_empty() {
            return;
        }
        out.push_str("# HELP ");
        out.push_str(name);
        out.push(' ');
        out.push_str(help);
        out.push_str("\n# TYPE ");
        out.push_str(name);
        out.push_str(" counter\n");
        for (labels, value) in values.iter() {
            out.push_str(name);
            out.push_str(labels);
            out.push(' ');
            out.push_str(&value.to_string());
            out.push('\n');
        }
    }

    fn render_in_flight(&self, out: &mut String) {
        let in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        out.push_str("# HELP ");
        out.push_str(REQUESTS_IN_FLIGHT);
        out.push_str(" Requests currently being proxied\n# TYPE ");
        out.push_str(REQUESTS_IN_FLIGHT);
        out.push_str(" gauge\n");
        out.push_str(REQUESTS_IN_FLIGHT);
        out.push(' ');
        out.push_str(&in_flight.to_string());
        out.push('\n');
    }

    fn render_errors(&self, out: &mut String) {
        Self::render_counter(
            out,
            ERRORS_TOTAL,
            "Gateway errors by type",
            &self.errors_total,
        );
    }

    fn render_routing(&self, out: &mut String) {
        Self::render_counter(
            out,
            ROUTING_ENDPOINT_SELECTED,
            "Endpoint selections by method",
            &self.routing_selected,
        );
    }

    fn render_rate_limit(&self, out: &mut String) {
        Self::render_counter(
            out,
            RATE_LIMIT_EXCEEDED_TOTAL,
            "Requests rejected by a rate limiter",
            &self.rate_limited,
        );
        let usage = self
            .rate_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if usage.is_empty() {
            return;
        }
        out.push_str("# HELP ");
        out.push_str(RATE_LIMIT_USAGE_RATIO);
        out.push_str(" Remaining quota of a limiter as a ratio\n# TYPE ");
        out.push_str(RATE_LIMIT_USAGE_RATIO);
        out.push_str(" gauge\n");
        for (labels, ratio) in usage.iter() {
            out.push_str(RATE_LIMIT_USAGE_RATIO);
            out.push_str(labels);
            out.push(' ');
            out.push_str(&render_float(*ratio));
            out.push('\n');
        }
    }

    fn render_durations(&self, out: &mut String) {
        let counts = self
            .durations
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if counts.is_empty() {
            return;
        }
        out.push_str("# HELP ");
        out.push_str(REQUEST_DURATION);
        out.push_str(" Duration of proxied requests by phase\n# TYPE ");
        out.push_str(REQUEST_DURATION);
        out.push_str(" histogram\n");
        // Labels are stored as `{a="x",b="y"}`; the cumulative `le` label of a
        // bucket is appended inside the same brace group.
        for (labels, buckets) in counts.iter() {
            let head = &labels[..labels.len() - 1];
            for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
                out.push_str(REQUEST_DURATION);
                out.push_str("_bucket");
                out.push_str(head);
                out.push_str(",le=\"");
                out.push_str(&bound.to_string());
                out.push_str("\"} ");
                out.push_str(&buckets[index].to_string());
                out.push('\n');
            }
        }
        drop(counts);

        let sums = self
            .durations
            .sums
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let totals = self
            .durations
            .totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (labels, total) in totals.iter() {
            let sum = sums.get(labels).copied().unwrap_or(0.0);
            out.push_str(REQUEST_DURATION);
            out.push_str("_sum");
            out.push_str(labels);
            out.push(' ');
            out.push_str(&render_float(sum));
            out.push('\n');
            out.push_str(REQUEST_DURATION);
            out.push_str("_count");
            out.push_str(labels);
            out.push(' ');
            out.push_str(&total.to_string());
            out.push('\n');
        }
    }
}

/// Renders a float the way the Prometheus exposition format expects.
fn render_float(value: f64) -> String {
    format!("{value:?}")
}

/// Whether `ratio` is inside the closed unit interval.
#[must_use]
pub fn is_unit_ratio(ratio: f64) -> bool {
    (0.0..=1.0).contains(&ratio)
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
