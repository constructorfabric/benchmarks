//! Idempotency, replay and the parallel-turn guard of `messages:stream`.
//!
//! Source of truth: DESIGN 3.3 "Streaming Contract" (Idempotency table,
//! "Parallel turn guard", "Replay is side-effect-free invariant"), 4 "Turn
//! Lifecycle, Crash Recovery and Orphan Handling" (Idempotency Rules, Parallel
//! Turn Policy with the normative check order, 409 Recovery), the REST error
//! table (`request_id_conflict`, `turn_already_running`), ADR-0010 (replay
//! rebuilds `quota_decision` from the stored models, no `downgrade_reason`, no
//! citations).

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

mod chat_support;
mod common;
mod stream_support;

use serde_json::{Value, json};
use stream_support::{
    MessageSeed, Opts, ProviderReply, StreamHarness, TurnSeed, assert_conflict, ctx_a, second_user,
    ts,
};
use uuid::Uuid;

/// Start a turn that stays `running` (the provider pauses after its first delta).
async fn hold_running(h: &StreamHarness, chat: Uuid, request_id: Uuid) -> stream_support::LiveSse {
    h.oagw.push(ProviderReply::text_with(
        &["held", " back"],
        &Opts::default().pause_after(1),
    ));
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "hold", "request_id": request_id}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.expect("first delta");
    h.oagw.wait_paused(1).await;
    live
}

// ═════════════════════════════════════════════════════════════════════════════
// completed turns are replayed
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_completed_request_id_is_replayed_without_a_provider_call() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["Hel", "lo ", "there"],
        &Opts::default().usage(20, 6),
    ));
    let first = h.say_with(chat, "Hello", rid).await;
    assert_eq!(first.terminal().event, "done");

    let counts = h.row_counts().await;
    let turn_before = h.turn_of(rid).await.unwrap();
    let chat_before = h.chat.chat_row(chat).await.unwrap();
    let calls = h.oagw.request_count();

    let replay = h.say_with(chat, "Hello", rid).await;
    replay.assert_well_formed();
    assert_eq!(
        replay.names_no_ping(),
        vec!["stream_started", "delta", "done"],
        "one delta with the full text, no citations"
    );
    let started = replay.started();
    assert_eq!(started["request_id"], json!(rid.to_string()));
    assert_eq!(
        started["message_id"],
        json!(first.message_id().to_string()),
        "the persisted assistant message id"
    );
    assert_eq!(started["is_new_turn"], json!(false));
    assert_eq!(
        replay.of("delta")[0].data,
        json!({"type": "text", "content": "Hello there"})
    );
    let done = replay.done();
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 20, "output_tokens": 6})
    );
    assert_eq!(done["effective_model"], json!("gpt-premium"));
    assert_eq!(done["selected_model"], json!("gpt-premium"));
    assert_eq!(done["quota_decision"], json!("allow"));
    assert!(done.get("downgrade_reason").is_none_or(Value::is_null));

    // side-effect free
    assert_eq!(h.oagw.request_count(), calls, "no provider call");
    assert_eq!(h.row_counts().await, counts, "no new rows");
    let turn_after = h.turn_of(rid).await.unwrap();
    assert_eq!(turn_after, turn_before, "the turn row is untouched");
    let chat_after = h.chat.chat_row(chat).await.unwrap();
    assert_eq!(
        chat_after.updated_at, chat_before.updated_at,
        "updated_at is not bumped"
    );
}

#[tokio::test]
async fn a_replay_ignores_the_body_content_and_does_not_store_it() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text(&["original answer"]));
    h.say_with(chat, "original question", rid).await;

    let replay = h
        .say_with(chat, "a completely different question", rid)
        .await;
    assert_eq!(replay.text(), "original answer");
    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|m| !m.content.contains("different")));
}

#[tokio::test]
async fn a_replay_is_served_from_storage_even_when_the_catalog_changed() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.say_with(chat, "q", rid).await;

    // the chat's model left the catalog and the kill switches changed afterwards
    h.policy.set_catalog(&[stream_support::stream_model_json(
        "another-model",
        "Standard",
    )]);
    h.policy
        .set_kill_switches(json!({"disable_web_search": true, "force_standard_tier": true}));
    let replay = h.say_with(chat, "q", rid).await;
    assert_eq!(replay.started()["is_new_turn"], json!(false));
    assert_eq!(replay.done()["effective_model"], json!("gpt-premium"));
    assert_eq!(replay.done()["selected_model"], json!("gpt-premium"));
}

#[tokio::test]
async fn an_older_turn_can_be_replayed_after_newer_ones() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let first_rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text(&["first answer"]));
    h.say_with(chat, "first", first_rid).await;
    h.oagw.push(ProviderReply::text(&["second answer"]));
    h.say(chat, "second").await;

    let replay = h.say_with(chat, "first", first_rid).await;
    assert_eq!(replay.text(), "first answer");
    assert_eq!(h.oagw.request_count(), 2);
}

