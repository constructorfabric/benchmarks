//! Knowledge search (DESIGN §4 "Knowledge Search"): the `search_knowledge`
//! function tool, the Azure vector-store retriever and the agentic loop.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::{Method, StatusCode};
use mini_chat::config::DEFAULT_KNOWLEDGE_SEARCH_GUARD;
use mini_chat::infra::db::entities::{chat_turn, message};
use mini_chat::infra::outbox::QueueKind;
use mini_chat::testing::fake_provider::{FAKE_RESPONSE_ID, delta_event};
use mini_chat::testing::providers::azure_entry;
use mini_chat::testing::{ScriptedStream, SseCapture, TestApp, TestUser};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const U: TestUser = TestUser::A1;
const SEARCH_PATH: &str = "/kb.openai.azure.com/openai/vector_stores/vs_kb/search";

async fn app_with(max_calls: u32) -> TestApp {
    TestApp::builder()
        .config(move |c| {
            c.providers
                .insert("kb".to_owned(), azure_entry("kb.openai.azure.com"));
            let ks = &mut c.knowledge_search;
            ks.enabled = true;
            ks.vector_store_id = Some("vs_kb".to_owned());
            ks.provider_id = Some("kb".to_owned());
            ks.max_chunk_chars = 8;
            ks.max_calls_per_message = max_calls;
        })
        .build()
        .await
}

