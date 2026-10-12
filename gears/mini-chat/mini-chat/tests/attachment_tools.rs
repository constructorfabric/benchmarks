//! Attachments reach the model: the `file_search` and `code_interpreter` tools of the provider
//! request, the citation mapping of file annotations, the carry-forward of a retry / edit and the
//! reserve surcharges that follow from the tools.
//!
//! Source of truth: DESIGN "File Search Tool Availability", "Retrieval Invariants", "Code
//! Interpreter Tool Availability" (tool entry `{"type": "code_interpreter", "container": {"type":
//! "auto", "file_ids": [...]}}`, `include: ["code_interpreter_call.outputs"]`, kill switch and
//! model capability gating), "File Search Retrieval Scope" (one store per chat, no cross-chat
//! leakage), "Citation File ID and Title Resolution", "Attachment Deletion" (a deleted attachment
//! leaves the inclusion checks and the citations at once), 3.6 "Retry / edit variant" (carry-forward
//! of the links) and 5.5.6 (surcharges `tool_surcharge_tokens` / `code_interpreter_surcharge_tokens`
//! only when the tool is sent).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod attachment_support;
mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::collections::BTreeSet;

use attachment_support::{
    AttRow, AttachmentHarness, Opts, PREMIUM, ProviderReply, STANDARD, file_annotation,
    file_search_pieces, find_model, last_user_content, quota_models, ref_reserve,
};
use serde_json::{Value, json};
use stream_support::{FILE_GUARD, contains_provider_identifier};
use uuid::Uuid;

fn tool_types(request: &stream_support::RecordedProviderRequest) -> BTreeSet<String> {
    request.tool_types().into_iter().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn file_ids_of(tool: &Value) -> BTreeSet<String> {
    tool["container"]["file_ids"]
        .as_array()
        .expect("container.file_ids")
        .iter()
        .map(|f| f.as_str().unwrap().to_owned())
        .collect()
}

async fn say_and_request(
    h: &AttachmentHarness,
    chat: Uuid,
    text: &str,
) -> stream_support::RecordedProviderRequest {
    let sent = h.say(chat, text).await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
    h.oagw.last_request()
}

fn premium_reserve(text: &str, surcharge: u64) -> i64 {
    let reserve = ref_reserve(
        &find_model(&quota_models(), PREMIUM),
        32_768,
        text,
        0,
        surcharge,
    );
    i64::try_from(reserve.2).unwrap()
}

// ═════════════════════════════════════════════════════════════════════════════
// file_search
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_ready_document_adds_file_search_on_the_chat_vector_store() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.doc(chat, "contract.pdf").await;
    let store = h.vector_store_of(chat).await;

    let request = say_and_request(&h, chat, "what does the contract say?").await;
    assert_eq!(
        request
            .tool("file_search")
            .expect("the file_search tool is sent"),
        json!({"type": "file_search", "vector_store_ids": [store], "max_num_results": 5}),
        "flat Responses API shape, the catalog max_num_results, no attribute filter"
    );
    assert_eq!(tool_types(&request), set(&["file_search"]));
    assert!(
        request.system_text().contains(FILE_GUARD),
        "the guard follows the tool"
    );
    assert_eq!(request.body["metadata"]["feature"], json!("file_search"));
    assert_eq!(
        request.body["max_tool_calls"],
        json!(3),
        "the catalog max_tool_calls"
    );
}

#[tokio::test]
async fn file_search_is_on_every_request_of_the_chat_not_only_the_attaching_one() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "a.pdf").await;

    let with = h.say_with(chat, "first, with the file", &[doc]).await;
    assert_eq!(with.terminal().event, "done");
    assert!(h.oagw.last_request().tool("file_search").is_some());
    let request = say_and_request(&h, chat, "second, without").await;
    assert!(
        request.tool("file_search").is_some(),
        "retrieval covers the whole chat store"
    );
}

