//! Thread summary end to end (slice S7): a real turn enqueues the work, the real worker generates
//! and commits the summary through the real outbox, the next turn's provider request carries it;
//! failures dead-letter without hurting the chat; retry, edit and delete of the covered turn drop
//! the summary and the next turn rebuilds it with no base.
//!
//! The queue runs the real handler; the chat provider (`h.oagw`) and the summary provider
//! (`h.summaries`) are scripted apart.
//!
//! Source of truth: DESIGN 3.6 "Thread Summary Update" (sequence: trigger, durable enqueue in the
//! causing turn's transaction, asynchronous delivery, CAS commit that saves the summary, advances
//! the frontier, marks the range compressed and enqueues the system usage event; "Any summary
//! produced by that outbox-driven work is eligible only for subsequent turns"; failure keeps the
//! previous summary, `Reject` after `thread_summary_worker.max_attempts`), "Summary Interaction on
//! Turn Mutation" ("The next summary trigger builds a new summary with no base frontier over all
//! live messages up to its target"), 4 "Context Plan Assembly and Truncation", B.9.3 / B.9.4.

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

use serde_json::json;
use stream_support::{Opts, ProviderReply};
use summary_support::{
    CLOSING, OPENING_FRESH, OPENING_MERGE, Op, Pair, SummaryHarness, SummaryRequestView, TENANT_A,
    is_summary_item, prompt_entries, summary_reply, summary_reply_with, summary_text_of,
};
use uuid::Uuid;

const ANSWER: &str = "assistant:Hello from the fake provider.";

fn labels(prompt: &str) -> Vec<String> {
    prompt_entries(prompt)
        .iter()
        .map(|e| e.split_once(": ").unwrap().1.chars().take(2).collect())
        .collect()
}

fn ids(pairs: &[Pair], upto: usize) -> BTreeSet<Uuid> {
    pairs[..upto]
        .iter()
        .flat_map(|p| [p.user_message, p.assistant_message.unwrap()])
        .collect()
}

// ═════════════════════════════════════════════════════════════════════════════
// the happy path
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_turn_over_the_threshold_produces_a_summary_that_the_next_turn_uses() {
    let h = SummaryHarness::real().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 2, 1000).await;
    h.summaries.push(summary_reply_with(
        "Flow summary text",
        &Opts::default().usage(900, 120),
    ));

    // 1. the triggering turn is an ordinary turn
    let first = h.say_ok(chat, "ping").await;
    assert!(SummaryHarness::summary_applied(&first).is_none());
    let first_items = h.oagw.last_request().conversation_texts();
    assert_eq!(first_items.len(), 5, "{first_items:?}");
    assert!(first_items.iter().all(|i| !is_summary_item(i)));

    // 2. the summary is committed asynchronously
    let row = h.wait_summary(chat).await;
    assert_eq!(
        row.summary_text.as_deref().map(str::trim),
        Some("Flow summary text")
    );
    assert_eq!(
        row.summarized_up_to_message_id,
        pairs[1].assistant_message.unwrap()
    );
    assert_eq!(row.token_estimate, Some(120));
    let compressed: BTreeSet<Uuid> = h.compressed_ids(chat).await.into_iter().collect();
    assert_eq!(
        compressed,
        ids(&pairs, 2),
        "the causing turn is not summarized"
    );
    let request = h.last_summary_request();
    // the summary call is a plain request / response (DESIGN 3.6), chat turns stream
    assert_eq!(request.body["stream"], json!(false), "{}", request.body);
    assert_eq!(
        request.headers.get("accept").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(h.oagw.last_request().body["stream"], json!(true));
    let prompt = request.prompt();
    assert!(prompt.starts_with(OPENING_FRESH) && prompt.trim_end().ends_with(CLOSING));
    assert_eq!(labels(&prompt), ["Q1", "A1", "Q2", "A2"]);
    let usage = h.wait_system_usage(1).await;
    assert_eq!(usage[0].chat_id, chat);
    assert_eq!(usage[0].tenant_id, TENANT_A);
    assert_eq!(usage[0].effective_model, summary_support::SUMMARY_MODEL);
    h.assert_summary_requests_stay(1).await;
    assert_eq!(h.summary_dead_letters().await, 0);

    // 3. the next turn gets the summary and only what follows the frontier
    let second = h.say_ok(chat, "second").await;
    let applied = SummaryHarness::summary_applied(&second).expect("thread_summary_applied");
    assert_eq!(applied, json!({"token_estimate": 120}));
    let items = h.oagw.last_request().conversation_texts();
    assert_eq!(items.len(), 4, "{items:?}");
    assert!(is_summary_item(&items[0]));
    assert_eq!(summary_text_of(&items[0]), "Flow summary text");
    assert_eq!(items[1..], ["user:ping", ANSWER, "user:second"]);

    // the small context triggers nothing more
    h.assert_summary_requests_stay(1).await;
    assert_eq!(h.summaries_of(chat).await.len(), 1);
}

