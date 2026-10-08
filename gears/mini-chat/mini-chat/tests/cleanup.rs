//! T066: chat deletion triggers reliable provider-side cleanup.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use mini_chat::domain::service::chats::ChatCleanupPayload;
use mini_chat::domain::service::summary::TaskOutcome;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn chat_delete_cleans_files_then_vector_store() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let d1 = h.upload_ok(chat, "a.txt", "text/plain", b"a").await;
    let d2 = h.upload_ok(chat, "b.txt", "text/plain", b"b").await;
    let img = h.upload_ok(chat, "c.png", "image/png", &png(4, 4)).await;
    h.send_body(chat, json!({"content": "x", "attachment_ids": [img]}))
        .await;
    let mut file_ids = Vec::new();
    for id in [d1, d2, img] {
        file_ids.push(db::attachment(&h, id).await.provider_file_id.unwrap());
    }

    let (s, _, _) = h
        .req(
            &h.ctx(),
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // only the chat row is soft-deleted; attachments are marked for cleanup
    assert!(db::chat(&h, chat).await.deleted_at.is_some());

    assert!(
        h.eventually(|| h.deletes().iter().any(|p| p.contains("/vector_stores/")))
            .await,
        "{:?}",
        h.deletes()
    );
    let deletes = h.deletes();
    let vs_pos = deletes
        .iter()
        .position(|p| p.contains("/vector_stores/"))
        .unwrap();
    for f in &file_ids {
        let pos = deletes
            .iter()
            .position(|p| p.ends_with(f.as_str()))
            .unwrap_or_else(|| panic!("{f} not deleted: {deletes:?}"));
        assert!(pos < vs_pos, "files are deleted before the vector store");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for id in [d1, d2, img] {
        assert_eq!(
            db::attachment(&h, id).await.cleanup_status.as_deref(),
            Some("done")
        );
    }
    assert_eq!(
        h.deletes()
            .iter()
            .filter(|p| p.contains("/vector_stores/"))
            .count(),
        1,
        "exactly once"
    );
}

#[tokio::test]
async fn chat_cleanup_retries_on_provider_failure() {
    let mut cfg = default_config();
    cfg["cleanup_worker"] = json!({"max_attempts": 2});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    let d = h.upload_ok(chat, "a.txt", "text/plain", b"a").await;
    *h.provider.delete_status.lock().unwrap() = 503;
    let (s, _, _) = h
        .req(
            &h.ctx(),
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let p = ChatCleanupPayload {
        tenant_id: h.tenant,
        chat_id: chat,
        system_request_id: Uuid::new_v4(),
        reason: "chat_soft_delete".into(),
        chat_deleted_at: time::OffsetDateTime::now_utc(),
    };
    let o = h.core.process_chat_cleanup(&p, 1).await;
    assert!(matches!(o, TaskOutcome::Retry(_)), "{o:?}");
    assert!(db::attachment(&h, d).await.cleanup_attempts >= 1);
    // provider recovers (404 = already gone counts as success)
    *h.provider.delete_status.lock().unwrap() = 404;
    let o = h.core.process_chat_cleanup(&p, 2).await;
    assert!(matches!(o, TaskOutcome::Ok), "{o:?}");
    assert_eq!(
        db::attachment(&h, d).await.cleanup_status.as_deref(),
        Some("done")
    );

    // a chat that is not soft-deleted is rejected
    let live = h.create_chat().await;
    let p2 = ChatCleanupPayload { chat_id: live, ..p };
    assert!(matches!(
        h.core.process_chat_cleanup(&p2, 1).await,
        TaskOutcome::Reject(_)
    ));
}

#[tokio::test]
async fn running_turn_is_not_cancelled_by_chat_delete() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["x".into()],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let mut s = h.open_send(chat, json!({"content": "x"})).await;
    assert!(s.until("delta").await);
    let (st, _, _) = h
        .req(
            &h.ctx(),
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    gate.notify_one();
    assert!(s.until("done").await, "{:?}", s.names());
}