#[tokio::test]
async fn without_a_ready_document_there_is_no_file_search_and_no_guard() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let request = say_and_request(&h, chat, "hello").await;
    assert!(request.tool_types().is_empty());
    assert!(!request.system_text().contains(FILE_GUARD));
    assert!(request.body.get("include").is_none(), "{}", request.body);
    assert_eq!(request.body["metadata"]["feature"], json!("none"));
}

#[tokio::test]
async fn documents_that_are_not_ready_or_deleted_do_not_count() {
    let h = AttachmentHarness::new().await;
    for label in ["pending", "uploaded", "failed", "deleted"] {
        let chat = h.seed_chat().await;
        h.seed_store(chat, "vs_seeded_store_0001", "openai").await;
        let row = AttRow::document(chat, "x.pdf");
        h.seed(if label == "deleted" {
            row.deleted()
        } else {
            row.status(label)
        })
        .await;
        let request = say_and_request(&h, chat, "hello").await;
        assert!(
            request.tool("file_search").is_none(),
            "{label}: only status = ready AND deleted_at IS NULL counts"
        );
    }
}

#[tokio::test]
async fn a_document_without_a_vector_store_row_gets_no_file_search() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed(AttRow::document(chat, "x.pdf")).await;
    let request = say_and_request(&h, chat, "hello").await;
    assert!(request.tool("file_search").is_none());
}

#[tokio::test]
async fn spreadsheets_and_images_are_not_searchable() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.xlsx(chat, "sheet.xlsx").await;
    h.image(chat, "pic.png").await;
    let request = say_and_request(&h, chat, "hello").await;
    assert!(
        request.tool("file_search").is_none(),
        "no vector store exists for them"
    );
    assert_eq!(h.stores_of(chat).await.len(), 0);
}

#[tokio::test]
async fn the_file_search_kill_switch_and_a_model_without_support_remove_the_tool() {
    // kill switch
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.doc(chat, "a.pdf").await;
    h.policy
        .set_kill_switches(json!({"disable_file_search": true}));
    let request = say_and_request(&h, chat, "hello").await;
    assert!(request.tool("file_search").is_none());
    assert!(!request.system_text().contains(FILE_GUARD));

    // a model without file_search
    let mut models = quota_models();
    models[1]["general_config"]["tool_support"]["file_search"] = json!(false);
    let h = AttachmentHarness::with_models(models).await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.seed_store(chat, "vs_seeded_store_0002", "openai").await;
    h.seed(AttRow::document(chat, "a.pdf")).await;
    let request = say_and_request(&h, chat, "hello").await;
    assert!(request.tool("file_search").is_none());
}

#[tokio::test]
async fn retrieval_is_scoped_to_the_chat_vector_store() {
    let h = AttachmentHarness::new().await;
    let a = h.seed_chat().await;
    let b = h.seed_chat().await;
    h.doc(a, "a.pdf").await;
    h.doc(b, "b.pdf").await;
    let (store_a, store_b) = (h.vector_store_of(a).await, h.vector_store_of(b).await);

    let request = say_and_request(&h, a, "q").await;
    assert_eq!(
        request.tool("file_search").unwrap()["vector_store_ids"],
        json!([store_a])
    );
    assert!(
        !request.body.to_string().contains(&store_b),
        "no foreign store id"
    );
    let request = say_and_request(&h, b, "q").await;
    assert_eq!(
        request.tool("file_search").unwrap()["vector_store_ids"],
        json!([store_b])
    );
    assert!(!request.body.to_string().contains(&store_a));
}

#[tokio::test]
async fn a_document_reference_adds_no_input_item_and_never_exposes_its_provider_id() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "a.pdf").await;
    let provider_file = h.provider_file_of(doc).await;

    let sent = h.say_with(chat, "summarise", &[doc]).await;
    assert_eq!(sent.terminal().event, "done");
    let request = h.oagw.last_request();
    let parts = last_user_content(&request);
    assert_eq!(parts.len(), 1, "text only: {parts:?}");
    assert_eq!(parts[0]["type"], json!("input_text"));
    assert_eq!(parts[0]["text"], json!("summarise"));
    assert!(
        !request.body.to_string().contains(&provider_file),
        "a document's provider file id is not part of the request"
    );
    assert!(
        !contains_provider_identifier(&sent.blob()),
        "{}",
        sent.blob()
    );
}