#[tokio::test]
async fn the_next_summary_merges_the_previous_one_with_the_new_messages() {
    let h = SummaryHarness::real().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 2, 1000).await;
    h.summaries.push(summary_reply("First summary"));
    h.say_ok(chat, "ping").await;
    let first = h.wait_summary_text(chat, "First summary").await;
    h.say_ok(chat, "second").await;
    h.assert_summary_requests_stay(1).await;

    // a large message pushes the context into truncation: the summary is stale, urgent trigger
    h.summaries.push(summary_reply("Merged summary"));
    let big = "b".repeat(5000);
    h.say_ok(chat, &big).await;

    let merged = h.wait_summary_text(chat, "Merged summary").await;
    assert_eq!(
        h.summaries_of(chat).await.len(),
        1,
        "one summary row per chat"
    );
    assert!(merged.summarized_up_to_created_at > first.summarized_up_to_created_at);
    let messages = h.messages_in(chat).await;
    // q1 a1 q2 a2 | ping answer second answer | big answer
    assert_eq!(messages.len(), 10);
    assert_eq!(
        merged.summarized_up_to_message_id, messages[7].id,
        "the last message before the big turn"
    );
    let requests = h.wait_summary_requests(2).await;
    let prompt = requests[1].prompt();
    assert!(prompt.starts_with(OPENING_MERGE), "{prompt}");
    assert!(prompt.contains("<existing_summary>") && prompt.contains("First summary"));
    assert_eq!(
        prompt_entries(&prompt),
        [
            "User: ping",
            "Assistant: Hello from the fake provider.",
            "User: second",
            "Assistant: Hello from the fake provider."
        ],
        "only what the first summary does not cover"
    );
    let compressed: BTreeSet<Uuid> = h.compressed_ids(chat).await.into_iter().collect();
    let mut expected = ids(&pairs, 2);
    expected.extend(messages[4..8].iter().map(|m| m.id));
    assert_eq!(compressed, expected);
    assert!(!compressed.contains(&messages[8].id) && !compressed.contains(&messages[9].id));
}

#[tokio::test]
async fn summaries_of_different_chats_do_not_mix() {
    let h = SummaryHarness::real().await;
    let a = h.seed_chat().await;
    let b = h.seed_chat().await;
    let pairs_a = h.seed_sized(a, 2, 1000).await;
    let pairs_b = h.seed_sized(b, 2, 1000).await;
    h.say_ok(a, "ping a").await;
    h.say_ok(b, "ping b").await;

    let row_a = h.wait_summary(a).await;
    let row_b = h.wait_summary(b).await;

    assert_eq!(
        row_a.summarized_up_to_message_id,
        pairs_a[1].assistant_message.unwrap()
    );
    assert_eq!(
        row_b.summarized_up_to_message_id,
        pairs_b[1].assistant_message.unwrap()
    );
    assert_eq!(
        h.compressed_ids(a)
            .await
            .into_iter()
            .collect::<BTreeSet<_>>(),
        ids(&pairs_a, 2)
    );
    assert_eq!(
        h.compressed_ids(b)
            .await
            .into_iter()
            .collect::<BTreeSet<_>>(),
        ids(&pairs_b, 2)
    );
    assert_eq!(h.wait_system_usage(2).await.len(), 2);
}

#[tokio::test]
async fn the_turn_does_not_wait_for_the_summary() {
    let h = SummaryHarness::real().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    // the summary provider stays busy after its first delta
    h.summaries.push(summary_reply_with(
        "slow summary",
        &Opts::default().pause_after(1),
    ));

    let sent = h.say_ok(chat, "ping").await;

    assert_eq!(sent.terminal().event, "done");
    h.summaries.wait_paused(1).await;
    assert!(
        h.summaries_of(chat).await.is_empty(),
        "the summary is not there yet"
    );
    h.summaries.release();
    h.wait_summary_text(chat, "slow summary").await;
}