async fn create_chat(app: &TestApp) -> Uuid {
    let r = app.call(U, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

async fn send(app: &TestApp, chat: Uuid, content: &str) -> SseCapture {
    app.stream(
        U,
        &format!("{CHATS}/{chat}/messages:stream"),
        json!({"content": content}),
    )
    .await
}

async fn turn(app: &TestApp, chat: Uuid) -> chat_turn::Model {
    let conn = app.db.conn().unwrap();
    chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("turn")
}

/// A Responses stream that ends with one function call (no text).
fn function_call(call_id: &str, name: &str, arguments: &Value) -> ScriptedStream {
    function_call_after(None, call_id, name, arguments)
}

/// A Responses stream with an optional text delta, then one function call.
fn function_call_after(
    text: Option<&str>,
    call_id: &str,
    name: &str,
    arguments: &Value,
) -> ScriptedStream {
    let item = json!({
        "type": "function_call", "id": format!("fc_{call_id}"), "call_id": call_id,
        "name": name, "arguments": arguments.to_string(), "status": "completed",
    });
    let mut events = vec![(
        "response.created".to_owned(),
        json!({"type": "response.created", "response": {"id": FAKE_RESPONSE_ID, "status": "in_progress", "output": []}}),
    )];
    if let Some(text) = text {
        events.push(delta_event(text));
    }
    events.extend([
        (
            "response.output_item.done".to_owned(),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        ),
        (
            "response.completed".to_owned(),
            json!({"type": "response.completed", "response": {
                "id": FAKE_RESPONSE_ID, "status": "completed", "output": [item],
                "usage": {"input_tokens": 11, "output_tokens": 3},
            }}),
        ),
    ]);
    ScriptedStream::events(events)
}

fn search_requests(app: &TestApp) -> Vec<Value> {
    app.provider
        .requests()
        .into_iter()
        .filter(|r| r.path == SEARCH_PATH)
        .map(|r| r.json.unwrap())
        .collect()
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    t["name"]
                        .as_str()
                        .or_else(|| t["type"].as_str())
                        .unwrap()
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `function_call_output` items of a Responses request body.
fn outputs(req: &Value) -> Vec<String> {
    req["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .map(|i| i["output"].as_str().unwrap().to_owned())
        .collect()
}

fn error_code(sse: &SseCapture) -> String {
    let (name, data) = sse.last().unwrap();
    assert_eq!(name, "error", "{:?}", sse.names());
    data["code"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn search_knowledge_tool_offered_only_without_file_search() {
    let app = app_with(3).await;
    let chat = create_chat(&app).await;

    let sse = send(&app, chat, "hello").await;
    assert_eq!(sse.last().unwrap().0, "done", "{:?}", sse.names());
    let first = &app.provider.chat_requests()[0];
    assert_eq!(tool_names(first), ["search_knowledge"]);
    assert_eq!(first["tools"][0]["type"], "function");
    assert!(
        first["instructions"]
            .as_str()
            .unwrap()
            .contains(DEFAULT_KNOWLEDGE_SEARCH_GUARD)
    );
    // search_knowledge is not a built-in tool.
    assert_eq!(first["metadata"]["feature"], "none");
    assert!(first.get("max_tool_calls").is_none());

    // A ready document brings file_search, which wins.
    let r = app
        .upload(U, chat, "a.pdf", "application/pdf", b"%PDF-1.4\n% doc\n")
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let sse = send(&app, chat, "again").await;
    assert_eq!(sse.last().unwrap().0, "done", "{:?}", sse.names());
    let second = &app.provider.chat_requests()[1];
    assert_eq!(tool_names(second), ["file_search"]);
    assert!(
        !second["instructions"]
            .as_str()
            .unwrap()
            .contains(DEFAULT_KNOWLEDGE_SEARCH_GUARD)
    );
}

#[tokio::test]
async fn knowledge_search_disabled_offers_no_tool() {
    let app = TestApp::builder().build().await;
    let chat = create_chat(&app).await;
    let sse = send(&app, chat, "hello").await;
    assert_eq!(sse.last().unwrap().0, "done", "{:?}", sse.names());
    assert!(app.provider.chat_requests()[0].get("tools").is_none());
}

#[tokio::test]
async fn agentic_loop_runs_retrieval_and_continues() {
    let app = app_with(3).await;
    let chat = create_chat(&app).await;
    app.provider.push_stream(function_call(
        "call_1",
        "search_knowledge",
        &json!({"query": "vacation policy", "top_k": 10}),
    ));
    app.provider
        .push_stream(ScriptedStream::text(&["Answer"], 50, 7));
    app.provider
        .push_search_results(vec!["0123456789abc", "short"]);

    let sse = send(&app, chat, "how many vacation days?").await;
    // The Responses adapter emits no tool event for the function tool.
    assert_eq!(sse.names(), ["stream_started", "delta", "done"]);
    assert_eq!(sse.first("delta").unwrap()["content"], "Answer");
    // Only the final iteration's usage is reported and settled.
    let done = sse.first("done").unwrap();
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 50, "output_tokens": 7})
    );

    // top_k is capped at knowledge_search.top_k (5).
    assert_eq!(
        search_requests(&app),
        [json!({"query": "vacation policy", "max_num_results": 5})]
    );
    let reqs = app.provider.chat_requests();
    assert_eq!(reqs.len(), 2);
    assert!(outputs(&reqs[0]).is_empty());
    let input = reqs[1]["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{input:?}");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_1");
    assert_eq!(input[1]["name"], "search_knowledge");
    // Chunks trimmed to max_chunk_chars (8).
    assert_eq!(
        outputs(&reqs[1]),
        [json!(["01234567", "short"]).to_string()]
    );
    // The loop keeps offering the tool.
    assert_eq!(tool_names(&reqs[1]), ["search_knowledge"]);

    let t = turn(&app, chat).await;
    assert_eq!(t.state, "completed");
    assert_eq!(t.file_search_completed_count, 1);
    assert_eq!(t.provider_response_id.as_deref(), Some(FAKE_RESPONSE_ID));
    let usage = app.outbox_messages(QueueKind::Usage).await;
    assert_eq!(usage[0]["file_search_calls"], 1, "{usage:?}");
    assert_eq!(usage[0]["usage"]["input_tokens"], 50, "{usage:?}");
}

#[tokio::test]
async fn failed_retrieval_is_reported_to_the_model() {
    let app = app_with(3).await;
    let chat = create_chat(&app).await;
    app.provider.push_stream(function_call(
        "call_1",
        "search_knowledge",
        &json!({"query": "q"}),
    ));
    app.provider
        .push_stream(ScriptedStream::text(&["Sorry"], 5, 2));
    app.provider.fail_next(SEARCH_PATH, 500);

    let sse = send(&app, chat, "q?").await;
    assert_eq!(sse.last().unwrap().0, "done", "{:?}", sse.names());
    let reqs = app.provider.chat_requests();
    let out = outputs(&reqs[1]);
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("failed"), "{out:?}");
    let t = turn(&app, chat).await;
    // Counted as a call (usage event) but not as a completed search.
    assert_eq!(t.file_search_completed_count, 0);
    let usage = app.outbox_messages(QueueKind::Usage).await;
    assert_eq!(usage[0]["file_search_calls"], 1, "{usage:?}");
}

#[tokio::test]
async fn iterations_exceeded_error() {
    // max_calls_per_message = 1: one retrieval, at most 3 provider iterations.
    let app = app_with(1).await;
    let chat = create_chat(&app).await;
    for n in 1..=3 {
        app.provider.push_stream(function_call(
            &format!("call_{n}"),
            "search_knowledge",
            &json!({"query": format!("q{n}")}),
        ));
    }
    app.provider.push_search_results(vec!["kb text"]);

    let sse = send(&app, chat, "loop forever").await;
    assert_eq!(error_code(&sse), "agentic_iterations_exceeded");

    let reqs = app.provider.chat_requests();
    assert_eq!(reqs.len(), 3);
    // One retrieval; the second call is answered with the limit notice.
    assert_eq!(search_requests(&app).len(), 1);
    let out = outputs(&reqs[2]);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], json!(["kb text"]).to_string());
    assert!(out[1].contains("limit"), "{out:?}");

    let t = turn(&app, chat).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("agentic_iterations_exceeded"));
}

