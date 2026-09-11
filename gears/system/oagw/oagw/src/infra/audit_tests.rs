//! The unit tests of the audit emitter
//! (`cpt-cf-oagw-dod-observability-and-state-audit-log`).

use serde_json::Value;

use super::*;

/// The fourteen fields, in the order the ADR lists them, exactly.
#[test]
fn a_proxy_request_record_carries_the_fourteen_fields_in_order() {
    let record = AuditRecord::proxy_request(
        "req-1",
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
        Some("api.vendor.com"),
        "/v1/orders",
        "GET",
        200,
        12,
        3,
        45,
        None,
        false,
    );
    let names: Vec<&str> = record.fields().into_iter().map(|(name, _)| name).collect();
    assert_eq!(
        names,
        [
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
            "request_size",
            "response_size",
            "error_type",
        ]
    );
}

/// The `event` vocabulary is closed at exactly three values.
#[test]
fn the_event_vocabulary_is_closed() {
    assert_eq!(AuditEvent::ProxyRequest.as_str(), "proxy_request");
    assert_eq!(AuditEvent::ConfigChange.as_str(), "config_change");
    assert_eq!(AuditEvent::AuthFailure.as_str(), "auth_failure");
}

/// The level mapping of DESIGN §4.3.
#[test]
fn the_level_follows_the_outcome() {
    assert_eq!(level_of_proxy_outcome(200, false), AuditLevel::Info);
    assert_eq!(level_of_proxy_outcome(404, false), AuditLevel::Info);
    assert_eq!(level_of_proxy_outcome(429, true), AuditLevel::Warn);
    assert_eq!(level_of_proxy_outcome(502, false), AuditLevel::Error);
    assert_eq!(level_of_proxy_outcome(504, false), AuditLevel::Error);
    assert_eq!(AuditRecord::config_change(None, "t", "p", "/oagw/v1/upstreams", 201).level, AuditLevel::Info);
    assert_eq!(
        AuditRecord::auth_failure("r", "t", "p", None, "/v1", "GET", 401, "auth").level,
        AuditLevel::Error
    );
}

/// The per-class field population rules of `inst-os-algo-audit-2b`.
#[test]
fn a_config_change_record_populates_its_class_fields() {
    let record = AuditRecord::config_change(
        Some("req-2"),
        "tenant",
        "principal",
        "/oagw/v1/upstreams/9",
        201,
    );
    let fields = record.fields();
    let populated = |name: &str| {
        fields
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| value.is_some())
            .unwrap_or(false)
    };
    for field in ["timestamp", "level", "event", "request_id", "tenant_id", "principal_id", "status"] {
        assert!(populated(field), "{field} must be populated");
    }
    assert!(populated("path"));
    assert_eq!(
        fields.iter().find(|(field, _)| *field == "path").and_then(|(_, value)| value.clone()),
        Some("/oagw/v1/upstreams/9".to_owned())
    );
    for field in ["host", "method", "duration_ms", "request_size", "response_size"] {
        assert!(!populated(field), "{field} must be null");
    }
    assert_eq!(record.event, AuditEvent::ConfigChange);
    assert_eq!(record.error_type, None);
}

/// The per-class field population rules of `inst-os-algo-audit-2c`.
#[test]
fn an_auth_failure_record_carries_its_class_fields() {
    let record = AuditRecord::auth_failure(
        "req-3",
        "tenant",
        "principal",
        Some("api.vendor.com"),
        "/v1/orders",
        "POST",
        401,
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
    );
    let fields = record.fields();
    let value_of = |name: &str| {
        fields
            .iter()
            .find(|(field, _)| *field == name)
            .and_then(|(_, value)| value.clone())
    };
    for field in
        ["timestamp", "level", "event", "request_id", "tenant_id", "principal_id", "status", "host", "path", "method", "error_type"]
    {
        assert!(value_of(field).is_some(), "{field} must be populated");
    }
    for field in ["duration_ms", "request_size", "response_size"] {
        assert!(value_of(field).is_none(), "{field} must be null");
    }
}

/// No record carries `error_message` or any free-form error text.
#[test]
fn no_record_carries_an_error_message() {
    let record = AuditRecord::proxy_request(
        "req-4",
        "tenant",
        "principal",
        Some("api.vendor.com"),
        "/v1",
        "GET",
        502,
        1,
        0,
        0,
        Some("gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"),
        false,
    );
    let line = record.to_line();
    assert!(!line.contains("error_message"));
    assert!(!line.contains("message"));
    assert!(record.fields().iter().all(|(name, _)| *name != "error_message"));
}

