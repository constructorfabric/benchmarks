use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;

use super::*;

async fn render(e: DomainError) -> (u16, Option<String>, Value) {
    let resp = CanonicalError::from(e).into_response();
    let status = resp.status().as_u16();
    let retry = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, retry, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn not_found_names_the_resource_type() {
    for (res, ty) in [
        (Res::Chat, "gts.cf.core.mini_chat.chat.v1~"),
        (Res::Message, "gts.cf.core.mini_chat.message.v1~"),
        (Res::Turn, "gts.cf.core.mini_chat.turn.v1~"),
        (Res::Attachment, "gts.cf.core.mini_chat.attachment.v1~"),
        (Res::Model, "gts.cf.core.mini_chat.model.v1~"),
    ] {
        let (status, _, body) = render(DomainError::not_found(res, "x")).await;
        assert_eq!(status, 404);
        assert_eq!(body["context"]["resource_type"], ty);
        assert!(body.get("code").is_none());
    }
}

#[tokio::test]
async fn invalid_argument_and_out_of_range_field_violations() {
    let (status, _, body) = render(DomainError::invalid_model(Res::Chat)).await;
    assert_eq!(status, 400);
    let v = &body["context"]["field_violations"][0];
    assert_eq!(
        (v["field"].as_str(), v["reason"].as_str()),
        (Some("model"), Some("INVALID_MODEL"))
    );
    let (status, _, body) = render(DomainError::out_of_range(
        Res::Message,
        "content",
        "INPUT_TOO_LONG",
        "too long",
    ))
    .await;
    assert_eq!(status, 400);
    assert_eq!(
        body["context"]["field_violations"][0]["reason"],
        "INPUT_TOO_LONG"
    );
}

#[tokio::test]
async fn preconditions_and_aborted() {
    let (status, _, body) = render(DomainError::feature_disabled("web_search")).await;
    assert_eq!(status, 400);
    let v = &body["context"]["violations"][0];
    assert_eq!(
        (v["subject"].as_str(), v["type"].as_str()),
        (Some("web_search"), Some("FEATURE_DISABLED"))
    );
    for (e, reason) in [
        (DomainError::turn_already_running(), "turn_already_running"),
        (DomainError::request_id_conflict(), "request_id_conflict"),
        (
            DomainError::aborted(Res::Turn, "NOT_LATEST_TURN", "x"),
            "NOT_LATEST_TURN",
        ),
    ] {
        let (status, _, body) = render(e).await;
        assert_eq!(status, 409);
        assert_eq!(body["context"]["reason"], reason);
    }
}

#[tokio::test]
async fn quota_and_chat_limits() {
    for (scope, subject) in [
        (QuotaScope::Tokens, "tokens"),
        (QuotaScope::WebSearch, "web_search"),
        (QuotaScope::CodeInterpreter, "code_interpreter"),
    ] {
        let (status, _, body) = render(DomainError::QuotaExceeded(scope)).await;
        assert_eq!(status, 429);
        let v = &body["context"]["violations"][0];
        assert_eq!(
            (v["subject"].as_str(), v["description"].as_str()),
            (Some(subject), Some("quota_exceeded"))
        );
    }
    let (status, _, body) = render(DomainError::ChatLimit {
        subject: "document_limit",
        description: "too many documents".into(),
    })
    .await;
    assert_eq!(status, 429);
    assert_eq!(
        body["context"]["violations"][0]["subject"],
        "document_limit"
    );
}

#[tokio::test]
async fn conflicts_carry_resource_name() {
    let (status, _, body) = render(DomainError::AlreadyExists {
        res: Res::Attachment,
        name: "attachment_locked".into(),
        detail: "locked".into(),
    })
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["context"]["resource_name"], "attachment_locked");
}

#[tokio::test]
async fn authz_and_unavailable() {
    let (status, _, body) = render(DomainError::AuthzDenied).await;
    assert_eq!(status, 403);
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
    let (status, retry, body) =
        render(DomainError::AuthzUnavailable("pdp down at 10.0.0.1".into())).await;
    assert_eq!((status, retry.as_deref()), (503, Some("5")));
    assert!(!body.to_string().contains("10.0.0.1"));
    let (status, retry, body) = render(DomainError::storage_unavailable(
        "vs_secret123456789 failed",
    ))
    .await;
    assert_eq!((status, retry.as_deref()), (503, Some("10")));
    assert!(!body.to_string().contains("vs_secret"));
    let (status, retry, _) = render(DomainError::Contention("busy".into())).await;
    assert_eq!(status, 503);
    assert!(retry.is_some());
}

#[tokio::test]
async fn internal_hides_the_diagnostic() {
    let (status, _, body) = render(DomainError::internal("driver said: password=hunter2")).await;
    assert_eq!(status, 500);
    assert!(!body.to_string().contains("hunter2"));
}