#[tokio::test]
async fn a_replay_rebuilds_usage_and_the_decision_from_the_stored_rows() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await; // gpt-premium
    let rid = Uuid::new_v4();
    h.chat
        .seed_message(
            MessageSeed::user(chat)
                .request(Some(rid))
                .content("stored question")
                .at(1),
        )
        .await;
    let assistant = h
        .chat
        .seed_message(
            MessageSeed::assistant(chat)
                .request(Some(rid))
                .content("stored answer")
                .model("gpt-standard")
                .tokens(77, 9)
                .at(2),
        )
        .await;
    let mut seed = TurnSeed::new(chat, rid, "completed")
        .assistant(assistant)
        .effective_model("gpt-standard");
    seed.started_at = ts(1);
    h.seed_turn(seed).await;

    let replay = h.say_with(chat, "anything", rid).await;
    assert_eq!(replay.started()["message_id"], json!(assistant.to_string()));
    assert_eq!(replay.started()["is_new_turn"], json!(false));
    assert_eq!(replay.text(), "stored answer");
    let done = replay.done();
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 77, "output_tokens": 9})
    );
    assert_eq!(done["effective_model"], json!("gpt-standard"));
    assert_eq!(done["selected_model"], json!("gpt-premium"), "chats.model");
    assert_eq!(done["quota_decision"], json!("downgrade"));
    assert_eq!(done["downgrade_from"], json!("gpt-premium"));
    assert!(
        done.get("downgrade_reason").is_none_or(Value::is_null),
        "the reason is not persisted, so a replay omits it"
    );
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn replaying_someone_elses_turn_is_404() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.say_with(chat, "q", rid).await;
    let before = h.oagw.request_count();
    for ctx in [second_user(), stream_support::tenant_b_user()] {
        let resp = h
            .open(chat, &ctx, &json!({"content": "q", "request_id": rid}))
            .await
            .into_resp()
            .await;
        chat_support::assert_not_found(&resp, "chat");
    }
    assert_eq!(h.oagw.request_count(), before);
}

#[tokio::test]
async fn the_same_request_id_in_another_chat_is_a_new_turn() {
    let h = StreamHarness::new().await;
    let rid = Uuid::new_v4();
    let one = h.seed_chat().await;
    let two = h.seed_chat().await;
    let first = h.say_with(one, "q", rid).await;
    let second = h.say_with(two, "q", rid).await;
    assert_eq!(first.started()["is_new_turn"], json!(true));
    assert_eq!(
        second.started()["is_new_turn"],
        json!(true),
        "the key is (chat_id, request_id)"
    );
    assert_ne!(first.message_id(), second.message_id());
    assert_eq!(h.oagw.request_count(), 2);
}

// ═════════════════════════════════════════════════════════════════════════════
// every other state of the key is a conflict
// ═════════════════════════════════════════════════════════════════════════════

fn assert_generic_conflict(resp: &stream_support::Resp, internal_ids: &[Uuid]) {
    assert_conflict(resp, "request_id_conflict");
    assert!(
        !resp.raw.to_lowercase().contains("unique") && !resp.raw.to_lowercase().contains("sqlite"),
        "no driver text: {}",
        resp.raw
    );
    for id in internal_ids {
        assert!(
            !resp.raw.contains(&id.to_string()),
            "internal id {id} leaked: {}",
            resp.raw
        );
    }
}

#[tokio::test]
async fn reusing_the_request_id_of_a_running_turn_is_409_request_id_conflict() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    let mut live = hold_running(&h, chat, rid).await;
    let turn_id = h.turn_of(rid).await.unwrap().id;
    let counts = h.row_counts().await;

    let resp = h
        .reject(chat, json!({"content": "again", "request_id": rid}))
        .await;
    assert_generic_conflict(&resp, &[turn_id]);
    assert_eq!(
        h.oagw.request_count(),
        1,
        "the provider is not called again"
    );
    assert_eq!(h.row_counts().await, counts);

    h.oagw.release();
    live.drain().await;
}

#[tokio::test]
async fn reusing_the_request_id_of_a_failed_turn_is_409_request_id_conflict() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::http_error(500, "nope"));
    let failed = h.say_with(chat, "q", rid).await;
    assert_eq!(failed.terminal().event, "error");
    let calls = h.oagw.request_count();

    let resp = h
        .reject(chat, json!({"content": "q", "request_id": rid}))
        .await;
    assert_generic_conflict(&resp, &[]);
    assert_eq!(h.oagw.request_count(), calls);
    assert_eq!(
        h.turn_of(rid).await.unwrap().state,
        "failed",
        "the turn is never reused"
    );
}