#[tokio::test]
async fn deleting_the_only_document_removes_the_tool_at_once() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "a.pdf").await;
    assert!(
        say_and_request(&h, chat, "q1")
            .await
            .tool("file_search")
            .is_some()
    );

    assert_eq!(h.delete(chat, doc).await.status.as_u16(), 204);
    let request = say_and_request(&h, chat, "q2").await;
    assert!(
        request.tool("file_search").is_none(),
        "the inclusion check counts live ready rows only"
    );
}

#[tokio::test]
async fn deleting_one_of_two_documents_keeps_the_tool() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.doc(chat, "keep.pdf").await;
    let drop = h.doc(chat, "drop.pdf").await;
    assert_eq!(h.delete(chat, drop).await.status.as_u16(), 204);
    assert!(
        say_and_request(&h, chat, "q")
            .await
            .tool("file_search")
            .is_some()
    );
}

#[tokio::test]
async fn the_tool_surcharge_is_reserved_only_when_the_tool_is_sent() {
    // one fresh chat per measurement: a later turn would also reserve its prior context
    let h = AttachmentHarness::new().await;
    let plain_chat = h.seed_chat().await;
    let plain = h.say(plain_chat, "hello").await;
    let plain_turn = h.turn_of(plain.request_id()).await.unwrap();
    assert_eq!(plain_turn.reserve_tokens, Some(premium_reserve("hello", 0)));

    let doc_chat = h.seed_chat().await;
    h.doc(doc_chat, "a.pdf").await;
    let with_doc = h.say(doc_chat, "hello").await;
    let turn = h.turn_of(with_doc.request_id()).await.unwrap();
    assert_eq!(
        turn.reserve_tokens,
        Some(premium_reserve("hello", 500)),
        "tool_surcharge_tokens (500) once, from the ready document"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// citations
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_file_annotation_of_an_uploaded_document_is_cited_by_attachment_id_and_filename() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "Q3 Report.pdf").await;
    let provider_file = h.provider_file_of(doc).await;
    h.oagw
        .push(ProviderReply::sse(responses_with_citation(&provider_file)));

    let sent = h.say(chat, "what is in it?").await;
    sent.assert_well_formed();
    assert_eq!(
        sent.of("citations")[0].data,
        json!({"items": [{
            "source": "file",
            "title": "Q3 Report.pdf",
            "attachment_id": doc.to_string(),
            "snippet": ""
        }]}),
        "the stored filename (not the provider one), the attachment id, no provider id"
    );
    assert!(!sent.blob().contains(&provider_file));
    let tools: Vec<_> = sent
        .of("tool")
        .iter()
        .map(|e| e.data["name"].clone())
        .collect();
    assert!(tools.contains(&json!("file_search")), "{tools:?}");
}

fn responses_with_citation(provider_file: &str) -> Vec<attachment_support::Piece> {
    let mut pieces = attachment_support::responses_stream(
        &["It says ", "hello."],
        &Opts::default().annotated(vec![file_annotation(
            provider_file,
            "provider-side-name.pdf",
        )]),
    );
    // a file_search call before the answer, like the real provider
    let search = file_search_pieces(None);
    let at = 2.min(pieces.len());
    for (offset, piece) in search.into_iter().enumerate() {
        pieces.insert(at + offset, piece);
    }
    pieces
}

