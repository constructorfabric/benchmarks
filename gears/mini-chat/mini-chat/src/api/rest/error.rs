//! `DomainError` → canonical `Problem` mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, Res};

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

/// Dispatch a builder expression over the resource marker of `res`.
macro_rules! with_res {
    ($res:expr, $m:ident => $body:expr) => {
        match $res {
            Res::Chat => {
                type $m = ChatResource;
                $body
            }
            Res::Message => {
                type $m = MessageResource;
                $body
            }
            Res::Turn => {
                type $m = TurnResource;
                $body
            }
            Res::Attachment => {
                type $m = AttachmentResource;
                $body
            }
            Res::Model => {
                type $m = ModelResource;
                $body
            }
        }
    };
}

fn res_name(res: Res) -> &'static str {
    match res {
        Res::Chat => "chat",
        Res::Message => "message",
        Res::Turn => "turn",
        Res::Attachment => "attachment",
        Res::Model => "model",
    }
}

impl From<DomainError> for CanonicalError {
    #[allow(clippy::cognitive_complexity)] // flat one-to-one mapping of every domain error variant
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound(res) => with_res!(res, M => M::not_found(format!("{} not found", res_name(res)))
                .with_resource(res_name(res))
                .create()),
            DomainError::InvalidArgument { res, field, reason, description } => {
                with_res!(res, M => M::invalid_argument()
                    .with_field_violation(field, description, reason)
                    .create())
            }
            DomainError::OutOfRange { res, field, reason, description } => {
                with_res!(res, M => M::out_of_range(description.clone())
                    .with_field_violation(field, description, reason)
                    .create())
            }
            DomainError::FailedPrecondition { res, subject, kind, description } => {
                with_res!(res, M => M::failed_precondition()
                    .with_precondition_violation(subject, description, kind)
                    .create())
            }
            DomainError::PermissionDenied => {
                ChatResource::permission_denied().with_reason("AUTHZ_DENIED").create()
            }
            DomainError::PdpUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .create(),
            DomainError::Aborted { res, reason, detail } => {
                with_res!(res, M => M::aborted(detail).with_reason(reason).create())
            }
            DomainError::AlreadyExists { res, resource_name, detail } => {
                with_res!(res, M => M::already_exists(detail).with_resource(resource_name).create())
            }
            DomainError::QuotaExceeded { scope } => ChatResource::resource_exhausted(format!(
                "Quota exceeded ({scope})"
            ))
            .with_quota_violation(scope, "quota_exceeded")
            .create(),
            DomainError::LimitExceeded { res, subject, description } => {
                with_res!(res, M => M::resource_exhausted(description.clone())
                    .with_quota_violation(subject, description)
                    .create())
            }
            DomainError::ServiceUnavailable { retry_after, detail } => {
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(retry_after)
                    .with_detail(detail)
                    .create()
            }
            DomainError::Internal(msg) => {
                tracing::error!(error = %msg, "mini-chat internal error");
                CanonicalError::internal(msg).create()
            }
            DomainError::Database(e) => {
                tracing::error!(error = %e, "mini-chat database error");
                CanonicalError::internal(format!("database error: {e}")).create()
            }
            DomainError::Canonical(err) => err,
        }
    }
}

#[cfg(test)]
mod tests {
    use toolkit_canonical_errors::Problem;

    use super::*;

    fn problem(err: DomainError) -> serde_json::Value {
        let canon: CanonicalError = err.into();
        serde_json::to_value(Problem::from(canon)).expect("problem json")
    }

    #[test]
    fn not_found_carries_resource_type() {
        let p = problem(DomainError::NotFound(Res::Attachment));
        assert_eq!(p["status"], 404);
        assert_eq!(p["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
    }

    #[test]
    fn invalid_model_is_field_violation() {
        let p = problem(DomainError::invalid_model());
        assert_eq!(p["status"], 400);
        assert_eq!(p["context"]["field_violations"][0]["field"], "model");
        assert_eq!(p["context"]["field_violations"][0]["reason"], "INVALID_MODEL");
    }

    #[test]
    fn quota_exceeded_is_429_with_subject() {
        let p = problem(DomainError::QuotaExceeded { scope: "web_search".into() });
        assert_eq!(p["status"], 429);
        assert_eq!(p["context"]["violations"][0]["subject"], "web_search");
        assert_eq!(p["context"]["violations"][0]["description"], "quota_exceeded");
    }

    #[test]
    fn aborted_reason() {
        let p = problem(DomainError::aborted(Res::Chat, "turn_already_running", "busy"));
        assert_eq!(p["status"], 409);
        assert_eq!(p["context"]["reason"], "turn_already_running");
    }

    #[test]
    fn precondition_violation_shape() {
        let p = problem(DomainError::feature_disabled("web_search"));
        assert_eq!(p["status"], 400);
        assert_eq!(p["context"]["violations"][0]["subject"], "web_search");
        assert_eq!(p["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    }

    #[test]
    fn pdp_unavailable_is_503_retry_after_5() {
        let p = problem(DomainError::PdpUnavailable);
        assert_eq!(p["status"], 503);
        assert_eq!(p["context"]["retry_after_seconds"], 5);
    }

    #[test]
    fn denied_is_403_authz_denied() {
        let p = problem(DomainError::PermissionDenied);
        assert_eq!(p["status"], 403);
        assert_eq!(p["context"]["reason"], "AUTHZ_DENIED");
    }

    #[test]
    fn attachment_locked_is_already_exists() {
        let p = problem(DomainError::AlreadyExists {
            res: Res::Attachment,
            resource_name: "attachment_locked".into(),
            detail: "locked".into(),
        });
        assert_eq!(p["status"], 409);
        assert_eq!(p["context"]["resource_name"], "attachment_locked");
    }
}
