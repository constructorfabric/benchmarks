use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::{DomainError, QuotaScope, ResourceKind};

const CHAT: &str = "gts.cf.core.mini_chat.chat.v1~";
const MESSAGE: &str = "gts.cf.core.mini_chat.message.v1~";
const TURN: &str = "gts.cf.core.mini_chat.turn.v1~";
const ATTACHMENT: &str = "gts.cf.core.mini_chat.attachment.v1~";
const MODEL: &str = "gts.cf.core.mini_chat.model.v1~";

/// Serialize the mapped error exactly as the wire sees it.
fn wire(e: DomainError) -> Value {
    let canonical = CanonicalError::from(e);
    let problem = Problem::from(canonical);
    serde_json::to_value(problem).expect("serialize problem")
}

fn category(v: &Value) -> &str {
    v["type"]
        .as_str()
        .and_then(|t| t.rsplit("cf.core.err.").next())
        .and_then(|t| t.strip_suffix(".v1~"))
        .expect("canonical type")
}

fn violation_reason<'a>(v: &'a Value, field: &str) -> Option<&'a str> {
    v["context"]["field_violations"]
        .as_array()?
        .iter()
        .find(|f| f["field"] == field)
        .and_then(|f| f["reason"].as_str())
}

#[test]
fn not_found_names_the_missing_resource_type() {
    for (kind, gts) in [
        (ResourceKind::Chat, CHAT),
        (ResourceKind::Message, MESSAGE),
        (ResourceKind::Turn, TURN),
        (ResourceKind::Attachment, ATTACHMENT),
        (ResourceKind::Model, MODEL),
    ] {
        let v = wire(DomainError::NotFound { resource: kind });
        assert_eq!(v["status"], 404, "{kind:?}");
        assert_eq!(category(&v), "not_found");
        assert_eq!(v["context"]["resource_type"], gts);
        assert_eq!(kind.gts_id(), gts);
    }
}

#[test]
fn invalid_model_is_400_with_model_violation() {
    let v = wire(DomainError::InvalidModel);
    assert_eq!(v["status"], 400);
    assert_eq!(category(&v), "invalid_argument");
    assert_eq!(violation_reason(&v, "model"), Some("INVALID_MODEL"));
}

#[test]
fn simple_field_validation_errors() {
    for (e, field, reason) in [
        (DomainError::EmptyContent, "content", "EMPTY_CONTENT"),
        (DomainError::InvalidTitle, "title", "INVALID_TITLE"),
        (DomainError::InvalidReaction, "reaction", "INVALID_REACTION"),
        (
            DomainError::InvalidAttachment,
            "attachment",
            "invalid_attachment",
        ),
        (
            DomainError::UnsupportedContentType,
            "content_type",
            "UNSUPPORTED_CONTENT_TYPE",
        ),
        (
            DomainError::VisionNotSupported,
            "model",
            "VISION_NOT_SUPPORTED",
        ),
        (
            DomainError::CodeInterpreterUnavailable,
            "file",
            "CODE_INTERPRETER_UNAVAILABLE",
        ),
    ] {
        let v = wire(e);
        assert_eq!(v["status"], 400, "{reason}");
        assert_eq!(category(&v), "invalid_argument", "{reason}");
        assert_eq!(violation_reason(&v, field), Some(reason));
    }
}

#[test]
fn code_interpreter_unavailable_reports_the_attachment_type() {
    let v = wire(DomainError::CodeInterpreterUnavailable);
    assert_eq!(v["context"]["resource_type"], ATTACHMENT);
}

#[test]
fn multipart_errors_carry_their_field_and_reason() {
    let v = wire(DomainError::Multipart {
        reason: "BOUNDARY_REQUIRED",
        field: "content_type",
    });
    assert_eq!(v["status"], 400);
    assert_eq!(category(&v), "invalid_argument");
    assert_eq!(
        violation_reason(&v, "content_type"),
        Some("BOUNDARY_REQUIRED")
    );
    assert_eq!(v["context"]["resource_type"], ATTACHMENT);

    let v = wire(DomainError::Multipart {
        reason: "MULTIPART_ERROR",
        field: "multipart",
    });
    assert_eq!(violation_reason(&v, "multipart"), Some("MULTIPART_ERROR"));
}

