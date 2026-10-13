//! Context assembly of a turn as seen in the provider request: recent messages
//! limit, deterministic `(created_at, id)` order, soft-deleted / foreign rows
//! excluded, truncation of the oldest whole turns under the model's budget.
//!
//! Source of truth: DESIGN 4 "Context Plan Assembly and Truncation" (order,
//! truncation priority, algorithm), "ContextPlan Determinism and Snapshot
//! Boundary" (stable ordering, recent messages query, `K =
//! context.recent_messages_limit`), 2.2 "Context Window Budget" and 5.5.4
//! (estimation). The pure functions are unit tested in `context_plan.rs`; this
//! file pins how the stream service applies them.

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

use mini_chat::domain::context::{ToolSet, estimate_tokens, token_budget};
use serde_json::{Value, json};
use stream_support::{
    MessageSeed, SYSTEM_PROMPT, StreamHarness, StreamOptions, WEB_GUARD, model_entries,
    premium_with,
};
use uuid::Uuid;

/// `role:first two characters` of every conversation item of the last request.
fn shape(h: &StreamHarness) -> Vec<String> {
    h.oagw
        .last_request()
        .conversation()
        .into_iter()
        .map(|(role, text)| format!("{role}:{}", text.chars().take(2).collect::<String>()))
        .collect()
}

// ── the recent messages window ───────────────────────────────────────────────

#[tokio::test]
async fn the_last_ten_messages_are_sent_by_default() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_history(chat, 7).await; // 14 messages
    h.say(chat, "current").await;
    let conversation = h.oagw.last_request().conversation_texts();
    assert_eq!(
        conversation.len(),
        11,
        "ten recent messages and the current one"
    );
    assert_eq!(conversation[0], "user:question 3");
    assert_eq!(conversation[9], "assistant:answer 7");
    assert_eq!(conversation[10], "user:current");
}

#[tokio::test]
async fn the_limit_comes_from_the_context_configuration() {
    for (limit, expected) in [
        (
            4,
            vec![
                "user:question 6",
                "assistant:answer 6",
                "user:question 7",
                "assistant:answer 7",
            ],
        ),
        (2, vec!["user:question 7", "assistant:answer 7"]),
        (0, vec![]),
    ] {
        let h = StreamHarness::with(StreamOptions {
            config: json!({"context": {"recent_messages_limit": limit}}),
            ..Default::default()
        })
        .await;
        let chat = h.seed_chat().await;
        h.seed_history(chat, 7).await;
        h.say(chat, "current").await;
        let mut want: Vec<String> = expected.into_iter().map(str::to_owned).collect();
        want.push("user:current".to_owned());
        assert_eq!(
            h.oagw.last_request().conversation_texts(),
            want,
            "limit {limit}"
        );
    }
}

#[tokio::test]
async fn messages_are_ordered_by_created_at_then_id() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    // three messages with the same timestamp, inserted out of id order
    for (n, role, text) in [
        (3, "user", "u-c"),
        (1, "user", "u-a"),
        (2, "assistant", "a-b"),
    ] {
        let seed = if role == "user" {
            MessageSeed::user(chat)
        } else {
            MessageSeed::assistant(chat)
        };
        h.chat
            .seed_message(
                seed.with_id(n)
                    .content(text)
                    .request(Some(Uuid::new_v4()))
                    .at(5),
            )
            .await;
    }
    h.say(chat, "current").await;
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec!["user:u-a", "assistant:a-b", "user:u-c", "user:current"]
    );
}

#[tokio::test]
async fn deleted_nameless_and_foreign_messages_are_not_part_of_the_context() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let other = h.seed_chat().await;
    h.chat
        .seed_message(MessageSeed::user(chat).content("kept question").at(1))
        .await;
    h.chat
        .seed_message(MessageSeed::assistant(chat).content("kept answer").at(2))
        .await;
    h.chat
        .seed_message(
            MessageSeed::user(chat)
                .content("deleted question")
                .at(3)
                .deleted(),
        )
        .await;
    h.chat
        .seed_message(
            MessageSeed::user(chat)
                .content("no request id")
                .request(None)
                .at(4),
        )
        .await;
    h.chat
        .seed_message(MessageSeed::user(other).content("another chat").at(5))
        .await;
    h.say(chat, "current").await;
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![
            "user:kept question",
            "assistant:kept answer",
            "user:current"
        ]
    );
}

