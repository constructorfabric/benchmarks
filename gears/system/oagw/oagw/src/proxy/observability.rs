//! Per-request observability: a structured log record and the Prometheus counters.
//!
//! Two rules hold for everything this module emits. Cardinality is bounded — the labels
//! are the upstream alias, the route pattern and the numeric status, never the raw path,
//! the query string or the caller's identity — and no secret material ever reaches a
//! record: the request and response bodies, the query string, the headers and every
//! resolved credential are excluded by construction, because the record is built from a
//! fixed set of fields rather than from the request itself.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// The metric names the gateway publishes.
pub const REQUESTS_TOTAL: &str = "oagw_proxy_requests_total";
pub const REQUEST_DURATION_SECONDS: &str = "oagw_proxy_request_duration_seconds";
pub const ERRORS_TOTAL: &str = "oagw_proxy_errors_total";
pub const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_proxy_rate_limit_exceeded_total";

/// The histogram buckets, in seconds, the request duration uses.
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The fixed field set of one proxied request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// Correlation identifier, echoing the caller's `X-Request-Id` when it sent one.
    pub request_id: String,
    /// The caller's own tenant.
    pub tenant_id: String,
    /// The upstream alias, the gateway's `host` label.
    pub host: String,
    /// The route pattern that matched, not the raw request path.
    pub route: String,
    /// The HTTP method, normalized.
    pub method: String,
    /// The upstream status, or the gateway's own when it answered instead.
    pub status: u16,
    /// Wall-clock duration of the relay.
    pub duration: Duration,
    /// The error kind, when the gateway answered with an error.
    pub error_type: Option<String>,
}

impl RequestRecord {
    /// Whether the record describes a gateway-generated error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.error_type.is_some() || self.status >= 500
    }

    /// Renders the record as the structured log line the gateway emits.
    ///
    /// The field set is closed: a caller-controlled value can only land in the fields it
    /// is explicitly assigned to, and no body, query string, header or credential value is
    /// among them.
    #[must_use]
    pub fn log_line(&self) -> String {
        let mut fields = vec![
            ("event", "oagw.proxy.request".to_owned()),
            ("request_id", self.request_id.clone()),
            ("tenant_id", self.tenant_id.clone()),
            ("host", self.host.clone()),
            ("http.route", self.route.clone()),
            ("method", self.method.clone()),
            ("status", self.status.to_string()),
            ("duration_ms", duration_ms(self.duration).to_string()),
        ];
        if let Some(error_type) = &self.error_type {
            fields.push(("error_type", error_type.clone()));
        }
        fields
            .into_iter()
            .map(|(key, value)| format!("{key}={}", log_quote(&value)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The label set of a counter sample.
type Labels = Vec<(&'static str, String)>;

#[derive(Default)]
struct Series {
    counters: HashMap<String, u64>,
    histogram: HashMap<String, [u64; 13]>,
    sums: HashMap<String, f64>,
}

/// The gateway's metric registry.
#[derive(Default)]
pub struct Metrics {
    series: Mutex<Series>,
}

impl Metrics {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn key(name: &str, labels: &Labels) -> String {
        if labels.is_empty() {
            return name.to_owned();
        }
        let rendered = labels
            .iter()
            .map(|(key, value)| format!("{key}={}", quote(value)))
            .collect::<Vec<_>>()
            .join(",");
        format!("{name}{{{rendered}}}")
    }

    /// Records one finished request.
    pub fn record(&self, record: &RequestRecord) {
        let host = ("host", record.host.clone());
        let route = ("http.route", record.route.clone());
        let method = ("http.request.method", normalized_method(&record.method));
        let status = (
            "http.response.status_code",
            record.status.to_string(),
        );

        let mut requests = vec![host.clone(), route.clone(), method, status];
        requests.sort_by(|a, b| a.0.cmp(b.0));
        self.increment(REQUESTS_TOTAL, &requests);

        let mut duration_labels = vec![host.clone(), route.clone()];
        duration_labels.sort_by(|a, b| a.0.cmp(b.0));
        self.observe(REQUEST_DURATION_SECONDS, &duration_labels, record.duration);

        if let Some(error_type) = &record.error_type {
            let mut errors = vec![host, route, ("error_type", error_type.clone())];
            errors.sort_by(|a, b| a.0.cmp(b.0));
            self.increment(ERRORS_TOTAL, &errors);
        }
    }

    /// Records one rejected request, keyed by the reason it was rejected.
    pub fn record_error(&self, host: &str, route: &str, error_type: &str) {
        let record = RequestRecord {
            request_id: String::new(),
            tenant_id: String::new(),
            host: host.to_owned(),
            route: route.to_owned(),
            method: String::new(),
            status: 0,
            duration: Duration::ZERO,
            error_type: Some(error_type.to_owned()),
        };
        let mut labels = vec![
            ("host", record.host.clone()),
            ("http.route", record.route),
            ("error_type", error_type.to_owned()),
        ];
        labels.sort_by(|a, b| a.0.cmp(b.0));
        self.increment(ERRORS_TOTAL, &labels);
    }

    /// Records a request the rate limiter rejected.
    pub fn record_rate_limited(&self, host: &str, route: &str) {
        let mut labels = vec![
            ("host", host.to_owned()),
            ("http.route", route.to_owned()),
        ];
        labels.sort_by(|a, b| a.0.cmp(b.0));
        self.increment(RATE_LIMIT_EXCEEDED_TOTAL, &labels);
    }

    fn increment(&self, name: &str, labels: &Labels) {
        let key = Self::key(name, labels);
        let mut series = self.series.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *series.counters.entry(key).or_insert(0) += 1;
    }

    fn observe(&self, name: &str, labels: &Labels, value: Duration) {
        let seconds = value.as_secs_f64();
        let key = Self::key(name, labels);
        let mut series = self.series.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let buckets = series
            .histogram
            .entry(key.clone())
            .or_insert([0u64; 13]);
        let mut slot = DURATION_BUCKETS.len();
        for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                slot = index;
                break;
            }
        }
        // Prometheus buckets are cumulative: every bound at or above the value counts
        // it, and `+Inf` is the running total.
        for entry in buckets.iter_mut().skip(slot) {
            *entry += 1;
        }
        *series.sums.entry(key).or_insert(0.0) += seconds;
    }

    /// The counter samples, sorted for a stable exposition.
    ///
    /// A poisoned lock is recovered from: the samples inside are still the truth.
    pub fn counters(&self) -> Vec<(String, u64)> {
        let series = self.series.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut samples: Vec<(String, u64)> = series
            .counters
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect();
        samples.sort();
        samples
    }

    /// The histogram samples, sorted for a stable exposition.
    ///
    /// A poisoned lock is recovered from, as with [`Metrics::counters`].
    pub fn histograms(&self) -> Vec<(String, [u64; 13], f64)> {
        let series = self.series.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut samples: Vec<(String, [u64; 13], f64)> = series
            .histogram
            .iter()
            .map(|(key, buckets)| {
                (
                    key.clone(),
                    *buckets,
                    series.sums.get(key).copied().unwrap_or_default(),
                )
            })
            .collect();
        samples.sort_by(|a, b| a.0.cmp(&b.0));
        samples
    }

    /// Renders the registry in the Prometheus text exposition format.
    ///
    /// Every metric name gets its own `# TYPE` line, so a document that carries more
    /// than one name is still a valid exposition.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut typed: Option<String> = None;

        for (key, value) in self.counters() {
            let (name, _) = split_key(&key);
            if typed.as_deref() != Some(name) {
                lines.push(format!("# TYPE {name} counter"));
                typed = Some(name.to_owned());
            }
            lines.push(format!("{key} {value}"));
        }

        for (key, buckets, sum) in self.histograms() {
            let (name, labels) = split_key(&key);
            if typed.as_deref() != Some(name) {
                lines.push(format!("# TYPE {name} histogram"));
                typed = Some(name.to_owned());
            }
            for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
                lines.push(format!(
                    "{name}_bucket{}le=\"{bound}\" {}",
                    labels_prefix(labels),
                    buckets[index]
                ));
            }
            lines.push(format!(
                "{name}_bucket{}le=\"+Inf\" {}",
                labels_prefix(labels),
                buckets[DURATION_BUCKETS.len()]
            ));
            lines.push(format!("{name}_sum{labels} {sum}"));
            lines.push(format!(
                "{name}_count{labels} {}",
                buckets[DURATION_BUCKETS.len()]
            ));
        }

        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }
}

