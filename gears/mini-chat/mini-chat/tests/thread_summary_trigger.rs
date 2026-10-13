//! Thread-summary trigger (slice S7): when a completed turn enqueues a durable thread-summary
//! message, what the message carries (the frozen target frontier, the base frontier, the stable
//! `system_request_id`) and when nothing is enqueued.
//!
//! The queue runs a recording stand-in, so the tests see exactly what the finalization
//! transaction handed to the shared outbox. Histories are seeded (2025) and the triggering turn is
//! a real send / retry / edit through the REST routes.
//!
//! Source of truth: DESIGN 3.6 "Thread Summary Update" (trigger timing, "Summary trigger based on
//! token budget": proactive `assembled_context_tokens >= compression_threshold_pct% of
//! effective_budget` with no summary, urgent on `messages_truncated`, an existing summary without
//! truncation never triggers; `effective_budget = min(max_input_tokens, context_window -
//! max_output_tokens_applied)` without surcharges and overhead; "Durable scheduling": payload fields,
//! queue `outbox.thread_summary_queue_name` partitioned by `chat_id`; "the frozen target frontier is
//! the latest non-deleted message of the chat that does not belong to the turn being finalized ...
//! If no earlier message exists, no work item is enqueued"; the trigger is evaluated only when
//! `thread_summary_worker.enabled`, for completed turns), "Summary Interaction on Turn Mutation",
//! B.9.3 / B.9.4, 4 "Context Plan Truncation Algorithm" (token estimation, budget).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::integer_division
)]

mod chat_support;
mod common;
mod mutation_support;
mod quota_support;
mod stream_support;
mod summary_support;

use std::collections::BTreeSet;

use chat_support::parse_ts;
use serde_json::json;
use stream_support::{Opts, ProviderReply, SYSTEM_PROMPT, failed_stream};
use summary_support::{
    DeliveredTask, Op, PREMIUM, PairSeed, SUMMARY_QUEUE, SummaryHarness, SummaryOptions, TASK_TYPE,
    THREAD_SUMMARY_PAYLOAD_TYPE, est_bytes, patch_model, summary_models, summary_models_with_input,
};
use uuid::Uuid;

/// Estimated tokens of everything the turn assembles: system prompt + history + current message.
fn assembled(history: &[String], user: &str) -> u64 {
    est_bytes(SYSTEM_PROMPT.len())
        + history.iter().map(|m| est_bytes(m.len())).sum::<u64>()
        + est_bytes(user.len())
}

fn sized(label: &str, bytes: usize) -> String {
    summary_support::filler(label, bytes)
}

/// `Q1 A1 Q2 A2 ...` texts of `n` turns of `bytes`-byte messages.
fn history_texts(n: usize, bytes: usize) -> Vec<String> {
    (1..=n)
        .flat_map(|i| {
            [
                sized(&format!("Q{i}"), bytes),
                sized(&format!("A{i}"), bytes),
            ]
        })
        .collect()
}

fn target_id(task: &DeliveredTask) -> Uuid {
    Uuid::parse_str(task.payload["frozen_target_message_id"].as_str().unwrap()).unwrap()
}

// ═════════════════════════════════════════════════════════════════════════════
// the proactive trigger
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_completed_turn_over_the_threshold_enqueues_one_summary_task() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 2, 1000).await;
    // 4 x 385 + 120 + 112 = 1772 >= 80 % of 2000, and it still fits the 1900 token budget
    let total = assembled(&history_texts(2, 1000), "ping");
    assert!((1600..=1900).contains(&total), "precondition: {total}");

    let sent = h.say_ok(chat, "ping").await;
    sent.assert_well_formed();

    let delivered = h.log.wait_for(1).await;
    h.log.assert_count_stays(1).await;
    let task = &delivered[0].payload;
    assert_eq!(task["tenant_id"], json!(common::TENANT_A));
    assert_eq!(task["chat_id"], json!(chat));
    assert_eq!(task["system_task_type"], json!(TASK_TYPE));
    let system_request_id = Uuid::parse_str(task["system_request_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        system_request_id.get_version_num(),
        4,
        "a server generated v4 id"
    );
    assert_eq!(delivered[0].payload_type, THREAD_SUMMARY_PAYLOAD_TYPE);

    // nothing was summarized before: no base frontier
    assert!(task["base_frontier_created_at"].is_null(), "{task}");
    assert!(task["base_frontier_message_id"].is_null(), "{task}");
    // the target is the last message before the causing turn
    let last = h.frontier_of(pairs[1].assistant_message.unwrap()).await;
    assert_eq!(task["frozen_target_message_id"], json!(last.message_id));
    assert_eq!(parse_ts(&task["frozen_target_created_at"]), last.created_at);
}

