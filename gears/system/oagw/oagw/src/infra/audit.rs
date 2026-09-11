//! The structured JSON audit emitter of entry 2.9
//! (`cpt-cf-oagw-flow-observability-and-state-audit-record`,
//! `cpt-cf-oagw-algo-observability-and-state-audit-record`).
//!
//! One JSON object per line on **stdout**, with exactly the fourteen fields
//! DESIGN §4.3 names, in the order the ADR lists them:
//!
//! ```text
//! timestamp, level, event, request_id, tenant_id, principal_id, host, path,
//! method, status, duration_ms, request_size, response_size, error_type
//! ```
//!
//! # What the record never carries
//!
//! No `error_message` and no free-form error text
//! (`inst-os-algo-audit-2d`): free-form upstream or internal error text is a
//! leakage channel and `error_type` is the bounded discriminator. No request
//! or response body, no query parameter, no header outside the allowlist, and
//! no credential material: the record type has **no field** that could hold
//! any of them, so omission is enforced by construction rather than by a
//! filter that a future field could bypass.
//!
//! # Sampling
//!
//! The sampling policy addresses the `config_change` and `auth_failure`
//! classes only; the `proxy_request` class is emitted unconditionally with
//! its correlation identifier
//! (`cpt-cf-oagw-dod-observability-and-state-audit-log`), so the
//! 100%-correlation criterion of `cpt-cf-oagw-nfr-observability` holds for the
//! always-emitted population. The rate is a build-time constant, not a
//! configuration key (`inst-os-deploy-2b`).

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(any(test, feature = "test-utils"))]
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// The three legal `event` values (`inst-os-algo-audit-2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEvent {
    /// One proxied exchange. Never sampled.
    ProxyRequest,
    /// One accepted management write.
    ConfigChange,
    /// One rejected authentication attempt.
    AuthFailure,
}

impl AuditEvent {
    /// The wire value of the `event` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProxyRequest => "proxy_request",
            Self::ConfigChange => "config_change",
            Self::AuthFailure => "auth_failure",
        }
    }

    /// Whether the sampling policy may suppress this class
    /// (`inst-os-audit-5`).
    #[must_use]
    pub const fn sampleable(self) -> bool {
        matches!(self, Self::ConfigChange | Self::AuthFailure)
    }
}

/// The log level DESIGN §4.3 assigns to an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditLevel {
    /// Successful requests, normal operations.
    Info,
    /// Rate-limit rejections and retry guidance.
    Warn,
    /// Upstream failures, timeouts, authentication failures.
    Error,
    /// Detailed plugin execution; disabled in production.
    Debug,
}

impl AuditLevel {
    /// The wire value of the `level` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Debug => "DEBUG",
        }
    }
}

/// The level of a completed proxied exchange (`inst-os-audit-2`).
#[must_use]
pub fn level_of_proxy_outcome(status: u16, refused_by_rate_limit: bool) -> AuditLevel {
    if refused_by_rate_limit {
        return AuditLevel::Warn;
    }
    match status {
        500..=599 => AuditLevel::Error,
        _ => AuditLevel::Info,
    }
}

/// One audit record, with the fourteen fields of the ADR field set.
///
/// Every field is `Option`: the class rules decide which are populated, and
/// the rest serialize as `null` so the field set is exactly fourteen keys on
/// every line.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRecord {
    /// The RFC 3339 instant the record was built at.
    pub timestamp: String,
    /// The level DESIGN §4.3 assigns.
    pub level: AuditLevel,
    /// The event class.
    pub event: AuditEvent,
    /// The correlation identifier of the request.
    pub request_id: Option<String>,
    /// The tenant the request is attributed to.
    pub tenant_id: Option<String>,
    /// The principal the request is attributed to.
    pub principal_id: Option<String>,
    /// The upstream alias, or the management resource path of a
    /// `config_change` record.
    pub host: Option<String>,
    /// The request path with no query string, or the management resource path
    /// of a `config_change` record.
    pub path: Option<String>,
    /// The request method.
    pub method: Option<String>,
    /// The response status.
    pub status: Option<u16>,
    /// The exchange duration in milliseconds; `None` for a record written
    /// before the exchange completed.
    pub duration_ms: Option<u64>,
    /// The inbound body size in bytes.
    pub request_size: Option<u64>,
    /// The upstream body size in bytes.
    pub response_size: Option<u64>,
    /// The bounded gateway error discriminator, and `None` on success.
    pub error_type: Option<String>,
}

