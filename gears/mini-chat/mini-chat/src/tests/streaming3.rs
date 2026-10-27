#![allow(clippy::wildcard_imports)]
//! Streaming tests (batch 3): client disconnect cancellation, tool gating,
//! code interpreter, input length limits.

use uuid::Uuid;

use super::*;
use crate::api::rest::dto::{StreamMessageRequest, WebSearchConfig};
use crate::domain::attachment_service::UploadInput;
use crate::domain::stream_service::StreamSetup;
use crate::error::DomainError;
use crate::test_support::{CannedStream, StubPolicy};

/// Upload a ready XLSX (code-interpreter) attachment.
async fn upload_xlsx(bed: &StreamBed, chat_id: Uuid) -> Uuid {
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat_id, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        .await
        .expect("prepare");
    bed.services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            UploadInput {
                filename: "data.xlsx".to_owned(),
                content_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_owned(),
                bytes: bytes::Bytes::from_static(b"pkzip-ish"),
            },
        )
        .await
        .expect("upload")
        .id
}

#[tokio::test]
async fn disconnect_cancels_and_persists_partial() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Provider sends one delta, then hangs (no terminal).
    bed.gateway.hang_next();
    bed.gateway.stream_script.lock().expect("lock").replace(CannedStream {
        events: vec![(
            "response.output_text.delta".to_owned(),
            r#"{"delta": "partial answer"}"#.to_owned(),
        )],
    });
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: None,
            },
        )
        .await
        .expect("send");
    let StreamSetup::Live {
        receiver,
        request_id,
        cancel,
        ..
    } = setup
    else {
        panic!("live");
    };
    let mut receiver = receiver;
    // Read the first events, then drop the stream (client disconnect).
    let first = receiver.recv().await.expect("started");
    assert!(matches!(first, MiniChatSseEvent::StreamStarted(_)));
    let second = receiver.recv().await.expect("delta");
    assert!(matches!(second, MiniChatSseEvent::Delta(_)));
    drop(receiver);
    // The SSE relay's drop guard cancels the token.
    cancel.cancel();

    // The turn finalizes as cancelled with the partial text persisted.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    wait_until(
        || {
            let conn = bed.bed.ctx.db.conn().expect("conn");
            let turn = futures::executor::block_on(bed.bed.ctx.repos.turns.get_any(
                &conn,
                &toolkit_db::secure::AccessScope::allow_all(),
                bed.bed.security.subject_tenant_id(),
                chat.id,
                request_id,
            ))
            .expect("turn");
            turn.is_some_and(|t| t.state == "cancelled")
        },
        "turn cancelled",
    )
    .await;
    let turn = bed
        .bed
        .ctx
        .repos
        .turns
        .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id, request_id)
        .await
        .expect("turn")
        .expect("row");
    assert_eq!(turn.state, "cancelled");
    assert!(turn.assistant_message_id.is_some(), "partial persisted");
    // The partial assistant message has the accumulated text.
    let msg = bed
        .bed
        .ctx
        .repos
        .messages
        .get(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id, turn.assistant_message_id.expect("msg"))
        .await
        .expect("msg")
        .expect("row");
    assert_eq!(msg.content, "partial answer");
    assert_eq!(msg.role, "assistant");

    // Aborted usage event with estimated settlement.
    wait_until(|| bed.bed.policy.usage_events.lock().expect("lock").len() == 1, "usage").await;
    let usage = bed.bed.policy.usage_events.lock().expect("lock");
    assert_eq!(usage[0].billing_outcome, "aborted");
    assert_eq!(usage[0].settlement_method, "estimated");

    // Turn status reports cancelled.
    let status = bed
        .services
        .turns
        .get_turn(&bed.bed.security, chat.id, request_id)
        .await
        .expect("status");
    assert_eq!(status.state, crate::api::rest::dto::TurnStatusState::Cancelled);
    assert!(status.assistant_message_id.is_some());
}

