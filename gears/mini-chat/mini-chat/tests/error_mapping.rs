//! Canonical error mapping: `DomainError` -> `CanonicalError` -> RFC 9457
//! `Problem` (ADR-0004, DESIGN 3.3 "REST error mapping" table).
//!
//! Each row pins HTTP status, canonical category (the `type` URI), and the
//! machine-readable fields clients branch on (`context.reason`,
//! `context.field_violations[]`, `context.violations[]`,
//! `context.resource_type`, `context.resource_name`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::domain::error::{DomainError, ResourceKind};
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};

/// Serialized Problem exactly as it goes on the wire.
fn problem(err: DomainError) -> (u16, Value) {
    let canonical = CanonicalError::from(err);
    let status = canonical.status_code();
    let problem = Problem::from_error(&canonical).expect("problem serializes");
    let json = serde_json::to_value(&problem).expect("problem is JSON");
    assert_eq!(
        json["status"],
        json!(status),
        "Problem.status mirrors the canonical status"
    );
    (status, json)
}

fn assert_category(body: &Value, category: &str) {
    let ty = body["type"].as_str().expect("type is a string");
    assert!(
        ty.contains(&format!("cf.core.err.{category}.v1")),
        "type `{ty}` should be the `{category}` category"
    );
}

fn field_violation(body: &Value, field: &str, reason: &str) {
    let list = body["context"]["field_violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no field_violations in {body}"));
    assert!(
        list.iter()
            .any(|v| v["field"] == json!(field) && v["reason"] == json!(reason)),
        "expected field_violations[{field}].reason = {reason}, got {list:?}"
    );
}

fn precondition_violation(body: &Value, subject: &str, kind: &str) {
    let list = body["context"]["violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no violations in {body}"));
    assert!(
        list.iter()
            .any(|v| v["subject"] == json!(subject) && v["type"] == json!(kind)),
        "expected violations[{{subject: {subject}, type: {kind}}}], got {list:?}"
    );
}

fn quota_violation(body: &Value, subject: &str) {
    let list = body["context"]["violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no violations in {body}"));
    assert!(
        list.iter().any(|v| v["subject"] == json!(subject)),
        "expected a violation for `{subject}`, got {list:?}"
    );
}

// ── envelope ─────────────────────────────────────────────────────────────────

#[test]
fn every_problem_has_the_rfc_9457_envelope_and_no_top_level_code() {
    let samples = vec![
        DomainError::NotFound {
            resource: ResourceKind::Chat,
            id: "c".to_owned(),
        },
        DomainError::AccessDenied { cause: None },
        DomainError::TurnAlreadyRunning,
        DomainError::Internal {
            diagnostic: "x".to_owned(),
            cause: None,
        },
        DomainError::ServiceUnavailable {
            detail: "later".to_owned(),
            retry_after_secs: Some(5),
            cause: None,
        },
    ];
    for err in samples {
        let (_, body) = problem(err);
        let obj = body.as_object().unwrap();
        for key in ["type", "title", "status", "detail", "context"] {
            assert!(obj.contains_key(key), "missing `{key}` in {body}");
        }
        assert!(
            !obj.contains_key("code"),
            "ADR-0004: no top-level `code`: {body}"
        );
    }
}

// ── not_found (404) ──────────────────────────────────────────────────────────

#[test]
fn not_found_names_the_missing_resource_type() {
    for (kind, resource_type) in [
        (ResourceKind::Chat, "gts.cf.core.mini_chat.chat.v1~"),
        (ResourceKind::Message, "gts.cf.core.mini_chat.message.v1~"),
        (ResourceKind::Turn, "gts.cf.core.mini_chat.turn.v1~"),
        (
            ResourceKind::Attachment,
            "gts.cf.core.mini_chat.attachment.v1~",
        ),
        (ResourceKind::Model, "gts.cf.core.mini_chat.model.v1~"),
    ] {
        let (status, body) = problem(DomainError::NotFound {
            resource: kind,
            id: "some-id".to_owned(),
        });
        assert_eq!(status, 404);
        assert_category(&body, "not_found");
        assert_eq!(body["context"]["resource_type"], json!(resource_type));
    }
}

// ── invalid_argument (400) ───────────────────────────────────────────────────

#[test]
fn field_level_validation_errors_carry_stable_reasons() {
    for (err, field, reason) in [
        (DomainError::InvalidModel, "model", "INVALID_MODEL"),
        (
            DomainError::InvalidTitle {
                detail: "too long".to_owned(),
            },
            "title",
            "INVALID_TITLE",
        ),
        (DomainError::EmptyContent, "content", "EMPTY_CONTENT"),
        (DomainError::InvalidReaction, "reaction", "INVALID_REACTION"),
        (
            DomainError::InvalidAttachment,
            "attachment",
            "invalid_attachment",
        ),
    ] {
        let (status, body) = problem(err);
        assert_eq!(status, 400, "{field}/{reason}");
        assert_category(&body, "invalid_argument");
        field_violation(&body, field, reason);
    }
}