#[tokio::test]
async fn a_turn_below_the_threshold_enqueues_nothing() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 1, 1000).await;
    // 2 x 385 + 232 = 1002 < 1600
    h.say_ok(chat, "ping").await;
    h.log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_short_chat_never_triggers() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_history(chat, 3).await;
    h.say_ok(chat, "ping").await;
    h.log.assert_count_stays(0).await;
}

#[tokio::test]
async fn the_threshold_is_inclusive_and_compared_with_the_effective_budget() {
    // The last answer's size is chosen so that the assembled estimate T is a multiple of 4:
    // with a budget of 5T/4 the 80 % threshold is exactly T (inclusive: it triggers), with a
    // budget 4 tokens larger the threshold is T + 3.2 (it does not). The truncation budget
    // (budget - 100) stays above T in both cases.
    let user = "ping";
    let mut history = history_texts(2, 1000);
    let mut total = 0;
    for bytes in 900..1100 {
        history[3] = sized("A2", bytes);
        total = assembled(&history, user);
        if total % 4 == 0 {
            break;
        }
    }
    assert_eq!(total % 4, 0, "a size with T % 4 == 0 exists");
    let budget = u32::try_from(total / 4 * 5).unwrap();

    for (max_input, triggers) in [(budget, true), (budget + 4, false)] {
        let h = SummaryHarness::with(
            SummaryOptions::default().with_models(summary_models_with_input(max_input)),
        )
        .await;
        let chat = h.seed_chat().await;
        h.seed_pair(chat, PairSeed::completed(&history[0], &history[1]).at(10))
            .await;
        h.seed_pair(chat, PairSeed::completed(&history[2], &history[3]).at(20))
            .await;
        h.say_ok(chat, user).await;
        h.log.assert_count_stays(usize::from(triggers)).await;
    }
}

#[tokio::test]
async fn compression_threshold_pct_moves_the_threshold() {
    // one 1000 byte turn: 1002 tokens = 50.1 % of the 2000 token budget
    for (pct, triggers) in [(50, true), (51, false)] {
        let h = SummaryHarness::with_config(
            json!({"thread_summary_worker": {"compression_threshold_pct": pct}}),
        )
        .await;
        let chat = h.seed_chat().await;
        h.seed_sized(chat, 1, 1000).await;
        h.say_ok(chat, "ping").await;
        h.log.assert_count_stays(usize::from(triggers)).await;
    }
}

#[tokio::test]
async fn the_effective_budget_is_min_of_max_input_and_window_minus_output() {
    // every variant has an effective budget of 2000 tokens: a 1000 byte turn (1002) is below the
    // threshold, two turns (1772) are above it
    let variants = [
        // max_input_tokens binds
        json!({"max_input_tokens": 2000}),
        // window - max_output binds (2500 - 500)
        json!({"max_input_tokens": 100_000, "context_window": 2500, "max_output_tokens": 500}),
        // no separate input limit (0): window - max_output
        json!({"max_input_tokens": 0, "context_window": 2500, "max_output_tokens": 500}),
    ];
    for patch in variants {
        let mut models = summary_models();
        patch_model(&mut models, PREMIUM, patch.clone());
        let h = SummaryHarness::with(SummaryOptions::default().with_models(models)).await;
        let quiet = h.seed_chat().await;
        h.seed_sized(quiet, 1, 1000).await;
        h.say_ok(quiet, "ping").await;
        h.log.assert_count_stays(0).await;

        let busy = h.seed_chat().await;
        h.seed_sized(busy, 2, 1000).await;
        h.say_ok(busy, "ping").await;
        let delivered = h.log.wait_for(1).await;
        assert_eq!(delivered[0].payload["chat_id"], json!(busy), "{patch}");
    }
}