#[tokio::test]
async fn reusing_the_request_id_of_a_cancelled_or_deleted_turn_is_409_request_id_conflict() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let cancelled = Uuid::new_v4();
    let deleted = Uuid::new_v4();
    let orphan = Uuid::new_v4();
    h.seed_turn(TurnSeed::new(chat, cancelled, "cancelled"))
        .await;
    h.seed_turn(TurnSeed::new(chat, deleted, "completed").deleted())
        .await;
    h.seed_turn(TurnSeed::new(chat, orphan, "failed").error("orphan_timeout"))
        .await;

    for rid in [cancelled, deleted, orphan] {
        let resp = h
            .reject(chat, json!({"content": "q", "request_id": rid}))
            .await;
        assert_generic_conflict(&resp, &[]);
    }
    h.oagw.assert_no_provider_call();
    assert_eq!(h.all_turns().await.len(), 3);
}

#[tokio::test]
async fn two_sends_with_the_same_new_request_id_yield_one_stream_and_one_conflict() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["slow"],
        &Opts::default().pause_after(0),
    ));
    let body = json!({"content": "race", "request_id": rid});
    let ctx = ctx_a();
    let (a, b) = tokio::join!(h.open(chat, &ctx, &body), h.open(chat, &ctx, &body));
    let (stream, loser) = if a.is_sse() { (a, b) } else { (b, a) };
    assert!(stream.is_sse(), "one of the two wins");
    assert!(!loser.is_sse(), "the other is rejected with JSON");
    let resp = loser.into_resp().await;
    assert_conflict(&resp, "request_id_conflict");

    h.oagw.release();
    let sent = stream.collect().await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(h.oagw.request_count(), 1);
    assert_eq!(
        h.messages_in(chat).await.len(),
        2,
        "one user message, one answer"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// at most one running turn per chat
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_second_send_while_a_turn_runs_is_409_turn_already_running() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let mut live = hold_running(&h, chat, Uuid::new_v4()).await;
    let counts = h.row_counts().await;

    for body in [
        json!({"content": "another", "request_id": Uuid::new_v4()}),
        json!({"content": "no request id at all"}),
    ] {
        let resp = h.reject(chat, body).await;
        assert_conflict(&resp, "turn_already_running");
    }
    assert_eq!(h.oagw.request_count(), 1, "the provider is called once");
    assert_eq!(
        h.row_counts().await,
        counts,
        "no user message of the rejected sends"
    );

    h.oagw.release();
    let rest = live.drain().await;
    assert_eq!(rest.last().unwrap().event, "done");
}

#[tokio::test]
async fn after_the_first_turn_completes_a_new_send_is_accepted() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let mut live = hold_running(&h, chat, Uuid::new_v4()).await;
    let resp = h.reject(chat, json!({"content": "too early"})).await;
    assert_conflict(&resp, "turn_already_running");

    h.oagw.release();
    live.drain().await;
    let sent = h.say(chat, "now it is fine").await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(h.messages_in(chat).await.len(), 4);
}

#[tokio::test]
async fn the_guard_is_per_chat() {
    let h = StreamHarness::new().await;
    let busy = h.seed_chat().await;
    let other = h.seed_chat().await;
    let mut live = hold_running(&h, busy, Uuid::new_v4()).await;
    let sent = h.say(other, "unaffected").await;
    assert_eq!(sent.terminal().event, "done");
    h.oagw.release();
    live.drain().await;
}

#[tokio::test]
async fn two_simultaneous_sends_with_different_request_ids_yield_one_stream_and_one_409() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["slow"],
        &Opts::default().pause_after(0),
    ));
    let ctx = ctx_a();
    let one = json!({"content": "one", "request_id": Uuid::new_v4()});
    let two = json!({"content": "two", "request_id": Uuid::new_v4()});
    let (a, b) = tokio::join!(h.open(chat, &ctx, &one), h.open(chat, &ctx, &two));
    let (stream, loser) = if a.is_sse() { (a, b) } else { (b, a) };
    assert!(stream.is_sse() && !loser.is_sse());
    assert_conflict(&loser.into_resp().await, "turn_already_running");

    h.oagw.release();
    let sent = stream.collect().await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(h.oagw.request_count(), 1);
    assert_eq!(
        h.messages_in(chat).await.len(),
        2,
        "the loser left nothing behind"
    );
    assert_eq!(h.all_turns().await.len(), 1);
}

