//! What a client sees after a stream is lost, end to end over the real router, SQLite, the real
//! quota service, the real provider task and a mock OAGW: a closed or dropped connection leaves
//! the outcome to the turn status API, and each `state` of that API (`done`, `running`, `error`,
//! `cancelled`) carries exactly what the UI needs to act on it (recover the answer, wait without
//! resending, or resend with a new `request_id`).
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: flow
//! `cpt-cf-mini-chat-flow-message-streaming-disconnect` (`inst-dc-client-closes`,
//! `inst-dc-return`); flow `cpt-cf-mini-chat-flow-message-streaming-turn-status`
//! (`inst-status-ui-action`); definitions of done
//! `cpt-cf-mini-chat-dod-message-streaming-cancellation` and
//! `cpt-cf-mini-chat-dod-message-streaming-turn-status`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::chat_list_tests::enc;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, assert_problem, keys};
use crate::test_support::provider::{
    Gate, MockProvider, Reply, Step, completed, delta, happy_steps,
};
use crate::test_support::send::{
    ReserveGate, SendRig, SendRigOptions, messages_uri, names, parse_sse, send_rig_with,
    stream_rig, turn_uri,
};

/// The states of the status API, the only values the UI acts on.
const UI_STATES: [&str; 4] = ["running", "done", "error", "cancelled"];

async fn streamed(reply: Reply) -> (SendRig, ChatRecord, Arc<MockProvider>) {
    let upstream = MockProvider::answering(reply);
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    (rig, chat, upstream)
}

async fn status(rig: &SendRig, chat: &ChatRecord, request_id: Uuid) -> Value {
    let reply = rig.get(&turn_uri(chat.id, request_id), &a1()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = reply.json();
    assert_eq!(body["request_id"], request_id.to_string(), "{body}");
    assert!(
        UI_STATES.contains(&body["state"].as_str().unwrap()),
        "a state the UI does not know: {body}"
    );
    body
}

fn question(text: &str, request_id: Uuid) -> Value {
    json!({ "content": text, "request_id": request_id.to_string() })
}

/// The UI resends with a new `request_id`: the chat accepts it and the stream ends with `done`.
async fn assert_resend_with_a_new_request_id_streams_done(
    rig: &SendRig,
    upstream: &MockProvider,
    chat: &ChatRecord,
) {
    upstream.set_default(Reply::Sse(happy_steps(&["again"], (1, 1))));
    let reply = rig
        .stream(chat.id, &question("again", Uuid::new_v4()))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let frames = parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "delta", "done"],
        "{}",
        reply.text
    );
    assert_eq!(frames[0].data["is_new_turn"], true);
}

// ── inst-dc-client-closes / inst-dc-return ───────────────────────────────────

#[tokio::test]
async fn a_client_that_closes_the_stream_resolves_the_outcome_through_the_status_api() {
    let gate = Gate::new();
    let steps = vec![
        delta("partial"),
        Step::Wait(gate),
        completed("partial", (1, 1)),
    ];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    let request_id = Uuid::new_v4();
    let mut open = rig.open_stream(chat.id, &question("hi", request_id)).await;
    let started = open.next_frame().await.unwrap();
    assert_eq!(started.event, "stream_started");
    assert_eq!(started.data["request_id"], request_id.to_string());
    assert_eq!(open.next_frame().await.unwrap().event, "delta");

    // the user closes the connection (or presses stop)
    open.disconnect();
    upstream.wait_for_aborted(1).await;
    rig.wait_terminal(chat.id).await;

    // the delivery is indeterminate for the client; the status API resolves it
    let body = status(&rig, &chat, request_id).await;
    assert_eq!(body["state"], "cancelled");
    assert_eq!(
        body["assistant_message_id"], started.data["message_id"],
        "the partial answer is the pre-allocated message"
    );
    assert!(body.get("error_code").is_none(), "{body}");
}

#[tokio::test]
async fn a_dropped_connection_before_any_text_resolves_as_cancelled_without_a_message() {
    let gate = Gate::new();
    let steps = vec![Step::Wait(gate), completed("never", (1, 1))];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    let request_id = Uuid::new_v4();
    // the handler answered and the provider request started; the client never read a byte
    let open = rig.open_stream(chat.id, &question("hi", request_id)).await;
    upstream.wait_for_calls(1).await;
    drop(open);
    upstream.wait_for_aborted(1).await;
    rig.wait_terminal(chat.id).await;

    let body = status(&rig, &chat, request_id).await;
    assert_eq!(body["state"], "cancelled");
    assert!(body.get("assistant_message_id").is_none(), "{body}");
    assert_resend_with_a_new_request_id_streams_done(&rig, &upstream, &chat).await;
}

