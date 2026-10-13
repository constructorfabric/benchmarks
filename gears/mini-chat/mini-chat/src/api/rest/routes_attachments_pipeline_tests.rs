#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration with the send pipeline of
//! `cpt-cf-mini-chat-feature-streaming-turns`
//! (`cpt-cf-mini-chat-dod-attachments-send-pipeline-data`): what an upload leaves
//! behind is what the next send reads: the chat vector store of a ready document
//! enables `file_search`, a ready spreadsheet feeds the code interpreter, a ready
//! image is sent by file id, citations resolve to the attachment, and a row that
//! is not ready (or is deleted) never contributes anything.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::attachment_test_env::{ALIAS, ANTHROPIC_ALIAS, AttEnv, png_bytes};
use crate::chat_test_env::Reply;
use crate::domain::service::quota::test_support::model;
use crate::stream_test_env::{body, oa, rid};
use mini_chat_sdk::ModelTier;

const PDF: &str = "application/pdf";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

async fn setup(model: &str) -> (AttEnv, Uuid) {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat(model).await;
    (env, chat)
}

fn id_of(reply: &Reply) -> Uuid {
    Uuid::parse_str(
        reply.json["id"]
            .as_str()
            .unwrap_or_else(|| panic!("no id in {}", reply.text)),
    )
    .unwrap()
}

/// The body of the last request sent to the chat endpoint of `alias`.
fn last_chat_request(env: &AttEnv, alias: &str, path: &str) -> Value {
    env.oagw
        .requests_to("POST", &format!("/{alias}{path}"))
        .last()
        .unwrap_or_else(|| panic!("no chat request: {:?}", env.sequence()))
        .json()
}

fn tools(request: &Value) -> Vec<Value> {
    request["tools"].as_array().cloned().unwrap_or_default()
}

fn tool_types(request: &Value) -> Vec<String> {
    tools(request)
        .iter()
        .map(|t| t["type"].as_str().unwrap().to_owned())
        .collect()
}

fn max_results() -> u32 {
    model("m-oa", ModelTier::Standard).build().max_num_results
}

#[tokio::test]
async fn a_ready_document_turns_file_search_on_with_the_chat_vector_store() {
    let (env, chat) = setup("m-oa").await;
    let uploaded = env
        .upload(chat, "report.pdf", PDF, b"%PDF-1.7 contract")
        .await;
    assert_eq!(uploaded.json["status"], "ready", "{}", uploaded.text);
    env.stream
        .script(&[oa::delta("Hello"), oa::completed(10, 5)]);

    let streamed = env
        .stream
        .stream(chat, &body("what does the contract say?", rid(1)))
        .await;

    assert_eq!(streamed.last().name, "done", "{:?}", streamed.names());
    let request = last_chat_request(&env, ALIAS, "/v1/responses");
    assert_eq!(
        tools(&request),
        vec![
            json!({ "type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": max_results() })
        ]
    );
    assert_eq!(request["metadata"]["feature"], "file_search");
}

#[tokio::test]
async fn a_file_citation_resolves_to_the_attachment_and_stops_after_the_delete() {
    let (env, chat) = setup("m-oa").await;
    let uploaded = env.upload(chat, "report.pdf", PDF, b"%PDF-1.7").await;
    let attachment = id_of(&uploaded);
    let cited = || {
        vec![
            oa::delta("Answer"),
            oa::annotation(
                json!({ "type": "file_citation", "file_id": "file-1", "filename": "x" }),
            ),
            oa::completed(10, 5),
        ]
    };
    env.stream.script(&cited());

    let first = env.stream.stream(chat, &body("q", rid(1))).await;

    assert_eq!(
        first.of("citations")[0].data,
        json!({ "items": [
            { "source": "file", "title": "report.pdf", "snippet": "", "attachment_id": attachment.to_string() }
        ]}),
        "provider_file_id is exactly the id the provider returns in file_citation.file_id"
    );
    assert!(
        !first.reply.text.contains("file-1"),
        "no provider id on the wire"
    );

    let deleted = env.delete_attachment(chat, attachment).await;
    assert_eq!(deleted.status.as_u16(), 204);
    env.stream.script(&cited());
    let second = env.stream.stream(chat, &body("q again", rid(2))).await;

    assert!(
        second.of("citations").is_empty(),
        "no citation for a deleted attachment: {:?}",
        second.names()
    );
}

#[tokio::test]
async fn a_deleted_document_stops_enabling_file_search_at_once() {
    let (env, chat) = setup("m-oa").await;
    let uploaded = env.upload(chat, "report.pdf", PDF, b"%PDF-1.7").await;
    env.delete_attachment(chat, id_of(&uploaded)).await;
    env.stream.script(&[oa::completed(1, 1)]);

    env.stream.stream(chat, &body("q", rid(1))).await;

    let request = last_chat_request(&env, ALIAS, "/v1/responses");
    assert!(tool_types(&request).is_empty(), "{request}");
    assert_eq!(request["metadata"]["feature"], "none");
}

