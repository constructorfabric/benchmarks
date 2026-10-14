//! Error contract tests: `DomainError` to canonical Problem mapping and the SSE `error`
//! envelope (ADR-0004, DESIGN 3.3 `Error Codes`).
//!
//! Slice S1, algorithm `error-mapping` and definition of done `error-contract`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::response::IntoResponse;
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};

use super::sse_error_data;
use crate::domain::error::{DomainError, EnqueueSite, ResourceKind, StreamErrorCode};
use crate::domain::streaming::provider_errors::sanitize_provider_text;

fn canonical(err: DomainError) -> CanonicalError {
    CanonicalError::from(err)
}

fn problem(err: DomainError) -> Value {
    let problem = Problem::from(canonical(err));
    serde_json::to_value(&problem).expect("problem serializes")
}

fn every_variant() -> Vec<DomainError> {
    vec![
        DomainError::NotFound {
            resource: ResourceKind::Chat,
            id: Some("c".to_owned()),
        },
        DomainError::InvalidModel {
            model: "ghost".to_owned(),
        },
        DomainError::AuthzDenied,
        DomainError::AuthzUnavailable {
            cause: "pdp down".to_owned(),
        },
        DomainError::PolicyUnavailable {
            cause: "plugin down".to_owned(),
        },
        DomainError::UniqueViolation {
            code: "unique_violation".to_owned(),
        },
        DomainError::OutboxPayloadTooLarge {
            site: EnqueueSite::ChatDelete,
        },
        DomainError::internal("driver text"),
    ]
}

// ── Problem shape ────────────────────────────────────────────────────────────

#[test]
fn every_error_is_a_problem_with_the_contract_fields_and_no_top_level_code() {
    for err in every_variant() {
        let label = format!("{err}");
        let value = problem(err);
        let object = value.as_object().unwrap();
        for key in ["type", "title", "status", "detail", "context"] {
            assert!(
                object.contains_key(key),
                "{label}: missing `{key}` in {value}"
            );
        }
        assert!(
            !object.contains_key("code"),
            "{label}: top-level `code` in {value}"
        );
        assert!(
            value["type"].as_str().unwrap().starts_with("gts://"),
            "{label}: {value}"
        );
        assert!(value["status"].is_u64(), "{label}");
        assert!(value["context"].is_object(), "{label}");
    }
}

#[test]
fn problems_are_served_as_problem_json() {
    for err in every_variant() {
        let response = canonical(err).into_response();
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(content_type, "application/problem+json");
    }
}

// ── category, status and reason per row ──────────────────────────────────────

#[test]
fn not_found_names_the_missing_resource_type() {
    let cases = vec![
        (ResourceKind::Chat, "gts.cf.core.mini_chat.chat.v1~"),
        (ResourceKind::Message, "gts.cf.core.mini_chat.message.v1~"),
        (ResourceKind::Turn, "gts.cf.core.mini_chat.turn.v1~"),
        (
            ResourceKind::Attachment,
            "gts.cf.core.mini_chat.attachment.v1~",
        ),
        (ResourceKind::Model, "gts.cf.core.mini_chat.model.v1~"),
    ];
    for (kind, resource_type) in cases {
        let value = problem(DomainError::NotFound {
            resource: kind,
            id: Some("x-1".to_owned()),
        });
        assert_eq!(value["status"], 404, "{kind:?}");
        assert!(
            value["type"].as_str().unwrap().contains("not_found"),
            "{kind:?}: {value}"
        );
        assert_eq!(value["context"]["resource_type"], resource_type, "{kind:?}");
    }
}

#[test]
fn not_found_never_reveals_why_the_resource_is_hidden() {
    let value = problem(DomainError::NotFound {
        resource: ResourceKind::Chat,
        id: None,
    });
    let rendered = value.to_string().to_lowercase();
    for word in [
        "forbidden",
        "foreign",
        "another user",
        "soft-deleted",
        "tenant",
    ] {
        assert!(!rendered.contains(word), "`{word}` leaked in {rendered}");
    }
}