#[tokio::test]
async fn a_running_turn_left_by_a_crashed_pod_blocks_the_chat_until_the_watchdog() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let stuck = Uuid::new_v4();
    h.seed_turn(TurnSeed::new(chat, stuck, "running")).await;

    let other = h
        .reject(chat, json!({"content": "q", "request_id": Uuid::new_v4()}))
        .await;
    assert_conflict(&other, "turn_already_running");
    let same = h
        .reject(chat, json!({"content": "q", "request_id": stuck}))
        .await;
    assert_conflict(&same, "request_id_conflict");
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn the_idempotency_check_runs_before_the_parallel_turn_guard() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let done_rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text(&["finished earlier"]));
    h.say_with(chat, "q", done_rid).await;

    let mut live = hold_running(&h, chat, Uuid::new_v4()).await;
    // a different request would be 409 turn_already_running ...
    let blocked = h.reject(chat, json!({"content": "x"})).await;
    assert_conflict(&blocked, "turn_already_running");
    // ... but the completed key is still replayed
    let replay = h.say_with(chat, "q", done_rid).await;
    assert_eq!(replay.started()["is_new_turn"], json!(false));
    assert_eq!(replay.text(), "finished earlier");

    h.oagw.release();
    live.drain().await;
}

// ═════════════════════════════════════════════════════════════════════════════
// concurrent sends: deterministic 409 classification (insert race)
// ═════════════════════════════════════════════════════════════════════════════

/// Outcome of one racing send.
#[derive(Debug, PartialEq, Eq)]
enum Raced {
    Stream { is_new_turn: bool },
    Conflict(String),
}

async fn race(h: &StreamHarness, chat: Uuid, ids: &[Uuid]) -> Vec<Raced> {
    let ctx = ctx_a();
    let bodies: Vec<Value> = ids
        .iter()
        .map(|id| json!({"content": "race", "request_id": id}))
        .collect();
    let opened =
        futures::future::join_all(bodies.iter().map(|body| h.open(chat, &ctx, body))).await;
    let mut out = Vec::new();
    for raw in opened {
        if raw.is_sse() {
            let sent = raw.collect().await;
            out.push(Raced::Stream {
                is_new_turn: sent.started()["is_new_turn"] == json!(true),
            });
        } else {
            let resp = raw.into_resp().await;
            assert_eq!(resp.status.as_u16(), 409, "{}", resp.raw);
            let reason = resp.json()["context"]["reason"]
                .as_str()
                .unwrap_or_else(|| panic!("no 409 reason: {}", resp.raw))
                .to_owned();
            out.push(Raced::Conflict(reason));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_sends_with_different_ids_lose_with_turn_already_running_only() {
    // The provider answers instantly, so a winner often finalizes before the
    // loser re-reads: the loss must still be turn_already_running, never a
    // raw unique violation / already_exists.
    let h = StreamHarness::new().await;
    h.oagw.set_default(|| ProviderReply::text(&["quick"]));
    for _ in 0..15 {
        let chat = h.seed_chat().await;
        let ids: Vec<Uuid> = (0..6).map(|_| Uuid::new_v4()).collect();
        let outcomes = race(&h, chat, &ids).await;
        assert!(
            outcomes.iter().any(|o| matches!(o, Raced::Stream { .. })),
            "{outcomes:?}"
        );
        for outcome in &outcomes {
            match outcome {
                Raced::Stream { is_new_turn } => assert!(is_new_turn, "{outcomes:?}"),
                Raced::Conflict(reason) => assert_eq!(reason, "turn_already_running"),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_sends_with_different_ids_leave_one_turn_per_winner() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.set_default(|| ProviderReply::text(&["only one"]));
    let ids: Vec<Uuid> = (0..6).map(|_| Uuid::new_v4()).collect();
    let outcomes = race(&h, chat, &ids).await;
    let streams = outcomes
        .iter()
        .filter(|o| matches!(o, Raced::Stream { .. }))
        .count();
    assert!(streams >= 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|o| !matches!(o, Raced::Conflict(r) if r != "turn_already_running")),
        "{outcomes:?}"
    );
    assert_eq!(h.all_turns().await.len(), streams);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_sends_with_the_same_id_never_report_turn_already_running() {
    let h = StreamHarness::new().await;
    h.oagw.set_default(|| ProviderReply::text(&["quick"]));
    for _ in 0..15 {
        let chat = h.seed_chat().await;
        let rid = Uuid::new_v4();
        let outcomes = race(&h, chat, &[rid, rid, rid, rid]).await;
        let new_turns = outcomes
            .iter()
            .filter(|o| matches!(o, Raced::Stream { is_new_turn: true }))
            .count();
        assert_eq!(new_turns, 1, "exactly one winner: {outcomes:?}");
        for outcome in &outcomes {
            match outcome {
                // the winner, or a loser served the completed turn (replay)
                Raced::Stream { .. } => {}
                Raced::Conflict(reason) => {
                    assert_eq!(reason, "request_id_conflict", "{outcomes:?}");
                }
            }
        }
        assert_eq!(
            h.all_turns()
                .await
                .iter()
                .filter(|t| t.chat_id == chat)
                .count(),
            1
        );
    }
}