#[tokio::test]
async fn the_first_message_of_a_chat_has_nothing_to_summarize() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    // one big message: 120 + 1485 = 1605 >= 1600 and it fits the budget, but no earlier message
    let big = "b".repeat(5000);
    assert!(assembled(&[], &big) >= 1600);
    h.say_ok(chat, &big).await;
    h.log.assert_count_stays(0).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// the urgent trigger and the existing summary
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_truncated_context_enqueues_a_task() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 3, 1000).await;
    // 6 x 385 + 232 = 2542 > 1900: the oldest turn is dropped from the context
    h.say_ok(chat, "ping").await;

    let delivered = h.log.wait_for(1).await;
    h.log.assert_count_stays(1).await;
    assert!(delivered[0].payload["base_frontier_message_id"].is_null());
    assert_eq!(
        target_id(&delivered[0]),
        pairs[2].assistant_message.unwrap(),
        "everything before the causing turn is summarized, also what was dropped from the context"
    );
}

#[tokio::test]
async fn an_existing_summary_without_truncation_does_not_trigger() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 3, 900).await;
    let text = "earlier turns in short";
    h.seed_summarized_up_to(chat, pairs[0].assistant_message.unwrap(), text)
        .await;
    // after the frontier: 4 x 358 + 232 + the summary item (~165) = ~1829: above the 80 %
    // threshold (1600) and within the budget (1900): no truncation
    let history = history_texts(3, 900)[2..].to_vec();
    let base = assembled(&history, "ping");
    let summary_item = est_bytes(summary_support::PREAMBLE.len() + 2 + text.len());
    assert!(
        base >= 1600,
        "above the threshold even without the summary: {base}"
    );
    assert!(
        base + summary_item + 20 < 1900,
        "no truncation: {base} + {summary_item}"
    );

    let sent = h.say_ok(chat, "ping").await;
    assert!(
        SummaryHarness::summary_applied(&sent).is_some(),
        "the summary is part of the context"
    );
    h.log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_truncated_context_with_a_summary_enqueues_a_task_based_on_its_frontier() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 4, 1000).await;
    h.seed_summarized_up_to(chat, pairs[0].assistant_message.unwrap(), "short summary")
        .await;
    // after the frontier 6 x 385 = 2310 tokens do not fit next to the summary: truncated
    let sent = h.say_ok(chat, "ping").await;
    assert!(SummaryHarness::summary_applied(&sent).is_some());

    let delivered = h.log.wait_for(1).await;
    h.log.assert_count_stays(1).await;
    let task = &delivered[0].payload;
    let frontier = h.frontier_of(pairs[0].assistant_message.unwrap()).await;
    assert_eq!(task["base_frontier_message_id"], json!(frontier.message_id));
    assert_eq!(
        parse_ts(&task["base_frontier_created_at"]),
        frontier.created_at
    );
    assert_eq!(
        task["frozen_target_message_id"],
        json!(pairs[3].assistant_message.unwrap()),
        "the target is the last message before the causing turn"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// the frozen target frontier
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_causing_turn_is_never_part_of_the_target() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 2, 1000).await;
    let sent = h.say_ok(chat, "ping").await;

    let delivered = h.log.wait_for(1).await;
    let rid = sent.request_id();
    let own: BTreeSet<Uuid> = h
        .messages_in(chat)
        .await
        .into_iter()
        .filter(|m| m.request_id == Some(rid))
        .map(|m| m.id)
        .collect();
    assert_eq!(
        own.len(),
        2,
        "the user message and the answer of the causing turn"
    );
    assert!(!own.contains(&target_id(&delivered[0])));
    assert_eq!(
        target_id(&delivered[0]),
        pairs[1].assistant_message.unwrap()
    );
}

