#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Provider-adapter features exercised through the REST API with `FakeOagw`
//! (S§9.2, ADR-0005, D§4 "Knowledge Search"): the knowledge-search agentic
//! loop and the Anthropic secondary copies of images.

mod common;

use axum::http::StatusCode;
use mini_chat::config::{MiniChatConfig, ProviderEntry};
use mini_chat::infra::db::repos::AttachmentRepo;
use serde_json::{Value, json};
use uuid::Uuid;

use common::*;

const KB_HOST: &str = "kb.openai.azure.com";
const KB_VS: &str = "vs_knowledgebase0001";
const KB_SEARCH_PATH: &str = "/openai/vector_stores/vs_knowledgebase0001/search";
const KB_API_VERSION: &str = "2025-04-01-preview";
const ANTHROPIC_HOST: &str = "api.anthropic.com";

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

fn provider(v: Value) -> ProviderEntry {
    serde_json::from_value(v).expect("provider entry")
}

/// Knowledge search on, served by an Azure entry `kb`; `max_calls`
/// retrievals per message, `top_k` 3, chunks cut at 12 characters.
fn knowledge_on(max_calls: u32) -> impl FnOnce(&mut MiniChatConfig) {
    move |c: &mut MiniChatConfig| {
        c.providers.insert(
            "kb".into(),
            provider(json!({
                "kind": "openai_responses", "host": KB_HOST, "storage_kind": "azure",
                "api_version": KB_API_VERSION, "api_path": "/openai/v1/responses",
            })),
        );
        let k = &mut c.knowledge_search;
        k.enabled = true;
        k.vector_store_id = Some(KB_VS.into());
        k.provider_id = Some("kb".into());
        k.max_calls_per_message = max_calls;
        k.top_k = 3;
        k.max_chunk_chars = 12;
    }
}

/// A Responses reply that ends with a function call of `name`.
fn function_call_reply(call_id: &str, name: &str, arguments: &Value) -> Vec<(&'static str, Value)> {
    vec![
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "call_id": call_id, "name": name,
                "arguments": arguments.to_string(),
            }}),
        ),
        completed(10, 2),
    ]
}

fn script_search(app: &TestApp) {
    app.oagw.push_json(
        KB_SEARCH_PATH,
        200,
        json!({
            "object": "vector_store.search_results.page",
            "search_query": "vacation policy",
            "data": [
                {"file_id": "assistant-abcdefghijklmnop", "filename": "handbook.pdf", "score": 0.91,
                 "attributes": {}, "content": [{"type": "text", "text": "Employees get 25 vacation days."}]},
                {"file_id": "assistant-bcdefghijklmnopq", "filename": "faq.md", "score": 0.5,
                 "attributes": {}, "content": [{"type": "text", "text": "Short."}]}
            ],
            "has_more": false, "next_page": null
        }),
    );
}

/// Provider (`/responses`) request bodies in order.
fn provider_bodies(app: &TestApp) -> Vec<Value> {
    app.oagw
        .requests()
        .into_iter()
        .filter(|r| r.uri.contains("/responses"))
        .map(|r| r.json_body.unwrap())
        .collect()
}

fn search_requests(app: &TestApp) -> Vec<Value> {
    app.oagw
        .requests()
        .into_iter()
        .filter(|r| r.uri.contains("/search"))
        .map(|r| {
            assert_eq!(r.method, "POST");
            assert_eq!(
                r.uri,
                format!("/{KB_HOST}{KB_SEARCH_PATH}?api-version={KB_API_VERSION}")
            );
            r.json_body.unwrap()
        })
        .collect()
}

fn has_knowledge_tool(body: &Value) -> bool {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| t["type"] == "function" && t["name"] == "search_knowledge")
}

/// `function_call_output` items of a Responses body.
fn outputs(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .map(|i| i["output"].as_str().unwrap().to_owned())
        .collect()
}

