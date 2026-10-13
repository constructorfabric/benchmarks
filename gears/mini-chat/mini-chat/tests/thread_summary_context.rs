//! Thread summary in the context of the next turns (slice S7): where the summary sits in the
//! provider request, which messages remain next to it, how it is budgeted and when the client is
//! told (`stream_started.thread_summary_applied`).
//!
//! Summaries and their frontiers are seeded; the turn is a real send / retry / edit through the
//! REST routes and the assertions read the provider request the fake OAGW recorded.
//!
//! Source of truth: DESIGN 4 "Context Plan Assembly and Truncation" (priority table: system prompt
//! and the current message are never truncated, the summary is droppable and ranks above the
//! recent messages; "Thread summary delivery format": one `user` message with the preamble of B.5.5
//! in front of the text, the preamble counts toward the size; algorithm steps 1 - 3; the "Recent
//! messages query": `is_compressed = false` and `(created_at, id) > frontier` when a summary
//! exists), 3.3 SSE contract (`stream_started.thread_summary_applied = {token_estimate}` only when
//! a summary is part of the context), "Summary Interaction on Turn Mutation" ("After a retry or
//! edit of a turn that no summary covers, the next context is the existing summary followed by the
//! replacement turn"), B.5.5 (preamble).

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

use serde_json::json;
use stream_support::SYSTEM_PROMPT;
use summary_support::{
    CHAT_MAX_INPUT, MessageSeed, Op, PREAMBLE, PairSeed, SummaryHarness, est_bytes, filler,
    is_summary_item, summary_text_of, ts, uid,
};

const BUDGET: u64 = CHAT_MAX_INPUT as u64 - 100;

fn item_text(item: &str) -> &str {
    item.split_once(':').unwrap().1
}

/// The kept history of the truncation algorithm (DESIGN 4): newest to oldest while it fits, then
/// the leading assistant messages are dropped. Independent of the production code.
fn kept(history: &[(&str, String)], remaining: u64) -> Vec<String> {
    let mut used = 0;
    let mut from = history.len();
    for (i, (_, text)) in history.iter().enumerate().rev() {
        let cost = est_bytes(text.len());
        if used + cost > remaining {
            break;
        }
        used += cost;
        from = i;
    }
    while from < history.len() && history[from].0 == "assistant" {
        from += 1;
    }
    history[from..]
        .iter()
        .map(|(role, text)| format!("{role}:{text}"))
        .collect()
}

// ═════════════════════════════════════════════════════════════════════════════
// placement
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_summary_is_a_user_message_between_the_system_prompt_and_the_recent_messages() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 3).await;
    h.seed_summarized_up_to(chat, pairs[0].assistant_message.unwrap(), "SUMMARY-TEXT-1")
        .await;

    let sent = h.say_ok(chat, "now").await;

    let request = h.oagw.last_request();
    let items = request.conversation_texts();
    assert_eq!(items.len(), 6, "{items:?}");
    assert!(
        is_summary_item(&items[0]),
        "the summary comes first: {items:?}"
    );
    assert_eq!(summary_text_of(&items[0]), "SUMMARY-TEXT-1");
    assert_eq!(
        items[1..],
        [
            "user:question 2",
            "assistant:answer 2",
            "user:question 3",
            "assistant:answer 3",
            "user:now"
        ],
        "only what follows the frontier stays next to the summary"
    );
    // a `user` message, never part of the system prompt
    let input = request.body["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], json!("user"));
    let system = request.system_text();
    assert!(system.contains(SYSTEM_PROMPT), "{system}");
    assert!(
        !system.contains("SUMMARY-TEXT-1") && !system.contains("summarized"),
        "{system}"
    );
    assert!(items[0].starts_with(&format!("user:{PREAMBLE}")));
    assert!(sent.started().get("thread_summary_applied").is_some());
}

#[tokio::test]
async fn messages_up_to_the_frontier_are_not_sent_even_when_they_are_not_flagged() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 3).await;
    // the summary row alone: the frontier decides, not the flag
    let frontier = h.frontier_of(pairs[1].assistant_message.unwrap()).await;
    h.seed_summary_text(
        chat,
        frontier.created_at,
        frontier.message_id,
        "ROW-ONLY",
        9,
    )
    .await;

    h.say_ok(chat, "now").await;

    let items = h.oagw.last_request().conversation_texts();
    assert_eq!(items.len(), 4, "{items:?}");
    assert!(is_summary_item(&items[0]));
    assert_eq!(
        items[1..],
        ["user:question 3", "assistant:answer 3", "user:now"]
    );
}

