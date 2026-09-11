//! Structured audit-log scaffold emission.
//!
//! Defines the fixed field set, the serialization shape, and the emission
//! mechanism later features call into. Per-request field population for a
//! live proxied request is `cpt-cf-oagw-feature-proxy-core`'s (2.5) concern;
//! this feature only provides the scaffold.
//!
//! See `docs/features/gear-foundation.md` §3 "Structured Audit-Log Scaffold
//! Emission" (`cpt-cf-oagw-algo-audit-log-emit`).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Caller-resolved fields for one audit-log entry. Any field left `None` at
/// emission time is serialized as an explicit `null`, never fabricated.
#[derive(Debug, Clone, Default)]
pub struct AuditLogFields {
    pub tenant_id: Option<String>,
    pub principal_id: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub method: Option<String>,
    pub status: Option<u16>,
    pub duration_ms: Option<u64>,
}

/// One structured JSON audit-log line, with exactly the documented field
/// set: `timestamp`, `level`, `event`, `request_id`, `tenant_id`,
/// `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`.
#[derive(Debug, Clone, Serialize)]
pub struct AuditLogEntry {
    pub timestamp: u64,
    pub level: String,
    pub event: String,
    pub request_id: String,
    pub tenant_id: Option<String>,
    pub principal_id: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub method: Option<String>,
    pub status: Option<u16>,
    pub duration_ms: Option<u64>,
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

impl AuditLogEntry {
    /// Build one audit-log entry: `timestamp`/`level`/`event`/`request_id`
    /// are always resolved by the scaffold itself (the request id comes
    /// from [`crate::correlation::assign_correlation_id`]); every other
    /// field is whatever the calling feature has resolved at emission time,
    /// left `None` (never fabricated) otherwise.
    // @cpt-algo:cpt-cf-oagw-algo-audit-log-emit:p2
    // @cpt-begin:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-01
    // @cpt-begin:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-02
    // @cpt-begin:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-03
    #[must_use]
    pub fn new(
        event: impl Into<String>,
        level: impl Into<String>,
        request_id: impl Into<String>,
        fields: AuditLogFields,
    ) -> Self {
        Self {
            timestamp: now_epoch_ms(),
            level: level.into(),
            event: event.into(),
            request_id: request_id.into(),
            tenant_id: fields.tenant_id,
            principal_id: fields.principal_id,
            host: fields.host,
            path: fields.path,
            method: fields.method,
            status: fields.status,
            duration_ms: fields.duration_ms,
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-03
    // @cpt-end:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-02
    // @cpt-end:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-01

    /// Serialize this entry as a single JSON line.
    ///
    /// # Errors
    /// Returns a `serde_json::Error` if serialization fails (never expected
    /// for this plain-data struct).
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Serialize and write this entry to the structured-logging sink.
    // @cpt-begin:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-04
    // @cpt-begin:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-05
    // @cpt-dod:cpt-cf-oagw-dod-audit-correlation-scaffold:p1
    pub fn emit(&self) {
        match self.to_json_line() {
            Ok(line) => tracing::info!(target: "oagw::audit", "{line}"),
            Err(error) => {
                tracing::error!(target: "oagw::audit", %error, "failed to serialize oagw audit log entry");
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-05
    // @cpt-end:cpt-cf-oagw-algo-audit-log-emit:p2:inst-audit-log-emit-04
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn serializes_exactly_the_documented_field_set() {
        let entry = AuditLogEntry::new("oagw.request", "info", "req-1", AuditLogFields::default());
        let line = entry.to_json_line().unwrap();
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        let obj = json.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = vec![
            "timestamp",
            "level",
            "event",
            "request_id",
            "tenant_id",
            "principal_id",
            "host",
            "path",
            "method",
            "status",
            "duration_ms",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn unresolved_fields_are_explicit_null_not_omitted_or_fabricated() {
        let entry = AuditLogEntry::new("oagw.request", "info", "req-1", AuditLogFields::default());
        let line = entry.to_json_line().unwrap();
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(json["tenant_id"].is_null());
        assert!(json["principal_id"].is_null());
        assert!(json["host"].is_null());
        assert!(json["path"].is_null());
        assert!(json["method"].is_null());
        assert!(json["status"].is_null());
        assert!(json["duration_ms"].is_null());
    }

    #[test]
    fn resolved_fields_are_carried_through() {
        let fields = AuditLogFields {
            host: Some("api.example.com".to_owned()),
            status: Some(200),
            ..Default::default()
        };
        let entry = AuditLogEntry::new("oagw.request", "info", "req-1", fields);
        let line = entry.to_json_line().unwrap();
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(json["host"], "api.example.com");
        assert_eq!(json["status"], 200);
    }
}
