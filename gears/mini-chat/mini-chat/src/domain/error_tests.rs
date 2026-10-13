#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{DomainError, reasons, resource_types};
use toolkit_canonical_errors::{CanonicalError, Problem};

fn problem(e: DomainError) -> serde_json::Value {
    let ce = CanonicalError::from(e);
    serde_json::to_value(Problem::from(ce)).unwrap()
}

#[test]
fn not_found_reports_resource_type() {
    let p = problem(DomainError::chat_not_found());
    assert_eq!(p["status"], 404);
    assert_eq!(p["context"]["resource_type"], resource_types::CHAT);
    assert!(p.get("code").is_none());
}

#[test]
fn invalid_argument_field_violation() {
    let p = problem(DomainError::invalid_model());
    assert_eq!(p["status"], 400);
    assert_eq!(p["context"]["field_violations"][0]["field"], "model");
    assert_eq!(p["context"]["field_violations"][0]["reason"], reasons::INVALID_MODEL);
}

#[test]
fn aborted_reason_and_precondition() {
    let p = problem(DomainError::aborted(reasons::TURN_ALREADY_RUNNING, "busy"));
    assert_eq!(p["status"], 409);
    assert_eq!(p["context"]["reason"], "turn_already_running");
    let p = problem(DomainError::feature_disabled("web_search"));
    assert_eq!(p["status"], 400);
    assert_eq!(p["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(p["context"]["violations"][0]["type"], "FEATURE_DISABLED");
}

#[test]
fn quota_exhausted_and_unavailable() {
    let p = problem(DomainError::quota_exceeded("tokens"));
    assert_eq!(p["status"], 429);
    assert_eq!(p["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(p["context"]["violations"][0]["description"], "quota_exceeded");
    let p = problem(DomainError::ServiceUnavailable { retry_after_secs: 5, detail: "x".into() });
    assert_eq!(p["status"], 503);
    assert_eq!(p["context"]["retry_after_seconds"], 5);
}

#[test]
fn already_exists_and_forbidden() {
    let p = problem(DomainError::AlreadyExists {
        resource: resource_types::ATTACHMENT,
        name: reasons::ATTACHMENT_LOCKED.into(),
        detail: "locked".into(),
    });
    assert_eq!(p["status"], 409);
    assert_eq!(p["context"]["resource_name"], "attachment_locked");
    let p = problem(DomainError::authz_denied());
    assert_eq!(p["status"], 403);
    assert_eq!(p["context"]["reason"], "AUTHZ_DENIED");
    let p = problem(DomainError::internal("secret db detail"));
    assert_eq!(p["status"], 500);
    assert!(!p.to_string().contains("secret db detail"));
}
