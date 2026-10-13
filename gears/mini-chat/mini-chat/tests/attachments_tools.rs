#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Uploaded attachments reach the provider as tools and inputs (DESIGN "File Search Tool
//! Availability", "Code Interpreter Tool Availability", "Citation File ID and Title Resolution",
//! "Internal: Multimodal Input Format", `attachment_ids` validation, `message_attachments`).
//!
//! `send_pipeline.rs`, `code_interpreter.rs` and `turn_mutations.rs` pin the same send pipeline
//! with attachment rows inserted by SQL; these tests drive the real upload endpoints first and
//! check what the provider gets: the chat vector store id, the code interpreter container file
//! ids and the image inputs come from rows the upload created.

mod attachments;
mod common;
mod mutation;

use attachments::{
    JPEG, PNG, assert_keys, assert_thumbnail, calls, chat_rows, delete_ok, detail_id,
    expect_created, link_rows, pdf_bytes, png, provider_error, row, script_file, script_plain_file,
    script_store_create, store_rows, upload, upload_pdf, upload_ready_document, upload_ready_image,
};
use common::{
    PREMIUM_MODEL, STANDARD_MODEL, XLSX_CONTENT_TYPE, assert_invalid_argument,
    assert_nothing_happened, assert_out_of_range, catalog_entry, chat_request_body, closing_frames,
    delta_frames, enqueue_chat, env_user_chat, env_with_policy, expect_stream, first, input_text,
    items, list_messages_ok, merge, new_chat, opening_frames, policy, send, send_text,
    set_chat_model, stream_request_id, terminal,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};

fn tools_of(body: &Value) -> Vec<Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn tool(body: &Value, kind: &str) -> Option<Value> {
    tools_of(body)
        .into_iter()
        .find(|tool| tool["type"] == json!(kind))
}

