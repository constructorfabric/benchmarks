//! Canonical error contract on the REST surface (RFC 9457 Problem, platform
//! extractor reasons) and the published `OpenAPI` document.
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use common::*;
use http::StatusCode;
use serde_json::{Value, json};
use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};

#[tokio::test(flavor = "multi_thread")]
async fn problem_shape_and_content_type() {
    let h = Harness::new().await;
    let r = h.get(&format!("/mini-chat/v1/chats/{}", uuid::Uuid::new_v4()), &user_a()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.headers["content-type"], "application/problem+json");
    assert_eq!(r.body["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~");
    assert_eq!(r.body["title"], "Not Found");
    assert_eq!(r.body["status"], 404);
    assert!(r.body["detail"].is_string());
    assert!(r.body["context"].is_object());
    assert!(r.body.get("code").is_none(), "no top-level code field");
}

#[tokio::test(flavor = "multi_thread")]
async fn platform_extractor_errors() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let routes = [
        ("POST", "/mini-chat/v1/chats".to_owned()),
        ("PATCH", format!("/mini-chat/v1/chats/{chat}")),
        ("POST", format!("/mini-chat/v1/chats/{chat}/messages:stream")),
    ];
    for (m, uri) in &routes {
        // malformed JSON → 400 json_syntax_error
        let r = h.raw_body(m, uri, &a, Some("application/json"), "{not json").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{m} {uri}");
        assert_eq!(r.body["context"]["field_violations"][0]["field"], "body");
        assert_eq!(r.body["context"]["field_violations"][0]["reason"], "json_syntax_error");
        // no JSON content type → 415
        let r = h.raw_body(m, uri, &a, Some("text/plain"), "{}").await;
        assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{m} {uri}");
        assert_eq!(r.body["context"]["field_violations"][0]["reason"], "missing_json_content_type");
    }
    // schema mismatch → 422 invalid_json_body
    let r = h
        .req(
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            &a,
            Some(json!({"content": "x", "attachment_ids": ["not-a-uuid"]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "invalid_json_body");
    let r = h
        .req("POST", &format!("/mini-chat/v1/chats/{chat}/messages:stream"), &a, Some(json!({})))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = h.req("POST", "/mini-chat/v1/chats", &a, Some(json!({"title": 5}))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    // non-UUID path parameters → 400 invalid_path_params
    for uri in [
        "/mini-chat/v1/chats/nope".to_owned(),
        format!("/mini-chat/v1/chats/{chat}/turns/nope"),
        format!("/mini-chat/v1/chats/{chat}/attachments/nope"),
        "/mini-chat/v1/chats/nope/messages".to_owned(),
    ] {
        let r = h.get(&uri, &a).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(r.body["context"]["field_violations"][0]["reason"], "invalid_path_params", "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unique_violation_maps_to_already_exists() {
    let e = mini_chat::api::rest::error::to_canonical(mini_chat::domain::error::DomainError::UniqueViolation);
    assert_eq!(e.status_code(), 409);
    let p = toolkit_canonical_errors::Problem::from(e);
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["context"]["resource_name"], "unique_violation");
}

fn find_op<'a>(doc: &'a Value, op_id: &str) -> (&'a str, &'a str, &'a Value) {
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            if op.get("operationId").and_then(Value::as_str) == Some(op_id) {
                return (path.as_str(), method.as_str(), op);
            }
        }
    }
    panic!("operation {op_id} not found");
}

#[tokio::test(flavor = "multi_thread")]
async fn openapi_declares_all_operations() {
    let h = Harness::new().await;
    let reg = OpenApiRegistryImpl::new();
    let _router = mini_chat::api::rest::register_routes(Router::new(), &reg, Arc::clone(&h.svc));
    let doc = serde_json::to_value(reg.build_openapi(&OpenApiInfo::default()).unwrap()).unwrap();
    let expected = [
        ("mini_chat.list_chats", "/mini-chat/v1/chats", "get"),
        ("mini_chat.create_chat", "/mini-chat/v1/chats", "post"),
        ("mini_chat.get_chat", "/mini-chat/v1/chats/{id}", "get"),
        ("mini_chat.update_chat", "/mini-chat/v1/chats/{id}", "patch"),
        ("mini_chat.delete_chat", "/mini-chat/v1/chats/{id}", "delete"),
        ("mini_chat.list_messages", "/mini-chat/v1/chats/{id}/messages", "get"),
        ("mini_chat.stream_message", "/mini-chat/v1/chats/{id}/messages:stream", "post"),
        ("mini_chat.upload_attachment", "/mini-chat/v1/chats/{id}/attachments", "post"),
        ("mini_chat.get_attachment", "/mini-chat/v1/chats/{id}/attachments/{attachment_id}", "get"),
        ("mini_chat.delete_attachment", "/mini-chat/v1/chats/{id}/attachments/{attachment_id}", "delete"),
        ("mini_chat.get_turn", "/mini-chat/v1/chats/{id}/turns/{request_id}", "get"),
        ("mini_chat.retry_turn", "/mini-chat/v1/chats/{id}/turns/{request_id}/retry", "post"),
        ("mini_chat.edit_turn", "/mini-chat/v1/chats/{id}/turns/{request_id}", "patch"),
        ("mini_chat.delete_turn", "/mini-chat/v1/chats/{id}/turns/{request_id}", "delete"),
        ("mini_chat.put_reaction", "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction", "put"),
        ("mini_chat.delete_reaction", "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction", "delete"),
        ("mini_chat.list_models", "/mini-chat/v1/models", "get"),
        ("mini_chat.get_model", "/mini-chat/v1/models/{id}", "get"),
        ("mini_chat.get_quota_status", "/mini-chat/v1/quota/status", "get"),
    ];
    for (op_id, path, method) in expected {
        let (p, m, op) = find_op(&doc, op_id);
        assert_eq!((p, m), (path, method), "{op_id}");
        let r503 = &op["responses"]["503"];
        assert!(r503["headers"]["Retry-After"].is_object(), "{op_id} 503 Retry-After");
    }
    let (_, _, create) = find_op(&doc, "mini_chat.create_chat");
    assert_eq!(create["x-throttling-rate-limit-zone"], "rl_mini_chat_chat");
    assert_eq!(create["x-throttling-in-flight-limit-zone"], "ifl_mini_chat_chat");
    assert!(create["responses"]["201"]["headers"]["Location"].is_object());
    let (_, _, stream) = find_op(&doc, "mini_chat.stream_message");
    assert!(stream["responses"]["200"]["content"]["text/event-stream"].is_object());
    let (_, _, upload) = find_op(&doc, "mini_chat.upload_attachment");
    assert!(upload["requestBody"]["content"]["multipart/form-data"].is_object());
    let schemas = doc["components"]["schemas"].as_object().unwrap();
    for s in [
        "ChatDetailDto",
        "MiniChatMessageDto",
        "AttachmentDetailDto",
        "TurnStatusResponse",
        "ModelListDto",
        "QuotaStatusResponse",
        "MiniChatSseEvent",
        "StreamMessageRequest",
        "CreateChatReq",
        "UpdateChatReq",
        "EditTurnRequest",
        "SetReactionReq",
        "MiniChatReactionDto",
    ] {
        assert!(schemas.contains_key(s), "schema {s}");
    }
}