#[tokio::test]
async fn the_frontier_comparison_is_on_created_at_then_message_id() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    // three messages at the same instant (ids 1 < 2 < 3) and one a second later
    for (n, role, text, at) in [
        (1_u128, "user", "tie one", 10),
        (2, "assistant", "tie two", 10),
        (3, "user", "tie three", 10),
        (4, "assistant", "later", 11),
    ] {
        let seed = if role == "user" {
            MessageSeed::user(chat)
        } else {
            MessageSeed::assistant(chat)
        };
        h.chat
            .seed_message(seed.with_id(n).content(text).at(at))
            .await;
    }
    // the frontier is the second message: only the larger id of the same instant and later ones follow
    h.seed_summary_text(chat, ts(10), uid(2), "TIE-SUMMARY", 5)
        .await;

    h.say_ok(chat, "now").await;

    let items = h.oagw.last_request().conversation_texts();
    assert!(is_summary_item(&items[0]), "{items:?}");
    assert_eq!(
        items[1..],
        ["user:tie three", "assistant:later", "user:now"]
    );
}

#[tokio::test]
async fn flagged_messages_stay_out_of_the_context_without_a_summary_row() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 3).await;
    h.mark_compressed(&[pairs[0].user_message, pairs[0].assistant_message.unwrap()])
        .await;

    let sent = h.say_ok(chat, "now").await;

    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        [
            "user:question 2",
            "assistant:answer 2",
            "user:question 3",
            "assistant:answer 3",
            "user:now"
        ],
        "`is_compressed = false` always applies; no summary row, no summary item"
    );
    assert!(SummaryHarness::summary_applied(&sent).is_none());
}

#[tokio::test]
async fn a_summary_of_another_chat_is_not_used() {
    let h = SummaryHarness::new().await;
    let with = h.seed_chat().await;
    let without = h.seed_chat().await;
    let with_pairs = h.seed_history(with, 2).await;
    h.seed_history(without, 2).await;
    h.seed_summarized_up_to(
        with,
        with_pairs[0].assistant_message.unwrap(),
        "ONLY-FOR-ONE",
    )
    .await;

    let sent = h.say_ok(without, "now").await;

    let items = h.oagw.last_request().conversation_texts();
    assert_eq!(
        items.len(),
        5,
        "the whole history of the other chat: {items:?}"
    );
    assert!(
        items
            .iter()
            .all(|i| !is_summary_item(i) && !i.contains("ONLY-FOR-ONE"))
    );
    assert!(SummaryHarness::summary_applied(&sent).is_none());
}

#[tokio::test]
async fn the_summary_stays_in_the_context_of_every_following_turn() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 2).await;
    h.seed_summarized_up_to(chat, pairs[0].assistant_message.unwrap(), "KEPT-SUMMARY")
        .await;

    h.say_ok(chat, "first").await;
    let second = h.say_ok(chat, "second").await;

    let items = h.oagw.last_request().conversation_texts();
    assert!(is_summary_item(&items[0]));
    assert_eq!(
        items[1..],
        [
            "user:question 2",
            "assistant:answer 2",
            "user:first",
            "assistant:Hello from the fake provider.",
            "user:second"
        ]
    );
    assert!(SummaryHarness::summary_applied(&second).is_some());
}

// ═════════════════════════════════════════════════════════════════════════════
// thread_summary_applied
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn stream_started_reports_the_token_estimate_of_the_summary_in_the_context() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 2).await;
    let frontier = h.frontier_of(pairs[0].assistant_message.unwrap()).await;
    h.seed_summary_text(chat, frontier.created_at, frontier.message_id, "text", 77)
        .await;

    let sent = h.say_ok(chat, "now").await;

    let applied = SummaryHarness::summary_applied(&sent).expect("thread_summary_applied");
    assert_eq!(applied, json!({"token_estimate": 77}));
    // the rest of the event is untouched
    let started = sent.started();
    assert_eq!(started["is_new_turn"], json!(true));
    assert!(started["request_id"].is_string() && started["message_id"].is_string());
    sent.assert_well_formed();
}

#[tokio::test]
async fn no_summary_no_thread_summary_applied() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_history(chat, 2).await;

    let sent = h.say_ok(chat, "now").await;

    assert!(
        SummaryHarness::summary_applied(&sent).is_none(),
        "{}",
        sent.started()
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// the budget
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_summary_that_does_not_fit_is_dropped_entirely() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 2).await;
    // 8000 bytes ~ 2200 tokens: more than what is left after the mandatory items (1668)
    let huge = "h".repeat(8000);
    let frontier = h.frontier_of(pairs[0].assistant_message.unwrap()).await;
    h.seed_summary_text(chat, frontier.created_at, frontier.message_id, &huge, 2000)
        .await;

    let sent = h.say_ok(chat, "now").await;

    let items = h.oagw.last_request().conversation_texts();
    assert_eq!(items, ["user:question 2", "assistant:answer 2", "user:now"]);
    assert!(
        SummaryHarness::summary_applied(&sent).is_none(),
        "{}",
        sent.started()
    );
}