#[tokio::test]
async fn a_ready_spreadsheet_feeds_the_code_interpreter_with_its_provider_file() {
    let (env, chat) = setup("m-oa").await;
    let uploaded = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;
    assert_eq!(uploaded.json["status"], "ready");
    env.stream.script(&[oa::completed(1, 1)]);

    env.stream.stream(chat, &body("sum column B", rid(1))).await;

    let request = last_chat_request(&env, ALIAS, "/v1/responses");
    assert!(
        tools(&request).contains(&json!({ "type": "code_interpreter", "container": { "type": "auto", "file_ids": ["file-1"] } })),
        "{request}"
    );
    assert!(
        !tool_types(&request).contains(&"file_search".to_owned()),
        "a spreadsheet is not indexed"
    );
}

#[tokio::test]
async fn a_ready_image_is_sent_by_its_provider_file_id() {
    let (env, chat) = setup("m-oa").await;
    let uploaded = env
        .upload(chat, "photo.png", "image/png", &png_bytes(64, 64))
        .await;
    let attachment = id_of(&uploaded);
    env.stream.script(&[oa::completed(1, 1)]);

    let streamed = env
        .stream
        .stream(
            chat,
            &json!({ "content": "what is this?", "request_id": rid(1), "attachment_ids": [attachment] }),
        )
        .await;

    assert_eq!(streamed.last().name, "done", "{:?}", streamed.names());
    let request = last_chat_request(&env, ALIAS, "/v1/responses");
    assert!(
        request
            .to_string()
            .contains(&json!({ "type": "input_image", "file_id": "file-1" }).to_string()),
        "{request}"
    );
    assert!(tool_types(&request).is_empty(), "an image enables no tool");
}

#[tokio::test]
async fn an_image_in_an_anthropic_chat_is_sent_as_an_image_block_with_its_secondary_file() {
    let (env, chat) = setup("m-an").await;
    let uploaded = env
        .upload(chat, "photo.png", "image/png", &png_bytes(64, 64))
        .await;
    let attachment = id_of(&uploaded);
    assert_eq!(uploaded.json["status"], "ready");

    let _ = env
        .stream
        .stream(
            chat,
            &json!({ "content": "what is this?", "request_id": rid(1), "attachment_ids": [attachment] }),
        )
        .await;

    let request = last_chat_request(&env, ANTHROPIC_ALIAS, "/v1/messages");
    assert!(
        request.to_string().contains(
            &json!({ "type": "image", "source": { "type": "file", "file_id": "file_ant_1" } })
                .to_string()
        ),
        "{request}"
    );
    assert!(
        !request.to_string().contains("\"file-1\""),
        "the primary file is not sent to Anthropic: {request}"
    );
}

#[tokio::test]
async fn an_attachment_that_is_not_ready_is_refused_and_never_contributes_a_tool() {
    let (env, chat) = setup("m-oa").await;
    env.seed_store(chat, Some("vs_chat"), "openai", env.now())
        .await;
    let mut not_ready = Vec::new();
    for (n, status) in [(1, "pending"), (2, "uploaded"), (3, "failed")] {
        not_ready.push(env.seed(chat, n, |row| row.set("status", status)).await);
    }
    env.seed(chat, 4, |row| {
        row.set("deleted_at", crate::chat_test_env::ts(5))
    })
    .await;

    for id in &not_ready {
        let reply = env
            .stream
            .post(
                chat,
                &json!({ "content": "q", "request_id": Uuid::new_v4(), "attachment_ids": [id] }),
            )
            .await;

        reply.assert_invalid("attachment", "invalid_attachment");
    }
    env.stream.script(&[oa::completed(1, 1)]);
    env.stream.stream(chat, &body("q", rid(9))).await;
    let request = last_chat_request(&env, ALIAS, "/v1/responses");
    assert!(
        tool_types(&request).is_empty(),
        "no ready document: a vector store row alone does not enable file_search: {request}"
    );
    // A real upload makes the difference.
    let uploaded = env.upload(chat, "ready.pdf", PDF, b"%PDF").await;
    assert_eq!(uploaded.json["status"], "ready", "{}", uploaded.text);
    env.stream.script(&[oa::completed(1, 1)]);
    env.stream.stream(chat, &body("q", rid(10))).await;
    assert_eq!(
        tool_types(&last_chat_request(&env, ALIAS, "/v1/responses")),
        vec!["file_search".to_owned()]
    );
}

#[tokio::test]
async fn a_second_document_joins_the_same_store_the_send_searches() {
    let (env, chat) = setup("m-oa").await;
    env.upload(chat, "a.pdf", PDF, b"%PDF a").await;
    env.upload(chat, "b.pdf", PDF, b"%PDF b").await;
    env.stream.script(&[oa::completed(1, 1)]);

    env.stream.stream(chat, &body("q", rid(1))).await;

    let request = last_chat_request(&env, ALIAS, "/v1/responses");
    assert_eq!(tools(&request)[0]["vector_store_ids"], json!(["vs_1"]));
    assert_eq!(env.calls_ending("POST", "/v1/vector_stores").len(), 1);
}