#[test]
fn generic_invalid_input_passes_field_and_reason_through() {
    let (status, body) = problem(DomainError::InvalidInput {
        resource: ResourceKind::Chat,
        field: "file",
        reason: "CODE_INTERPRETER_UNAVAILABLE",
        detail: "no code interpreter".to_owned(),
    });
    assert_eq!(status, 400);
    assert_category(&body, "invalid_argument");
    field_violation(&body, "file", "CODE_INTERPRETER_UNAVAILABLE");
}

#[test]
fn invalid_input_reports_the_requested_resource_type() {
    for (kind, resource_type) in [
        (ResourceKind::Chat, "gts.cf.core.mini_chat.chat.v1~"),
        (
            ResourceKind::Attachment,
            "gts.cf.core.mini_chat.attachment.v1~",
        ),
    ] {
        let (status, body) = problem(DomainError::InvalidInput {
            resource: kind,
            field: "file",
            reason: "CODE_INTERPRETER_UNAVAILABLE",
            detail: "no code interpreter".to_owned(),
        });
        assert_eq!(status, 400);
        assert_category(&body, "invalid_argument");
        assert_eq!(body["context"]["resource_type"], json!(resource_type));
        field_violation(&body, "file", "CODE_INTERPRETER_UNAVAILABLE");
    }
}

#[test]
fn out_of_range_reports_the_requested_resource_type() {
    for (kind, resource_type) in [
        (ResourceKind::Chat, "gts.cf.core.mini_chat.chat.v1~"),
        (
            ResourceKind::Attachment,
            "gts.cf.core.mini_chat.attachment.v1~",
        ),
    ] {
        let (status, body) = problem(DomainError::OutOfRange {
            resource: kind,
            field: "file",
            reason: "FILE_TOO_LARGE",
            detail: "too large".to_owned(),
        });
        assert_eq!(status, 400);
        assert_category(&body, "out_of_range");
        assert_eq!(body["context"]["resource_type"], json!(resource_type));
        field_violation(&body, "file", "FILE_TOO_LARGE");
    }
}

// ── out_of_range (400) ───────────────────────────────────────────────────────

#[test]
fn out_of_range_errors_are_400_with_field_violations() {
    for (field, reason) in [
        ("content_length", "FILE_TOO_LARGE"),
        ("image_count", "TOO_MANY_IMAGES"),
        ("content", "INPUT_TOO_LONG"),
        ("content", "CONTEXT_BUDGET_EXCEEDED"),
    ] {
        let (status, body) = problem(DomainError::OutOfRange {
            resource: ResourceKind::Chat,
            field,
            reason,
            detail: "limit".to_owned(),
        });
        assert_eq!(status, 400, "{reason}");
        assert_category(&body, "out_of_range");
        field_violation(&body, field, reason);
    }
}

// ── failed_precondition (400) ────────────────────────────────────────────────

#[test]
fn kill_switch_is_a_failed_precondition_naming_the_feature() {
    for feature in ["web_search", "images"] {
        let (status, body) = problem(DomainError::FeatureDisabled { feature });
        assert_eq!(status, 400);
        assert_category(&body, "failed_precondition");
        precondition_violation(&body, feature, "FEATURE_DISABLED");
    }
}

#[test]
fn state_violations_are_failed_preconditions() {
    let (status, body) = problem(DomainError::InvalidTurnState);
    assert_eq!(status, 400);
    assert_category(&body, "failed_precondition");
    precondition_violation(&body, "turn_state", "STATE");

    let (status, body) = problem(DomainError::InvalidReactionTarget);
    assert_eq!(status, 400);
    assert_category(&body, "failed_precondition");
    precondition_violation(&body, "reaction_target", "STATE");
}

// ── permission_denied (403) ──────────────────────────────────────────────────

#[test]
fn authz_denial_is_403_with_authz_denied_reason_and_no_cause() {
    let secret = std::io::Error::other("pdp-internal-secret");
    let (status, body) = problem(DomainError::AccessDenied {
        cause: Some(Box::new(secret)),
    });
    assert_eq!(status, 403);
    assert_category(&body, "permission_denied");
    assert_eq!(body["context"]["reason"], json!("AUTHZ_DENIED"));
    assert!(
        !body.to_string().contains("pdp-internal-secret"),
        "the cause is logged, never serialized: {body}"
    );
}

// ── aborted (409) ────────────────────────────────────────────────────────────

#[test]
fn concurrency_and_idempotency_conflicts_are_409_aborted_with_reasons() {
    for (err, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict, "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ] {
        let (status, body) = problem(err);
        assert_eq!(status, 409, "{reason}");
        assert_category(&body, "aborted");
        assert_eq!(body["context"]["reason"], json!(reason));
    }
}