async fn send_hi(client: &UserClient<'_>, chat: Uuid) -> Vec<(String, Value)> {
    let resp = client
        .post_json(
            &stream_path(chat),
            &json!({"content": "What is the vacation policy?"}),
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    resp.sse_events()
}

// ---------------------------------------------------------------------------
// Knowledge search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_knowledge_loop_runs_retrieval_and_continues() {
    let app = TestApp::builder().config(knowledge_on(3)).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_sse(
        PROVIDER_PATH,
        function_call_reply(
            "call_1",
            "search_knowledge",
            &json!({"query": "vacation policy", "top_k": 10}),
        ),
    );
    script_search(&app);
    app.oagw
        .push_sse(PROVIDER_PATH, ok_reply(&["25 days", "."], 20, 5));

    let events = send_hi(&client, chat).await;
    assert_eq!(
        names(&events),
        vec!["stream_started", "delta", "delta", "done"],
        "{events:?}"
    );
    assert_eq!(
        event(&events, "done")["usage"],
        json!({"input_tokens": 20, "output_tokens": 5})
    );

    let bodies = provider_bodies(&app);
    assert_eq!(bodies.len(), 2);
    assert!(has_knowledge_tool(&bodies[0]), "{}", bodies[0]);
    assert!(
        bodies[0]["instructions"]
            .as_str()
            .unwrap()
            .contains(&app.config.knowledge_search.guard),
        "guard appended: {}",
        bodies[0]["instructions"]
    );
    // The model's top_k (10) is capped at knowledge_search.top_k (3).
    assert_eq!(
        search_requests(&app),
        vec![json!({"query": "vacation policy", "max_num_results": 3})]
    );
    // Second request: the call and its output appended to the input.
    let input = bodies[1]["input"].as_array().unwrap();
    let call = input.iter().find(|i| i["type"] == "function_call").unwrap();
    assert_eq!(call["call_id"], "call_1");
    assert_eq!(call["name"], "search_knowledge");
    let outs = outputs(&bodies[1]);
    assert_eq!(outs.len(), 1);
    let out: Value = serde_json::from_str(&outs[0]).unwrap();
    assert_eq!(
        out["results"],
        json!([
            {"filename": "handbook.pdf", "score": 0.91, "text": "Employees ge"},
            {"filename": "faq.md", "score": 0.5, "text": "Short."},
        ])
    );

    let turn = all_turns(&app).await.pop().unwrap();
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.file_search_completed_count, 1);
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage[0]["file_search_calls"], 1, "{}", usage[0]);
    assert_eq!(usage[0]["usage"]["input_tokens"], 20);
}

#[tokio::test]
async fn limit_reached_output_after_max_calls() {
    let app = TestApp::builder().config(knowledge_on(1)).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "s1").await;
    let args = json!({"query": "vacation policy"});
    app.oagw.push_sse(
        PROVIDER_PATH,
        function_call_reply("call_1", "search_knowledge", &args),
    );
    script_search(&app);
    app.oagw.push_sse(
        PROVIDER_PATH,
        function_call_reply("call_2", "search_knowledge", &args),
    );
    app.oagw
        .push_sse(PROVIDER_PATH, ok_reply(&["Answer"], 30, 4));

    let events = send_hi(&client, chat).await;
    assert_eq!(names(&events).last(), Some(&"done"), "{events:?}");
    assert_eq!(search_requests(&app).len(), 1, "one retrieval only");
    let bodies = provider_bodies(&app);
    assert_eq!(bodies.len(), 3);
    let outs = outputs(&bodies[2]);
    assert_eq!(outs.len(), 2);
    assert!(outs[0].contains("handbook.pdf"), "{}", outs[0]);
    assert!(
        outs[1].to_lowercase().contains("limit reached"),
        "limit output: {}",
        outs[1]
    );
    let turn = all_turns(&app).await.pop().unwrap();
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.file_search_completed_count, 1);
}