#[tokio::test]
async fn the_same_history_yields_the_same_request_context() {
    let h = StreamHarness::new().await;
    let mut shapes = Vec::new();
    for _ in 0..2 {
        let chat = h.seed_chat().await;
        h.seed_history(chat, 5).await;
        h.say(chat, "current").await;
        shapes.push(h.oagw.last_request().conversation_texts());
    }
    assert_eq!(shapes[0], shapes[1]);
}

// ── truncation under a small context window ──────────────────────────────────

const HISTORY_MESSAGE_BYTES: usize = 400;

fn padded(label: &str) -> String {
    format!("{label}{}", "x".repeat(HISTORY_MESSAGE_BYTES - label.len()))
}

/// Window of a model whose input budget is exactly `mandatory + n * message`
/// (no separate input limit, output cap 100).
fn window_for(entry_json: &Value, budget_target: u64, tools: ToolSet) -> u32 {
    let mut value = entry_json.clone();
    for w in 200..100_000_u32 {
        value["context_window"] = json!(w);
        let entry = model_entries(&[value.clone()]).remove(0);
        if token_budget(&entry, 100, tools).is_some_and(|b| b >= budget_target) {
            return w;
        }
    }
    panic!("no window reaches the budget {budget_target}");
}

struct Sizes {
    mandatory: u64,
    message: u64,
}

fn sizes(content_len: usize, system: &str, current: &str) -> Sizes {
    let budgets = mini_chat_sdk::EstimationBudgets::default();
    Sizes {
        mandatory: estimate_tokens(system, &budgets) + estimate_tokens(current, &budgets),
        message: estimate_tokens(&"x".repeat(content_len), &budgets),
    }
}

async fn seed_three_turns(h: &StreamHarness, chat: Uuid) {
    for i in 1..=3_i64 {
        let rid = Uuid::new_v4();
        h.chat
            .seed_message(
                MessageSeed::user(chat)
                    .request(Some(rid))
                    .content(&padded(&format!("Q{i}")))
                    .at(i * 2),
            )
            .await;
        h.chat
            .seed_message(
                MessageSeed::assistant(chat)
                    .request(Some(rid))
                    .content(&padded(&format!("A{i}")))
                    .at(i * 2 + 1),
            )
            .await;
    }
}

fn small_model(window: u32) -> Vec<Value> {
    premium_with(json!({"context_window": window, "max_output_tokens": 100, "max_input_tokens": 0}))
}