fn container_file_ids(tool: &Value) -> Vec<String> {
    let mut ids: Vec<String> = tool["container"]["file_ids"]
        .as_array()
        .unwrap_or_else(|| panic!("container.file_ids is an array: {tool}"))
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

async fn upload_xlsx(env: &TestEnv, user: &TestUser, chat: &str, file: &str) -> Value {
    script_plain_file(env, file);
    expect_created(
        &upload(
            env,
            user,
            chat,
            "data.xlsx",
            XLSX_CONTENT_TYPE,
            &pdf_bytes(256),
        )
        .await,
    )
}

fn citation_frames(provider_file_ids: &[&str]) -> Vec<(String, Value)> {
    let annotations: Vec<Value> = provider_file_ids
        .iter()
        .map(|id| json!({ "type": "file_citation", "file_id": id, "filename": "provider-side.pdf", "index": 0 }))
        .collect();
    let mut frames = opening_frames();
    frames.extend(delta_frames(&["See the files."]));
    frames.extend(closing_frames(
        "See the files.",
        &json!(annotations),
        Some((10, 5)),
        "response.completed",
    ));
    frames
}

// ===========================================================================
// file_search
// ===========================================================================

#[tokio::test]
async fn an_uploaded_ready_document_enables_file_search_on_its_vector_store() {
    let (env, user, chat) = env_user_chat().await;
    upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;

    expect_stream(&send_text(&env, &user, &chat, "what does it say").await);

    let body = chat_request_body(&env, 0);
    assert_eq!(
        tools_of(&body),
        vec![
            json!({ "type": "file_search", "vector_store_ids": ["vs_chat1"], "max_num_results": 5 })
        ],
        "flat vector_store_ids, max_num_results of the catalog model, no attribute filter"
    );
    assert!(tool(&body, "code_interpreter").is_none());
}

#[tokio::test]
async fn max_num_results_comes_from_the_catalog_model() {
    let model = merge(
        catalog_entry(PREMIUM_MODEL, "Premium", true, true),
        json!({ "max_num_results": 7 }),
    );
    let env = env_with_policy(policy(vec![
        model,
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;

    expect_stream(&send_text(&env, &user, &chat, "hi").await);

    assert_eq!(
        tool(&chat_request_body(&env, 0), "file_search").unwrap()["max_num_results"],
        json!(7)
    );
}

#[tokio::test]
async fn file_search_is_not_offered_without_a_ready_document() {
    let (env, user, chat) = env_user_chat().await;
    expect_stream(&send_text(&env, &user, &chat, "empty chat").await);
    upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;

    expect_stream(&send_text(&env, &user, &chat, "image only").await);

    assert!(chat_request_body(&env, 0).get("tools").is_none());
    assert!(
        chat_request_body(&env, 1).get("tools").is_none(),
        "an image is never indexed, so there is no retrieval"
    );
}

#[tokio::test]
async fn a_failed_document_upload_does_not_enable_file_search() {
    let (env, user, chat) = env_user_chat().await;
    script_file(&env, "file-doc1");
    script_store_create(&env, "vs_chat1");
    env.provider()
        .enqueue("/vector_stores/vs_chat1/files", provider_error(500));
    assert_eq!(upload_pdf(&env, &user, &chat).await.status.as_u16(), 503);
    assert_eq!(
        store_rows(&env, &chat).await.len(),
        1,
        "the chat has its vector store row"
    );

    expect_stream(&send_text(&env, &user, &chat, "hi").await);

    assert!(chat_request_body(&env, 0).get("tools").is_none());
}

#[tokio::test]
async fn deleting_the_last_document_stops_file_search_and_one_left_keeps_it() {
    let (env, user, chat) = env_user_chat().await;
    let first =
        upload_ready_document(&env, &user, &chat, "a.pdf", "file-a", "vs_chat1", true).await;
    let second =
        upload_ready_document(&env, &user, &chat, "b.pdf", "file-b", "vs_chat1", false).await;

    delete_ok(&env, &user, &chat, detail_id(&first)).await;
    expect_stream(&send_text(&env, &user, &chat, "one left").await);
    delete_ok(&env, &user, &chat, detail_id(&second)).await;
    expect_stream(&send_text(&env, &user, &chat, "none left").await);

    assert!(tool(&chat_request_body(&env, 0), "file_search").is_some());
    assert!(
        chat_request_body(&env, 1).get("tools").is_none(),
        "only ready, non-deleted documents count"
    );
    assert_eq!(
        store_rows(&env, &chat).await.len(),
        1,
        "the vector store stays until the chat is deleted"
    );
}

#[tokio::test]
async fn other_chats_documents_do_not_enable_file_search() {
    let (env, user, chat_with_document) = env_user_chat().await;
    let empty_chat = new_chat(&env, &user).await;
    upload_ready_document(
        &env,
        &user,
        &chat_with_document,
        "a.pdf",
        "file-a",
        "vs_chat1",
        true,
    )
    .await;

    expect_stream(&send_text(&env, &user, &empty_chat, "hi").await);

    assert!(chat_request_body(&env, 0).get("tools").is_none());
}

// ===========================================================================
// code_interpreter
// ===========================================================================

#[tokio::test]
async fn an_uploaded_xlsx_enables_the_code_interpreter_with_its_provider_file() {
    let (env, user, chat) = env_user_chat().await;
    upload_xlsx(&env, &user, &chat, "file-xlsx1").await;

    expect_stream(&send_text(&env, &user, &chat, "sum the sheet").await);

    let body = chat_request_body(&env, 0);
    assert_eq!(
        tools_of(&body),
        vec![
            json!({ "type": "code_interpreter", "container": { "type": "auto", "file_ids": ["file-xlsx1"] } })
        ],
        "no vector store, so no file_search"
    );
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
}

#[tokio::test]
async fn every_ready_xlsx_is_listed_and_documents_and_images_are_not() {
    let (env, user, chat) = env_user_chat().await;
    upload_xlsx(&env, &user, &chat, "file-x2").await;
    upload_xlsx(&env, &user, &chat, "file-x1").await;
    upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;
    upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;

    expect_stream(&send_text(&env, &user, &chat, "both tools").await);

    let body = chat_request_body(&env, 0);
    assert_eq!(
        container_file_ids(&tool(&body, "code_interpreter").unwrap()),
        vec!["file-x1".to_owned(), "file-x2".to_owned()]
    );
    assert_eq!(
        tool(&body, "file_search").unwrap()["vector_store_ids"],
        json!(["vs_chat1"])
    );
}

#[tokio::test]
async fn a_deleted_xlsx_is_no_longer_offered() {
    let (env, user, chat) = env_user_chat().await;
    let keep = upload_xlsx(&env, &user, &chat, "file-keep").await;
    let gone = upload_xlsx(&env, &user, &chat, "file-gone").await;
    delete_ok(&env, &user, &chat, detail_id(&gone)).await;

    expect_stream(&send_text(&env, &user, &chat, "one sheet").await);
    delete_ok(&env, &user, &chat, detail_id(&keep)).await;
    expect_stream(&send_text(&env, &user, &chat, "no sheet").await);

    assert_eq!(
        container_file_ids(&tool(&chat_request_body(&env, 0), "code_interpreter").unwrap()),
        vec!["file-keep".to_owned()]
    );
    assert!(chat_request_body(&env, 1).get("tools").is_none());
}

#[tokio::test]
async fn a_model_without_the_tools_gets_neither_at_stream_time() {
    // The harness `gpt-standard` model has no file_search / code_interpreter tool support.
    let (env, user, chat) = env_user_chat().await;
    upload_xlsx(&env, &user, &chat, "file-xlsx1").await;
    upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;
    set_chat_model(&env, &chat, STANDARD_MODEL).await;

    expect_stream(&send_text(&env, &user, &chat, "hi").await);

    assert!(
        chat_request_body(&env, 0).get("tools").is_none(),
        "the effective model decides"
    );
}

// ===========================================================================
// Image input
// ===========================================================================

#[tokio::test]
async fn an_uploaded_image_is_appended_as_input_image_on_the_current_message() {
    let (env, user, chat) = env_user_chat().await;
    let image = upload_ready_image(&env, &user, &chat, "cat.png", "file-img1").await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "what is this", "attachment_ids": [image["id"]] }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "done");
    let body = chat_request_body(&env, 0);
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], json!("user"));
    assert_eq!(
        last["content"],
        json!([
            { "type": "input_text", "text": "what is this" },
            { "type": "input_image", "file_id": "file-img1" }
        ])
    );
}