#[tokio::test]
async fn iteration_cap_is_agentic_iterations_exceeded() {
    // max_calls_per_message 1 → at most 3 provider requests.
    let app = TestApp::builder().config(knowledge_on(1)).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "s1").await;
    let args = json!({"query": "vacation policy"});
    script_search(&app);
    for i in 0..4 {
        app.oagw.push_sse(
            PROVIDER_PATH,
            function_call_reply(&format!("call_{i}"), "search_knowledge", &args),
        );
    }

    let events = send_hi(&client, chat).await;
    assert_eq!(names(&events).last(), Some(&"error"), "{events:?}");
    assert_eq!(
        event(&events, "error")["code"],
        "agentic_iterations_exceeded"
    );
    assert_eq!(provider_bodies(&app).len(), 3);
    assert_eq!(search_requests(&app).len(), 1);
    let turn = all_turns(&app).await.pop().unwrap();
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("agentic_iterations_exceeded")
    );
}

#[tokio::test]
async fn unknown_function_is_unexpected_tool_use() {
    let app = TestApp::builder().config(knowledge_on(3)).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_sse(
        PROVIDER_PATH,
        function_call_reply("call_1", "load_files", &json!({})),
    );

    let events = send_hi(&client, chat).await;
    assert_eq!(event(&events, "error")["code"], "unexpected_tool_use");
    assert!(search_requests(&app).is_empty());
    assert_eq!(provider_bodies(&app).len(), 1);
    let turn = all_turns(&app).await.pop().unwrap();
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("unexpected_tool_use"));
}

#[tokio::test]
async fn knowledge_off_when_file_search_included() {
    let app = TestApp::builder().config(knowledge_on(3)).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "s1").await;
    upload_doc_ready(
        &app,
        &client,
        chat,
        "file-doc00000000000001",
        "vs_chat0000000000001",
        true,
    )
    .await;
    push_hello(&app);

    let events = send_hi(&client, chat).await;
    assert_eq!(names(&events).last(), Some(&"done"), "{events:?}");
    let body = provider_bodies(&app).pop().unwrap();
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["type"].as_str().unwrap())
        .collect();
    assert_eq!(tools, vec!["file_search"]);
    assert!(
        !body["instructions"]
            .as_str()
            .unwrap()
            .contains(&app.config.knowledge_search.guard)
    );
}

#[tokio::test]
async fn knowledge_off_when_provider_kind_unsupported() {
    let app = TestApp::builder()
        .config(knowledge_on(3))
        .config(|c| {
            c.providers.get_mut("kb").unwrap().kind =
                mini_chat::config::ProviderKind::OpenaiChatCompletions;
        })
        .build()
        .await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "s1").await;
    push_hello(&app);

    send_hi(&client, chat).await;
    let body = provider_bodies(&app).pop().unwrap();
    assert!(!has_knowledge_tool(&body), "{body}");
}

// ---------------------------------------------------------------------------
// Anthropic secondary files
// ---------------------------------------------------------------------------

/// An `anthropic` entry (files stored with `openai`) and a catalog model
/// `claude` served by it.
fn anthropic_app() -> TestAppBuilder {
    let mut claude = standard_model("claude");
    claude.provider_id = "anthropic".into();
    TestApp::builder()
        .catalog(vec![standard_model("s1"), claude])
        .config(|c| {
            c.providers.insert(
                "anthropic".into(),
                provider(json!({
                    "kind": "anthropic_messages", "host": ANTHROPIC_HOST,
                    "api_path": "/v1/messages", "storage_kind": "openai",
                    "rag_provider": "openai",
                })),
            );
        })
}

fn anthropic_files_path() -> String {
    format!("/{ANTHROPIC_HOST}/v1/files")
}

