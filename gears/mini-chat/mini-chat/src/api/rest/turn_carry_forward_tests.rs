//! The carry-forward of a retry and an edit over the real router, SQLite, the real quota service
//! and a mock OAGW that records the proxied request: the original images as `input_image` model
//! input in link order under the send-time guards, deleted attachments silently absent, documents
//! kept as associations only, the replacement message listing the carried attachments while the
//! original links stay on the soft-deleted message, the carried web search flag adding the
//! `web_search` tool, the other tools decided afresh from the chat's current facts, and the
//! replaced turn keeping its counters, its settled quota and its single usage event.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward`
//! (`inst-acf-select`, `inst-acf-exclude-deleted`, `inst-acf-no-revalidation`, `inst-acf-order`,
//! `inst-acf-classify`, `inst-acf-only-original`, `inst-acf-return`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`
//! (`inst-tcf-web-search-flag`, `inst-tcf-counters`, `inst-tcf-no-refund`, `inst-tcf-tools-afresh`,
//! `inst-tcf-limits`, `inst-tcf-return`); `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit`
//! (`inst-mc-copy-links`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-carry-forward`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::store::{AttachmentStore, DeleteOutcome, DeleteRequest};
use crate::test_support::USER_A1;
use crate::test_support::attachments::seed_vector_store;
use crate::test_support::chats::at;
use crate::test_support::mutations::{
    current_of, history_of, replacement_of, seed, soft_delete_attachment, sse_of, started_ids,
    streaming_chat, tool_of, turn, turn_by_request,
};
use crate::test_support::provider::{MockProvider, Reply, happy_steps};
use crate::test_support::quota::all_quota_rows;
use crate::test_support::send::{
    AttachmentSeed, SendRig, SendRigOptions, link_rows, seed_att, send_rig_with,
};
use crate::test_support::tool_fakes::tool_settings;

const STORE: &str = "vs_carry";

fn upstream() -> Arc<MockProvider> {
    MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))))
}

/// The parts of the current user message: a plain string without images, else `input_text` and
/// one `input_image` per image.
fn image_files(body: &Value) -> Vec<String> {
    match current_of(body)["content"].as_array() {
        Some(parts) => parts
            .iter()
            .filter(|part| part["type"] == "input_image")
            .map(|part| part["file_id"].as_str().unwrap().to_owned())
            .collect(),
        None => Vec::new(),
    }
}

async fn premium_rig() -> (
    SendRig,
    crate::infra::storage::repo_impl::ChatRecord,
    Arc<MockProvider>,
) {
    let upstream = upstream();
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream.clone())).await;
    let chat = rig.prem_chat().await;
    (rig, chat, upstream)
}

// ── images ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_retry_resends_the_original_images_as_input_images_in_link_order() {
    // inst-acf-order, inst-acf-classify
    let (rig, chat, upstream) = premium_rig().await;
    let db = &rig.quota.db;
    let late = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-late"),
    )
    .await;
    let early = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-early"),
    )
    .await;
    let target = seed(
        db,
        &chat,
        turn("look at these").attach(late, 30).attach(early, 20),
    )
    .await;
    let reply = rig.retry(chat.id, target.request_id).await;
    sse_of(&reply);
    let call = upstream.only_call();
    assert_eq!(image_files(&call.body), vec!["file-early", "file-late"]);
    let parts = current_of(&call.body)["content"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        parts[0],
        json!({ "type": "input_text", "text": "look at these" })
    );
    assert!(
        !reply.text.contains("file-early"),
        "no provider identifier reaches the client"
    );
}

#[tokio::test]
async fn an_edit_resends_the_original_images_with_the_edited_text() {
    let (rig, chat, upstream) = premium_rig().await;
    let db = &rig.quota.db;
    let image = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-one"),
    )
    .await;
    let target = seed(db, &chat, turn("old text").attach(image, 20)).await;
    let reply = rig
        .edit(
            chat.id,
            target.request_id,
            &json!({ "content": "new text" }),
        )
        .await;
    sse_of(&reply);
    let call = upstream.only_call();
    let parts = current_of(&call.body)["content"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        parts[0],
        json!({ "type": "input_text", "text": "new text" })
    );
    assert_eq!(image_files(&call.body), vec!["file-one"]);
}

#[tokio::test]
async fn an_attachment_deleted_since_the_original_turn_is_silently_absent() {
    // inst-acf-exclude-deleted: no error, no warning
    let (rig, chat, upstream) = premium_rig().await;
    let db = &rig.quota.db;
    let kept = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-kept"),
    )
    .await;
    let gone = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-gone"),
    )
    .await;
    let target = seed(
        db,
        &chat,
        turn("two images").attach(kept, 20).attach(gone, 21),
    )
    .await;
    soft_delete_attachment(db, gone).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let frames = sse_of(&reply);
    assert!(
        frames
            .iter()
            .all(|f| f.event != "error" && f.event != "warning"),
        "{}",
        reply.text
    );
    assert_eq!(image_files(&upstream.only_call().body), vec!["file-kept"]);
}