#[test]
fn an_invalid_model_is_a_400_field_violation_with_the_invalid_model_reason() {
    let value = problem(DomainError::InvalidModel {
        model: "ghost".to_owned(),
    });
    assert_eq!(value["status"], 400);
    assert!(value["type"].as_str().unwrap().contains("invalid_argument"));
    let violation = &value["context"]["field_violations"][0];
    assert_eq!(violation["field"], "model");
    assert_eq!(violation["reason"], "INVALID_MODEL");
}

#[test]
fn an_authorization_deny_is_a_403_with_the_authz_denied_reason() {
    let value = problem(DomainError::AuthzDenied);
    assert_eq!(value["status"], 403);
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("permission_denied")
    );
    assert_eq!(value["context"]["reason"], "AUTHZ_DENIED");
}

#[test]
fn a_pdp_failure_is_a_503_with_retry_after_5_and_a_generic_detail() {
    let err = DomainError::AuthzUnavailable {
        cause: "connect ECONNREFUSED 10.0.0.7:8443".to_owned(),
    };
    let value = problem(err);
    assert_eq!(value["status"], 503);
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("service_unavailable")
    );
    assert_eq!(value["context"]["retry_after_seconds"], 5);
    assert!(
        !value.to_string().contains("ECONNREFUSED"),
        "cause leaked: {value}"
    );

    let response = canonical(DomainError::AuthzUnavailable {
        cause: "x".to_owned(),
    })
    .into_response();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers().get("retry-after").unwrap(), "5");
}

#[test]
fn a_policy_plugin_failure_is_a_500_with_a_generic_detail() {
    let value = problem(DomainError::PolicyUnavailable {
        cause: "plugin instance gts.x.provider-secret-detail failed".to_owned(),
    });
    assert_eq!(value["status"], 500);
    assert!(value["type"].as_str().unwrap().contains("internal"));
    assert!(
        !value.to_string().contains("provider-secret-detail"),
        "cause leaked: {value}"
    );
}

#[test]
fn an_unhandled_unique_violation_is_a_409_already_exists_with_the_conflict_code() {
    let value = problem(DomainError::UniqueViolation {
        code: "unique_violation".to_owned(),
    });
    assert_eq!(value["status"], 409);
    assert!(value["type"].as_str().unwrap().contains("already_exists"));
    assert_eq!(value["context"]["resource_name"], "unique_violation");
}

#[test]
fn an_unique_violation_detail_is_generic() {
    let value = problem(DomainError::UniqueViolation {
        code: "unique_violation".to_owned(),
    });
    let detail = value["detail"].as_str().unwrap().to_lowercase();
    for word in ["sqlite", "postgres", "constraint failed", "duplicate key"] {
        assert!(
            !detail.contains(word),
            "driver text `{word}` in detail: {detail}"
        );
    }
}

#[test]
fn outbox_payload_too_large_is_400_for_chat_delete_and_500_elsewhere() {
    let value = problem(DomainError::OutboxPayloadTooLarge {
        site: EnqueueSite::ChatDelete,
    });
    assert_eq!(value["status"], 400);
    assert!(value["type"].as_str().unwrap().contains("invalid_argument"));

    for site in [
        EnqueueSite::AttachmentDelete,
        EnqueueSite::TurnRetry,
        EnqueueSite::TurnEdit,
        EnqueueSite::TurnDelete,
    ] {
        let value = problem(DomainError::OutboxPayloadTooLarge { site });
        assert_eq!(value["status"], 500, "{site:?}");
        assert!(
            value["type"].as_str().unwrap().contains("internal"),
            "{site:?}"
        );
    }
}

#[test]
fn an_internal_error_is_a_500_that_hides_driver_text_and_provider_identifiers() {
    let diagnostic =
        "UNIQUE constraint failed: chats.id; file-abc123XYZ vs_0123456789abcdef assistant-77aa";
    let value = problem(DomainError::internal(diagnostic));
    assert_eq!(value["status"], 500);
    assert!(value["type"].as_str().unwrap().contains("internal"));
    let rendered = value.to_string();
    for needle in [
        "UNIQUE constraint",
        "file-abc123XYZ",
        "vs_0123456789abcdef",
        "assistant-77aa",
    ] {
        assert!(
            !rendered.contains(needle),
            "`{needle}` leaked in {rendered}"
        );
    }
}