#[tokio::test]
async fn images_are_not_reused_on_later_turns_unless_they_are_attached_again() {
    let (env, user, chat) = env_user_chat().await;
    let image = upload_ready_image(&env, &user, &chat, "cat.png", "file-img1").await;
    let attach = json!({ "content": "look", "attachment_ids": [image["id"]] });

    expect_stream(&send(&env, &user, &chat, attach.clone()).await);
    expect_stream(&send_text(&env, &user, &chat, "and now without it").await);
    expect_stream(&send(&env, &user, &chat, attach).await);

    let second = input_text(&chat_request_body(&env, 1));
    assert!(
        !second.contains("input_image") && !second.contains("file-img1"),
        "uploading or attaching an image once does not carry it to later turns: {second}"
    );
    let third = input_text(&chat_request_body(&env, 2));
    assert_eq!(
        third.matches("input_image").count(),
        1,
        "explicit re-attachment includes it once, on the current message only: {third}"
    );
    let body = chat_request_body(&env, 2);
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["content"][1]["file_id"], json!("file-img1"));
}

#[tokio::test]
async fn several_images_are_appended_in_attachment_order() {
    let (env, user, chat) = env_user_chat().await;
    let first = upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;
    let second = upload_ready_image(&env, &user, &chat, "b.png", "file-img2").await;

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "compare", "attachment_ids": [first["id"], second["id"]] }),
        )
        .await,
    );

    let body = chat_request_body(&env, 0);
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"][1],
        json!({ "type": "input_image", "file_id": "file-img1" })
    );
    assert_eq!(
        last["content"][2],
        json!({ "type": "input_image", "file_id": "file-img2" })
    );
}

#[tokio::test]
async fn more_images_than_the_per_message_limit_are_400_too_many_images() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "rag": { "max_images_per_message": 1 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let first = upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;
    let second = upload_ready_image(&env, &user, &chat, "b.png", "file-img2").await;

    let rejected = send(
        &env,
        &user,
        &chat,
        json!({ "content": "two", "attachment_ids": [first["id"], second["id"]] }),
    )
    .await;

    assert_out_of_range(&rejected, "TOO_MANY_IMAGES");
    assert_nothing_happened(&env).await;
    assert!(
        env.provider().chat_requests().is_empty(),
        "no provider call"
    );
    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "one", "attachment_ids": [first["id"]] }),
        )
        .await,
    );
}

