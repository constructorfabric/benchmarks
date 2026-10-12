#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

//! Helpers for tests that need the S2-only harness API: `request_streaming` /
//! `StreamingResponse::next_event`, `wait_for_turn_state`, `drain_outbox`, `published_usage` and
//! `delivered_audit` (HARNESS.md). They are kept apart from `common` so the S1 test binaries
//! compile without them.
//!
//! A test file that needs them declares `mod common; mod live;`.

use http::Method;
use mini_chat::test_support::{SseEvent, StreamingResponse, TestEnv, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    chat_request_body, enqueue_chat, hang_script, stream_path, text_script, turn_row,
};

/// Opens `POST messages:stream` and returns as soon as the response headers arrive.
pub async fn open(env: &TestEnv, user: &TestUser, chat_id: &str, body: Value) -> StreamingResponse {
    env.request_streaming(user, Method::POST, &stream_path(chat_id), Some(body))
        .await
}

/// The next event that is not a `ping` keepalive; `None` when the stream ended (or stalled).
pub async fn next_event(stream: &mut StreamingResponse) -> Option<SseEvent> {
    loop {
        let event = stream.next_event().await?;
        if event.event != "ping" {
            return Some(event);
        }
    }
}

/// Reads events up to and including the first one called `name`; panics when the stream ends first.
pub async fn read_until(stream: &mut StreamingResponse, name: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    loop {
        let event = next_event(stream)
            .await
            .unwrap_or_else(|| panic!("the stream ended before `{name}`; got {events:?}"));
        let done = event.event == name;
        events.push(event);
        if done {
            return events;
        }
    }
}

/// Reads until the stream ends.
pub async fn read_to_end(stream: &mut StreamingResponse) -> Vec<SseEvent> {
    let mut events = Vec::new();
    while let Some(event) = next_event(stream).await {
        events.push(event);
    }
    events
}

/// Starts a turn whose provider stream stays open after `chunks`, and reads until the client has
/// seen `stream_started` and every delta. The turn is `running` and the provider has the request.
pub async fn start_hung_turn(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    request_id: Uuid,
    content: &str,
    chunks: &[&str],
) -> StreamingResponse {
    enqueue_chat(env, hang_script(chunks));
    let mut stream = open(
        env,
        user,
        chat_id,
        json!({ "content": content, "request_id": request_id.to_string() }),
    )
    .await;
    assert_eq!(stream.status, http::StatusCode::OK);
    let started = next_event(&mut stream).await.expect("stream_started");
    assert_eq!(started.event, "stream_started", "{started:?}");
    for chunk in chunks {
        let delta = next_event(&mut stream).await.expect("a delta event");
        assert_eq!(delta.event, "delta", "{delta:?}");
        assert_eq!(delta.data["content"], json!(chunk), "{delta:?}");
    }
    stream
}

/// Disconnects the client and waits until the turn is `cancelled`.
pub async fn disconnect_and_wait_cancelled(
    env: &TestEnv,
    chat_id: &str,
    request_id: Uuid,
    stream: StreamingResponse,
) {
    stream.disconnect();
    env.wait_for_turn_state(Uuid::parse_str(chat_id).unwrap(), request_id, "cancelled")
        .await;
}

/// Runs a complete turn with a fresh stream, reading the whole body through `request_streaming`.
pub async fn complete_via_streaming(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    content: &str,
    chunks: &[&str],
) -> (Uuid, Vec<SseEvent>) {
    enqueue_chat(env, text_script(chunks, 10, 5));
    let mut stream = open(env, user, chat_id, json!({ "content": content })).await;
    let events = read_to_end(&mut stream).await;
    let request_id = Uuid::parse_str(events[0].data["request_id"].as_str().unwrap()).unwrap();
    (request_id, events)
}

/// The state of a turn straight from the database.
pub async fn turn_state(env: &TestEnv, chat_id: &str, request_id: Uuid) -> String {
    turn_row(env, chat_id, request_id)
        .await
        .expect("the turn row exists")
        .state
}