// ═════════════════════════════════════════════════════════════════════════════
// failures
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_failing_provider_is_retried_then_dead_lettered_and_the_chat_keeps_working() {
    let h = SummaryHarness::real_with_config(json!({"thread_summary_worker": {"max_attempts": 2}}))
        .await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 2, 1000).await;
    h.summaries
        .set_default(|| ProviderReply::http_error(500, "summary provider down"));

    h.say_ok(chat, "ping").await;

    // the delivery that would retry on its last attempt rejects: two calls, one dead letter
    h.wait_summary_requests(2).await;
    h.wait_summary_dead_letters(1).await;
    h.assert_summary_requests_stay(2).await;
    assert!(h.summaries_of(chat).await.is_empty(), "no summary");
    assert!(
        h.compressed_ids(chat).await.is_empty(),
        "nothing is compressed"
    );
    assert!(
        h.system_usage().is_empty(),
        "no usage event for a failed call"
    );

    // the chat keeps working with the uncompressed history
    let sent = h.say_ok(chat, "next").await;
    assert!(SummaryHarness::summary_applied(&sent).is_none());
    let items = h.oagw.last_request().conversation_texts();
    assert!(items.iter().all(|i| !is_summary_item(i)), "{items:?}");
    assert_eq!(items.last().map(String::as_str), Some("user:next"));
    assert!(
        items.contains(&ANSWER.to_owned()),
        "the recent turn is in the context: {items:?}"
    );

    // the new turn is over the budget again: a wider work item is scheduled and fails the same way
    h.wait_summary_dead_letters(2).await;
    assert_eq!(h.summary_requests().len(), 4);
    let _ = pairs;
}

#[tokio::test]
async fn a_provider_that_recovers_is_retried_into_a_single_summary() {
    let h = SummaryHarness::real().await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    h.summaries
        .push(ProviderReply::http_error(503, "unavailable"));
    h.summaries.push(summary_reply("Second try"));

    h.say_ok(chat, "ping").await;

    h.wait_summary_text(chat, "Second try").await;
    assert_eq!(h.summary_requests().len(), 2);
    assert_eq!(h.wait_system_usage(1).await.len(), 1);
    h.assert_system_usage_stays(1).await;
    assert_eq!(h.summary_dead_letters().await, 0);
}

#[tokio::test]
async fn an_unusable_answer_is_retried_like_a_failure() {
    let h = SummaryHarness::real_with_config(json!({"thread_summary_worker": {"max_attempts": 2}}))
        .await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;
    h.summaries.set_default(|| {
        ProviderReply::text_with(&["<analysis>nothing more</analysis>"], &Opts::default())
    });

    h.say_ok(chat, "ping").await;

    h.wait_summary_dead_letters(1).await;
    assert_eq!(h.summary_requests().len(), 2);
    assert!(h.summaries_of(chat).await.is_empty());
}

#[tokio::test]
async fn a_disabled_summary_model_dead_letters_at_once() {
    let h = SummaryHarness::real_with_config(
        json!({"thread_summary_worker": {"summary_model_id": "gpt-off"}}),
    )
    .await;
    let chat = h.seed_chat().await;
    h.seed_sized(chat, 2, 1000).await;

    // the trigger does not check the summary model: the turn works and enqueues
    let sent = h.say_ok(chat, "ping").await;
    assert_eq!(sent.terminal().event, "done");

    h.wait_summary_dead_letters(1).await;
    h.assert_summary_requests_stay(0).await;
    assert!(h.summaries_of(chat).await.is_empty());
}

// ═════════════════════════════════════════════════════════════════════════════
// mutation-driven invalidation
// ═════════════════════════════════════════════════════════════════════════════

