//! Concurrent mutations through the real router, SQLite, the real quota service and a mock OAGW:
//! two concurrent retries of the latest turn yield exactly one stream and one 409 `aborted`
//! (`NOT_LATEST_TURN` or `GENERATION_IN_PROGRESS` by the interleaving), retry against delete, edit
//! against retry and delete against delete each leave exactly one committed mutation, one audit
//! event and no row of the loser, and a lost race is never a 500.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithm
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution`
//! (`inst-cr-no-early-lock`, `inst-cr-first-commit-wins`, `inst-cr-match`, `inst-cr-case-deleted`,
//! `inst-cr-case-newer`, `inst-cr-case-running-index`, `inst-cr-loser-clean`,
//! `inst-cr-no-server-retry`, `inst-cr-return`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-concurrency`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::chats::Reply;
use crate::test_support::mutations::{
    mutation_events, seed, started_ids, streaming_chat, turn, turn_events,
};
use crate::test_support::send::{SendRig, parse_sse, turn_rows};

const LOSER_REASONS: [&str; 2] = ["NOT_LATEST_TURN", "GENERATION_IN_PROGRESS"];

/// Whether the reply is the winning stream of a retry or edit.
fn is_stream(reply: &Reply) -> bool {
    reply.status == StatusCode::OK && reply.content_type.starts_with("text/event-stream")
}

/// A 409 `aborted` with one of the two loser reasons.
fn assert_loser(reply: &Reply, what: &str) {
    assert_eq!(
        reply.status,
        StatusCode::CONFLICT,
        "{what}: {} {}",
        reply.status,
        reply.text
    );
    let body: Value = reply.json();
    assert!(
        body["type"].as_str().unwrap().contains("aborted"),
        "{what}: {body}"
    );
    let reason = body["context"]["reason"].as_str().unwrap();
    assert!(LOSER_REASONS.contains(&reason), "{what}: {body}");
}

async fn settle(rig: &SendRig, chat_id: Uuid, replies: &[&Reply]) {
    for reply in replies.iter().filter(|r| is_stream(r)) {
        let (request_id, _) = started_ids(&parse_sse(&reply.text));
        rig.wait_turn_terminal(chat_id, request_id).await;
    }
}

#[tokio::test]
async fn two_concurrent_retries_yield_one_stream_and_one_409() {
    // inst-cr-first-commit-wins, inst-cr-return
    for round in 0..6 {
        let (rig, chat, upstream) = streaming_chat().await;
        seed(&rig.quota.db, &chat, turn("first").at(10)).await;
        let target = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
        let (a, b) = tokio::join!(
            rig.retry(chat.id, target.request_id),
            rig.retry(chat.id, target.request_id),
        );
        let replies = [&a, &b];
        assert_eq!(
            replies.iter().filter(|r| is_stream(r)).count(),
            1,
            "round {round}: {} / {}",
            a.text,
            b.text
        );
        for reply in replies.iter().filter(|r| !is_stream(r)) {
            assert_loser(reply, &format!("round {round}"));
        }
        settle(&rig, chat.id, &replies).await;
        assert_eq!(
            upstream.call_count(),
            1,
            "round {round}: one provider request"
        );
        assert_eq!(
            mutation_events(&rig).await.len(),
            1,
            "round {round}: one mutation event"
        );
        assert_eq!(turn_events(&rig).await.len(), 1, "round {round}");
        assert_eq!(
            rig.events().await.len(),
            1,
            "round {round}: one usage event"
        );
        let live = turn_rows(&rig.quota.db, chat.id)
            .await
            .into_iter()
            .filter(|row| row.deleted_at.is_none())
            .count();
        assert_eq!(live, 2, "round {round}: T1 and the one replacement");
    }
}