#[test]
fn feature_disabled_is_a_failed_precondition_violation() {
    let v = wire(DomainError::FeatureDisabled {
        subject: "web_search",
    });
    assert_eq!(v["status"], 400);
    assert_eq!(category(&v), "failed_precondition");
    let violation = &v["context"]["violations"][0];
    assert_eq!(violation["subject"], "web_search");
    assert_eq!(violation["type"], "FEATURE_DISABLED");

    let v = wire(DomainError::FeatureDisabled { subject: "images" });
    assert_eq!(v["context"]["violations"][0]["subject"], "images");
}

#[test]
fn state_preconditions() {
    let v = wire(DomainError::TurnNotTerminal);
    assert_eq!(v["status"], 400);
    assert_eq!(category(&v), "failed_precondition");
    assert_eq!(v["context"]["violations"][0]["subject"], "turn_state");
    assert_eq!(v["context"]["violations"][0]["type"], "STATE");

    let v = wire(DomainError::ReactionTargetNotAssistant);
    assert_eq!(v["status"], 400);
    assert_eq!(category(&v), "failed_precondition");
    assert_eq!(v["context"]["violations"][0]["subject"], "reaction_target");
    assert_eq!(v["context"]["violations"][0]["type"], "STATE");
}

#[test]
fn out_of_range_errors() {
    for (e, field, reason) in [
        (
            DomainError::FileTooLarge,
            "content_length",
            "FILE_TOO_LARGE",
        ),
        (DomainError::TooManyImages, "image_count", "TOO_MANY_IMAGES"),
        (DomainError::InputTooLong, "content", "INPUT_TOO_LONG"),
        (
            DomainError::ContextBudgetExceeded,
            "content",
            "CONTEXT_BUDGET_EXCEEDED",
        ),
    ] {
        let v = wire(e);
        assert_eq!(v["status"], 400, "{reason}");
        assert_eq!(category(&v), "out_of_range", "{reason}");
        assert_eq!(violation_reason(&v, field), Some(reason));
    }
}

#[test]
fn authz_denied_is_403_with_reason() {
    let v = wire(DomainError::AuthzDenied);
    assert_eq!(v["status"], 403);
    assert_eq!(category(&v), "permission_denied");
    assert_eq!(v["context"]["reason"], "AUTHZ_DENIED");
}

#[test]
fn not_requester_is_403_authz_denied() {
    let v = wire(DomainError::NotRequester);
    assert_eq!(v["status"], 403);
    assert_eq!(category(&v), "permission_denied");
    assert_eq!(v["context"]["reason"], "AUTHZ_DENIED");
}

#[test]
fn authz_unavailable_is_503_with_retry_after_5() {
    let v = wire(DomainError::AuthzUnavailable);
    assert_eq!(v["status"], 503);
    assert_eq!(category(&v), "service_unavailable");
    assert_eq!(v["context"]["retry_after_seconds"], 5);
}

#[test]
fn upload_concurrency_is_503_with_retry_after_5() {
    let v = wire(DomainError::UploadConcurrency);
    assert_eq!(v["status"], 503);
    assert_eq!(v["context"]["retry_after_seconds"], 5);
}

#[test]
fn storage_unavailable_is_503_with_retry_after_10() {
    let v = wire(DomainError::StorageUnavailable);
    assert_eq!(v["status"], 503);
    assert_eq!(category(&v), "service_unavailable");
    assert_eq!(v["context"]["retry_after_seconds"], 10);
}

#[test]
fn aborted_conflicts_carry_a_reason() {
    for (e, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict, "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ] {
        let v = wire(e);
        assert_eq!(v["status"], 409, "{reason}");
        assert_eq!(category(&v), "aborted", "{reason}");
        assert_eq!(v["context"]["reason"], reason);
    }
}