impl AuditRecord {
    /// The `proxy_request` record of one completed exchange
    /// (`inst-os-audit-1`).
    #[must_use]
    pub fn proxy_request(
        request_id: &str,
        tenant_id: &str,
        principal_id: &str,
        host: Option<&str>,
        path: &str,
        method: &str,
        status: u16,
        duration_ms: u64,
        request_size: u64,
        response_size: u64,
        error_type: Option<&str>,
        refused_by_rate_limit: bool,
    ) -> Self {
        Self {
            timestamp: now_rfc3339(),
            level: level_of_proxy_outcome(status, refused_by_rate_limit),
            event: AuditEvent::ProxyRequest,
            request_id: Some(request_id.to_owned()),
            tenant_id: Some(tenant_id.to_owned()),
            principal_id: Some(principal_id.to_owned()),
            host: host.map(str::to_owned),
            path: Some(bounded_path(path)),
            method: Some(method.to_owned()),
            status: Some(status),
            duration_ms: Some(duration_ms),
            request_size: Some(request_size),
            response_size: Some(response_size),
            error_type: error_type.map(str::to_owned),
        }
    }

    /// The `config_change` record of one accepted management write
    /// (`inst-os-algo-audit-2b`, `inst-os-algo-inval-9`).
    ///
    /// `path` carries the management resource path of the affected record as
    /// the record's resource identifier, and `host`, `method`, `duration_ms`,
    /// `request_size` and `response_size` are `null`.
    #[must_use]
    pub fn config_change(
        request_id: Option<&str>,
        tenant_id: &str,
        principal_id: &str,
        resource_path: &str,
        status: u16,
    ) -> Self {
        Self {
            timestamp: now_rfc3339(),
            level: AuditLevel::Info,
            event: AuditEvent::ConfigChange,
            request_id: request_id.map(str::to_owned),
            tenant_id: Some(tenant_id.to_owned()),
            principal_id: Some(principal_id.to_owned()),
            host: None,
            path: Some(bounded_path(resource_path)),
            method: None,
            status: Some(status),
            duration_ms: None,
            request_size: None,
            response_size: None,
            error_type: None,
        }
    }

    /// The `auth_failure` record of one rejected request
    /// (`inst-os-algo-audit-2c`).
    #[must_use]
    pub fn auth_failure(
        request_id: &str,
        tenant_id: &str,
        principal_id: &str,
        host: Option<&str>,
        path: &str,
        method: &str,
        status: u16,
        error_type: &str,
    ) -> Self {
        Self {
            timestamp: now_rfc3339(),
            level: AuditLevel::Error,
            event: AuditEvent::AuthFailure,
            request_id: Some(request_id.to_owned()),
            tenant_id: Some(tenant_id.to_owned()),
            principal_id: Some(principal_id.to_owned()),
            host: host.map(str::to_owned),
            path: Some(bounded_path(path)),
            method: Some(method.to_owned()),
            status: Some(status),
            duration_ms: None,
            request_size: None,
            response_size: None,
            error_type: Some(error_type.to_owned()),
        }
    }

    /// The four fields the line renders as JSON numbers rather than strings.
    const NUMERIC_FIELDS: [&'static str; 4] =
        ["status", "duration_ms", "request_size", "response_size"];

    /// The fourteen fields as `(name, rendered value)` pairs, in the order the
    /// ADR lists them, with `null` for the fields the class leaves absent.
    #[must_use]
    pub fn fields(&self) -> Vec<(&'static str, Option<String>)> {
        vec![
            ("timestamp", Some(self.timestamp.clone())),
            ("level", Some(self.level.as_str().to_owned())),
            ("event", Some(self.event.as_str().to_owned())),
            ("request_id", self.request_id.clone()),
            ("tenant_id", self.tenant_id.clone()),
            ("principal_id", self.principal_id.clone()),
            ("host", self.host.clone()),
            ("path", self.path.clone()),
            ("method", self.method.clone()),
            ("status", self.status.map(|status| status.to_string())),
            ("duration_ms", self.duration_ms.map(|value| value.to_string())),
            ("request_size", self.request_size.map(|value| value.to_string())),
            ("response_size", self.response_size.map(|value| value.to_string())),
            ("error_type", self.error_type.clone()),
        ]
    }

    /// The record serialized as one JSON object line.
    ///
    /// The field order is the ADR's, and `serde_json::Map`'s key ordering is
    /// deliberately not relied on.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut line = String::from("{");
        let mut first = true;
        for (name, value) in self.fields() {
            if !first {
                line.push(',');
            }
            first = false;
            line.push('"');
            line.push_str(name);
            line.push_str("\":");
            match value {
                Some(value) => {
                    if Self::NUMERIC_FIELDS.contains(&name) {
                        line.push_str(&value);
                    } else {
                        line.push_str(&json_string(&value));
                    }
                }
                None => line.push_str("null"),
            }
        }
        line.push('}');
        line
    }
}