#[tokio::test]
async fn an_image_for_a_model_without_vision_is_400_vision_not_supported() {
    let mut no_vision = catalog_entry(PREMIUM_MODEL, "Premium", true, true);
    no_vision["multimodal_capabilities"] = json!([]);
    let env = env_with_policy(policy(vec![
        no_vision,
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hi", "attachment_ids": [image["id"]] }),
    )
    .await;

    assert_invalid_argument(&resp, "content_type", "VISION_NOT_SUPPORTED");
    assert_nothing_happened(&env).await;
    assert!(env.provider().chat_requests().is_empty());
}

// ===========================================================================
// attachment_ids validation and message_attachments
// ===========================================================================

#[tokio::test]
async fn attachment_ids_of_unusable_uploads_are_400_invalid_attachment() {
    let (env, user, chat) = env_user_chat().await;
    let other_chat = new_chat(&env, &user).await;
    let in_other_chat = upload_ready_image(&env, &user, &other_chat, "a.png", "file-img1").await;
    let deleted = upload_ready_image(&env, &user, &chat, "b.png", "file-img2").await;
    delete_ok(&env, &user, &chat, detail_id(&deleted)).await;
    env.provider().enqueue("/v1/files", provider_error(500));
    upload(&env, &user, &chat, "c.png", PNG, &png(32, 32)).await;
    let failed = chat_rows(&env, &chat)
        .await
        .into_iter()
        .find(|row| row.status == "failed")
        .expect("a failed upload row");
    let cases = [
        ("another chat's attachment", in_other_chat["id"].clone()),
        ("a deleted attachment", deleted["id"].clone()),
        ("a failed upload", json!(failed.id.to_string())),
    ];

    for (name, id) in cases {
        let resp = send(
            &env,
            &user,
            &chat,
            json!({ "content": "hello", "attachment_ids": [id] }),
        )
        .await;

        assert_invalid_argument(&resp, "attachment", "invalid_attachment");
        assert!(
            env.provider().chat_requests().is_empty(),
            "{name}: no provider call"
        );
    }
    assert_nothing_happened(&env).await;
    assert!(link_rows(&env, &chat).await.is_empty());
}

#[tokio::test]
async fn a_duplicate_attachment_id_is_rejected_before_any_provider_call() {
    let (env, user, chat) = env_user_chat().await;
    let image = upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": [image["id"], image["id"]] }),
    )
    .await;

    assert_invalid_argument(&resp, "attachment", "invalid_attachment");
    assert!(env.provider().chat_requests().is_empty());
}

#[tokio::test]
async fn a_sent_message_is_linked_to_exactly_its_attachments() {
    let (env, user, chat) = env_user_chat().await;
    let document =
        upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;
    let image = upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;
    let unused = upload_ready_image(&env, &user, &chat, "b.png", "file-img2").await;

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "both", "attachment_ids": [document["id"], image["id"]] }),
        )
        .await,
    );

    let page = list_messages_ok(&env, &user, &chat, &[]).await;
    let user_message = common::message_ids(&page)[0]
        .replace('-', "")
        .to_uppercase();
    let mut linked: Vec<String> = link_rows(&env, &chat)
        .await
        .into_iter()
        .map(|(message, attachment)| {
            assert_eq!(message, user_message, "linked to the user message");
            attachment
        })
        .collect();
    linked.sort();
    let mut expected: Vec<String> = [&document, &image]
        .iter()
        .map(|detail| {
            detail["id"]
                .as_str()
                .unwrap()
                .replace('-', "")
                .to_uppercase()
        })
        .collect();
    expected.sort();
    assert_eq!(linked, expected, "the unused attachment is not linked");
    assert_eq!(row(&env, detail_id(&unused)).await.status, "ready");
}

#[tokio::test]
async fn a_document_in_attachment_ids_is_listed_but_not_sent_as_an_input() {
    let (env, user, chat) = env_user_chat().await;
    let document = upload_ready_document(
        &env,
        &user,
        &chat,
        "Q3 Report.pdf",
        "file-doc1",
        "vs_chat1",
        true,
    )
    .await;

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "summarise", "attachment_ids": [document["id"]] }),
        )
        .await,
    );

    let body = chat_request_body(&env, 0);
    let serialized = input_text(&body);
    assert!(
        !serialized.contains("file-doc1") && !serialized.contains("input_file"),
        "documents reach the model through file_search only: {serialized}"
    );
    assert!(tool(&body, "file_search").is_some());
    let page = list_messages_ok(&env, &user, &chat, &[]).await;
    let attachments = items(&page)[0]["attachments"].as_array().unwrap().clone();
    assert_eq!(attachments.len(), 1);
    let mut summary = attachments[0].clone();
    if summary.get("img_thumbnail") == Some(&Value::Null) {
        summary.as_object_mut().unwrap().remove("img_thumbnail");
    }
    assert_eq!(
        summary,
        json!({
            "attachment_id": document["id"], "kind": "document",
            "filename": "Q3 Report.pdf", "status": "ready"
        })
    );
}

