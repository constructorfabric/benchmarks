//! Audit logging (feature `cpt-cf-oagw-feature-observability-audit`,
//! DESIGN §4.3 Audit Logging; DoD
//! `cpt-cf-oagw-dod-observability-audit-audit-log`).
//!
//! The audit record is the deterministic 14-field set (plus the optional
//! `error_message` DESIGN reserves for failed requests):
//!
//! `timestamp` (UTC), `level`, `event`, `request_id`, `tenant_id`,
//! `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`,
//! `request_size`, `response_size`, `error_type` (empty on success).
//!
//! Records are emitted as structured `tracing` events under the
//! `oagw::audit` target (level INFO success/normal, WARN rate-limit
//! exceeded / retry guidance, ERROR upstream failures/timeouts/auth
//! failures, DEBUG detailed plugin execution) — the structured tracing
//! record *is* the audit sink per DESIGN §4.3 ("Structured JSON logs to
//! stdout, ingested by a centralized logging system"): the host's JSON
//! formatter turns the event fields into the transport line, and each
//! entry also carries its [`AuditEntry::to_json`] rendering for a
//! deterministic record (algorithm `cpt-cf-oagw-algo-observability-audit-audit-log`).
//!
//! The `error_type`/`error_message` fields reuse the GTS instance
//! identifiers chosen in Error Semantics (§63): a gateway failure carries
//! `gts.cf.core.errors.err.v1~cf.oagw.<instance>`, an upstream-produced
//! failure carries the bounded `upstream_http_error` marker.
//!
//! # Privacy constraints (DESIGN §4.3, `cpt-cf-oagw-principle-*`)
//!
//! No request/response bodies, query parameters, or (non-allowlisted)
//! headers are ever recorded; credential material never reaches the audit
//! fields.  Emission is non-blocking (a tracing `event!` — no I/O in the
//! record path; the centralized sink is the host's concern).

use tracing::Level;

/// The `tracing` target carrying audit records.
pub const AUDIT_TARGET: &str = "oagw::audit";

/// Event name for a completed proxy request.
pub const EVENT_PROXY_COMPLETED: &str = "proxy.request.completed";
/// Event name for a proxy request rejected by the gateway (4xx, gateway
/// envelope).
pub const EVENT_PROXY_REJECTED: &str = "proxy.request.rejected";
/// Event name for a rate-limited proxy request.
pub const EVENT_RATE_LIMIT_EXCEEDED: &str = "proxy.rate_limit.exceeded";
/// Event name for an upstream-originated failure/timeout.
pub const EVENT_UPSTREAM_ERROR: &str = "proxy.upstream_error";
/// Event name for a data-plane authorization failure.
pub const EVENT_AUTH_FAILED: &str = "proxy.auth_failed";

/// One audit-log entry (DESIGN §4.3 field set; DoD
/// `cpt-cf-oagw-dod-observability-audit-audit-log`,
/// `inst-ob-al-*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// UTC wall-clock timestamp (RFC 3339).
    pub timestamp: String,
    /// Record level (INFO/WARN/ERROR/DEBUG per DESIGN §4.3).
    pub level: Level,
    /// Event name (e.g. `proxy.request.completed`, `config.upstream.deleted`).
    pub event: String,
    /// The correlation request id (`inst-ob-al-identity`).
    pub request_id: Option<String>,
    /// Subject tenant id, when authenticated (`inst-ob-al-identity`).
    pub tenant_id: Option<String>,
    /// Subject/principal id, when authenticated (`inst-ob-al-identity`).
    pub principal_id: Option<String>,
    /// The upstream host/alias (proxy) — `None` for control-plane entries.
    pub host: Option<String>,
    /// The request path (`inst-ob-al-req`).
    pub path: Option<String>,
    /// The HTTP method (`inst-ob-al-req`).
    pub method: Option<String>,
    /// The response status code (`inst-ob-al-req`).
    pub status: Option<u16>,
    /// End-to-end duration in milliseconds (`inst-ob-al-req`).
    pub duration_ms: Option<u64>,
    /// Inbound request body size in bytes, when known (`inst-ob-al-size`).
    pub request_size: Option<u64>,
    /// Outbound response body size in bytes, when known (`inst-ob-al-size`).
    pub response_size: Option<u64>,
    /// GTS error instance (or `upstream_http_error`) — empty on success
    /// (`inst-ob-al-size`).
    pub error_type: String,
    /// Human-readable failure detail (DESIGN "Failed requests"):
    /// present only on failure.
    pub error_message: Option<String>,
}