#[tokio::test]
async fn a_connection_dropped_after_the_reserve_resolves_as_cancelled_and_allows_a_resend() {
    let gate = Arc::new(ReserveGate::default());
    let upstream = MockProvider::new();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_gate(Arc::clone(&gate)),
    )
    .await;
    let chat = rig.std_chat().await;
    let request_id = Uuid::new_v4();
    let body = question("hi", request_id);
    let request = rig.stream(chat.id, &body);
    tokio::select! {
        _ = request => panic!("the setup must be parked after the reserve commit"),
        () = gate.committed.notified() => {}
    }
    gate.release.notify_one();
    rig.wait_terminal(chat.id).await;
    assert_eq!(upstream.call_count(), 0, "a provider request was sent");

    let status_body = status(&rig, &chat, request_id).await;
    assert_eq!(status_body["state"], "cancelled");
    assert!(status_body.get("assistant_message_id").is_none());
    // the rig parks every send after its reserve commit: let the resend through
    gate.release.notify_one();
    assert_resend_with_a_new_request_id_streams_done(&rig, &upstream, &chat).await;
}

// ── inst-status-ui-action ────────────────────────────────────────────────────

#[tokio::test]
async fn done_lets_the_ui_recover_the_answer_by_replay_or_by_the_message_list() {
    let (rig, chat, upstream) =
        streamed(Reply::Sse(happy_steps(&["The ", "answer"], (5, 3)))).await;
    let request_id = Uuid::new_v4();
    let reply = rig.stream(chat.id, &question("q", request_id)).await;
    assert_eq!(
        names(&parse_sse(&reply.text)).last(),
        Some(&"done"),
        "{}",
        reply.text
    );

    let body = status(&rig, &chat, request_id).await;
    assert_eq!(body["state"], "done");
    let assistant_id = body["assistant_message_id"].as_str().unwrap().to_owned();
    assert!(body.get("error_code").is_none(), "{body}");

    // recovery by the message list
    let uri = format!(
        "{}?$filter={}",
        messages_uri(chat.id),
        enc(&format!("id eq {assistant_id}"))
    );
    let list = rig.get(&uri, &a1()).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    let items = list.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{}", list.text);
    assert_eq!(items[0]["content"], "The answer");
    assert_eq!(items[0]["role"], "assistant");

    // recovery by replay: the same request id, no new provider call
    let again = rig.stream(chat.id, &question("q", request_id)).await;
    let frames = parse_sse(&again.text);
    assert_eq!(names(&frames), vec!["stream_started", "delta", "done"]);
    assert_eq!(frames[0].data["is_new_turn"], false);
    assert_eq!(frames[0].data["message_id"], assistant_id);
    assert_eq!(frames[1].data["content"], "The answer");
    assert_eq!(upstream.call_count(), 1);
}

#[tokio::test]
async fn running_tells_the_ui_to_wait_and_a_resend_of_its_request_id_is_a_conflict() {
    let gate = Gate::new();
    let steps = vec![
        delta("so far"),
        Step::Wait(gate.clone()),
        completed("so far", (1, 1)),
    ];
    let (rig, chat, _upstream) = streamed(Reply::Sse(steps)).await;
    let request_id = Uuid::new_v4();
    let mut open = rig.open_stream(chat.id, &question("q", request_id)).await;
    open.until("delta").await;

    let body = status(&rig, &chat, request_id).await;
    assert_eq!(body["state"], "running");
    assert_eq!(
        keys(&body),
        BTreeSet::from(["request_id", "state", "updated_at"]),
        "a running turn has no answer, error or message yet: {body}"
    );
    // a UI that resent would be refused: the turn is still in progress
    let resend = rig.stream(chat.id, &question("q", request_id)).await;
    let problem = assert_problem(&resend, 409, "aborted");
    assert_eq!(problem["context"]["reason"], "request_id_conflict");
    let other = rig.stream(chat.id, &json!({ "content": "other" })).await;
    let problem = assert_problem(&other, 409, "aborted");
    assert_eq!(problem["context"]["reason"], "turn_already_running");

    // waiting resolves it: the status turns `done` once the stream finished
    gate.open();
    open.rest().await;
    rig.wait_terminal(chat.id).await;
    assert_eq!(status(&rig, &chat, request_id).await["state"], "done");
}