#[tokio::test]
async fn code_interpreter_tool_wired_for_xlsx() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    upload_xlsx(&bed, chat.id).await;
    send_and_drain(&bed.services, &bed.bed, chat.id, "analyze", None).await;
    let reqs = bed.gateway.requests.lock().expect("lock");
    let tools = reqs[0]["tools"].as_array().expect("tools");
    let ci = tools.iter().find(|t| t["type"] == "code_interpreter").expect("ci tool");
    assert_eq!(ci["container"]["type"], "auto");
    let files = ci["container"]["file_ids"].as_array().expect("file ids");
    assert_eq!(files.len(), 1);
    // include is set for code interpreter outputs.
    let include = reqs[0]["include"].as_array().expect("include");
    assert!(include.iter().any(|i| i == "code_interpreter_call.outputs"));
}

#[tokio::test]
async fn code_interpreter_kill_switch_omits_tool_at_stream() {
    let mut kill = mini_chat_sdk::KillSwitches::default();
    kill.disable_code_interpreter = true;
    let bed = stream_bed_with(std::sync::Arc::new(StubPolicy::new(
        crate::test_support::StubPolicy::default_catalog(),
        kill,
    )))
    .await;
    let chat = create_chat(&bed.bed, None, None).await;
    // XLSX upload rejected while the kill switch is on.
    let err = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        .await;
    assert!(matches!(err, Err(DomainError::CodeInterpreterUnavailable)));
}

#[tokio::test]
async fn daily_code_interpreter_quota() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    upload_xlsx(&bed, chat.id).await;
    // Drive the daily code interpreter counter to the limit (50).
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let now = time::OffsetDateTime::now_utc();
    let day = crate::domain::services::periods::daily_start(now);
    bed.bed
        .ctx
        .repos
        .quota
        .add_reserve(
            &conn,
            bed.bed.security.subject_tenant_id(),
            bed.bed.security.subject_id(),
            crate::infra::storage::repo::quota::BucketKey {
                period_type: "daily",
                period_start: day,
                bucket: "total",
            },
            1,
        )
        .await
        .expect("seed");
    bed.bed
        .ctx
        .repos
        .quota
        .settle(
            &conn,
            bed.bed.security.subject_tenant_id(),
            bed.bed.security.subject_id(),
            crate::infra::storage::repo::quota::BucketKey {
                period_type: "daily",
                period_start: day,
                bucket: "total",
            },
            1,
            1,
            0,
            0,
            0,
            50,
        )
        .await
        .expect("settle");
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "analyze".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::QuotaExceeded("code_interpreter"))));
}

#[tokio::test]
async fn input_too_long_rejected() {
    let mut policy = StubPolicy::default();
    policy.snapshot.model_catalog[0].max_input_tokens = 10; // tiny
    let bed = stream_bed_with(std::sync::Arc::new(policy)).await;
    let chat = create_chat(&bed.bed, None, Some("gpt-premium")).await;
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "x".repeat(1000),
                request_id: None,
                attachment_ids: None,
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::InputTooLong)));
    assert_eq!(provider_calls(&bed.gateway), 0);
}

#[tokio::test]
async fn web_search_not_sent_when_model_lacks_support() {
    let mut policy = StubPolicy::default();
    policy.snapshot.model_catalog[0].general_config.tool_support.web_search = false;
    let bed = stream_bed_with(std::sync::Arc::new(policy)).await;
    let chat = create_chat(&bed.bed, None, Some("gpt-premium")).await;
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: Some(WebSearchConfig { enabled: true }),
            },
        )
        .await
        .expect("send");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }
    let reqs = bed.gateway.requests.lock().expect("lock");
    let tools = &reqs[0]["tools"];
    assert!(
        tools.is_null() || tools.as_array().is_none_or(|t| t.iter().all(|x| x["type"] != "web_search")),
        "no web_search tool: {tools:?}"
    );
}
