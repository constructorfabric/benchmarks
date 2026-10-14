//! The ask-about-files flow over the real router, SQLite, the real quota service, the real provider
//! task and a mock OAGW that records the proxied request: the request of a chat with a ready
//! document carries `file_search` over the chat vector store with the catalog `max_num_results`
//! and the guard, an XLSX-only chat sends `code_interpreter` with the ready file ids and no
//! `file_search`, `tool` events named `file_search` are relayed, file citations carry the
//! attachment id and the filename (a deleted attachment and an unknown file are omitted), an image
//! in `attachment_ids` is model input as `input_image` with its internal file id, `attachment_ids`
//! never scopes retrieval, and no provider identifier reaches the client.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! flow `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files` (`inst-ask-send`,
//! `inst-ask-facts`, `inst-ask-contributions`, `inst-ask-validation`, `inst-ask-request`,
//! `inst-ask-citations`, `inst-ask-return`), algorithms `-file-search-contribution`
//! (`inst-fs-citation-map`, `inst-fs-no-limit`), `-code-interpreter-contribution` and
//! `-ready-document-fact`; definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-tool-contributions` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-deferred-scope` (no attribute filter, no
//! attachment-scoped retrieval, no per-turn limit).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::store::{AttachmentStore, DeleteOutcome, DeleteRequest};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::USER_A1;
use crate::test_support::attachments::seed_vector_store;
use crate::test_support::chats::at;
use crate::test_support::provider::{
    MockProvider, Reply, completed_parts, delta, file_annotation, fs_done, fs_start, happy_steps,
    output_part,
};
use crate::test_support::send::{
    AttachmentSeed, FILE_GUARD, SendRig, SendRigOptions, names, parse_sse, seed_att, send_rig_with,
    t,
};
use crate::test_support::tool_fakes::tool_settings;

const STORE: &str = "vs_chat_sse";

async fn rig_for(upstream: &Arc<MockProvider>) -> SendRig {
    send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_attachment_tools(tool_settings(KnowledgeSearchConfig::default())),
    )
    .await
}

async fn bound_chat(rig: &SendRig) -> ChatRecord {
    let chat = rig.std_chat().await;
    seed_vector_store(&rig.quota.db, &chat, Some(STORE), "azure_openai", at(0)).await;
    chat
}

fn question() -> Value {
    json!({ "content": "what does the handbook say?" })
}

fn tool_of<'a>(body: &'a Value, kind: &str) -> Option<&'a Value> {
    body["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["type"] == kind))
}

fn all_keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                out.push(key.clone());
                all_keys(inner, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| all_keys(item, out)),
        _ => {}
    }
}

fn cited_answer(file_ids: &[(&str, &str)]) -> Vec<crate::test_support::provider::Step> {
    let annotations: Vec<Value> = file_ids
        .iter()
        .map(|(id, name)| file_annotation(id, name))
        .collect();
    vec![
        fs_start(),
        fs_done(),
        delta("The handbook says "),
        completed_parts(
            &[output_part(
                "The handbook says vacation is 25 days",
                &annotations,
            )],
            (100, 20),
        ),
    ]
}

// ── the request ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_turn_in_a_chat_with_a_ready_document_carries_file_search_over_the_chat_store() {
    // inst-ask-contributions, inst-ask-request, inst-fs-definition, inst-fs-guard
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    let reply = rig.stream(chat.id, &question()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let call = upstream.only_call();
    assert_eq!(
        tool_of(&call.body, "file_search").expect("file_search"),
        &json!({ "type": "file_search", "vector_store_ids": [STORE], "max_num_results": 5 })
    );
    assert!(
        call.body["instructions"]
            .as_str()
            .unwrap()
            .ends_with(FILE_GUARD),
        "the guard ends the instructions"
    );
}

#[tokio::test]
async fn a_chat_without_a_ready_document_sends_no_file_search_and_no_guard() {
    // inst-fs-no-docs
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document").status("uploaded"),
    )
    .await;
    rig.stream(chat.id, &question()).await;
    let call = upstream.only_call();
    assert!(call.body.get("tools").is_none(), "{}", call.body);
    assert!(
        !call.body["instructions"]
            .as_str()
            .unwrap()
            .contains(FILE_GUARD)
    );
}