#[tokio::test]
async fn the_messages_list_carries_attachment_summaries_with_the_image_thumbnail() {
    let (env, user, chat) = env_user_chat().await;
    script_plain_file(&env, "file-img1");
    let image = expect_created(
        &upload(
            &env,
            &user,
            &chat,
            "wide.jpg",
            JPEG,
            &attachments::jpeg(400, 200),
        )
        .await,
    );
    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image["id"]] }),
        )
        .await,
    );

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    let messages = items(&page);
    let summary = &messages[0]["attachments"][0];
    assert_keys(
        summary,
        &[
            "attachment_id",
            "kind",
            "filename",
            "status",
            "img_thumbnail",
        ],
    );
    assert_eq!(summary["attachment_id"], image["id"]);
    assert_eq!(summary["kind"], json!("image"));
    assert_eq!(summary["filename"], json!("wide.jpg"));
    assert_eq!(summary["status"], json!("ready"));
    assert_eq!(summary["img_thumbnail"], image["img_thumbnail"]);
    assert_thumbnail(&summary["img_thumbnail"]);
    assert!(
        messages[1]["attachments"].as_array().unwrap().is_empty(),
        "assistant messages have none"
    );
}

#[tokio::test]
async fn deleted_attachments_are_not_listed_on_their_messages() {
    let (env, user, chat) = env_user_chat().await;
    let keep = upload_ready_image(&env, &user, &chat, "keep.png", "file-img1").await;
    let drop = upload_ready_image(&env, &user, &chat, "drop.png", "file-img2").await;
    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [keep["id"], drop["id"]] }),
        )
        .await,
    );
    // The API refuses to delete a referenced attachment (409); a soft delete by other means
    // (here SQL) must still hide it from the message.
    mutation::soft_delete_attachment(&env, detail_id(&drop)).await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    let attachments = items(&page)[0]["attachments"].as_array().unwrap().clone();
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0]["attachment_id"], keep["id"]);
}

// ===========================================================================
// File citations
// ===========================================================================

#[tokio::test]
async fn citations_resolve_uploaded_documents_and_omit_deleted_and_foreign_files() {
    let (env, user, chat) = env_user_chat().await;
    let other_chat = new_chat(&env, &user).await;
    upload_ready_document(
        &env,
        &user,
        &other_chat,
        "other.pdf",
        "file-other",
        "vs_other",
        true,
    )
    .await;
    let kept = upload_ready_document(
        &env,
        &user,
        &chat,
        "Kept Report.pdf",
        "file-kept",
        "vs_chat1",
        true,
    )
    .await;
    let removed = upload_ready_document(
        &env,
        &user,
        &chat,
        "Removed.pdf",
        "file-removed",
        "vs_chat1",
        false,
    )
    .await;
    delete_ok(&env, &user, &chat, detail_id(&removed)).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(citation_frames(&[
            "file-kept",
            "file-removed",
            "file-other",
            "file-never-seen",
        ])),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "what do the files say").await);

    let citations = first(&events, "citations");
    assert_eq!(
        citations.data["items"],
        json!([{
            "source": "file",
            "attachment_id": kept["id"],
            "title": "Kept Report.pdf",
            "snippet": ""
        }]),
        "deleted, other-chat and unknown provider files are omitted"
    );
    let serialized = citations.data.to_string();
    assert!(
        !serialized.contains("file-") && !serialized.contains("vs_"),
        "no provider identifier reaches the client: {serialized}"
    );
}

#[tokio::test]
async fn every_cited_document_of_the_chat_is_resolved_with_its_own_filename() {
    let (env, user, chat) = env_user_chat().await;
    let first_doc =
        upload_ready_document(&env, &user, &chat, "One.pdf", "file-one", "vs_chat1", true).await;
    let second_doc =
        upload_ready_document(&env, &user, &chat, "Two.pdf", "file-two", "vs_chat1", false).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(citation_frames(&["file-two", "file-one"])),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "compare").await);

    let mut resolved: Vec<(String, String)> = first(&events, "citations").data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["attachment_id"].as_str().unwrap().to_owned(),
                item["title"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    resolved.sort();
    let mut expected = vec![
        (
            first_doc["id"].as_str().unwrap().to_owned(),
            "One.pdf".to_owned(),
        ),
        (
            second_doc["id"].as_str().unwrap().to_owned(),
            "Two.pdf".to_owned(),
        ),
    ];
    expected.sort();
    assert_eq!(resolved, expected);
}

