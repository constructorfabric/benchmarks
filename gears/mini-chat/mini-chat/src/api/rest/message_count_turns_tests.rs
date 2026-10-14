//! `message_count` and the message list after each kind of streamed turn, over the real router,
//! SQLite, the real quota service, the real provider task and a mock OAGW: the count changes by
//! exactly what the turn durably wrote and always equals the length of the message list.
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-message-order` (`inst-mo-count`, `inst-mo-return`);
//! definitions of done `cpt-cf-mini-chat-dod-message-streaming-message-order` and
//! `cpt-cf-mini-chat-dod-message-streaming-turn-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use uuid::Uuid;

use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, chat_uri};
use crate::test_support::provider::{
    Gate, MockProvider, Reply, Step, completed, delta, happy_steps,
};
use crate::test_support::send::{SendRig, messages_uri, stream_rig};

async fn count(rig: &SendRig, chat: &ChatRecord) -> i64 {
    rig.get(&chat_uri(chat.id), &a1()).await.json()["message_count"]
        .as_i64()
        .unwrap()
}

/// The roles of the listed messages, oldest first.
async fn listed_roles(rig: &SendRig, chat: &ChatRecord) -> Vec<String> {
    let list = rig.get(&messages_uri(chat.id), &a1()).await.json();
    list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["role"].as_str().unwrap().to_owned())
        .collect()
}

/// The count changed by `expected` since `before` and equals the list length.
async fn assert_adds(
    rig: &SendRig,
    chat: &ChatRecord,
    before: i64,
    expected: i64,
    what: &str,
) -> i64 {
    let after = count(rig, chat).await;
    assert_eq!(after - before, expected, "{what}: {before} -> {after}");
    let roles = listed_roles(rig, chat).await;
    assert_eq!(
        i64::try_from(roles.len()).unwrap(),
        after,
        "{what}: the count follows the list {roles:?}"
    );
    after
}

#[tokio::test]
async fn each_turn_outcome_adds_exactly_the_messages_it_wrote() {
    let upstream = MockProvider::new();
    // scripts in the order the turns below use the provider
    upstream.queue(Reply::Sse(happy_steps(&["one"], (10, 4)))); // completed
    upstream.queue(Reply::http(500, &json!({ "error": { "message": "boom" } }))); // failed
    upstream.queue(Reply::Sse(vec![Step::Wait(Gate::new())])); // cancelled, no text
    upstream.queue(Reply::Sse(vec![delta("part"), Step::Wait(Gate::new())])); // cancelled, partial
    upstream.queue(Reply::Sse(vec![delta("lost"), Step::Break])); // failed after deltas
    upstream.queue(Reply::Sse(vec![completed("", (3, 0))])); // completed without deltas
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    let mut total = count(&rig, &chat).await;
    assert_eq!(total, 0);

    // a completed turn adds the user and the assistant message
    let completed_id = Uuid::new_v4();
    rig.stream(
        chat.id,
        &json!({ "content": "q1", "request_id": completed_id.to_string() }),
    )
    .await;
    rig.wait_idle(chat.id).await;
    total = assert_adds(&rig, &chat, total, 2, "a completed turn").await;

    // a failed turn adds the user message only
    let failed_id = Uuid::new_v4();
    rig.stream(
        chat.id,
        &json!({ "content": "q2", "request_id": failed_id.to_string() }),
    )
    .await;
    total = assert_adds(&rig, &chat, total, 1, "a failed turn").await;

    // a cancelled turn without text adds the user message only
    let mut open = rig.open_stream(chat.id, &json!({ "content": "q3" })).await;
    open.until("stream_started").await;
    upstream.wait_for_calls(3).await;
    open.disconnect();
    upstream.wait_for_aborted(1).await;
    rig.wait_idle(chat.id).await;
    total = assert_adds(&rig, &chat, total, 1, "a cancelled turn without text").await;

    // a cancelled turn with a persisted partial message adds two
    let mut open = rig.open_stream(chat.id, &json!({ "content": "q4" })).await;
    open.until("delta").await;
    open.disconnect();
    upstream.wait_for_aborted(2).await;
    rig.wait_idle(chat.id).await;
    total = assert_adds(&rig, &chat, total, 2, "a cancelled turn with partial text").await;

    // a provider failure after deltas persists no assistant message: one
    rig.stream(chat.id, &json!({ "content": "q5" })).await;
    rig.wait_idle(chat.id).await;
    total = assert_adds(&rig, &chat, total, 1, "a failure after deltas").await;

    // a completion without deltas persists an empty assistant message: two
    rig.stream(chat.id, &json!({ "content": "q6" })).await;
    rig.wait_idle(chat.id).await;
    total = assert_adds(&rig, &chat, total, 2, "a completion without deltas").await;

    // a replay adds none and calls no provider
    let calls = upstream.call_count();
    let replay = rig
        .stream(
            chat.id,
            &json!({ "content": "q1", "request_id": completed_id.to_string() }),
        )
        .await;
    assert_eq!(replay.status.as_u16(), 200, "{}", replay.text);
    assert_eq!(upstream.call_count(), calls);
    total = assert_adds(&rig, &chat, total, 0, "a replay").await;

    // reusing the request id of the failed turn is a conflict and adds none
    let conflict = rig
        .stream(
            chat.id,
            &json!({ "content": "again", "request_id": failed_id.to_string() }),
        )
        .await;
    assert_eq!(conflict.status.as_u16(), 409, "{}", conflict.text);
    total = assert_adds(&rig, &chat, total, 0, "a request id conflict").await;

    // a send rejected by the quota adds none
    rig.gateway.set_limits((1, 1), (1, 1));
    let rejected = rig.stream(chat.id, &json!({ "content": "too much" })).await;
    assert_eq!(rejected.status.as_u16(), 429, "{}", rejected.text);
    total = assert_adds(&rig, &chat, total, 0, "a quota rejection").await;
    rig.gateway
        .set_limits((60_000_000, 600_000_000), (22_000_000, 300_000_000));

    // a send rejected by validation adds none
    let invalid = rig.stream(chat.id, &json!({ "content": "" })).await;
    assert_eq!(invalid.status.as_u16(), 400, "{}", invalid.text);
    assert_adds(&rig, &chat, total, 0, "a validation rejection").await;
}

#[tokio::test]
async fn a_completed_turn_adds_a_user_and_an_assistant_message_in_that_order() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["a"], (1, 1))));
    let rig = stream_rig(upstream).await;
    let chat = rig.std_chat().await;
    for turn in 0..3 {
        rig.stream(chat.id, &json!({ "content": format!("q{turn}") }))
            .await;
        rig.wait_idle(chat.id).await;
    }
    assert_eq!(count(&rig, &chat).await, 6);
    assert_eq!(
        listed_roles(&rig, &chat).await,
        [
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
}