#[tokio::test]
async fn an_xlsx_only_chat_sends_code_interpreter_with_the_file_ids_and_no_file_search() {
    // inst-ask-facts, inst-ci-definition: `container` `{type: auto, file_ids}` and the `include`
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .no_file_search()
            .file_id("file-sheet-1"),
    )
    .await;
    rig.stream(chat.id, &question()).await;
    let call = upstream.only_call();
    assert!(
        tool_of(&call.body, "file_search").is_none(),
        "{}",
        call.body
    );
    assert_eq!(
        tool_of(&call.body, "code_interpreter").unwrap(),
        &json!({
            "type": "code_interpreter",
            "container": { "type": "auto", "file_ids": ["file-sheet-1"] }
        })
    );
    assert_eq!(
        call.body["include"],
        json!(["code_interpreter_call.outputs"])
    );
    assert!(
        !call.body["instructions"]
            .as_str()
            .unwrap()
            .contains(FILE_GUARD)
    );
}

#[tokio::test]
async fn the_request_carries_no_attribute_filter_and_attachment_ids_never_scope_retrieval() {
    // inst-ask-validation, inst-fs-definition, deferred scope: retrieval covers every document of
    // the chat store
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    let first = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "q", "attachment_ids": [first] }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let call = upstream.only_call();
    assert_eq!(
        tool_of(&call.body, "file_search").unwrap(),
        &json!({ "type": "file_search", "vector_store_ids": [STORE], "max_num_results": 5 })
    );
    let mut keys = Vec::new();
    all_keys(&call.body["tools"], &mut keys);
    for banned in [
        "filters",
        "filter",
        "attributes",
        "attribute_filter",
        "ranking_options",
    ] {
        assert!(
            !keys.iter().any(|key| key == banned),
            "`{banned}` in {keys:?}"
        );
    }
}

#[tokio::test]
async fn an_image_in_attachment_ids_is_model_input_with_its_internal_file_id() {
    // inst-ask-request: `input_image` with the internal `provider_file_id`; the client never
    // sees a provider identifier
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))));
    let rig = rig_for(&upstream).await;
    let chat = rig.prem_chat().await;
    let image = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-img-77"),
    )
    .await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "look", "attachment_ids": [image] }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let call = upstream.only_call();
    let content = call.body["input"][0]["content"].as_array().unwrap();
    assert_eq!(
        content[1],
        json!({ "type": "input_image", "file_id": "file-img-77" })
    );
    assert!(!reply.text.contains("file-img-77"), "{}", reply.text);
}

// ── the relayed events and the citations ─────────────────────────────────────

#[tokio::test]
async fn file_search_tool_events_are_relayed_and_file_citations_carry_attachment_and_filename() {
    // inst-ask-citations, inst-fs-citation-map: `title` is the filename
    let upstream = MockProvider::answering(Reply::Sse(cited_answer(&[(
        "file-pdf-1",
        "provider-side-name.pdf",
    )])));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    let pdf = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .file_id("file-pdf-1")
            .filename("handbook.pdf"),
    )
    .await;
    let reply = rig.stream(chat.id, &question()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let frames = parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec![
            "stream_started",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ],
        "{}",
        reply.text
    );
    assert_eq!(
        frames[1].data,
        json!({ "phase": "start", "name": "file_search", "details": {} })
    );
    assert_eq!(frames[2].data["name"], "file_search");
    assert_eq!(frames[2].data["phase"], "done");
    let items = frames[4].data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["source"], "file");
    assert_eq!(items[0]["attachment_id"], pdf.to_string());
    assert_eq!(
        items[0]["title"], "handbook.pdf",
        "the attachment filename, not the provider's"
    );
    assert!(items[0].get("url").is_none(), "{}", items[0]);
}

