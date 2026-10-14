//! The deferred and not-implemented scope of the attachments feature stays unimplemented,
//! observed end to end: the document summary columns stay NULL and never reach a response, a
//! soft-deleted attachment is never hard-purged and a deleted document's provider cleanup makes
//! no Vector Stores call (no immediate retrieval exclusion), no dedicated per-turn `file_search`
//! limit or `file_search_calls_exceeded` code exists, `quota_usage.file_search_calls` is never
//! counted, retrieval is never scoped by `attachment_ids` and an Anthropic chat gets no document
//! search.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-deferred-scope` (algorithms
//! `-file-search-contribution` and `-detail-projection` as the places the scope is deferred from);
//! flow `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;

use crate::config::KnowledgeSearchConfig;
use crate::domain::error::StreamErrorCode;
use crate::test_support::attachments::{
    RowSeed, cleanup_message, deleted_payload, outcome_of, rig, seed_row, seed_vector_store,
};
use crate::test_support::chats::at;
use crate::test_support::provider::{
    MockProvider, Reply, Step, completed, delta, fs_done, fs_start,
};
use crate::test_support::quota::all_quota_rows;
use crate::test_support::send::{
    AttachmentSeed, SendRigOptions, names, parse_sse, seed_att, send_rig_with,
};
use crate::test_support::storage_mock::Route;
use crate::test_support::tool_fakes::tool_settings;
use toolkit_db::outbox::LeasedMessageHandler;

#[tokio::test]
async fn the_document_summary_columns_stay_null_through_delete_cleanup_and_reaper() {
    // ADR-0007: `doc_summary`, `summary_model` and `summary_updated_at` are never populated
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let doc = seed_row(&rig.db, &chat, RowSeed::document()).await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("uploaded")
            .updated(crate::test_support::quota::default_now() - time::Duration::seconds(900)),
    )
    .await;
    rig.delete_attachment(chat.id, doc).await;
    let cleanup = rig.cleanup();
    let message = rig.attachment_cleanup_rows().await.remove(0).payload;
    cleanup.handler.handle(&cleanup_message(&message, 0)).await;
    rig.reaper().scan_once().await.unwrap();
    for row in rig.rows(chat.id).await {
        assert_eq!(row.doc_summary, None, "{}", row.id);
        assert_eq!(row.summary_model, None, "{}", row.id);
        assert_eq!(row.summary_updated_at, None, "{}", row.id);
    }
}

#[tokio::test]
async fn no_response_ever_carries_a_summary_field() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig.upload_pdf(chat.id).await;
    let body = reply.json();
    for key in ["doc_summary", "summary_model", "summary_updated_at"] {
        assert!(body.get(key).is_none(), "{key} in {body}");
    }
}

#[tokio::test]
async fn a_soft_deleted_attachment_is_never_hard_purged() {
    // deferred: hard-purge of soft-deleted attachments
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    rig.delete_attachment(chat.id, id).await;
    let message = rig.attachment_cleanup_rows().await.remove(0).payload;
    let cleanup = rig.cleanup();
    let result = cleanup.handler.handle(&cleanup_message(&message, 0)).await;
    assert_eq!(outcome_of(&result), "ok");
    rig.reaper().scan_once().await.unwrap();
    let rows = rig.rows(chat.id).await;
    assert_eq!(rows.len(), 1, "the row is kept");
    assert!(rows[0].deleted_at.is_some());
    assert_eq!(rows[0].cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn a_deleted_document_is_removed_from_the_store_only_through_its_provider_file() {
    // deferred: immediate retrieval exclusion; the chat vector store is never touched per document
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_vector_store(&rig.db, &chat, Some("vs_chat"), "azure_openai", at(0)).await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().file_id("file-1")).await;
    rig.delete_attachment(chat.id, id).await;
    let cleanup = rig.cleanup();
    cleanup
        .handler
        .handle(&cleanup_message(
            &deleted_payload(&chat, id, Some("file-1")),
            0,
        ))
        .await;
    for route in [
        Route::VsCreate,
        Route::VsDelete,
        Route::VsAddFile,
        Route::VsReadFile,
        Route::VsSearch,
    ] {
        assert_eq!(rig.storage.count_of(route), 0, "{route:?}");
    }
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
}

#[test]
fn there_is_no_file_search_limit_error_code() {
    // deferred: a dedicated per-turn `file_search` call limit and `file_search_calls_exceeded`
    for code in StreamErrorCode::ALL {
        assert!(!code.as_str().contains("file_search"), "{}", code.as_str());
    }
}

#[tokio::test]
async fn many_file_search_calls_in_one_turn_are_relayed_without_a_limit_of_the_gear() {
    // inst-fs-no-limit: calls are bounded only by the catalog `max_tool_calls` the provider
    // enforces; the gear counts them and never aborts
    let mut steps: Vec<Step> = Vec::new();
    for _ in 0..12 {
        steps.push(fs_start());
        steps.push(fs_done());
    }
    steps.push(delta("answer"));
    steps.push(completed("answer", (100, 20)));
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_attachment_tools(tool_settings(KnowledgeSearchConfig::default())),
    )
    .await;
    let chat = rig.std_chat().await;
    seed_vector_store(&rig.quota.db, &chat, Some("vs_chat"), "azure_openai", at(0)).await;
    seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    let reply = rig.stream(chat.id, &json!({ "content": "q" })).await;
    let frames = parse_sse(&reply.text);
    assert_eq!(names(&frames).last(), Some(&"done"), "{}", reply.text);
    assert_eq!(
        frames.iter().filter(|frame| frame.event == "tool").count(),
        24
    );
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.file_search_completed_count, 12);
    let events = rig.events().await;
    assert_eq!(
        events[0].file_search_calls, 12,
        "reported in the usage event only"
    );
}

#[tokio::test]
async fn quota_usage_never_counts_file_search_calls() {
    // deferred: a per-user daily `file_search` limit or `quota_usage.file_search_calls` counting
    let upstream = MockProvider::answering(Reply::Sse(vec![
        fs_start(),
        fs_done(),
        fs_start(),
        fs_done(),
        delta("answer"),
        completed("answer", (100, 20)),
    ]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_attachment_tools(tool_settings(KnowledgeSearchConfig::default())),
    )
    .await;
    let chat = rig.std_chat().await;
    seed_vector_store(&rig.quota.db, &chat, Some("vs_chat"), "azure_openai", at(0)).await;
    seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    rig.stream(chat.id, &json!({ "content": "q" })).await;
    rig.wait_terminal(chat.id).await;
    let rows = all_quota_rows(&rig.quota.db).await;
    assert!(!rows.is_empty());
    for row in rows {
        assert_eq!(row.file_search_calls, 0, "{row:?}");
        assert_eq!(row.rag_retrieval_calls, 0, "{row:?}");
    }
}