#[tokio::test]
async fn citations_of_deleted_other_chat_and_spreadsheet_files_are_omitted() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let other = h.seed_chat().await;
    let kept = h.doc(chat, "kept.pdf").await;
    let dropped = h.doc(chat, "dropped.pdf").await;
    let foreign = h.doc(other, "foreign.pdf").await;
    let sheet = h.xlsx(chat, "sheet.xlsx").await;
    let (kept_f, dropped_f, foreign_f, sheet_f) = (
        h.provider_file_of(kept).await,
        h.provider_file_of(dropped).await,
        h.provider_file_of(foreign).await,
        h.provider_file_of(sheet).await,
    );
    assert_eq!(h.delete(chat, dropped).await.status.as_u16(), 204);

    h.oagw.push(ProviderReply::text_with(
        &["answer"],
        &Opts::default().annotated(vec![
            file_annotation(&dropped_f, "dropped.pdf"),
            file_annotation(&foreign_f, "foreign.pdf"),
            file_annotation("file-unknownunknown0000", "x.pdf"),
            file_annotation(&kept_f, "kept.pdf"),
            file_annotation(&sheet_f, "sheet.xlsx"),
        ]),
    ));
    let sent = h.say(chat, "q").await;
    let items = sent.of("citations")[0].data["items"]
        .as_array()
        .unwrap()
        .clone();
    let cited: Vec<_> = items.iter().map(|i| i["attachment_id"].clone()).collect();
    assert!(cited.contains(&json!(kept.to_string())), "{items:?}");
    assert!(
        !cited.contains(&json!(dropped.to_string())),
        "a deleted attachment is never cited"
    );
    assert!(
        !cited.contains(&json!(foreign.to_string())),
        "a foreign chat file is never cited"
    );
    for provider_file in [&dropped_f, &foreign_f, &kept_f, &sheet_f] {
        assert!(
            !sent.blob().contains(provider_file.as_str()),
            "{}",
            sent.blob()
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// code_interpreter
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_ready_spreadsheet_adds_the_code_interpreter_tool_with_its_file_ids() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let sheet = h.xlsx(chat, "sales.xlsx").await;
    let provider_file = h.provider_file_of(sheet).await;

    let request = say_and_request(&h, chat, "sum column B").await;
    assert_eq!(
        request
            .tool("code_interpreter")
            .expect("the code_interpreter tool is sent"),
        json!({"type": "code_interpreter", "container": {"type": "auto", "file_ids": [provider_file]}})
    );
    assert_eq!(
        request.body["include"],
        json!(["code_interpreter_call.outputs"]),
        "the provider returns the code output"
    );
    assert_eq!(tool_types(&request), set(&["code_interpreter"]));
}

#[tokio::test]
async fn the_tool_is_chat_wide_and_lists_every_live_ready_spreadsheet_of_the_chat() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let other = h.seed_chat().await;
    let one = h.xlsx(chat, "one.xlsx").await;
    let two = h.xlsx(chat, "two.xlsx").await;
    let foreign = h.xlsx(other, "foreign.xlsx").await;
    let pending = h.seed(AttRow::xlsx(chat, "p.xlsx").status("pending")).await;
    let failed = h.seed(AttRow::xlsx(chat, "f.xlsx").status("failed")).await;
    let deleted = h.seed(AttRow::xlsx(chat, "d.xlsx").deleted()).await;
    let mut expected = BTreeSet::new();
    for id in [one, two] {
        expected.insert(h.provider_file_of(id).await);
    }

    // the message does not even reference them
    let request = say_and_request(&h, chat, "analyse").await;
    let tool = request.tool("code_interpreter").unwrap();
    assert_eq!(file_ids_of(&tool), expected);
    for id in [foreign, pending, failed, deleted] {
        let file = h.provider_file_of(id).await;
        assert!(
            !request.body.to_string().contains(&file),
            "{id} must not be offered"
        );
    }
}

#[tokio::test]
async fn without_a_ready_spreadsheet_there_is_no_code_interpreter_and_no_include() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed(AttRow::xlsx(chat, "p.xlsx").status("uploaded"))
        .await;
    h.doc(chat, "doc.pdf").await;
    let request = say_and_request(&h, chat, "hello").await;
    assert!(request.tool("code_interpreter").is_none());
    assert!(request.body.get("include").is_none(), "{}", request.body);
}

#[tokio::test]
async fn deleting_the_only_spreadsheet_removes_the_tool_at_once() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let sheet = h.xlsx(chat, "sales.xlsx").await;
    assert!(
        say_and_request(&h, chat, "q1")
            .await
            .tool("code_interpreter")
            .is_some()
    );
    assert_eq!(h.delete(chat, sheet).await.status.as_u16(), 204);
    assert!(
        say_and_request(&h, chat, "q2")
            .await
            .tool("code_interpreter")
            .is_none()
    );
}