#[tokio::test]
async fn the_target_is_the_latest_non_deleted_message() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 2, 1000).await;
    // a later turn that was deleted by the user: it is no target
    h.seed_pair(
        chat,
        PairSeed::completed("removed question", "removed answer")
            .at(30)
            .deleted(),
    )
    .await;
    h.say_ok(chat, "ping").await;

    let delivered = h.log.wait_for(1).await;
    assert_eq!(
        target_id(&delivered[0]),
        pairs[1].assistant_message.unwrap()
    );
}

#[tokio::test]
async fn the_task_is_visible_only_with_the_durable_causing_turn() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    let sent = h.say_ok(chat, "ping").await;

    let delivered = h.log.wait_for(1).await;
    // the message is committed in the finalization transaction: when the outbox hands it out the
    // turn is already `completed` and its answer stored
    let states = &delivered[0].turn_states;
    assert!(states.iter().all(|s| s == "completed"), "{states:?}");
    assert!(
        h.messages_in(chat)
            .await
            .iter()
            .any(|m| m.request_id == Some(sent.request_id()) && m.role == "assistant"),
        "the assistant message is stored"
    );
}

#[tokio::test]
async fn the_summary_does_not_change_the_context_of_the_causing_turn() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    let sent = h.say_ok(chat, "ping").await;
    h.log.wait_for(1).await;

    assert_eq!(
        sent.names_no_ping(),
        vec!["stream_started", "delta", "delta", "done"],
        "the user-visible stream is the ordinary one"
    );
    assert!(SummaryHarness::summary_applied(&sent).is_none());
    let request = h.oagw.last_request();
    let items = request.conversation_texts();
    assert_eq!(
        items.len(),
        5,
        "four history messages and the current one: {items:?}"
    );
    assert!(items.iter().all(|i| !summary_support::is_summary_item(i)));
}

// ═════════════════════════════════════════════════════════════════════════════
// exactly once, and only for completed turns
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_replay_of_a_completed_turn_enqueues_nothing_more() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    let rid = Uuid::new_v4();
    let first = h.say_with(chat, "ping", rid).await;
    assert_eq!(first.terminal().event, "done");
    h.log.wait_for(1).await;

    let replay = h.say_with(chat, "ping", rid).await;
    assert_eq!(replay.started()["is_new_turn"], json!(false));
    h.log.assert_count_stays(1).await;
}