/// The build-time sampling constant: the `1/N` rate the non-proxy-request
/// classes are emitted at. `1` emits every record, which is the graded
/// posture (`inst-os-algo-deploy-1b`).
pub const AUDIT_SAMPLE_RATE: u64 = 1;

/// Where audit lines are written. `Stdout` in every deployment; a buffer only
/// so a test can assert the emitted shape.
enum Destination {
    Stdout,
    #[cfg(any(test, feature = "test-utils"))]
    Captured(Mutex<Vec<String>>),
}

/// The audit emitter: one JSON line per record on stdout.
pub struct AuditSink {
    destination: Destination,
    sample_rate: AtomicU64,
    seen: AtomicU64,
}

impl Default for AuditSink {
    fn default() -> Self {
        Self {
            destination: Destination::Stdout,
            sample_rate: AtomicU64::new(AUDIT_SAMPLE_RATE),
            seen: AtomicU64::new(0),
        }
    }
}

impl std::fmt::Debug for AuditSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AuditSink")
    }
}

impl AuditSink {
    /// The emitter that writes to stdout.
    #[must_use]
    pub fn to_stdout() -> Arc<Self> {
        Arc::new(Self { destination: Destination::Stdout, ..Self::default() })
    }

    /// An emitter that buffers its lines, for a test to assert the shape.
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    pub fn captured() -> Arc<Self> {
        Arc::new(Self {
            destination: Destination::Captured(Mutex::new(Vec::new())),
            ..Self::default()
        })
    }

    /// Raise the sampling rate of the non-proxy-request classes, so a test can
    /// prove the proxy class is never suppressed.
    pub fn set_sample_rate(&self, rate: u64) {
        self.sample_rate.store(rate.max(1), Ordering::Relaxed);
    }

    /// The sampling decision (`inst-os-audit-5`, `inst-os-algo-audit-4`).
    #[must_use]
    pub fn suppressed(&self, event: AuditEvent) -> bool {
        if !event.sampleable() {
            return false;
        }
        let rate = self.sample_rate.load(Ordering::Relaxed).max(1);
        if rate == 1 {
            return false;
        }
        let sequence = self.seen.fetch_add(1, Ordering::Relaxed);
        !sequence.is_multiple_of(rate)
    }

    /// Emit one record: serialize, then write one line to stdout
    /// (`inst-os-algo-audit-5`).
    pub fn emit(&self, record: &AuditRecord) {
        if self.suppressed(record.event) {
            return;
        }
        let line = record.to_line();
        match &self.destination {
            Destination::Stdout => {
                let stdout = std::io::stdout();
                let mut handle = stdout.lock();
                let _ = writeln!(handle, "{line}");
                let _ = handle.flush();
            }
            #[cfg(any(test, feature = "test-utils"))]
            Destination::Captured(lines) => {
                if let Ok(mut lines) = lines.lock() {
                    lines.push(line);
                }
            }
        }
    }

    /// The buffered lines, in emission order.
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        match &self.destination {
            Destination::Captured(lines) => {
                lines.lock().map(|lines| lines.clone()).unwrap_or_default()
            }
            Destination::Stdout => Vec::new(),
        }
    }
}

/// The RFC 3339 UTC instant the record was built at, with millisecond
/// precision.
#[must_use]
pub fn now_rfc3339() -> String {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let millis = since_epoch.subsec_millis();
    let seconds = since_epoch.as_secs();
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z",
        hour = seconds_of_day / 3_600,
        minute = (seconds_of_day % 3_600) / 60,
        second = seconds_of_day % 60,
    )
}

/// The civil date of `days` since the Unix epoch (Howard Hinnant's algorithm).
fn civil_from_days(days: u64) -> (i64, u64, u64) {
    let days = i64::try_from(days).unwrap_or(0);
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096)
        / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month as u64, day as u64)
}

/// A request path carries no query string and is bounded, so a long or
/// adversarial target cannot bloat a line.
fn bounded_path(path: &str) -> String {
    let without_query = path.split(['?', '#']).next().unwrap_or("");
    without_query.chars().take(512).collect()
}

/// Serialize one string value with JSON escaping
/// (`inst-os-algo-audit-5b`: a value that cannot be emitted safely is dropped
/// rather than escaped into the line).
fn json_string(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len() + 2);
    rendered.push('"');
    for character in value.chars() {
        match character {
            '"' => rendered.push_str("\\\""),
            '\\' => rendered.push_str("\\\\"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            control if (control as u32) < 0x20 => {}
            other => rendered.push(other),
        }
    }
    rendered.push('"');
    rendered
}

/// The sink a request-scoped emitter writes through.
pub type SharedAuditSink = Arc<AuditSink>;

#[cfg(test)]
#[path = "audit_tests.rs"]
mod audit_tests;