#[tokio::test]
async fn the_code_interpreter_kill_switch_silently_omits_the_tool() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.xlsx(chat, "sales.xlsx").await;
    h.policy
        .set_kill_switches(json!({"disable_code_interpreter": true}));

    let request = say_and_request(&h, chat, "analyse").await;
    assert!(
        request.tool("code_interpreter").is_none(),
        "omitted, the stream is not rejected"
    );
    assert!(request.body.get("include").is_none());
}

#[tokio::test]
async fn a_model_without_code_interpreter_gets_no_tool_even_after_a_downgrade() {
    let mut models = quota_models();
    models[1]["general_config"]["tool_support"]["code_interpreter"] = json!(false);
    let h = AttachmentHarness::with_models(models).await;
    // a chat on the premium model with a ready spreadsheet
    let chat = h.seed_chat().await;
    h.xlsx(chat, "sales.xlsx").await;
    assert!(
        say_and_request(&h, chat, "q1")
            .await
            .tool("code_interpreter")
            .is_some()
    );

    // the effective model of the next turn is the standard one: no tool (judged on the effective model)
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    let sent = h.say(chat, "q2").await;
    assert_eq!(sent.terminal().event, "done");
    let request = h.oagw.last_request();
    assert_eq!(request.model(), stream_support::provider_model(STANDARD));
    assert!(request.tool("code_interpreter").is_none());
}

#[tokio::test]
async fn the_code_interpreter_surcharge_is_reserved_only_when_the_tool_is_sent() {
    // one fresh chat per measurement: a later turn would also reserve its prior context
    let h = AttachmentHarness::new().await;
    let plain_chat = h.seed_chat().await;
    let plain = h.say(plain_chat, "hello").await;
    assert_eq!(
        h.turn_of(plain.request_id()).await.unwrap().reserve_tokens,
        Some(premium_reserve("hello", 0))
    );

    let sheet_chat = h.seed_chat().await;
    h.xlsx(sheet_chat, "sales.xlsx").await;
    let with_tool = h.say(sheet_chat, "hello").await;
    assert_eq!(
        h.turn_of(with_tool.request_id())
            .await
            .unwrap()
            .reserve_tokens,
        Some(premium_reserve("hello", 1000)),
        "code_interpreter_surcharge_tokens (1000)"
    );

    let killed_chat = h.seed_chat().await;
    h.xlsx(killed_chat, "sales.xlsx").await;
    h.policy
        .set_kill_switches(json!({"disable_code_interpreter": true}));
    let killed = h.say(killed_chat, "hello").await;
    assert_eq!(
        h.turn_of(killed.request_id()).await.unwrap().reserve_tokens,
        Some(premium_reserve("hello", 0)),
        "no tool, no surcharge"
    );
}