/// Three 1000-byte turns, then the real turn `ping` whose completion triggers the summary of the
/// three: returns the harness, the chat, the seeded pairs and the request id of `ping`, once the
/// summary (frontier = answer 3) is committed.
async fn summarized_chat() -> (SummaryHarness, Uuid, Vec<Pair>, Uuid) {
    let h = SummaryHarness::real().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, 3, 1000).await;
    h.summaries.push(summary_reply("Covers turns one to three"));
    let ping = h.say_ok(chat, "ping").await;
    let row = h.wait_summary_text(chat, "Covers turns one to three").await;
    assert_eq!(
        row.summarized_up_to_message_id,
        pairs[2].assistant_message.unwrap()
    );
    h.assert_summary_requests_stay(1).await;
    (h, chat, pairs, ping.request_id())
}

#[tokio::test]
async fn a_mutation_of_the_covered_turn_drops_the_summary_and_the_next_summary_has_no_base() {
    for op in [Op::Retry, Op::Edit, Op::Delete] {
        let (h, chat, pairs, ping) = summarized_chat().await;
        let kept = h.summary_of(chat).await;

        // the summary stops before `ping`: deleting `ping` keeps the summary and its flags
        h.mutate(Op::Delete, chat, ping).await;
        assert_eq!(h.summary_of(chat).await.id, kept.id, "{op:?}");
        assert_eq!(h.compressed_ids(chat).await.len(), 6, "{op:?}");

        // turn 3 is the latest again and the summary covers it: the mutation drops the summary
        // (the answer of the summary provider is queued first: the next trigger may be quick)
        h.summaries.push(summary_reply("Rebuilt summary"));
        h.mutate(op, chat, pairs[2].request_id).await;
        let context = if op == Op::Delete {
            h.say_ok(chat, "next").await;
            h.oagw.last_request().conversation_texts()
        } else {
            h.oagw.last_request().conversation_texts()
        };

        // until the next summary the context is the uncompressed history, bounded by the budget
        assert!(
            context.iter().all(|i| !is_summary_item(i)),
            "{op:?}: no summary item: {context:?}"
        );
        assert!(
            context.iter().any(|i| i.starts_with("assistant:A2")),
            "{op:?}: formerly summarized turns are part of the context again: {context:?}"
        );
        assert!(
            context
                .iter()
                .all(|i| !i.starts_with("user:ping") && !i.contains("assistant:Hello")),
            "{op:?}: the deleted turn is gone: {context:?}"
        );

        // the next trigger builds a new summary with no base over the live messages
        let rebuilt = h.wait_summary_text(chat, "Rebuilt summary").await;
        assert_eq!(
            rebuilt.summarized_up_to_message_id,
            pairs[1].assistant_message.unwrap(),
            "{op:?}: the target is the last message before the causing turn"
        );
        assert_ne!(
            rebuilt.id, kept.id,
            "{op:?}: a new row after the old one was deleted"
        );
        let requests = h.wait_summary_requests(2).await;
        let prompt = requests[1].prompt();
        assert!(
            prompt.starts_with(OPENING_FRESH),
            "{op:?}: no base frontier: {prompt}"
        );
        assert!(!prompt.contains("<existing_summary>"), "{op:?}");
        assert_eq!(
            labels(&prompt),
            ["Q1", "A1", "Q2", "A2"],
            "{op:?}: deleted messages and the replacement turn are not summarized"
        );
        let compressed: BTreeSet<Uuid> = h.compressed_ids(chat).await.into_iter().collect();
        assert_eq!(compressed, ids(&pairs, 2), "{op:?}");
        assert_eq!(h.summaries_of(chat).await.len(), 1, "{op:?}");
        h.assert_summary_requests_stay(2).await;
    }
}

#[tokio::test]
async fn a_retry_of_the_causing_turn_keeps_the_summary_in_the_context() {
    let (h, chat, _pairs, ping) = summarized_chat().await;
    let kept = h.summary_of(chat).await;

    let retried = h.retried(chat, ping).await;
    assert_eq!(retried.terminal().event, "done");

    let items = h.oagw.last_request().conversation_texts();
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(is_summary_item(&items[0]));
    assert_eq!(summary_text_of(&items[0]), "Covers turns one to three");
    assert_eq!(items[1], "user:ping");
    assert!(SummaryHarness::summary_applied(&retried).is_some());
    assert_eq!(
        h.summary_of(chat).await.id,
        kept.id,
        "the summary row is untouched"
    );
    assert_eq!(h.compressed_ids(chat).await.len(), 6);
    h.assert_summary_requests_stay(1).await;
}