// ── already_exists (409) ─────────────────────────────────────────────────────

#[test]
fn already_exists_reports_the_conflict_code_in_resource_name() {
    for (err, name) in [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (
            DomainError::Conflict {
                code: "unique_violation",
            },
            "unique_violation",
        ),
    ] {
        let (status, body) = problem(err);
        assert_eq!(status, 409, "{name}");
        assert_category(&body, "already_exists");
        assert_eq!(body["context"]["resource_name"], json!(name));
    }
}

#[test]
fn unique_conflict_detail_is_generic() {
    let (_, body) = problem(DomainError::Conflict {
        code: "unique_violation",
    });
    let detail = body["detail"].as_str().unwrap();
    assert!(
        !detail.to_lowercase().contains("sqlite") && !detail.to_lowercase().contains("constraint"),
        "driver text must not reach the client: {detail}"
    );
}

// ── resource_exhausted (429) ─────────────────────────────────────────────────

#[test]
fn quota_exhaustion_is_429_with_the_quota_scope_as_subject() {
    for scope in ["tokens", "web_search", "code_interpreter"] {
        let (status, body) = problem(DomainError::QuotaExceeded { scope });
        assert_eq!(status, 429);
        assert_category(&body, "resource_exhausted");
        quota_violation(&body, scope);
        assert_eq!(
            body["context"]["violations"][0]["description"],
            json!("quota_exceeded")
        );
    }
}

#[test]
fn per_chat_limits_are_429_resource_exhausted() {
    for limit in ["document_limit", "storage_limit"] {
        let (status, body) = problem(DomainError::LimitExceeded { limit });
        assert_eq!(status, 429);
        assert_category(&body, "resource_exhausted");
        quota_violation(&body, limit);
    }
}

// ── service_unavailable (503) ────────────────────────────────────────────────

#[test]
fn service_unavailable_carries_the_retry_hint() {
    for secs in [5_u64, 10] {
        let (status, body) = problem(DomainError::ServiceUnavailable {
            detail: "try later".to_owned(),
            retry_after_secs: Some(secs),
            cause: None,
        });
        assert_eq!(status, 503);
        assert_category(&body, "service_unavailable");
        assert_eq!(body["context"]["retry_after_seconds"], json!(secs));
    }
}

#[test]
fn service_unavailable_without_a_hint_omits_it() {
    let (status, body) = problem(DomainError::ServiceUnavailable {
        detail: "try later".to_owned(),
        retry_after_secs: None,
        cause: None,
    });
    assert_eq!(status, 503);
    assert!(body["context"].get("retry_after_seconds").is_none());
}

#[test]
fn service_unavailable_cause_is_not_serialized() {
    let (_, body) = problem(DomainError::ServiceUnavailable {
        detail: "authorization evaluation failed".to_owned(),
        retry_after_secs: Some(5),
        cause: Some(Box::new(std::io::Error::other(
            "pdp-host-10.0.0.7:8443 refused",
        ))),
    });
    assert!(!body.to_string().contains("10.0.0.7"), "{body}");
}

// ── internal (500) ───────────────────────────────────────────────────────────

#[test]
fn internal_errors_are_500_with_a_generic_wire_detail() {
    let (status, body) = problem(DomainError::Internal {
        diagnostic: "postgres://admin:hunter2@db/prod: connection refused".to_owned(),
        cause: Some(Box::new(std::io::Error::other("driver says hunter2"))),
    });
    assert_eq!(status, 500);
    assert_category(&body, "internal");
    let rendered = body.to_string();
    assert!(
        !rendered.contains("hunter2") && !rendered.contains("postgres://"),
        "internal diagnostics are logged, not returned: {rendered}"
    );
}

// ── totality ─────────────────────────────────────────────────────────────────

#[test]
fn every_status_follows_from_the_category() {
    // HTTP status follows from the canonical category, never from a per-gear table.
    let cases: Vec<(DomainError, u16)> = vec![
        (DomainError::InvalidModel, 400),
        (DomainError::InvalidTurnState, 400),
        (DomainError::AccessDenied { cause: None }, 403),
        (
            DomainError::NotFound {
                resource: ResourceKind::Model,
                id: "m".to_owned(),
            },
            404,
        ),
        (DomainError::TurnAlreadyRunning, 409),
        (DomainError::AttachmentLocked, 409),
        (DomainError::QuotaExceeded { scope: "tokens" }, 429),
        (
            DomainError::Internal {
                diagnostic: String::new(),
                cause: None,
            },
            500,
        ),
        (
            DomainError::ServiceUnavailable {
                detail: String::new(),
                retry_after_secs: None,
                cause: None,
            },
            503,
        ),
    ];
    for (err, want) in cases {
        let (status, _) = problem(err);
        assert_eq!(status, want);
    }
}