#[tokio::test]
async fn every_trigger_gets_its_own_system_request_id() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    h.say_ok(chat, "first").await;
    // no summary was committed: the chat grew and the next turn triggers again
    h.say_ok(chat, "second").await;

    let delivered = h.log.wait_for(2).await;
    h.log.assert_count_stays(2).await;
    let ids: BTreeSet<String> = delivered
        .iter()
        .map(|d| d.payload["system_request_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    // the second target is the answer of the first turn: q1 a1 q2 a2 first answer second answer
    let messages = h.messages_in(chat).await;
    assert_eq!(messages.len(), 8);
    assert_eq!(target_id(&delivered[0]), messages[3].id);
    assert_eq!(target_id(&delivered[1]), messages[5].id);
}

#[tokio::test]
async fn a_failed_turn_enqueues_nothing() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 3, 1000).await;
    h.oagw.push(ProviderReply::http_error(500, "provider down"));
    let sent = h.say(chat, "ping").await;
    assert_eq!(sent.terminal().event, "error");
    h.log.assert_count_stays(0).await;
    assert!(h.turns_in(chat).await.iter().any(|t| t.state == "failed"));

    // an answer that fails mid-stream ends the same way
    h.oagw.push(ProviderReply::sse(failed_stream(
        "server_error",
        "boom",
        &["partial"],
        Some((10, 2)),
    )));
    let sent = h.say(chat, "again").await;
    assert_eq!(sent.terminal().event, "error");
    h.log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_cancelled_turn_enqueues_nothing() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 3, 1000).await;
    h.oagw.push(ProviderReply::text_with(
        &["held", " back"],
        &Opts::default().pause_after(1),
    ));
    let mut live = h
        .open(chat, &common::ctx_a(), &json!({"content": "ping"}))
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.expect("first delta");
    h.oagw.wait_paused(1).await;
    drop(live);
    h.oagw.release();
    stream_support::eventually("the turn is cancelled", || async {
        h.turns_in(chat)
            .await
            .iter()
            .any(|t| t.state == "cancelled")
    })
    .await;
    h.log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_disabled_worker_enqueues_nothing() {
    let h = SummaryHarness::with_config(json!({"thread_summary_worker": {"enabled": false}})).await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 3, 1000).await;
    // truncated and far above the threshold
    h.say_ok(chat, "ping").await;
    h.log.assert_count_stays(0).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// retry and edit
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_retry_or_edit_turn_triggers_with_the_last_message_before_it() {
    for op in [Op::Retry, Op::Edit] {
        let h = SummaryHarness::new().await;
        let chat = h.seed_chat().await;
        // three turns: the retried one is the latest; its replacement sees turns 1 and 2
        let pairs = h.seed_sized(chat, 3, 1000).await;
        h.mutate(op, chat, pairs[2].request_id).await;

        let delivered = h.log.wait_for(1).await;
        h.log.assert_count_stays(1).await;
        assert_eq!(
            target_id(&delivered[0]),
            pairs[1].assistant_message.unwrap(),
            "{op:?}: the replaced turn is soft-deleted and the replacement is the causing turn"
        );
        assert!(
            delivered[0].payload["base_frontier_message_id"].is_null(),
            "{op:?}"
        );
    }
}

#[tokio::test]
async fn a_delete_enqueues_nothing() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 3, 1000).await;
    h.mutate(Op::Delete, chat, pairs[2].request_id).await;
    h.log.assert_count_stays(0).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// the queue
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_queue_is_named_by_the_configuration() {
    let h = SummaryHarness::with_config(
        json!({"outbox": {"thread_summary_queue_name": "custom.summaries"}}),
    )
    .await;
    assert_eq!(
        h.config.outbox.thread_summary_queue_name,
        "custom.summaries"
    );
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    h.say_ok(chat, "ping").await;
    h.log.wait_for(1).await;
}

#[tokio::test]
async fn the_default_queue_is_mini_chat_thread_summary() {
    let h = SummaryHarness::new().await;
    assert_eq!(h.config.outbox.thread_summary_queue_name, SUMMARY_QUEUE);
    assert_eq!(h.config.outbox.num_partitions, 4);
}

#[tokio::test]
async fn messages_are_partitioned_by_chat() {
    let h = SummaryHarness::new().await;
    // ids 1 and 5 share the partition `low32(chat_id) % 4`, id 2 does not
    let same_a = h.seed_chat_with_id(1).await;
    let same_b = h.seed_chat_with_id(5).await;
    let other = h.seed_chat_with_id(2).await;
    for chat in [same_a, same_b, other] {
        h.seed_sized(chat, 2, 1000).await;
        h.say_ok(chat, "ping").await;
    }
    // a second message of the first chat
    h.say_ok(same_a, "again").await;

    let delivered = h.log.wait_for(4).await;
    let partition_of = |chat: Uuid| -> Vec<i64> {
        delivered
            .iter()
            .filter(|d| d.payload["chat_id"] == json!(chat))
            .map(|d| d.partition_id)
            .collect()
    };
    let a = partition_of(same_a);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0], a[1], "all messages of a chat share one partition");
    assert_eq!(
        partition_of(same_b),
        vec![a[0]],
        "ids congruent modulo the partition count"
    );
    assert_ne!(
        partition_of(other),
        vec![a[0]],
        "another partition for another residue"
    );
}