impl AuditEntry {
    /// Opens an entry; populate the optional fields with the builder
    /// methods and finish with [`Self::emit`].
    #[must_use]
    pub fn new(timestamp: String, level: Level, event: impl Into<String>) -> Self {
        Self {
            timestamp,
            level,
            event: event.into(),
            request_id: None,
            tenant_id: None,
            principal_id: None,
            host: None,
            path: None,
            method: None,
            status: None,
            duration_ms: None,
            request_size: None,
            response_size: None,
            error_type: String::new(),
            error_message: None,
        }
    }

    /// Sets the correlation request id.
    #[must_use]
    pub fn with_correlation(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    /// Sets the identity fields (tenant, principal).
    #[must_use]
    pub fn with_identity(
        mut self,
        tenant_id: Option<String>,
        principal_id: Option<String>,
    ) -> Self {
        self.tenant_id = tenant_id;
        self.principal_id = principal_id;
        self
    }

    /// Sets the request context fields (host, path, method).
    #[must_use]
    pub fn with_request(
        mut self,
        host: Option<String>,
        path: Option<String>,
        method: Option<String>,
    ) -> Self {
        self.host = host;
        self.path = path;
        self.method = method;
        self
    }

    /// Sets the outcome fields (status, duration, sizes).
    #[must_use]
    pub fn with_outcome(
        mut self,
        status: u16,
        duration_ms: Option<u64>,
        request_size: Option<u64>,
        response_size: Option<u64>,
    ) -> Self {
        self.status = Some(status);
        self.duration_ms = duration_ms;
        self.request_size = request_size;
        self.response_size = response_size;
        self
    }

    /// Sets the failure attribution (GTS instance or `upstream_http_error`
    /// plus a human-readable detail).
    #[must_use]
    pub fn with_error(
        mut self,
        error_type: impl Into<String>,
        error_message: Option<String>,
    ) -> Self {
        self.error_type = error_type.into();
        self.error_message = error_message;
        self
    }

    /// Emits this entry as one structured `tracing` event on the
    /// `oagw::audit` target (`inst-ob-al-return`).  Non-blocking: no I/O
    /// happens here, a tracing event is dispatched for the host's logger.
    pub fn emit(&self) {
        match self.level {
            Level::ERROR => tracing::event!(
                target: AUDIT_TARGET,
                Level::ERROR,
                timestamp = %self.timestamp,
                event = %self.event,
                request_id = ?self.request_id,
                tenant_id = ?self.tenant_id,
                principal_id = ?self.principal_id,
                host = ?self.host,
                path = ?self.path,
                method = ?self.method,
                status = ?self.status,
                duration_ms = ?self.duration_ms,
                request_size = ?self.request_size,
                response_size = ?self.response_size,
                error_type = %self.error_type,
                error_message = ?self.error_message,
                json = %self.to_json(),
                "audit log entry"
            ),
            Level::WARN => tracing::event!(
                target: AUDIT_TARGET,
                Level::WARN,
                timestamp = %self.timestamp,
                event = %self.event,
                request_id = ?self.request_id,
                tenant_id = ?self.tenant_id,
                principal_id = ?self.principal_id,
                host = ?self.host,
                path = ?self.path,
                method = ?self.method,
                status = ?self.status,
                duration_ms = ?self.duration_ms,
                request_size = ?self.request_size,
                response_size = ?self.response_size,
                error_type = %self.error_type,
                error_message = ?self.error_message,
                json = %self.to_json(),
                "audit log entry"
            ),
            Level::INFO => tracing::event!(
                target: AUDIT_TARGET,
                Level::INFO,
                timestamp = %self.timestamp,
                event = %self.event,
                request_id = ?self.request_id,
                tenant_id = ?self.tenant_id,
                principal_id = ?self.principal_id,
                host = ?self.host,
                path = ?self.path,
                method = ?self.method,
                status = ?self.status,
                duration_ms = ?self.duration_ms,
                request_size = ?self.request_size,
                response_size = ?self.response_size,
                error_type = %self.error_type,
                error_message = ?self.error_message,
                json = %self.to_json(),
                "audit log entry"
            ),
            Level::DEBUG => tracing::event!(
                target: AUDIT_TARGET,
                Level::DEBUG,
                timestamp = %self.timestamp,
                event = %self.event,
                request_id = ?self.request_id,
                tenant_id = ?self.tenant_id,
                principal_id = ?self.principal_id,
                host = ?self.host,
                path = ?self.path,
                method = ?self.method,
                status = ?self.status,
                duration_ms = ?self.duration_ms,
                request_size = ?self.request_size,
                response_size = ?self.response_size,
                error_type = %self.error_type,
                error_message = ?self.error_message,
                json = %self.to_json(),
                "audit log entry"
            ),
            Level::TRACE => tracing::event!(
                target: AUDIT_TARGET,
                Level::TRACE,
                timestamp = %self.timestamp,
                event = %self.event,
                request_id = ?self.request_id,
                tenant_id = ?self.tenant_id,
                principal_id = ?self.principal_id,
                host = ?self.host,
                path = ?self.path,
                method = ?self.method,
                status = ?self.status,
                duration_ms = ?self.duration_ms,
                request_size = ?self.request_size,
                response_size = ?self.response_size,
                error_type = %self.error_type,
                error_message = ?self.error_message,
                json = %self.to_json(),
                "audit log entry"
            ),
        }
    }

    /// Human-readable level token (for the JSON record).
    #[must_use]
    pub fn level_name(&self) -> &'static str {
        match self.level {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN",
            Level::INFO => "INFO",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        }
    }