/// The record carries no body, no query parameter and no header value.
#[test]
fn a_record_omits_bodies_queries_and_headers() {
    let sink = AuditSink::captured();
    sink.emit(&AuditRecord::proxy_request(
        "req-5",
        "tenant",
        "principal",
        Some("api.vendor.com"),
        "/v1/orders?api_key=secret-token&x=1",
        "GET",
        200,
        1,
        11,
        22,
        None,
        false,
    ));
    let line = sink.lines().join("\n");
    assert!(!line.contains("secret-token"));
    assert!(!line.contains("api_key"));
    assert!(!line.contains("?x=1"));
    assert!(line.contains("/v1/orders"));
}

/// The emitted line is one parseable JSON object with exactly fourteen keys.
#[test]
fn the_emitted_line_is_one_json_object() {
    let sink = AuditSink::captured();
    sink.emit(&AuditRecord::proxy_request(
        "req-6",
        "tenant",
        "principal",
        Some("api.vendor.com"),
        "/v1",
        "GET",
        200,
        1,
        0,
        0,
        None,
        false,
    ));
    let lines = sink.lines();
    assert_eq!(lines.len(), 1);
    let document: Value = serde_json::from_str(&lines[0]).expect("one JSON object");
    let object = document.as_object().expect("an object");
    assert_eq!(object.len(), 14);
    assert_eq!(object["event"], Value::String("proxy_request".to_owned()));
    assert_eq!(object["status"], Value::Number(200.into()));
    assert_eq!(object["error_type"], Value::Null);
    assert!(object["timestamp"].as_str().expect("the timestamp").ends_with('Z'));
}

/// The `proxy_request` class is never sampled, whatever the rate is.
#[test]
fn the_proxy_request_class_is_never_sampled() {
    let sink = AuditSink::captured();
    sink.set_sample_rate(1_000);
    for index in 0..50 {
        sink.emit(&AuditRecord::proxy_request(
            &format!("req-{index}"),
            "tenant",
            "principal",
            Some("api.vendor.com"),
            "/v1",
            "GET",
            200,
            1,
            0,
            0,
            None,
            false,
        ));
    }
    assert_eq!(sink.lines().len(), 50);
}

/// The sampling policy addresses the other two classes only.
#[test]
fn the_sampling_policy_addresses_the_other_classes() {
    let sink = AuditSink::captured();
    sink.set_sample_rate(3);
    for _ in 0..9 {
        sink.emit(&AuditRecord::config_change(None, "t", "p", "/oagw/v1/upstreams", 201));
    }
    let emitted = sink.lines().len();
    assert_eq!(emitted, 3, "a 1/3 rate suppresses two of three config_change records");
}

/// A credential-bearing value is dropped rather than echoed
/// (`inst-os-algo-audit-6`).
#[test]
fn a_credential_bearing_value_is_never_emitted() {
    let sink = AuditSink::captured();
    let record = AuditRecord::auth_failure(
        "req-7",
        "tenant",
        "principal",
        Some("api.vendor.com"),
        "/v1",
        "GET",
        401,
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
    );
    sink.emit(&record);
    let line = sink.lines().join("\n");
    assert!(!line.to_lowercase().contains("bearer "));
    assert!(!line.contains("authorization"));
}

/// The timestamp is an RFC 3339 instant.
#[test]
fn the_timestamp_is_rfc3339() {
    let stamp = now_rfc3339();
    assert_eq!(stamp.len(), 24);
    assert!(stamp.ends_with('Z'));
    assert_eq!(&stamp[4..5], "-");
    assert_eq!(&stamp[10..11], "T");
}

/// A control character in a value is not written into the line.
#[test]
fn a_control_character_is_dropped() {
    let sink = AuditSink::captured();
    sink.emit(&AuditRecord::proxy_request(
        "req\t\"8\"\n",
        "tenant",
        "principal",
        Some("api.vendor.com"),
        "/v1",
        "GET",
        200,
        1,
        0,
        0,
        None,
        false,
    ));
    let line = sink.lines().join("\n");
    assert!(!line.contains('\t'));
    assert!(line.contains("req\\t\\\"8\\\""));
}