fn labels_prefix(labels: &str) -> String {
    if labels == "{}" {
        String::new()
    } else {
        // `{a="b"}` becomes `,{...}` for a sub-series label set.
        format!("{},", &labels[1..labels.len() - 1])
    }
}

fn split_key(key: &str) -> (&str, &str) {
    match key.find('{') {
        Some(index) => (&key[..index], &key[index..]),
        None => (key, "{}"),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    // A duration longer than the whole `u64` millisecond range is meaningless here; the
    // histogram saturates instead of wrapping.
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Normalizes a method to a standard verb or `_OTHER`.
#[must_use]
pub fn normalized_method(method: &str) -> String {
    match method.to_ascii_uppercase().as_str() {
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" | "TRACE" | "CONNECT" => {
            method.to_ascii_uppercase()
        }
        _ => "_OTHER".to_owned(),
    }
}

/// Quotes a log field value only when it could be mistaken for field syntax.
fn log_quote(value: &str) -> String {
    let needs = value.is_empty() || value.chars().any(|ch| matches!(ch, ' ' | '"' | '\\' | '=' | '\n'));
    if !needs {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Quotes a label value so a label value cannot forge a second label.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Emits the structured log record for one proxied request.
///
/// The record is built from the fixed [`RequestRecord`] field set, so a body, a query
/// string, a header or a credential value has no path into the log line.
pub fn log_request(record: &RequestRecord) {
    if record.is_error() {
        tracing::warn!(target: "oagw.proxy", "{}", record.log_line());
    } else {
        tracing::info!(target: "oagw.proxy", "{}", record.log_line());
    }
}

/// Scrubs credential-shaped substrings out of text destined for a log or an error.
///
/// The auth plugins' redaction: any material that arrived as a resolved credential is
/// replaced before the text is rendered, so a token that leaked into a detail string is
/// not echoed back.
#[must_use]
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }
    out
}

/// The placeholder a redacted value is replaced with.
pub const REDACTED: &str = "[redacted]";