#[tokio::test]
async fn unexpected_tool_use_error() {
    // Knowledge search on, but the model calls another function.
    let app = app_with(3).await;
    let chat = create_chat(&app).await;
    app.provider
        .push_stream(function_call("call_1", "delete_everything", &json!({})));
    let sse = send(&app, chat, "hi").await;
    assert_eq!(error_code(&sse), "unexpected_tool_use");
    assert_eq!(app.provider.chat_requests().len(), 1);
    assert!(search_requests(&app).is_empty());
    let t = turn(&app, chat).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("unexpected_tool_use"));

    // Knowledge search off: any tool use is unexpected.
    let app = TestApp::builder().build().await;
    let chat = create_chat(&app).await;
    app.provider.push_stream(function_call(
        "call_1",
        "search_knowledge",
        &json!({"query": "q"}),
    ));
    let sse = send(&app, chat, "hi").await;
    assert_eq!(error_code(&sse), "unexpected_tool_use");
    assert_eq!(app.provider.chat_requests().len(), 1);
}

#[tokio::test]
async fn iteration_texts_are_joined_by_a_blank_line() {
    let app = app_with(3).await;
    let chat = create_chat(&app).await;
    app.provider.push_stream(function_call_after(
        Some("Let me check."),
        "call_1",
        "search_knowledge",
        &json!({"query": "q"}),
    ));
    // An iteration without text adds no separator.
    app.provider.push_stream(function_call(
        "call_2",
        "search_knowledge",
        &json!({"query": "q2"}),
    ));
    app.provider
        .push_stream(ScriptedStream::text(&["Answer"], 5, 2));

    let sse = send(&app, chat, "q?").await;
    assert_eq!(sse.last().unwrap().0, "done", "{:?}", sse.names());
    let streamed: String = sse
        .events
        .iter()
        .filter(|(n, _)| n == "delta")
        .map(|(_, d)| d["content"].as_str().unwrap())
        .collect();
    assert_eq!(streamed, "Let me check.\n\nAnswer");

    let conn = app.db.conn().unwrap();
    let stored = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat))
        .filter(message::Column::Role.eq("assistant"))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("assistant message");
    assert_eq!(stored.content, "Let me check.\n\nAnswer");
}