#[test]
fn only_the_pdp_failure_carries_retry_after() {
    for err in every_variant() {
        let unavailable = matches!(err, DomainError::AuthzUnavailable { .. });
        let response = canonical(err).into_response();
        assert_eq!(response.headers().contains_key("retry-after"), unavailable);
    }
}

// ── SSE error envelope ───────────────────────────────────────────────────────

#[test]
fn streaming_error_codes_have_the_documented_wire_values() {
    let expected = vec![
        (StreamErrorCode::ProviderError, "provider_error"),
        (StreamErrorCode::ProviderTimeout, "provider_timeout"),
        (StreamErrorCode::RateLimited, "rate_limited"),
        (
            StreamErrorCode::WebSearchCallsExceeded,
            "web_search_calls_exceeded",
        ),
        (
            StreamErrorCode::CodeInterpreterCallsExceeded,
            "code_interpreter_calls_exceeded",
        ),
        (
            StreamErrorCode::AgenticIterationsExceeded,
            "agentic_iterations_exceeded",
        ),
        (StreamErrorCode::UnexpectedToolUse, "unexpected_tool_use"),
        (
            StreamErrorCode::MessagePersistenceFailed,
            "message_persistence_failed",
        ),
        (StreamErrorCode::FinalizationFailed, "finalization_failed"),
        (StreamErrorCode::StreamInterrupted, "stream_interrupted"),
    ];
    assert_eq!(expected.len(), StreamErrorCode::ALL.len());
    for (code, wire) in expected {
        assert_eq!(code.as_str(), wire);
        assert!(StreamErrorCode::ALL.contains(&code));
    }
}

#[test]
fn quota_exceeded_is_not_a_streaming_code() {
    for code in StreamErrorCode::ALL {
        assert_ne!(code.as_str(), "quota_exceeded");
    }
}

#[test]
fn the_sse_error_data_carries_only_code_and_message() {
    for code in StreamErrorCode::ALL {
        let value = sse_error_data(code, "the provider failed");
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 2, "{value}");
        assert_eq!(value["code"], code.as_str());
        assert_eq!(value["message"], "the provider failed");
        for forbidden in [
            "type", "title", "status", "detail", "context", "instance", "trace_id",
        ] {
            assert!(!object.contains_key(forbidden), "{forbidden} in {value}");
        }
    }
}

#[test]
fn the_sse_error_message_is_sanitized() {
    let value = sse_error_data(
        StreamErrorCode::ProviderError,
        "upstream rejected file-AbCdEfGhIjKlMn for store vs_AbCdEfGhIjKlMn and assistant-AbCdEfGhIjKlMn (resp_0aB1c2D3e4F5g6H7)",
    );
    let message = value["message"].as_str().unwrap();
    for needle in [
        "file-AbCdEfGhIjKlMn",
        "vs_AbCdEfGhIjKlMn",
        "assistant-AbCdEfGhIjKlMn",
        "resp_0aB1c2D3e4F5g6H7",
    ] {
        assert!(!message.contains(needle), "`{needle}` leaked: {message}");
    }
}

#[test]
fn the_sse_error_message_keeps_ordinary_words_and_masks_ids_like_the_stored_detail() {
    let kept = sanitize_provider_text("The file-based store failed");
    let value = sse_error_data(StreamErrorCode::ProviderError, &kept);
    assert_eq!(value["message"], "The file-based store failed");

    let masked = sanitize_provider_text("failed for resp_abc123def456ghi");
    let value = sse_error_data(StreamErrorCode::ProviderError, &masked);
    assert_eq!(value["message"], masked);
    assert!(!value["message"].as_str().unwrap().contains("resp_abc"));
}

#[test]
fn the_rest_and_sse_shapes_are_different_envelopes() {
    let rest = problem(DomainError::AuthzDenied);
    let sse = sse_error_data(StreamErrorCode::ProviderError, "boom");
    assert!(rest.get("code").is_none());
    assert!(sse.get("type").is_none());
    assert_eq!(json!(["code", "message"]), {
        let mut keys: Vec<&String> = sse.as_object().unwrap().keys().collect();
        keys.sort();
        json!(keys)
    });
}