#[tokio::test]
async fn a_deleted_attachment_and_an_unknown_file_are_omitted_from_the_citations() {
    // inst-ask-citations: "unknown and soft-deleted attachments are omitted"
    let upstream = MockProvider::answering(Reply::Sse(cited_answer(&[
        ("file-live", "live.pdf"),
        ("file-gone", "gone.pdf"),
        ("file-nobody", "x.pdf"),
    ])));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    let live = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .file_id("file-live")
            .filename("live.pdf"),
    )
    .await;
    seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .file_id("file-gone")
            .filename("gone.pdf")
            .deleted(t(5)),
    )
    .await;
    let reply = rig.stream(chat.id, &question()).await;
    let frames = parse_sse(&reply.text);
    let citations = frames
        .iter()
        .find(|frame| frame.event == "citations")
        .expect("citations");
    let items = citations.data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["attachment_id"], live.to_string());
    assert!(!reply.text.contains("gone.pdf"));
}

#[tokio::test]
async fn when_no_citation_resolves_there_is_no_citations_event() {
    let upstream = MockProvider::answering(Reply::Sse(cited_answer(&[("file-nobody", "x.pdf")])));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document").file_id("file-real"),
    )
    .await;
    let reply = rig.stream(chat.id, &question()).await;
    assert!(
        !names(&parse_sse(&reply.text)).contains(&"citations"),
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn an_attachment_deleted_before_the_turn_is_neither_searched_nor_cited() {
    // inst-dtx-exclusion across the two features: the delete transaction, then the next turn
    let upstream =
        MockProvider::answering(Reply::Sse(cited_answer(&[("file-pdf-1", "handbook.pdf")])));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    let pdf = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .file_id("file-pdf-1")
            .filename("handbook.pdf"),
    )
    .await;
    let request = DeleteRequest {
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        attachment_id: pdf,
        uploader_id: USER_A1,
        now: at(100),
        secondary_upstream_alias: None,
    };
    let outcome = (&*rig.quota.db.provider as &dyn AttachmentStore)
        .delete_attachment(&request, rig.quota.enqueuer.clone())
        .await
        .unwrap();
    assert_eq!(outcome, DeleteOutcome::Deleted);
    let reply = rig.stream(chat.id, &question()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let call = upstream.only_call();
    assert!(
        tool_of(&call.body, "file_search").is_none(),
        "{}",
        call.body
    );
    assert!(!names(&parse_sse(&reply.text)).contains(&"citations"));
}

#[tokio::test]
async fn no_provider_identifier_reaches_the_client() {
    // the client never sends and never sees a provider identifier
    let upstream =
        MockProvider::answering(Reply::Sse(cited_answer(&[("file-pdf-1", "handbook.pdf")])));
    let rig = rig_for(&upstream).await;
    let chat = bound_chat(&rig).await;
    seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .file_id("file-pdf-1")
            .filename("handbook.pdf"),
    )
    .await;
    let reply = rig.stream(chat.id, &question()).await;
    for needle in ["file-pdf-1", STORE, "resp_"] {
        assert!(!reply.text.contains(needle), "`{needle}` in {}", reply.text);
    }
}

// ── the quota and the tools together ─────────────────────────────────────────

#[tokio::test]
async fn an_xlsx_only_turn_is_billed_without_the_file_search_surcharge() {
    // inst-ask-facts: the reserve of the turn matches the tools that are sent
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))));
    let rig = rig_for(&upstream).await;
    let xlsx_chat = bound_chat(&rig).await;
    seed_att(
        &rig.quota.db,
        &xlsx_chat,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .no_file_search(),
    )
    .await;
    let pdf_chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &pdf_chat, AttachmentSeed::ready("document")).await;
    let empty_chat = bound_chat(&rig).await;
    for chat in [&xlsx_chat, &pdf_chat, &empty_chat] {
        let reply = rig.stream(chat.id, &question()).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
    let mut reserved = Vec::new();
    for chat in [&xlsx_chat, &pdf_chat, &empty_chat] {
        let turns = crate::test_support::send::turn_rows(&rig.quota.db, chat.id).await;
        reserved.push(turns[0].reserve_tokens.expect("the reserve of the turn"));
    }
    let (xlsx, pdf, empty) = (reserved[0], reserved[1], reserved[2]);
    assert_eq!(xlsx - empty, 1000, "code interpreter surcharge only");
    assert_eq!(pdf - empty, 500, "file search surcharge only");
}