#[tokio::test]
async fn documents_stay_associations_only_and_are_not_model_input() {
    // inst-acf-classify: retrieval covers the entire chat vector store
    let (rig, chat, upstream) = premium_rig().await;
    let db = &rig.quota.db;
    let document = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let target = seed(db, &chat, turn("about the handbook").attach(document, 20)).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    sse_of(&reply);
    let call = upstream.only_call();
    assert_eq!(
        current_of(&call.body),
        &json!({ "role": "user", "content": "about the handbook" })
    );
    // the association is carried to the replacement message
    let new_message = crate::test_support::mutations::live_messages(db, chat.id)
        .await
        .remove(0);
    let carried: Vec<Uuid> = link_rows(db, chat.id)
        .await
        .into_iter()
        .filter(|l| l.message_id == new_message.id)
        .map(|l| l.attachment_id)
        .collect();
    assert_eq!(carried, vec![document]);
}

// ── the links and the messages list ──────────────────────────────────────────

#[tokio::test]
async fn the_replacement_message_lists_the_carried_attachments_and_the_original_rows_stay() {
    // inst-mc-copy-links: new `message_attachments` rows, the original rows kept
    let (rig, chat, _upstream) = premium_rig().await;
    let db = &rig.quota.db;
    let image = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").filename("cat.png"),
    )
    .await;
    let gone = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("document").filename("gone.pdf"),
    )
    .await;
    let target = seed(db, &chat, turn("files").attach(image, 20).attach(gone, 21)).await;
    soft_delete_attachment(db, gone).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;

    let items = rig.list_messages(chat.id).await;
    assert_eq!(items[0]["role"], "user");
    let attachments = items[0]["attachments"].as_array().unwrap();
    assert_eq!(
        attachments.len(),
        1,
        "the deleted attachment is absent: {attachments:?}"
    );
    assert_eq!(attachments[0]["attachment_id"], image.to_string());
    assert_eq!(attachments[0]["filename"], "cat.png");

    // two original rows stay on the soft-deleted message, one new row on the replacement
    let links = link_rows(db, chat.id).await;
    let on = |message: Uuid| links.iter().filter(|l| l.message_id == message).count();
    assert_eq!(on(target.user_message_id), 2);
    let new_message = items[0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    assert_eq!(on(new_message), 1);
    for link in &links {
        assert_eq!(link.chat_id, chat.id);
        assert_eq!(link.tenant_id, chat.tenant_id);
    }
}

#[tokio::test]
async fn a_carried_attachment_stays_locked_against_deletion_through_the_replaced_message() {
    // feature-local deviation: a replaced turn's links stay, so its attachments remain locked
    let (rig, chat, _upstream) = premium_rig().await;
    let db = &rig.quota.db;
    let image = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let target = seed(db, &chat, turn("img").attach(image, 20)).await;
    rig.delete_turn(chat.id, target.request_id).await;
    let outcome = AttachmentStore::delete_attachment(
        &*rig.quota.db.provider,
        &DeleteRequest {
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            attachment_id: image,
            uploader_id: USER_A1,
            now: at(2_000_000),
            secondary_upstream_alias: None,
        },
        rig.quota.enqueuer.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DeleteOutcome::Locked), "{outcome:?}");
}

// ── tools ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_web_search_flag_of_the_original_turn_adds_the_web_search_tool() {
    // inst-tcf-web-search-flag: the effective model supports web search
    let (rig, chat, upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("news?").web_search()).await;
    sse_of(&rig.retry(chat.id, target.request_id).await);
    let call = upstream.only_call();
    assert!(tool_of(&call.body, "web_search").is_some(), "{}", call.body);
    let replacement = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    assert!(
        replacement.web_search_enabled,
        "the replacement turn carries the flag"
    );

    let (rig, chat, upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("no web")).await;
    sse_of(&rig.retry(chat.id, target.request_id).await);
    assert!(tool_of(&upstream.only_call().body, "web_search").is_none());
    let replacement = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    assert!(!replacement.web_search_enabled);
}