#[tokio::test]
async fn image_secondary_copy_uploaded_for_anthropic_chat() {
    let app = anthropic_app().build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "claude").await;
    script_file(&app, "file-img0000000000000001");
    app.oagw.push_json(
        &anthropic_files_path(),
        200,
        json!({"id": "file_011CNha8iCJcU1wXNR6q4V8w", "type": "file"}),
    );

    let body = created(&upload(&client, chat, "pic.png", "image/png", &png(300, 200)).await);
    assert_eq!(body["status"], "ready", "{body}");

    let uploads = requests_matching(&app, "POST", "/v1/files");
    assert_eq!(uploads.len(), 2, "primary + secondary upload");
    assert!(uploads[0].uri.starts_with("/api.openai.com/"));
    let secondary = &uploads[1];
    assert_eq!(secondary.uri, anthropic_files_path());
    let names: Vec<&str> = secondary
        .multipart_fields
        .iter()
        .map(|(n, _, _)| n.as_str())
        .collect();
    assert_eq!(names, vec!["file"], "only the file part, no purpose");
    assert_eq!(
        secondary.multipart_fields[0].2.as_deref(),
        Some("image/png")
    );
    assert!(
        secondary
            .headers
            .contains(&("anthropic-version".to_owned(), "2023-06-01".to_owned())),
        "{:?}",
        secondary.headers
    );

    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(
        row.provider_file_id.as_deref(),
        Some("file-img0000000000000001")
    );
    assert_eq!(
        row.secondary_file_id.as_deref(),
        Some("file_011CNha8iCJcU1wXNR6q4V8w")
    );
    assert_eq!(row.secondary_status, "uploaded");
    assert_eq!(row.secondary_provider_kind.as_deref(), Some("anthropic"));
}

#[tokio::test]
async fn no_secondary_copy_for_documents_or_non_anthropic_chats() {
    let app = anthropic_app().build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let claude_chat = create_chat(&client, "claude").await;
    upload_doc_ready(
        &app,
        &client,
        claude_chat,
        "file-doc00000000000001",
        "vs_chat0000000000001",
        true,
    )
    .await;
    let openai_chat = create_chat(&client, "s1").await;
    upload_image_ready(&app, &client, openai_chat, "file-img0000000000000002").await;

    assert!(requests_matching(&app, "POST", ANTHROPIC_HOST).is_empty());
    for row in attachment_rows(&app).await {
        assert_eq!(row.secondary_status, "not_attempted");
        assert!(row.secondary_file_id.is_none());
    }
}

#[tokio::test]
async fn failed_secondary_upload_keeps_the_image_ready() {
    let app = anthropic_app().build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "claude").await;
    script_file(&app, "file-img0000000000000001");
    app.oagw.push_json(
        &anthropic_files_path(),
        500,
        json!({"type": "error", "error": {"type": "api_error", "message": "boom"}}),
    );

    let body = created(&upload(&client, chat, "pic.png", "image/png", &png(300, 200)).await);
    assert_eq!(body["status"], "ready", "{body}");
    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(row.secondary_status, "failed");
    assert!(row.secondary_file_id.is_none());
}

#[tokio::test]
async fn anthropic_turn_sends_secondary_image_ids() {
    let app = anthropic_app().build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "claude").await;
    script_file(&app, "file-img0000000000000001");
    app.oagw.push_json(
        &anthropic_files_path(),
        200,
        json!({"id": "file_011secondary000001", "type": "file"}),
    );
    let body = created(&upload(&client, chat, "pic.png", "image/png", &png(300, 200)).await);
    let attachment: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    app.oagw.push_sse(
        "/v1/messages",
        vec![
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 9, "output_tokens": 1}}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "A cat"}}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ],
    );

    let resp = client
        .post_json(
            &stream_path(chat),
            &json!({"content": "What is this?", "attachment_ids": [attachment]}),
        )
        .await;
    let events = resp.sse_events();
    assert_eq!(names(&events).last(), Some(&"done"), "{events:?}");
    let req = requests_matching(&app, "POST", "/v1/messages")
        .pop()
        .unwrap();
    assert_eq!(req.uri, format!("/{ANTHROPIC_HOST}/v1/messages"));
    let messages = req.json_body.unwrap()["messages"].clone();
    let last = messages.as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"][1],
        json!({"type": "image", "source": {"type": "file", "file_id": "file_011secondary000001"}})
    );
}