// ===========================================================================
// Mutated turns carry the uploaded attachments forward (end to end)
// ===========================================================================

async fn turn_with_uploads() -> (TestEnv, TestUser, String, Value, Value, uuid::Uuid) {
    let (env, user, chat) = env_user_chat().await;
    let document =
        upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;
    let image = upload_ready_image(&env, &user, &chat, "cat.png", "file-img1").await;
    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "original", "attachment_ids": [document["id"], image["id"]] }),
        )
        .await,
    );
    let request_id = stream_request_id(&events);
    (env, user, chat, document, image, request_id)
}

async fn assert_carried_forward(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    document: &Value,
    image: &Value,
) {
    let body = mutation::last_chat_body(env);
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"][1],
        json!({ "type": "input_image", "file_id": "file-img1" }),
        "the image is part of the mutated turn's input: {last}"
    );
    assert_eq!(
        tool(&body, "file_search").unwrap()["vector_store_ids"],
        json!(["vs_chat1"])
    );
    let page = list_messages_ok(env, user, chat, &[]).await;
    let messages = items(&page);
    assert_eq!(messages.len(), 2, "the replaced turn is gone: {page}");
    let mut listed: Vec<String> = messages[0]["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|attachment| attachment["attachment_id"].as_str().unwrap().to_owned())
        .collect();
    listed.sort();
    let mut expected = vec![
        document["id"].as_str().unwrap().to_owned(),
        image["id"].as_str().unwrap().to_owned(),
    ];
    expected.sort();
    assert_eq!(
        listed, expected,
        "the new user message lists both attachments"
    );
    let new_user_message = common::message_ids(&page)[0]
        .replace('-', "")
        .to_uppercase();
    let links = link_rows(env, chat).await;
    let new_links = links
        .iter()
        .filter(|(message, _)| *message == new_user_message)
        .count();
    assert_eq!(
        new_links, 2,
        "message_attachments rows of the new message: {links:?}"
    );
}

#[tokio::test]
async fn a_retried_turn_carries_its_uploaded_attachments_forward() {
    let (env, user, chat, document, image, request_id) = turn_with_uploads().await;

    let resp = mutation::retry(&env, &user, &chat, request_id).await;

    let events = mutation::expect_mutated(&resp, mutation::Op::Retry);
    assert_eq!(terminal(&events).event, "done");
    assert_carried_forward(&env, &user, &chat, &document, &image).await;
}

#[tokio::test]
async fn an_edited_turn_carries_its_uploaded_attachments_forward() {
    let (env, user, chat, document, image, request_id) = turn_with_uploads().await;

    let resp = mutation::edit(&env, &user, &chat, request_id, "edited question").await;

    let events = mutation::expect_mutated(&resp, mutation::Op::Edit);
    assert_eq!(terminal(&events).event, "done");
    assert_carried_forward(&env, &user, &chat, &document, &image).await;
    let body = mutation::last_chat_body(&env);
    assert_eq!(
        body["input"].as_array().unwrap().last().unwrap()["content"][0]["text"],
        json!("edited question")
    );
}

#[tokio::test]
async fn the_attachments_of_a_mutated_turn_stay_locked_against_deletion() {
    let (env, user, chat, _document, image, request_id) = turn_with_uploads().await;
    mutation::expect_mutated(
        &mutation::retry(&env, &user, &chat, request_id).await,
        mutation::Op::Retry,
    );

    let resp =
        attachments::delete_attachment(&env, &user, &chat, image["id"].as_str().unwrap()).await;

    let problem = common::assert_problem(&resp, 409, Some("already_exists"));
    assert_eq!(
        problem["context"]["resource_name"],
        json!("attachment_locked")
    );
}

#[tokio::test]
async fn a_chat_request_is_the_only_provider_traffic_of_a_send_with_ready_uploads() {
    let (env, user, chat) = env_user_chat().await;
    let image = upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await;
    let before = calls(&env).len();

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image["id"]] }),
        )
        .await,
    );

    assert_eq!(
        calls(&env).len(),
        before,
        "sending does not touch the Files or Vector Stores APIs"
    );
}