#[tokio::test]
async fn documents_spreadsheets_and_web_search_combine() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.doc(chat, "doc.pdf").await;
    h.xlsx(chat, "sheet.xlsx").await;

    let sent = h
        .send(
            chat,
            &json!({"content": "everything", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(sent.terminal().event, "done");
    let request = h.oagw.last_request();
    assert_eq!(
        tool_types(&request),
        set(&["file_search", "code_interpreter", "web_search"])
    );
    assert_eq!(
        request.body["include"],
        json!(["code_interpreter_call.outputs"])
    );
    // 500 (file search) + 500 (web search) + 1000 (code interpreter)
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(
        turn.reserve_tokens,
        Some(premium_reserve("everything", 2000))
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// retry / edit carry the attachments into the provider request (S5 carry-forward)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_retry_sends_the_same_images_and_tools_again() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "doc.pdf").await;
    let sheet = h.xlsx(chat, "sheet.xlsx").await;
    let image = h.image(chat, "photo.png").await;
    let image_file = h.provider_file_of(image).await;

    let first = h
        .say_with(chat, "look at these", &[doc, sheet, image])
        .await;
    assert_eq!(first.terminal().event, "done");
    let original = h.oagw.last_request();

    let retried = h.retry(chat, first.request_id()).await;
    assert_eq!(retried.terminal().event, "done", "{:?}", retried.names());
    let request = h.oagw.last_request();
    assert_eq!(
        attachment_support::image_file_ids(&request),
        vec![image_file]
    );
    assert_eq!(
        last_user_content(&request)[0],
        json!({"type": "input_text", "text": "look at these"})
    );
    assert_eq!(tool_types(&request), tool_types(&original));
    assert_eq!(request.tool("file_search"), original.tool("file_search"));
    assert_eq!(
        request.tool("code_interpreter"),
        original.tool("code_interpreter")
    );

    // the links of the new message point at the same three attachments
    let new_message = h.user_message_of(chat, retried.request_id()).await;
    let mut copied: Vec<Uuid> = h
        .links_of_chat(chat)
        .await
        .into_iter()
        .filter(|l| l.message_id == new_message)
        .map(|l| l.attachment_id)
        .collect();
    copied.sort();
    let mut expected = vec![doc, sheet, image];
    expected.sort();
    assert_eq!(copied, expected);
}

#[tokio::test]
async fn an_edit_sends_the_new_text_with_the_same_images() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let image = h.image(chat, "photo.png").await;
    let image_file = h.provider_file_of(image).await;
    let first = h.say_with(chat, "what is this?", &[image]).await;
    assert_eq!(first.terminal().event, "done");

    let edited = h
        .edit(chat, first.request_id(), "describe the colours")
        .await;
    assert_eq!(edited.terminal().event, "done", "{:?}", edited.names());
    let request = h.oagw.last_request();
    let parts = last_user_content(&request);
    assert_eq!(
        parts[0],
        json!({"type": "input_text", "text": "describe the colours"})
    );
    assert_eq!(
        attachment_support::image_file_ids(&request),
        vec![image_file]
    );
    assert!(
        !request.body.to_string().contains("what is this?"),
        "the replaced turn is not in the context"
    );
}

#[tokio::test]
async fn a_deleted_attachment_is_not_carried_into_the_retry() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, "look at these").await;
    // attachments linked to the submitted message; one of them was deleted since
    let kept = h.seed(AttRow::image(chat, "kept.png")).await;
    let gone = h.seed(AttRow::image(chat, "gone.png").deleted()).await;
    let kept_file = h.provider_file_of(kept).await;
    let gone_file = h.provider_file_of(gone).await;
    let first_user = h.user_message_of(chat, first.request_id()).await;
    h.chat.link_attachment(chat, first_user, kept).await;
    h.chat.link_attachment(chat, first_user, gone).await;

    let retried = h.retry(chat, first.request_id()).await;
    assert_eq!(retried.terminal().event, "done", "{:?}", retried.names());
    let request = h.oagw.last_request();
    assert_eq!(
        attachment_support::image_file_ids(&request),
        vec![kept_file]
    );
    assert!(!request.body.to_string().contains(&gone_file));
    let new_user = h.user_message_of(chat, retried.request_id()).await;
    let new_links: Vec<_> = h
        .links_of_chat(chat)
        .await
        .into_iter()
        .filter(|l| l.message_id == new_user)
        .collect();
    assert_eq!(
        new_links.len(),
        1,
        "the deleted attachment is silently excluded from the copy"
    );
    assert_eq!(new_links[0].attachment_id, kept);
}

#[tokio::test]
async fn a_retry_sends_the_spreadsheet_tool_even_when_the_original_did_not_reference_it() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, "hello").await;
    let sheet = h.xlsx(chat, "late.xlsx").await;
    let file = h.provider_file_of(sheet).await;

    let retried = h.retry(chat, first.request_id()).await;
    assert_eq!(retried.terminal().event, "done");
    let tool = h
        .oagw
        .last_request()
        .tool("code_interpreter")
        .expect("chat-wide");
    assert_eq!(file_ids_of(&tool), BTreeSet::from([file]));
}