    /// Deterministic RFC 8259 rendering of the audit fields in DESIGN §4.3
    /// order.  Optional fields are omitted when absent; `error_type` is
    /// always present (empty on success per the DoD).
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        let mut first = true;
        let push = |k: &str, v: String, out: &mut String, first: &mut bool| {
            if !*first {
                out.push(',');
            }
            *first = false;
            out.push('"');
            out.push_str(k);
            out.push_str("\":");
            out.push_str(&v);
        };
        push(
            "timestamp",
            json_string(&self.timestamp),
            &mut out,
            &mut first,
        );
        push(
            "level",
            json_string(self.level_name()),
            &mut out,
            &mut first,
        );
        push("event", json_string(&self.event), &mut out, &mut first);
        if let Some(v) = &self.request_id {
            push("request_id", json_string(v), &mut out, &mut first);
        }
        if let Some(v) = &self.tenant_id {
            push("tenant_id", json_string(v), &mut out, &mut first);
        }
        if let Some(v) = &self.principal_id {
            push("principal_id", json_string(v), &mut out, &mut first);
        }
        if let Some(v) = &self.host {
            push("host", json_string(v), &mut out, &mut first);
        }
        if let Some(v) = &self.path {
            push("path", json_string(v), &mut out, &mut first);
        }
        if let Some(v) = &self.method {
            push("method", json_string(v), &mut out, &mut first);
        }
        if let Some(v) = self.status {
            push("status", v.to_string(), &mut out, &mut first);
        }
        if let Some(v) = self.duration_ms {
            push("duration_ms", v.to_string(), &mut out, &mut first);
        }
        if let Some(v) = self.request_size {
            push("request_size", v.to_string(), &mut out, &mut first);
        }
        if let Some(v) = self.response_size {
            push("response_size", v.to_string(), &mut out, &mut first);
        }
        push(
            "error_type",
            json_string(&self.error_type),
            &mut out,
            &mut first,
        );
        if let Some(v) = &self.error_message {
            push("error_message", json_string(v), &mut out, &mut first);
        }
        out.push('}');
        out
    }
}

/// A control-plane config-change audit entry (DESIGN §4.3, "Config
/// changes": upstream/route/plugin create/update/delete).  Emitted by the
/// management handlers after a successful mutation.
//
// All eight parameters are part of the DESIGN §4.3 audit field set; the
// builder hides the optional-field ergonomics, so the long parameter list is
// intentional.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn config_change(
    timestamp: String,
    tenant_id: Option<String>,
    principal_id: Option<String>,
    request_id: Option<String>,
    event: &str,
    method: &str,
    path: &str,
    status: u16,
) -> AuditEntry {
    AuditEntry::new(timestamp, Level::INFO, event)
        .with_correlation(request_id)
        .with_identity(tenant_id, principal_id)
        .with_request(None, Some(path.to_owned()), Some(method.to_owned()))
        .with_outcome(status, None, None, None)
}