#[tokio::test]
async fn error_carries_its_code_and_allows_a_resend_with_a_new_request_id() {
    let (rig, chat, upstream) =
        streamed(Reply::http(500, &json!({ "error": { "message": "boom" } }))).await;
    let request_id = Uuid::new_v4();
    let reply = rig.stream(chat.id, &question("q", request_id)).await;
    assert_eq!(
        names(&parse_sse(&reply.text)).last(),
        Some(&"error"),
        "{}",
        reply.text
    );
    rig.wait_terminal(chat.id).await;

    let body = status(&rig, &chat, request_id).await;
    assert_eq!(body["state"], "error");
    assert_eq!(body["error_code"], "provider_error");
    assert!(body.get("assistant_message_id").is_none(), "{body}");

    // the old request id stays conflicting, a new one is accepted
    let same = rig.stream(chat.id, &question("q", request_id)).await;
    let problem = assert_problem(&same, 409, "aborted");
    assert_eq!(problem["context"]["reason"], "request_id_conflict");
    assert_resend_with_a_new_request_id_streams_done(&rig, &upstream, &chat).await;
}

#[tokio::test]
async fn cancelled_allows_a_resend_with_a_new_request_id() {
    let gate = Gate::new();
    let steps = vec![delta("half"), Step::Wait(gate), completed("half", (1, 1))];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    let request_id = Uuid::new_v4();
    let mut open = rig.open_stream(chat.id, &question("q", request_id)).await;
    open.until("delta").await;
    open.disconnect();
    upstream.wait_for_aborted(1).await;
    rig.wait_terminal(chat.id).await;

    let body = status(&rig, &chat, request_id).await;
    assert_eq!(body["state"], "cancelled");
    let same = rig.stream(chat.id, &question("q", request_id)).await;
    let problem = assert_problem(&same, 409, "aborted");
    assert_eq!(problem["context"]["reason"], "request_id_conflict");
    assert_resend_with_a_new_request_id_streams_done(&rig, &upstream, &chat).await;
}

#[tokio::test]
async fn the_status_api_projects_exactly_the_four_ui_states_across_one_chat() {
    // one chat, four turns: done, error, cancelled and (while it runs) running
    let upstream = MockProvider::new();
    let hold = Gate::new();
    upstream.queue(Reply::Sse(happy_steps(&["ok"], (1, 1))));
    upstream.queue(Reply::http(500, &json!({ "error": { "message": "boom" } })));
    upstream.queue(Reply::Sse(vec![delta("p"), Step::Wait(Gate::new())]));
    upstream.queue(Reply::Sse(vec![delta("r"), Step::Wait(hold.clone())]));
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();

    rig.stream(chat.id, &question("done", ids[0])).await;
    rig.stream(chat.id, &question("error", ids[1])).await;
    let mut open = rig
        .open_stream(chat.id, &question("cancelled", ids[2]))
        .await;
    open.until("delta").await;
    open.disconnect();
    rig.wait_idle(chat.id).await;
    let mut running = rig.open_stream(chat.id, &question("running", ids[3])).await;
    running.until("delta").await;

    let mut seen = BTreeSet::new();
    for (id, expected) in ids.iter().zip(UI_STATES_IN_ORDER) {
        let body = status(&rig, &chat, *id).await;
        assert_eq!(body["state"], expected, "{body}");
        seen.insert(expected);
        // never billing or internal data, whatever the state
        for hidden in [
            "chat_id",
            "turn_id",
            "usage",
            "reserved_credits",
            "provider_response_id",
        ] {
            assert!(body.get(hidden).is_none(), "{hidden} in {body}");
        }
        assert_eq!(
            body.get("error_code").is_some(),
            expected == "error",
            "error_code only for error: {body}"
        );
    }
    assert_eq!(seen, BTreeSet::from(UI_STATES));
    hold.open();
    running.rest().await;
}

/// The turns of the projection test in the order they are sent.
const UI_STATES_IN_ORDER: [&str; 4] = ["done", "error", "cancelled", "running"];

/// FEATURE `turn-status` and DESIGN 3.6 document the recovery call with a quoted literal,
/// `$filter=id eq '{assistant_message_id}'`; the `OData` filter of the message list types `id` as
/// a UUID and today answers 400 `INVALID_FILTER` to the quoted form (the unquoted form works, see
/// `done_lets_the_ui_recover_the_answer_by_replay_or_by_the_message_list`). The author stage either
/// accepts the quoted literal for `id` or amends the FEATURE and DESIGN text.
#[tokio::test]
async fn the_documented_quoted_id_filter_of_the_recovery_call_is_accepted() {
    let (rig, chat, _upstream) = streamed(Reply::Sse(happy_steps(&["The answer"], (5, 3)))).await;
    let request_id = Uuid::new_v4();
    rig.stream(chat.id, &question("q", request_id)).await;
    let body = status(&rig, &chat, request_id).await;
    let assistant_id = body["assistant_message_id"].as_str().unwrap().to_owned();
    let uri = format!(
        "{}?$filter={}",
        messages_uri(chat.id),
        enc(&format!("id eq '{assistant_id}'"))
    );
    let list = rig.get(&uri, &a1()).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    let items = list.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{}", list.text);
    assert_eq!(items[0]["content"], "The answer");
}