#[tokio::test]
async fn a_concurrent_edit_and_retry_yield_one_stream_and_one_409() {
    for round in 0..6 {
        let (rig, chat, _upstream) = streaming_chat().await;
        let target = seed(&rig.quota.db, &chat, turn("question")).await;
        let body = json!({ "content": "edited" });
        let (edit, retry) = tokio::join!(
            rig.edit(chat.id, target.request_id, &body),
            rig.retry(chat.id, target.request_id),
        );
        let replies = [&edit, &retry];
        assert_eq!(
            replies.iter().filter(|r| is_stream(r)).count(),
            1,
            "round {round}"
        );
        for reply in replies.iter().filter(|r| !is_stream(r)) {
            assert_loser(reply, &format!("round {round}"));
        }
        settle(&rig, chat.id, &replies).await;
        assert_eq!(mutation_events(&rig).await.len(), 1, "round {round}");
    }
}

#[tokio::test]
async fn a_concurrent_retry_and_delete_leave_exactly_one_committed_mutation() {
    for round in 0..6 {
        let (rig, chat, _upstream) = streaming_chat().await;
        let target = seed(&rig.quota.db, &chat, turn("question")).await;
        let (retry, delete) = tokio::join!(
            rig.retry(chat.id, target.request_id),
            rig.delete_turn(chat.id, target.request_id),
        );
        let retry_won = is_stream(&retry);
        let delete_won = delete.status == StatusCode::NO_CONTENT;
        assert_ne!(
            retry_won, delete_won,
            "round {round}: {} / {}",
            retry.text, delete.text
        );
        if retry_won {
            assert_loser(&delete, &format!("round {round}"));
            settle(&rig, chat.id, &[&retry]).await;
        } else {
            assert_loser(&retry, &format!("round {round}"));
        }
        assert_eq!(mutation_events(&rig).await.len(), 1, "round {round}");
        let live = turn_rows(&rig.quota.db, chat.id)
            .await
            .into_iter()
            .filter(|row| row.deleted_at.is_none())
            .count();
        assert_eq!(live, usize::from(retry_won), "round {round}");
    }
}

#[tokio::test]
async fn two_concurrent_deletes_yield_one_204_and_one_409() {
    for round in 0..6 {
        let (rig, chat, _upstream) = streaming_chat().await;
        let target = seed(&rig.quota.db, &chat, turn("question")).await;
        let (a, b) = tokio::join!(
            rig.delete_turn(chat.id, target.request_id),
            rig.delete_turn(chat.id, target.request_id),
        );
        let wins = [&a, &b]
            .iter()
            .filter(|r| r.status == StatusCode::NO_CONTENT)
            .count();
        assert_eq!(wins, 1, "round {round}: {} / {}", a.text, b.text);
        for reply in [&a, &b]
            .iter()
            .filter(|r| r.status != StatusCode::NO_CONTENT)
        {
            assert_loser(reply, &format!("round {round}"));
        }
        assert_eq!(mutation_events(&rig).await.len(), 1, "round {round}");
        assert_eq!(rig.message_count(chat.id).await, 0, "round {round}");
    }
}

#[tokio::test]
async fn a_send_racing_a_mutation_never_leaves_two_running_turns_and_never_a_500() {
    // inst-cr-case-newer, inst-cr-case-running-index, inst-cr-send-race
    for round in 0..6 {
        let (rig, chat, _upstream) = streaming_chat().await;
        let target = seed(&rig.quota.db, &chat, turn("question")).await;
        let body = json!({ "content": "another question" });
        let (retry, send) = tokio::join!(
            rig.retry(chat.id, target.request_id),
            rig.stream(chat.id, &body),
        );
        for reply in [&retry, &send] {
            assert_ne!(
                reply.status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "round {round}: {}",
                reply.text
            );
            assert!(
                is_stream(reply) || reply.status == StatusCode::CONFLICT,
                "round {round}: {} {}",
                reply.status,
                reply.text
            );
        }
        // the retry wins or loses; the chat never ends with two live running turns
        settle(&rig, chat.id, &[&retry, &send]).await;
        let running = turn_rows(&rig.quota.db, chat.id)
            .await
            .into_iter()
            .filter(|row| row.deleted_at.is_none() && row.state == "running")
            .count();
        assert_eq!(running, 0, "round {round}");
    }
}