/// Builds an RFC 3339 UTC timestamp from the wall clock (e.g.
/// `2026-08-27T14:03:09Z`).  Seconds precision; the algorithm is the
/// standard civil-from-days conversion so no date dependency is needed.
#[must_use]
pub fn now_utc_timestamp() -> String {
    utc_timestamp(std::time::SystemTime::now())
}

/// Builds an RFC 3339 UTC timestamp from a `SystemTime` (injectable for
/// deterministic tests).
#[must_use]
pub fn utc_timestamp(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Standard (Howard Hinnant) civil-from-days conversion.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// JSON string escaping (minimal RFC 8259: quotes, backslash, control
/// characters).
fn json_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for ch in v.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn utc_timestamp_renders_rfc3339() {
        let now = UNIX_EPOCH + Duration::from_secs(1_781_536_187);
        // epoch 1781536187 == 2026-06-15T15:09:47Z (verified independently)
        assert_eq!(utc_timestamp(now), "2026-06-15T15:09:47Z");
        assert!(now_utc_timestamp().ends_with('Z'));
    }

    #[test]
    fn entry_renders_deterministic_json_with_full_field_set() {
        let entry = AuditEntry::new(
            "2026-06-22T22:43:07Z".to_owned(),
            Level::WARN,
            EVENT_RATE_LIMIT_EXCEEDED,
        )
        .with_correlation(Some("req-1".to_owned()))
        .with_identity(Some("tenant-9".to_owned()), Some("user-7".to_owned()))
        .with_request(
            Some("svc".to_owned()),
            Some("/proxy/svc/v1".to_owned()),
            Some("GET".to_owned()),
        )
        .with_outcome(429, Some(12), Some(0), Some(123))
        .with_error(
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            Some("budget exhausted".to_owned()),
        );
        let json = entry.to_json();
        assert_eq!(
            json,
            "{\"timestamp\":\"2026-06-22T22:43:07Z\",\"level\":\"WARN\",\"event\":\"proxy.rate_limit.exceeded\",\"request_id\":\"req-1\",\"tenant_id\":\"tenant-9\",\"principal_id\":\"user-7\",\"host\":\"svc\",\"path\":\"/proxy/svc/v1\",\"method\":\"GET\",\"status\":429,\"duration_ms\":12,\"request_size\":0,\"response_size\":123,\"error_type\":\"gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1\",\"error_message\":\"budget exhausted\"}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(parsed["event"], "proxy.rate_limit.exceeded");
        assert_eq!(parsed["tenant_id"], "tenant-9");
        assert_eq!(parsed["status"], 429);
        assert_eq!(
            parsed["error_type"],
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
    }

    #[test]
    fn success_entry_has_empty_error_type_and_omits_none_fields() {
        let entry = AuditEntry::new(
            "2026-06-22T22:43:07Z".to_owned(),
            Level::INFO,
            EVENT_PROXY_COMPLETED,
        )
        .with_request(
            Some("svc".to_owned()),
            Some("/proxy/svc".to_owned()),
            Some("GET".to_owned()),
        )
        .with_outcome(200, Some(3), None, None);
        let parsed: serde_json::Value = serde_json::from_str(&entry.to_json()).expect("valid json");
        assert_eq!(parsed["error_type"], "", "error_type empty on success");
        assert!(parsed.get("error_message").is_none());
        assert!(parsed.get("request_id").is_none());
        assert!(
            parsed.get("tenant_id").is_none(),
            "optional identity absent"
        );
        assert_eq!(parsed["level"], "INFO");
        assert_eq!(parsed["status"], 200);
    }

    #[test]
    fn config_change_entry_carries_the_event_and_identity() {
        let entry = config_change(
            "2026-06-22T22:43:07Z".to_owned(),
            Some("tenant-9".to_owned()),
            Some("user-7".to_owned()),
            Some("req-1".to_owned()),
            "config.upstream.created",
            "POST",
            "/upstreams",
            201,
        );
        let parsed: serde_json::Value = serde_json::from_str(&entry.to_json()).expect("valid json");
        assert_eq!(parsed["level"], "INFO");
        assert_eq!(parsed["event"], "config.upstream.created");
        assert_eq!(parsed["method"], "POST");
        assert_eq!(parsed["path"], "/upstreams");
        assert_eq!(parsed["status"], 201);
        assert_eq!(parsed["error_type"], "", "config changes are not failures");
    }

    #[test]
    fn json_string_escapes_quotes_and_control_chars() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