#[tokio::test]
async fn the_oldest_whole_turns_are_dropped_when_the_budget_is_exceeded() {
    let base = premium_with(json!({"max_output_tokens": 100, "max_input_tokens": 0}));
    let s = sizes(HISTORY_MESSAGE_BYTES, SYSTEM_PROMPT, "now");

    // room for exactly three history messages: A2 U3 A3 fit, the leading A2 is dropped as well
    let window = window_for(&base[0], s.mandatory + 3 * s.message, ToolSet::default());
    let h = StreamHarness::with(StreamOptions {
        models: small_model(window),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    seed_three_turns(&h, chat).await;
    let sent = h.say(chat, "now").await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(shape(&h), vec!["user:Q3", "assistant:A3", "user:no"]);
    let req = h.oagw.last_request();
    assert!(
        req.system_text().contains(SYSTEM_PROMPT),
        "the system prompt is never dropped"
    );

    // nothing is deleted from the conversation itself
    assert_eq!(h.messages_in(chat).await.len(), 6 + 2);
    assert_eq!(h.listed_messages(chat).await.len(), 8);
}

#[tokio::test]
async fn a_larger_budget_keeps_more_turns() {
    let base = premium_with(json!({"max_output_tokens": 100, "max_input_tokens": 0}));
    let s = sizes(HISTORY_MESSAGE_BYTES, SYSTEM_PROMPT, "now");

    let four = window_for(&base[0], s.mandatory + 4 * s.message, ToolSet::default());
    let h = StreamHarness::with(StreamOptions {
        models: small_model(four),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    seed_three_turns(&h, chat).await;
    h.say(chat, "now").await;
    assert_eq!(
        shape(&h),
        vec![
            "user:Q2",
            "assistant:A2",
            "user:Q3",
            "assistant:A3",
            "user:no"
        ]
    );

    let all = window_for(&base[0], s.mandatory + 6 * s.message, ToolSet::default());
    let h = StreamHarness::with(StreamOptions {
        models: small_model(all),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    seed_three_turns(&h, chat).await;
    h.say(chat, "now").await;
    assert_eq!(shape(&h).len(), 7, "everything fits");
}

#[tokio::test]
async fn only_the_mandatory_items_are_sent_when_nothing_else_fits() {
    let base = premium_with(json!({"max_output_tokens": 100, "max_input_tokens": 0}));
    let s = sizes(HISTORY_MESSAGE_BYTES, SYSTEM_PROMPT, "now");
    let window = window_for(&base[0], s.mandatory, ToolSet::default());
    let h = StreamHarness::with(StreamOptions {
        models: small_model(window),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    seed_three_turns(&h, chat).await;
    let sent = h.say(chat, "now").await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(shape(&h), vec!["user:no"]);
    assert!(h.oagw.last_request().system_text().contains(SYSTEM_PROMPT));
}

#[tokio::test]
async fn a_tool_in_the_request_shrinks_the_budget_by_its_surcharge_and_guard() {
    let base = premium_with(json!({"max_output_tokens": 100, "max_input_tokens": 0}));
    let s = sizes(HISTORY_MESSAGE_BYTES, SYSTEM_PROMPT, "now");
    let window = window_for(&base[0], s.mandatory + 3 * s.message, ToolSet::default());

    let h = StreamHarness::with(StreamOptions {
        models: small_model(window),
        ..Default::default()
    })
    .await;
    let plain_chat = h.seed_chat().await;
    seed_three_turns(&h, plain_chat).await;
    h.say(plain_chat, "now").await;
    let without = h.oagw.last_request().conversation().len();

    let web_chat = h.seed_chat().await;
    seed_three_turns(&h, web_chat).await;
    let sent = h
        .send(
            web_chat,
            &json!({"content": "now", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(sent.terminal().event, "done");
    let req = h.oagw.last_request();
    assert!(req.system_text().contains(WEB_GUARD));
    assert!(
        req.conversation().len() < without,
        "the web_search surcharge (500 tokens) pushes older turns out: {} vs {without}",
        req.conversation().len()
    );
}

#[tokio::test]
async fn a_model_without_tool_support_pays_no_surcharge_for_a_requested_tool() {
    let mut standard = stream_support::stream_model_json("gpt-standard", "Standard");
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    standard["max_output_tokens"] = json!(100);
    standard["max_input_tokens"] = json!(0);
    let s = sizes(HISTORY_MESSAGE_BYTES, SYSTEM_PROMPT, "now");
    let window = window_for(&standard, s.mandatory + 3 * s.message, ToolSet::default());
    standard["context_window"] = json!(window);

    let h = StreamHarness::with(StreamOptions {
        models: vec![standard],
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat_on("gpt-standard").await;
    seed_three_turns(&h, chat).await;
    h.send(
        chat,
        &json!({"content": "now", "web_search": {"enabled": true}}),
    )
    .await;
    assert_eq!(
        shape(&h),
        vec!["user:Q3", "assistant:A3", "user:no"],
        "the tool is not sent, so nothing is deducted"
    );
}
