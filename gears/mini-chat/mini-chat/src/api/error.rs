//! `DomainError` to canonical `Problem` mapping (ADR-0004).
//!
//! The machine-readable contract is the category, the HTTP status and the reason fields; the
//! `detail` texts are for humans only. `Internal`-class variants never expose their message.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::DomainError;

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResource;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageResource;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnResource;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentResource;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelResource;

/// `Retry-After` hint for a PDP evaluation failure and for the upload concurrency limit.
const RETRY_AFTER_DEFAULT_SECS: u64 = 5;
/// `Retry-After` hint for a storage backend failure.
const RETRY_AFTER_STORAGE_SECS: u64 = 10;

fn service_unavailable(detail: &str, retry_after_seconds: u64) -> CanonicalError {
    CanonicalError::service_unavailable()
        .with_detail(detail)
        .with_retry_after_seconds(retry_after_seconds)
        .create()
}

#[allow(clippy::too_many_lines)] // one flat arm per variant keeps the table reviewable
impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            // --- not found ---
            DomainError::ChatNotFound { id } => ChatResource::not_found("Chat not found")
                .with_resource(id)
                .create(),
            DomainError::MessageNotFound { id } => MessageResource::not_found("Message not found")
                .with_resource(id)
                .create(),
            DomainError::TurnNotFound { id } => TurnResource::not_found("Turn not found")
                .with_resource(id)
                .create(),
            DomainError::AttachmentNotFound { id } => {
                AttachmentResource::not_found("Attachment not found")
                    .with_resource(id)
                    .create()
            }
            DomainError::ModelNotFound { id } => ModelResource::not_found("Model not found")
                .with_resource(id)
                .create(),

            // --- invalid argument ---
            DomainError::InvalidModel => ChatResource::invalid_argument()
                .with_field_violation("model", "Unknown or disabled model", "INVALID_MODEL")
                .create(),
            DomainError::EmptyContent => ChatResource::invalid_argument()
                .with_field_violation("content", "Content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle => ChatResource::invalid_argument()
                .with_field_violation(
                    "title",
                    "Title must be 1 to 255 characters after trimming",
                    "INVALID_TITLE",
                )
                .create(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "Reaction must be `like` or `dislike`",
                    "INVALID_REACTION",
                )
                .create(),
            DomainError::InvalidAttachment(msg) => ChatResource::invalid_argument()
                .with_field_violation("attachment", msg, "invalid_attachment")
                .create(),
            DomainError::UnsupportedContentType => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "Unsupported content type",
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "Code interpreter is unavailable for this upload",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::MultipartBoundaryRequired => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "Multipart boundary is required",
                    "BOUNDARY_REQUIRED",
                )
                .create(),
            DomainError::MultipartError(_) => AttachmentResource::invalid_argument()
                .with_field_violation("multipart", "Malformed multipart body", "MULTIPART_ERROR")
                .create(),
            DomainError::MissingFile => AttachmentResource::invalid_argument()
                .with_field_violation("file", "Multipart `file` field is missing", "MISSING_FILE")
                .create(),
            DomainError::MissingContentType => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "Multipart `file` part has no content type",
                    "MISSING_CONTENT_TYPE",
                )
                .create(),
            DomainError::VisionNotSupported => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "The chat model does not support image input",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::ChatCleanupPayloadTooLarge(msg) => {
                ChatResource::invalid_argument().with_format(msg).create()
            }

            // --- out of range ---
            DomainError::FileTooLarge => AttachmentResource::out_of_range("File too large")
                .with_field_violation(
                    "content_length",
                    "File exceeds the size limit",
                    "FILE_TOO_LARGE",
                )
                .create(),
            DomainError::TooManyImages => ChatResource::out_of_range("Too many images")
                .with_field_violation(
                    "image_count",
                    "Too many images in one message",
                    "TOO_MANY_IMAGES",
                )
                .create(),
            DomainError::InputTooLong => ChatResource::out_of_range("Input too long")
                .with_field_violation(
                    "content",
                    "Message exceeds the model input limit",
                    "INPUT_TOO_LONG",
                )
                .create(),
            DomainError::ContextBudgetExceeded => {
                ChatResource::out_of_range("Context budget exceeded")
                    .with_field_violation(
                        "content",
                        "Mandatory context does not fit the model budget",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }

            // --- failed precondition ---
            DomainError::FeatureDisabled { subject } => ChatResource::failed_precondition()
                .with_precondition_violation(subject, "Feature is disabled", "FEATURE_DISABLED")
                .create(),
            DomainError::TurnNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation("turn_state", "Turn is not terminal", "STATE")
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "Reactions are only allowed on assistant messages",
                    "STATE",
                )
                .create(),

            // --- authorization ---
            DomainError::AccessDenied => ChatResource::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::ModelAccessDenied => ModelResource::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::AuthzUnavailable => service_unavailable(
                "Authorization is temporarily unavailable",
                RETRY_AFTER_DEFAULT_SECS,
            ),

            // --- aborted ---
            DomainError::TurnAlreadyRunning => {
                ChatResource::aborted("Another turn is running in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict => {
                ChatResource::aborted("The request id conflicts with an earlier turn")
                    .with_reason("request_id_conflict")
                    .create()
            }
            DomainError::NotLatestTurn => TurnResource::aborted("Turn is not the latest")
                .with_reason("NOT_LATEST_TURN")
                .create(),
            DomainError::GenerationInProgress => {
                ChatResource::aborted("A generation is already in progress")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::Replay => TurnResource::aborted("Turn already completed")
                .with_reason("REPLAY")
                .create(),

            // --- already exists ---
            DomainError::AttachmentLocked => {
                AttachmentResource::already_exists("Attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => AttachmentResource::already_exists(
                "The chat's vector store belongs to another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::Conflict { code } => ChatResource::already_exists("Conflicting request")
                .with_resource(code)
                .create(),

            // --- resource exhausted ---
            DomainError::QuotaExceeded { scope } => {
                ChatResource::resource_exhausted("Quota exceeded")
                    .with_quota_violation(scope, "quota_exceeded")
                    .create()
            }
            DomainError::DocumentLimit => {
                AttachmentResource::resource_exhausted("Document limit reached")
                    .with_quota_violation("document_limit", "Per-chat document limit reached")
                    .create()
            }
            DomainError::StorageLimit => {
                AttachmentResource::resource_exhausted("Storage limit reached")
                    .with_quota_violation("storage_limit", "Per-chat storage limit reached")
                    .create()
            }

            // --- service unavailable ---
            DomainError::StorageUnavailable(_) => service_unavailable(
                "Storage backend is temporarily unavailable",
                RETRY_AFTER_STORAGE_SECS,
            ),
            DomainError::UploadConcurrencyLimit => {
                service_unavailable("Too many concurrent uploads", RETRY_AFTER_DEFAULT_SECS)
            }

            // --- internal: the message never reaches the wire ---
            DomainError::OutboxPayloadTooLarge(msg)
            | DomainError::ProviderUnavailable(msg)
            | DomainError::Internal(msg) => CanonicalError::internal(msg).create(),

            DomainError::OData(err) => CanonicalError::from(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::response::IntoResponse;
    use serde_json::{Value, json};
    use toolkit_canonical_errors::CanonicalError;

    use crate::domain::error::DomainError;

    const CHAT: &str = "gts.cf.core.mini_chat.chat.v1~";
    const MESSAGE: &str = "gts.cf.core.mini_chat.message.v1~";
    const TURN: &str = "gts.cf.core.mini_chat.turn.v1~";
    const ATTACHMENT: &str = "gts.cf.core.mini_chat.attachment.v1~";
    const MODEL: &str = "gts.cf.core.mini_chat.model.v1~";

    struct Rendered {
        status: u16,
        retry_after: Option<String>,
        body: Value,
    }

    async fn render(err: DomainError) -> Rendered {
        let response = CanonicalError::from(err).into_response();
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().expect("ascii").to_owned());
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        Rendered {
            status,
            retry_after,
            body: serde_json::from_slice(&bytes).expect("problem json"),
        }
    }

    fn not_found(id: &str) -> String {
        id.to_owned()
    }

    /// `(variant, status, category, [(json pointer into the Problem, expected value)])`.
    type Row = (DomainError, u16, &'static str, Vec<(&'static str, Value)>);

    #[allow(clippy::too_many_lines)]
    fn table() -> Vec<Row> {
        let fv = |rt: &'static str, field: &'static str, reason: &'static str| {
            vec![
                ("/context/resource_type", json!(rt)),
                ("/context/field_violations/0/field", json!(field)),
                ("/context/field_violations/0/reason", json!(reason)),
            ]
        };
        let pre = |rt: &'static str, subject: &'static str, ty: &'static str| {
            vec![
                ("/context/resource_type", json!(rt)),
                ("/context/violations/0/subject", json!(subject)),
                ("/context/violations/0/type", json!(ty)),
            ]
        };
        let nf = |rt: &'static str, id: &'static str| {
            vec![
                ("/context/resource_type", json!(rt)),
                ("/context/resource_name", json!(id)),
            ]
        };
        let aborted = |rt: &'static str, reason: &'static str| {
            vec![
                ("/context/resource_type", json!(rt)),
                ("/context/reason", json!(reason)),
            ]
        };
        let exists = |rt: &'static str, name: &'static str| {
            vec![
                ("/context/resource_type", json!(rt)),
                ("/context/resource_name", json!(name)),
            ]
        };
        let quota = |subject: &'static str| {
            vec![
                ("/context/violations/0/subject", json!(subject)),
                ("/context/violations/0/description", json!("quota_exceeded")),
            ]
        };
        let unavailable = |secs: u64| vec![("/context/retry_after_seconds", json!(secs))];

        vec![
            (
                DomainError::ChatNotFound {
                    id: not_found("c1"),
                },
                404,
                "not_found",
                nf(CHAT, "c1"),
            ),
            (
                DomainError::MessageNotFound { id: "m1".into() },
                404,
                "not_found",
                nf(MESSAGE, "m1"),
            ),
            (
                DomainError::TurnNotFound { id: "t1".into() },
                404,
                "not_found",
                nf(TURN, "t1"),
            ),
            (
                DomainError::AttachmentNotFound { id: "a1".into() },
                404,
                "not_found",
                nf(ATTACHMENT, "a1"),
            ),
            (
                DomainError::ModelNotFound { id: "gpt".into() },
                404,
                "not_found",
                nf(MODEL, "gpt"),
            ),
            (
                DomainError::InvalidModel,
                400,
                "invalid_argument",
                fv(CHAT, "model", "INVALID_MODEL"),
            ),
            (
                DomainError::EmptyContent,
                400,
                "invalid_argument",
                fv(CHAT, "content", "EMPTY_CONTENT"),
            ),
            (
                DomainError::InvalidTitle,
                400,
                "invalid_argument",
                fv(CHAT, "title", "INVALID_TITLE"),
            ),
            (
                DomainError::InvalidReaction,
                400,
                "invalid_argument",
                fv(MESSAGE, "reaction", "INVALID_REACTION"),
            ),
            (
                DomainError::InvalidAttachment("dup".into()),
                400,
                "invalid_argument",
                fv(CHAT, "attachment", "invalid_attachment"),
            ),
            (
                DomainError::UnsupportedContentType,
                400,
                "invalid_argument",
                fv(ATTACHMENT, "content_type", "UNSUPPORTED_CONTENT_TYPE"),
            ),
            (
                DomainError::CodeInterpreterUnavailable,
                400,
                "invalid_argument",
                fv(ATTACHMENT, "file", "CODE_INTERPRETER_UNAVAILABLE"),
            ),
            (
                DomainError::MultipartBoundaryRequired,
                400,
                "invalid_argument",
                fv(ATTACHMENT, "content_type", "BOUNDARY_REQUIRED"),
            ),
            (
                DomainError::MultipartError("eof".into()),
                400,
                "invalid_argument",
                fv(ATTACHMENT, "multipart", "MULTIPART_ERROR"),
            ),
            (
                DomainError::MissingFile,
                400,
                "invalid_argument",
                fv(ATTACHMENT, "file", "MISSING_FILE"),
            ),
            (
                DomainError::MissingContentType,
                400,
                "invalid_argument",
                fv(ATTACHMENT, "content_type", "MISSING_CONTENT_TYPE"),
            ),
            (
                DomainError::VisionNotSupported,
                400,
                "invalid_argument",
                fv(ATTACHMENT, "content_type", "VISION_NOT_SUPPORTED"),
            ),
            (
                DomainError::FileTooLarge,
                400,
                "out_of_range",
                fv(ATTACHMENT, "content_length", "FILE_TOO_LARGE"),
            ),
            (
                DomainError::TooManyImages,
                400,
                "out_of_range",
                fv(CHAT, "image_count", "TOO_MANY_IMAGES"),
            ),
            (
                DomainError::InputTooLong,
                400,
                "out_of_range",
                fv(CHAT, "content", "INPUT_TOO_LONG"),
            ),
            (
                DomainError::ContextBudgetExceeded,
                400,
                "out_of_range",
                fv(CHAT, "content", "CONTEXT_BUDGET_EXCEEDED"),
            ),
            (
                DomainError::FeatureDisabled {
                    subject: "web_search",
                },
                400,
                "failed_precondition",
                pre(CHAT, "web_search", "FEATURE_DISABLED"),
            ),
            (
                DomainError::FeatureDisabled { subject: "images" },
                400,
                "failed_precondition",
                pre(CHAT, "images", "FEATURE_DISABLED"),
            ),
            (
                DomainError::TurnNotTerminal,
                400,
                "failed_precondition",
                pre(TURN, "turn_state", "STATE"),
            ),
            (
                DomainError::ReactionTargetNotAssistant,
                400,
                "failed_precondition",
                pre(MESSAGE, "reaction_target", "STATE"),
            ),
            (
                DomainError::AccessDenied,
                403,
                "permission_denied",
                vec![
                    ("/context/reason", json!("AUTHZ_DENIED")),
                    ("/context/resource_type", json!(CHAT)),
                ],
            ),
            (
                DomainError::ModelAccessDenied,
                403,
                "permission_denied",
                vec![
                    ("/context/reason", json!("AUTHZ_DENIED")),
                    ("/context/resource_type", json!(MODEL)),
                ],
            ),
            (
                DomainError::AuthzUnavailable,
                503,
                "service_unavailable",
                unavailable(5),
            ),
            (
                DomainError::TurnAlreadyRunning,
                409,
                "aborted",
                aborted(CHAT, "turn_already_running"),
            ),
            (
                DomainError::RequestIdConflict,
                409,
                "aborted",
                aborted(CHAT, "request_id_conflict"),
            ),
            (
                DomainError::NotLatestTurn,
                409,
                "aborted",
                aborted(TURN, "NOT_LATEST_TURN"),
            ),
            (
                DomainError::GenerationInProgress,
                409,
                "aborted",
                aborted(CHAT, "GENERATION_IN_PROGRESS"),
            ),
            (DomainError::Replay, 409, "aborted", aborted(TURN, "REPLAY")),
            (
                DomainError::AttachmentLocked,
                409,
                "already_exists",
                exists(ATTACHMENT, "attachment_locked"),
            ),
            (
                DomainError::ProviderMismatch,
                409,
                "already_exists",
                exists(ATTACHMENT, "provider_mismatch"),
            ),
            (
                DomainError::Conflict {
                    code: "unique_violation",
                },
                409,
                "already_exists",
                exists(CHAT, "unique_violation"),
            ),
            (
                DomainError::QuotaExceeded { scope: "tokens" },
                429,
                "resource_exhausted",
                quota("tokens"),
            ),
            (
                DomainError::QuotaExceeded {
                    scope: "web_search",
                },
                429,
                "resource_exhausted",
                quota("web_search"),
            ),
            (
                DomainError::QuotaExceeded {
                    scope: "code_interpreter",
                },
                429,
                "resource_exhausted",
                quota("code_interpreter"),
            ),
            (
                DomainError::DocumentLimit,
                429,
                "resource_exhausted",
                vec![("/context/violations/0/subject", json!("document_limit"))],
            ),
            (
                DomainError::StorageLimit,
                429,
                "resource_exhausted",
                vec![("/context/violations/0/subject", json!("storage_limit"))],
            ),
            (
                DomainError::StorageUnavailable("s3 down".into()),
                503,
                "service_unavailable",
                unavailable(10),
            ),
            (
                DomainError::UploadConcurrencyLimit,
                503,
                "service_unavailable",
                unavailable(5),
            ),
            (
                DomainError::ChatCleanupPayloadTooLarge("payload 70000 > 65536".into()),
                400,
                "invalid_argument",
                vec![
                    ("/detail", json!("payload 70000 > 65536")),
                    ("/context/format", json!("payload 70000 > 65536")),
                ],
            ),
            (
                DomainError::OutboxPayloadTooLarge("big".into()),
                500,
                "internal",
                vec![],
            ),
            (
                DomainError::ProviderUnavailable("down".into()),
                500,
                "internal",
                vec![],
            ),
            (
                DomainError::Internal("boom".into()),
                500,
                "internal",
                vec![],
            ),
            (
                DomainError::OData(toolkit_odata::Error::InvalidFilter("x".into())),
                400,
                "invalid_argument",
                vec![
                    (
                        "/context/resource_type",
                        json!("gts.cf.core.odata.query.v1~"),
                    ),
                    (
                        "/context/field_violations/0/reason",
                        json!("INVALID_FILTER"),
                    ),
                ],
            ),
        ]
    }

    #[tokio::test]
    async fn maps_every_variant() {
        for (err, status, category, checks) in table() {
            let label = format!("{err:?}");
            let r = render(err).await;
            assert_eq!(r.status, status, "{label}: status");
            let ty = r.body["type"].as_str().expect("type");
            assert!(
                ty.ends_with(&format!("cf.core.err.{category}.v1~")),
                "{label}: type {ty}"
            );
            assert!(r.body.get("code").is_none(), "{label}: no top-level code");
            for (pointer, expected) in checks {
                assert_eq!(
                    r.body.pointer(pointer),
                    Some(&expected),
                    "{label}: {pointer} in {}",
                    r.body
                );
            }
        }
    }

    #[tokio::test]
    async fn retry_after_header_only_on_service_unavailable() {
        assert_eq!(
            render(DomainError::AuthzUnavailable)
                .await
                .retry_after
                .as_deref(),
            Some("5")
        );
        assert_eq!(
            render(DomainError::StorageUnavailable("x".into()))
                .await
                .retry_after
                .as_deref(),
            Some("10")
        );
        assert_eq!(
            render(DomainError::UploadConcurrencyLimit)
                .await
                .retry_after
                .as_deref(),
            Some("5")
        );
        assert_eq!(
            render(DomainError::ChatNotFound { id: "x".into() })
                .await
                .retry_after,
            None
        );
    }

    #[tokio::test]
    async fn internal_hides_diagnostic() {
        for err in [
            DomainError::Internal("secret db password leaked".into()),
            DomainError::ProviderUnavailable("secret db password leaked".into()),
            DomainError::OutboxPayloadTooLarge("secret db password leaked".into()),
        ] {
            let r = render(err).await;
            assert_eq!(
                r.body["detail"],
                "An internal error occurred. Please retry later."
            );
            assert!(!r.body.to_string().contains("secret"), "{}", r.body);
        }
    }

    #[tokio::test]
    async fn service_unavailable_detail_is_generic() {
        let r = render(DomainError::StorageUnavailable(
            "bucket s3://internal down".into(),
        ))
        .await;
        assert!(!r.body.to_string().contains("internal down"), "{}", r.body);
    }

    #[test]
    fn db_errors_map_to_domain_errors() {
        use sea_orm::DbErr;
        use toolkit_db::DbError;
        use toolkit_db::secure::ScopeError;

        use crate::domain::error::map_scope_err;

        let unique = DbErr::Custom("UNIQUE constraint failed: chats.id".into());
        let err: DomainError = DbError::Sea(unique.clone()).into();
        assert!(
            matches!(
                err,
                DomainError::Conflict {
                    code: "unique_violation"
                }
            ),
            "{err:?}"
        );
        assert!(matches!(
            map_scope_err(ScopeError::Db(unique)),
            DomainError::Conflict {
                code: "unique_violation"
            }
        ));
        assert!(matches!(
            map_scope_err(ScopeError::Db(DbErr::Custom("connection refused".into()))),
            DomainError::Internal(_)
        ));
        assert!(matches!(
            map_scope_err(ScopeError::Denied("x")),
            DomainError::AccessDenied
        ));
        assert!(matches!(
            map_scope_err(ScopeError::Invalid("x")),
            DomainError::Internal(_)
        ));
        let other: DomainError = DbError::ConnRequestedInsideTx.into();
        assert!(matches!(other, DomainError::Internal(_)));
    }

    #[test]
    fn domain_error_is_send_sync_static_error() {
        fn assert_bounds<T: std::error::Error + Send + Sync + 'static>() {}
        assert_bounds::<DomainError>();
    }
}
