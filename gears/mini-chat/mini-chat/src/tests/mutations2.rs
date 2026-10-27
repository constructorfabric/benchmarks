#![allow(clippy::wildcard_imports)]
//! Mutation carry-forward + indexing failure + summary end-to-end.

use uuid::Uuid;

use super::*;
use crate::api::rest::dto::StreamMessageRequest;
use crate::domain::attachment_service::UploadInput;
use crate::domain::stream_service::StreamSetup;
use crate::error::DomainError;
use crate::test_support::CannedStream;

#[tokio::test]
async fn retry_carries_attachments_forward() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, Some("gpt-standard")).await;
    // Upload a document and reference it in the original turn.
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "text/plain")
        .await
        .expect("prepare");
    let att = bed
        .services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            UploadInput {
                filename: "doc.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                bytes: bytes::Bytes::from_static(b"hello"),
            },
        )
        .await
        .expect("upload");
    let rid1 = Uuid::new_v4();
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "about the doc".to_owned(),
                request_id: Some(rid1),
                attachment_ids: Some(vec![att.id]),
                web_search: None,
            },
        )
        .await
        .expect("send");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }

    // Retry: the new user message carries the same attachment.
    let setup = bed
        .services
        .turns
        .retry_turn(&bed.bed.security, chat.id, rid1)
        .await
        .expect("retry");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    let user = page
        .items
        .iter()
        .find(|m| m.role == crate::api::rest::dto::MessageRoleDto::User)
        .expect("user");
    assert_eq!(user.attachments.len(), 1, "attachment carried forward");
    assert_eq!(user.attachments[0].attachment_id, att.id);
}

#[tokio::test]
async fn edit_preserves_attachment_links() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, Some("gpt-standard")).await;
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "text/plain")
        .await
        .expect("prepare");
    let att = bed
        .services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            UploadInput {
                filename: "doc.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                bytes: bytes::Bytes::from_static(b"hello"),
            },
        )
        .await
        .expect("upload");
    let rid1 = Uuid::new_v4();
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "original".to_owned(),
                request_id: Some(rid1),
                attachment_ids: Some(vec![att.id]),
                web_search: None,
            },
        )
        .await
        .expect("send");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }
    let setup = bed
        .services
        .turns
        .edit_turn(&bed.bed.security, chat.id, rid1, "edited".to_owned())
        .await
        .expect("edit");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    let user = page
        .items
        .iter()
        .find(|m| m.role == crate::api::rest::dto::MessageRoleDto::User)
        .expect("user");
    assert_eq!(user.content, "edited");
    assert_eq!(user.attachments.len(), 1);
}

#[tokio::test]
async fn indexing_failure_returns_503_and_row_failed() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, Some("gpt-standard")).await;
    // The stub fails vector-store adds when the marker is set.
    bed.gateway.set_vs_file_status("failed");
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "text/plain")
        .await
        .expect("prepare");
    let err = bed
        .services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            UploadInput {
                filename: "doc.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                bytes: bytes::Bytes::from_static(b"hello"),
            },
        )
        .await;
    let Err(err) = err else {
        panic!("expected indexing failure")
    };
    assert!(matches!(err, DomainError::StorageUnavailable), "{err:?}");
    // The provider file was deleted best-effort, and no attachment is
    // ready.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let ready = bed
        .bed
        .ctx
        .repos
        .attachments
        .ready_with_provider_ids(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id)
        .await
        .expect("ready");
    assert!(ready.is_empty(), "no ready attachments after failure");
    assert!(!bed.gateway.deleted_files.lock().expect("lock").is_empty(), "provider file deleted");
}

#[tokio::test]
async fn summary_end_to_end_via_pipeline() {
    // A small-context model so the assembled context crosses the 80%
    // threshold after a couple of turns.
    let mut policy = crate::test_support::StubPolicy::default();
    let mut m = policy.snapshot.model_catalog[1].clone(); // gpt-standard
    m.context_window = 400;
    m.max_output_tokens = 100;
    m.estimation_budgets.fixed_overhead_tokens = 20;
    policy.snapshot.model_catalog = vec![m.clone()];
    let bed = stream_bed_full(crate::config::MiniChatConfig::default(), std::sync::Arc::new(policy)).await;
    let chat = create_chat(&bed.bed, None, Some("gpt-standard")).await;
    // Three turns so history exceeds the recent limit and truncation fires.
    for i in 0..3 {
        send_and_drain(&bed.services, &bed.bed, chat.id, &format!("question {i} {}", "x".repeat(200)), None).await;
    }
    // The thread summary work item is enqueued during finalization; the
    // pipeline handler commits a summary covering earlier turns.
    wait_until(
        || {
            let conn = bed.bed.ctx.db.conn().expect("conn");
            futures::executor::block_on(
                bed.bed.ctx.repos.summaries.get(&conn, bed.bed.security.subject_tenant_id(), chat.id),
            )
            .expect("get")
            .is_some()
        },
        "summary committed",
    )
    .await;
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let summary = bed
        .bed
        .ctx
        .repos
        .summaries
        .get(&conn, bed.bed.security.subject_tenant_id(), chat.id)
        .await
        .expect("get")
        .expect("summary");
    assert_eq!(summary.summary_text, "summary text");
    // The next turn's stream_started carries thread_summary_applied.
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "after summary", None).await;
    let started = started(&events);
    assert!(started.thread_summary_applied.is_some(), "summary applied");
}