#[tokio::test]
async fn the_summary_ranks_above_the_recent_messages() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    // turn 0 is summarized; four turns of 600 byte messages follow the frontier
    let first = h.seed_sized(chat, 1, 100).await;
    let mut texts = Vec::new();
    for i in 1..=4_i64 {
        let q = filler(&format!("U{i}"), 600);
        let a = filler(&format!("B{i}"), 600);
        h.seed_pair(chat, PairSeed::completed(&q, &a).at(100 + i * 10))
            .await;
        texts.push(("user", q));
        texts.push(("assistant", a));
    }
    let summary_text = "s".repeat(1000);
    h.seed_summarized_up_to(chat, first[0].assistant_message.unwrap(), &summary_text)
        .await;

    let mandatory = est_bytes(SYSTEM_PROMPT.len()) + est_bytes("now".len());
    let remaining = BUDGET - mandatory;
    let summary_item = est_bytes(PREAMBLE.len() + 2 + summary_text.len());
    let without = kept(&texts, remaining);
    let with = kept(&texts, remaining - summary_item);
    assert!(
        with.len() < without.len(),
        "precondition: the summary takes room from the history ({} / {})",
        with.len(),
        without.len()
    );

    h.say_ok(chat, "now").await;

    let items = h.oagw.last_request().conversation_texts();
    assert!(is_summary_item(&items[0]), "the summary is kept first");
    assert_eq!(
        items[1..items.len() - 1],
        with,
        "the history is fitted into what the summary leaves"
    );
    assert_eq!(items.last().map(String::as_str), Some("user:now"));
}

#[tokio::test]
async fn the_preamble_counts_toward_the_size_of_the_summary() {
    // nothing follows the frontier; 1668 tokens remain after the system prompt and the message
    let remaining = BUDGET - est_bytes(SYSTEM_PROMPT.len()) - est_bytes("now".len());
    let largest = (1..20_000_usize)
        .take_while(|b| est_bytes(*b) <= remaining)
        .last()
        .unwrap();
    // `largest` bytes alone fit, but not with the preamble in front; 200 bytes less do
    for (bytes, applied) in [(largest, false), (largest - 200, true)] {
        let h = SummaryHarness::new().await;
        let chat = h.seed_chat().await;
        let pairs = h.seed_history(chat, 1).await;
        let frontier = h.frontier_of(pairs[0].assistant_message.unwrap()).await;
        h.seed_summary_text(
            chat,
            frontier.created_at,
            frontier.message_id,
            &"p".repeat(bytes),
            10,
        )
        .await;

        let sent = h.say_ok(chat, "now").await;

        let items = h.oagw.last_request().conversation_texts();
        assert_eq!(
            SummaryHarness::summary_applied(&sent).is_some(),
            applied,
            "{bytes} bytes of summary text"
        );
        assert_eq!(items.iter().any(|i| is_summary_item(i)), applied);
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// retry and edit
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn after_a_retry_or_edit_the_context_is_the_summary_and_the_replacement_turn() {
    for op in [Op::Retry, Op::Edit] {
        let h = SummaryHarness::new().await;
        let chat = h.seed_chat().await;
        let pairs = h.seed_history(chat, 3).await;
        // the summary covers turns 1 and 2, not the mutated turn 3
        h.seed_summarized_up_to(
            chat,
            pairs[1].assistant_message.unwrap(),
            "COVERS-ONE-AND-TWO",
        )
        .await;

        h.mutate(op, chat, pairs[2].request_id).await;

        let items = h.oagw.last_request().conversation_texts();
        assert_eq!(items.len(), 2, "{op:?}: {items:?}");
        assert!(is_summary_item(&items[0]), "{op:?}");
        assert_eq!(summary_text_of(&items[0]), "COVERS-ONE-AND-TWO");
        let expected = if op == Op::Retry {
            "user:question 3"
        } else {
            "user:replacement text"
        };
        assert_eq!(items[1], expected, "{op:?}");
        assert_eq!(
            h.summaries_of(chat).await.len(),
            1,
            "{op:?}: the summary is kept"
        );
        let _ = item_text(&items[1]);
    }
}