#[tokio::test]
async fn the_tools_are_decided_afresh_from_the_current_ready_facts_of_the_chat() {
    // inst-tcf-tools-afresh: a document that became ready after the original turn adds
    // `file_search`; the original message never linked it
    let upstream = upstream();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_attachment_tools(tool_settings(KnowledgeSearchConfig::default())),
    )
    .await;
    let chat = rig.std_chat().await;
    seed_vector_store(&rig.quota.db, &chat, Some(STORE), "azure_openai", at(0)).await;
    let target = seed(&rig.quota.db, &chat, turn("old question")).await;
    // uploaded after the original turn
    let document = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    sse_of(&rig.retry(chat.id, target.request_id).await);
    let call = upstream.only_call();
    assert_eq!(
        tool_of(&call.body, "file_search").expect("file_search"),
        &json!({ "type": "file_search", "vector_store_ids": [STORE], "max_num_results": 5 })
    );
    // never carried: the new upload is not linked to the replacement message
    assert!(
        link_rows(&rig.quota.db, chat.id)
            .await
            .iter()
            .all(|l| l.attachment_id != document)
    );

    // and without a ready document the tool is gone again
    let upstream2 = self::upstream();
    let rig2 = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream2.clone())
            .with_attachment_tools(tool_settings(KnowledgeSearchConfig::default())),
    )
    .await;
    let chat2 = rig2.std_chat().await;
    seed_vector_store(&rig2.quota.db, &chat2, Some(STORE), "azure_openai", at(0)).await;
    let gone = seed_att(
        &rig2.quota.db,
        &chat2,
        AttachmentSeed::ready("document").deleted(at(1)),
    )
    .await;
    let target2 = seed(&rig2.quota.db, &chat2, turn("q").attach(gone, 10)).await;
    sse_of(&rig2.retry(chat2.id, target2.request_id).await);
    assert!(tool_of(&upstream2.only_call().body, "file_search").is_none());
}

#[tokio::test]
async fn the_history_of_the_proxied_request_excludes_the_replaced_turn() {
    let (rig, chat, upstream) = streaming_chat().await;
    seed(
        &rig.quota.db,
        &chat,
        turn("one").at(10).answer("answer one"),
    )
    .await;
    let second = seed(
        &rig.quota.db,
        &chat,
        turn("two").at(100).answer("stale answer"),
    )
    .await;
    sse_of(
        &rig.edit(
            chat.id,
            second.request_id,
            &json!({ "content": "two, again" }),
        )
        .await,
    );
    let call = upstream.only_call();
    assert_eq!(
        history_of(&call.body),
        vec![
            ("user".to_owned(), "one".to_owned()),
            ("assistant".to_owned(), "answer one".to_owned())
        ]
    );
    assert!(!call.body.to_string().contains("stale answer"));
}

// ── counters, settlement and usage ───────────────────────────────────────────

#[tokio::test]
async fn the_replacement_turn_starts_at_zero_counters_and_the_replaced_turn_keeps_its_own() {
    // inst-tcf-counters
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q").web_search().counts(3, 1, 2)).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    let new = rig.wait_turn_terminal(chat.id, new_request).await;
    assert_eq!(
        (
            new.web_search_completed_count,
            new.code_interpreter_completed_count,
            new.file_search_completed_count
        ),
        (0, 0, 0),
        "no tool ran in the replacement turn"
    );
    let old = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(
        (
            old.web_search_completed_count,
            old.code_interpreter_completed_count,
            old.file_search_completed_count
        ),
        (3, 1, 2)
    );
}

#[tokio::test]
async fn the_replaced_turn_keeps_its_settled_quota_and_its_single_usage_event() {
    // inst-tcf-no-refund: a mutation never refunds or re-settles the replaced turn
    let (rig, chat, _upstream) = streaming_chat().await;
    let first = rig.stream(chat.id, &json!({ "content": "question" })).await;
    let (first_request, _) = started_ids(&sse_of(&first));
    rig.wait_turn_terminal(chat.id, first_request).await;
    let events_before = rig.events().await;
    assert_eq!(events_before.len(), 1);
    let spent_before: i64 = all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .filter(|r| r.bucket == "total" && r.period == "daily")
        .map(|r| r.spent)
        .sum();
    assert!(spent_before > 0);

    let reply = rig.retry(chat.id, first_request).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;

    let events = rig.events().await;
    assert_eq!(
        events.len(),
        2,
        "one usage event per finalized turn: {events:?}"
    );
    assert!(
        events.contains(&events_before[0]),
        "the first event is not touched or repeated"
    );
    let second = events
        .iter()
        .find(|e| e.request_id == new_request)
        .expect("the new event");
    let spent_after: i64 = all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .filter(|r| r.bucket == "total" && r.period == "daily")
        .map(|r| r.spent)
        .sum();
    assert_eq!(
        spent_after,
        spent_before + second.actual_credits_micro,
        "the replaced turn's spend stays, the replacement adds its own"
    );
    let reserved: i64 = all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .map(|r| r.reserved)
        .sum();
    assert_eq!(reserved, 0, "every reserve is settled");
}

#[tokio::test]
async fn the_replacement_turn_is_charged_to_the_requester_of_the_mutation() {
    // ADR-0003: the mutating user is the requester
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;
    let new = turn_by_request(&rig.quota.db, chat.id, new_request).await;
    assert_eq!(new.requester_user_id, Some(crate::test_support::USER_A1));
    let rows = all_quota_rows(&rig.quota.db).await;
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|r| r.user_id == crate::test_support::USER_A1)
    );
}