#[test]
fn already_exists_conflicts_carry_a_resource_name() {
    for (e, name) in [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (DomainError::UniqueViolation, "unique_violation"),
    ] {
        let v = wire(e);
        assert_eq!(v["status"], 409, "{name}");
        assert_eq!(category(&v), "already_exists", "{name}");
        assert_eq!(v["context"]["resource_name"], name);
    }
}

#[test]
fn quota_exceeded_names_the_scope() {
    for (scope, subject) in [
        (QuotaScope::Tokens, "tokens"),
        (QuotaScope::WebSearch, "web_search"),
        (QuotaScope::CodeInterpreter, "code_interpreter"),
    ] {
        let v = wire(DomainError::QuotaExceeded { scope });
        assert_eq!(v["status"], 429, "{subject}");
        assert_eq!(category(&v), "resource_exhausted");
        assert_eq!(v["context"]["violations"][0]["subject"], subject);
        assert_eq!(
            v["context"]["violations"][0]["description"],
            "quota_exceeded"
        );
    }
}

#[test]
fn document_and_storage_limits_are_429() {
    let v = wire(DomainError::DocumentLimit);
    assert_eq!(v["status"], 429);
    assert_eq!(category(&v), "resource_exhausted");
    assert_eq!(v["context"]["violations"][0]["subject"], "document_limit");

    let v = wire(DomainError::StorageLimit);
    assert_eq!(v["status"], 429);
    assert_eq!(v["context"]["violations"][0]["subject"], "storage_limit");
}

#[test]
fn outbox_payload_too_large_is_400_only_for_chat_delete() {
    let v = wire(DomainError::OutboxPayloadTooLarge {
        during_chat_delete: true,
    });
    assert_eq!(v["status"], 400);
    assert_eq!(category(&v), "invalid_argument");
    assert_eq!(v["context"]["format"], v["detail"]);
    assert!(
        v["context"]["format"]
            .as_str()
            .is_some_and(|f| !f.is_empty())
    );

    let v = wire(DomainError::OutboxPayloadTooLarge {
        during_chat_delete: false,
    });
    assert_eq!(v["status"], 500);
    assert_eq!(category(&v), "internal");
}

#[test]
fn internal_errors_do_not_leak_their_message() {
    for e in [
        DomainError::Internal("secret db detail resp_abc123".to_owned()),
        DomainError::ProviderResolution("provider vs_abcdefghijkl".to_owned()),
        DomainError::PolicySnapshotGone("secret db detail".to_owned()),
    ] {
        let v = wire(e);
        assert_eq!(v["status"], 500);
        assert_eq!(category(&v), "internal");
        let text = v.to_string();
        assert!(!text.contains("secret db detail"), "{text}");
        assert!(!text.contains("vs_abcdefghijkl"), "{text}");
    }
}

#[test]
fn unique_violation_context_is_exactly_type_and_conflict_code() {
    let v = wire(DomainError::UniqueViolation);
    assert_eq!(
        v["context"],
        json!({"resource_type": CHAT, "resource_name": "unique_violation"})
    );
}

#[test]
fn odata_query_errors_keep_the_odata_resource_type() {
    let cases = [
        (
            toolkit_odata::Error::InvalidFilter("unknown field".to_owned()),
            "$filter",
            "INVALID_FILTER",
        ),
        (
            toolkit_odata::Error::InvalidOrderByField("model".to_owned()),
            "$orderby",
            "INVALID_ORDERBY_FIELD",
        ),
        (
            toolkit_odata::Error::InvalidCursor,
            "cursor",
            "INVALID_CURSOR",
        ),
    ];
    for (err, field, reason) in cases {
        let v = wire(DomainError::from(err));
        assert_eq!(v["status"], 400, "{reason}");
        assert_eq!(category(&v), "invalid_argument");
        assert_eq!(v["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
        assert_eq!(violation_reason(&v, field), Some(reason));
    }
    let v = wire(DomainError::from(toolkit_odata::Error::Db(
        "boom".to_owned(),
    )));
    assert_eq!(v["status"], 500);
}