#[tokio::test]
async fn secondary_copy_deleted_by_attachment_cleanup() {
    let app = anthropic_app().real_handlers().build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "claude").await;
    script_file(&app, "file-img0000000000000001");
    app.oagw.push_json(
        &anthropic_files_path(),
        200,
        json!({"id": "file_011secondary000001", "type": "file"}),
    );
    let body = created(&upload(&client, chat, "pic.png", "image/png", &png(300, 200)).await);
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let resp = client.delete(&attachment_path(chat, id)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    wait_until(10, || async {
        attachment_by_id(&app, id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    let deletes = requests_matching(&app, "DELETE", "/v1/files/");
    let uris: Vec<&str> = deletes.iter().map(|r| r.uri.as_str()).collect();
    assert!(
        uris.contains(&"/api.openai.com/v1/files/file-img0000000000000001"),
        "{uris:?}"
    );
    assert!(
        uris.contains(&"/api.anthropic.com/v1/files/file_011secondary000001"),
        "{uris:?}"
    );
}

#[tokio::test]
async fn secondary_copy_deleted_by_chat_cleanup() {
    let app = anthropic_app().real_handlers().build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "claude").await;
    script_file(&app, "file-img0000000000000001");
    app.oagw.push_json(
        &anthropic_files_path(),
        200,
        json!({"id": "file_011secondary000001", "type": "file"}),
    );
    let body = created(&upload(&client, chat, "pic.png", "image/png", &png(300, 200)).await);
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let resp = client.delete(&chat_path(chat)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    wait_until(10, || async {
        attachment_by_id(&app, id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(
        requests_matching(
            &app,
            "DELETE",
            "/api.anthropic.com/v1/files/file_011secondary000001"
        )
        .len(),
        1
    );
}

#[tokio::test]
async fn secondary_cleanup_skipped_without_anthropic_provider() {
    // The row has a secondary copy but no anthropic_messages entry is
    // configured any more: the delete is skipped and counted.
    let recorder = MetricsRecorder::new();
    let app = TestApp::builder()
        .real_handlers()
        .metrics(recorder.metrics.clone())
        .build()
        .await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let chat = create_chat(&client, "s1").await;
    let id = upload_image_ready(&app, &client, chat, "file-img0000000000000001").await;
    let rows = AttachmentRepo
        .set_secondary(
            &app.db.conn().unwrap(),
            &tenant_scope(t, u),
            chat,
            id,
            "uploaded",
            Some("file_011secondary000001"),
            time::OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert_eq!(rows, 1);

    let resp = client.delete(&chat_path(chat)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    wait_until(10, || async {
        attachment_by_id(&app, id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(recorder.counter("mini_chat_secondary_cleanup_skipped"), 1);
    assert!(requests_matching(&app, "DELETE", "file_011secondary000001").is_empty());
}

// ---------------------------------------------------------------------------
// Secondary deletes go to the chat model's Anthropic provider
// ---------------------------------------------------------------------------

const ANTHROPIC_A_HOST: &str = "a.anthropic.example";
const ANTHROPIC_B_HOST: &str = "b.anthropic.example";

/// Two Anthropic accounts: `anthropic_a` (first in id order) and
/// `anthropic_b`; the catalog model `claude-b` is served by `anthropic_b`.
fn two_anthropic_app(metrics: Option<&MetricsRecorder>) -> TestAppBuilder {
    let mut claude_b = standard_model("claude-b");
    claude_b.provider_id = "anthropic_b".into();
    let mut b = TestApp::builder()
        .catalog(vec![standard_model("s1"), claude_b])
        .real_handlers()
        .config(|c| {
            for (id, host) in [
                ("anthropic_a", ANTHROPIC_A_HOST),
                ("anthropic_b", ANTHROPIC_B_HOST),
            ] {
                c.providers.insert(
                    id.into(),
                    provider(json!({
                        "kind": "anthropic_messages", "host": host,
                        "api_path": "/v1/messages", "storage_kind": "openai",
                        "rag_provider": "openai",
                    })),
                );
            }
        });
    if let Some(m) = metrics {
        b = b.metrics(m.metrics.clone());
    }
    b
}

/// Upload an image into a `claude-b` chat (secondary copy on `anthropic_b`).
async fn claude_b_image(app: &TestApp, client: &UserClient<'_>) -> (Uuid, Uuid) {
    let chat = create_chat(client, "claude-b").await;
    let before = requests_matching(app, "POST", ANTHROPIC_B_HOST).len();
    script_file(app, "file-img0000000000000001");
    app.oagw.push_json(
        &format!("/{ANTHROPIC_B_HOST}/v1/files"),
        200,
        json!({"id": "file_011secondary000001", "type": "file"}),
    );
    let body = created(&upload(client, chat, "pic.png", "image/png", &png(300, 200)).await);
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let uploads = requests_matching(app, "POST", ANTHROPIC_B_HOST);
    assert_eq!(
        uploads.len(),
        before + 1,
        "secondary upload on the chat's provider"
    );
    (chat, id)
}

fn secondary_deletes(app: &TestApp) -> Vec<String> {
    requests_matching(app, "DELETE", "file_011secondary000001")
        .into_iter()
        .map(|r| r.uri)
        .collect()
}

#[tokio::test]
async fn attachment_cleanup_deletes_secondary_on_the_chat_providers_account() {
    let app = two_anthropic_app(None).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let (chat, id) = claude_b_image(&app, &client).await;

    let resp = client.delete(&attachment_path(chat, id)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    wait_until(10, || async {
        attachment_by_id(&app, id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(
        secondary_deletes(&app),
        vec![format!(
            "/{ANTHROPIC_B_HOST}/v1/files/file_011secondary000001"
        )]
    );
}

#[tokio::test]
async fn chat_cleanup_deletes_secondary_on_the_chat_providers_account() {
    let app = two_anthropic_app(None).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let (chat, id) = claude_b_image(&app, &client).await;

    let resp = client.delete(&chat_path(chat)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    wait_until(10, || async {
        attachment_by_id(&app, id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(
        secondary_deletes(&app),
        vec![format!(
            "/{ANTHROPIC_B_HOST}/v1/files/file_011secondary000001"
        )]
    );
}

/// The chat's model now resolves to a non-Anthropic provider.
fn move_claude_b_to_openai(app: &TestApp) {
    app.policy
        .set_catalog(vec![standard_model("s1"), standard_model("claude-b")]);
}

#[tokio::test]
async fn secondary_delete_skipped_when_chat_provider_is_no_longer_anthropic() {
    let recorder = MetricsRecorder::new();
    let app = two_anthropic_app(Some(&recorder)).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let (chat, id) = claude_b_image(&app, &client).await;
    let (chat2, id2) = claude_b_image(&app, &client).await;
    move_claude_b_to_openai(&app);

    // Attachment delete: skipped at enqueue time.
    let resp = client.delete(&attachment_path(chat, id)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    wait_until(10, || async {
        attachment_by_id(&app, id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(recorder.counter("mini_chat_secondary_cleanup_skipped"), 1);

    // Chat delete: skipped by the chat cleanup.
    let resp = client.delete(&chat_path(chat2)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    wait_until(10, || async {
        attachment_by_id(&app, id2).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(recorder.counter("mini_chat_secondary_cleanup_skipped"), 2);
    assert!(
        secondary_deletes(&app).is_empty(),
        "{:?}",
        secondary_deletes(&app)
    );
}

#[tokio::test]
async fn skip_metric_not_counted_when_the_delete_rolls_back() {
    let recorder = MetricsRecorder::new();
    let app = two_anthropic_app(Some(&recorder)).build().await;
    let (t, u) = ids();
    let client = app.as_user(u, t);
    let (chat, id) = claude_b_image(&app, &client).await;
    move_claude_b_to_openai(&app);
    // Reference the image from a message: the delete is refused (409).
    push_hello(&app);
    let resp = client
        .post_json(
            &stream_path(chat),
            &json!({"content": "What is this?", "attachment_ids": [id]}),
        )
        .await;
    assert_eq!(names(&resp.sse_events()).last(), Some(&"done"));

    let resp = client.delete(&attachment_path(chat, id)).await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "{}", resp.text());
    assert_eq!(recorder.counter("mini_chat_secondary_cleanup_skipped"), 0);
}
