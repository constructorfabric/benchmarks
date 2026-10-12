//! Thread-summary worker (slice S7): the outbox handler that generates, stores and commits one
//! summary. Messages are handed to the real handler directly (`MessageResult` is asserted), the
//! summary provider is scripted (`h.summaries`), the commit is read back from SQLite.
//!
//! Source of truth: DESIGN 3.6 "Thread Summary Update" (sequence, "Execution stages and
//! invariants": range selection, request format, summary model resolution, response parsing,
//! `token_estimate`, prompt fitting, context-length retry, CAS-protected commit, retry / `Reject`
//! after `thread_summary_worker.max_attempts`, observability result names), "Thread Summary -
//! Stable Range and Commit Invariant", "Summary Interaction on Turn Mutation" (the commit
//! transaction skips a deleted target frontier), 3.2 "System Task Isolation Invariant" and "System
//! Task Attribution Rules" (usage event of a system task, `dedupe_key`), 4 "Provider Request
//! Metadata", B.5.5 (prompt texts), B.9.3 (lease), B.9.4 (keys and defaults), 3.7
//! `thread_summaries` / `messages.is_compressed`.

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
use std::time::Duration;

use mini_chat::infra::outbox::thread_summary_lease;
use mini_chat_sdk::{
    RequesterType, SettlementMethod, SystemTaskType, TerminalState, UsageTokens,
    system_task_dedupe_key,
};
use serde_json::{Value, json};
use stream_support::{Opts, Piece, ProviderReply, ev, failed_stream, provider_model};
use summary_support::{
    CLOSING, DEFAULT_SUMMARY_MODEL, DEFAULT_SUMMARY_PROMPT_START, MessageSeed, OPENING_FRESH,
    OPENING_MERGE, Op, Pair, PairSeed, SUMMARY_MAX_OUTPUT, SUMMARY_MODEL, SYSTEM_SUBJECT,
    SummaryHarness, SummaryOptions, SummaryRequestView, TENANT_A, TENANT_B, TaskSpec, handle_with,
    hex, is_ok, is_reject, is_retry, patch_model, plain_reply, prompt_entries, summary_model_json,
    summary_models, summary_reply, summary_reply_with, ts, uid,
};
use uuid::Uuid;

struct Fixture {
    h: SummaryHarness,
    chat: Uuid,
    pairs: Vec<Pair>,
}

impl Fixture {
    /// `n` completed turns `question i` / `answer i` (user at `10 i` s, answer one second later).
    async fn new(n: usize) -> Self {
        Self::on(SummaryHarness::new().await, n).await
    }

    async fn on(h: SummaryHarness, n: usize) -> Self {
        let chat = h.seed_chat().await;
        let pairs = h.seed_history(chat, n).await;
        Self { h, chat, pairs }
    }

    /// A task that summarizes up to the answer of turn `n` (1-based) with no base.
    async fn task_to(&self, n: usize) -> TaskSpec {
        TaskSpec::new(
            self.chat,
            self.h
                .frontier_of(self.pairs[n - 1].assistant_message.unwrap())
                .await,
        )
    }

    fn answer(&self, n: usize) -> Uuid {
        self.pairs[n - 1].assistant_message.unwrap()
    }

    fn question(&self, n: usize) -> Uuid {
        self.pairs[n - 1].user_message
    }

    /// Everything the worker must leave alone when it does not commit.
    async fn assert_untouched(&self, before: &Value, what: &str) {
        assert_eq!(
            &self.h.fingerprint().await,
            before,
            "{what}: nothing changed"
        );
        assert!(self.h.system_usage().is_empty(), "{what}: no usage event");
    }
}

fn texts(entries: &[String]) -> Vec<&str> {
    entries.iter().map(String::as_str).collect()
}

// ═════════════════════════════════════════════════════════════════════════════
// the commit
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_summary_is_stored_with_its_frontier_and_the_range_is_compressed() {
    let f = Fixture::new(3).await;
    f.h.summaries.push(summary_reply_with(
        "The user asked about X.",
        &Opts::default().usage(500, 80).details(20, 30),
    ));
    let task = f.task_to(2).await;

    let result = f.h.deliver_task(&task).await;

    assert!(is_ok(&result), "{result:?}");
    let row = f.h.summary_of(f.chat).await;
    assert_eq!(row.tenant_id, TENANT_A);
    assert_eq!(row.chat_id, f.chat);
    assert_eq!(
        row.summary_text.as_deref().map(str::trim),
        Some("The user asked about X.")
    );
    assert_eq!(row.summarized_up_to_message_id, f.answer(2));
    assert_eq!(
        chat_support::from_db(row.summarized_up_to_created_at),
        ts(21)
    );
    assert_eq!(row.token_estimate, Some(50), "output 80 minus reasoning 30");
    let flags = f.h.compressed_flags(f.chat).await;
    let compressed: BTreeSet<Uuid> = flags
        .iter()
        .filter(|(_, c)| *c)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(
        compressed,
        BTreeSet::from([f.question(1), f.answer(1), f.question(2), f.answer(2)]),
        "exactly the summarized range is compressed: {flags:?}"
    );
    assert_eq!(f.h.summaries.request_count(), 1);
}

#[tokio::test]
async fn the_summary_request_goes_to_the_summary_model_with_the_system_identity() {
    let f = Fixture::new(2).await;
    let task = f.task_to(2).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    let request = f.h.last_summary_request();
    assert_eq!(request.method, http::Method::POST);
    assert_eq!(request.path(), "/provider-alias/v1/responses");
    assert_eq!(request.model(), provider_model(SUMMARY_MODEL));
    assert_eq!(request.tenant_id, TENANT_A, "the chat's tenant");
    assert!(
        request.tools().is_empty(),
        "no tools: {:?}",
        request.tools()
    );
    assert!(request.is_summary());
    let meta = request.metadata();
    assert_eq!(meta["request_type"], json!("summary"));
    assert_eq!(meta["feature"], json!("none"));
    assert_eq!(meta["chat_id"], json!(f.chat));
    assert_eq!(meta["tenant_id"], json!(TENANT_A));
    assert_eq!(meta["user_id"], json!(SYSTEM_SUBJECT));
    let user = request.body["user"].as_str().unwrap();
    assert_eq!(user, format!("{}{}", hex(TENANT_A), hex(SYSTEM_SUBJECT)));
    assert_eq!(user.len(), 64);
    assert_eq!(
        f.h.chat_requests().len(),
        0,
        "nothing is sent to the provider of the chat model"
    );
}

#[tokio::test]
async fn the_output_budget_is_the_catalog_maximum_of_the_summary_model() {
    // the chat turns are capped at 1024 output tokens; the summary request is not
    let f = Fixture::on(
        SummaryHarness::with_config(json!({"streaming": {"max_output_tokens": 1024}})).await,
        2,
    )
    .await;
    let task = f.task_to(2).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    assert_eq!(
        f.h.last_summary_request().body["max_output_tokens"],
        json!(SUMMARY_MAX_OUTPUT)
    );
}

#[tokio::test]
async fn the_summary_does_not_depend_on_the_model_of_the_chat() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat_on(summary_support::STANDARD).await;
    let pairs = h.seed_history(chat, 2).await;
    let task = TaskSpec::new(
        chat,
        h.frontier_of(pairs[1].assistant_message.unwrap()).await,
    );

    assert!(is_ok(&h.deliver_task(&task).await));

    assert_eq!(
        h.last_summary_request().model(),
        provider_model(SUMMARY_MODEL)
    );
    assert_eq!(
        h.wait_system_usage(1).await[0].effective_model,
        SUMMARY_MODEL
    );
}

#[tokio::test]
async fn an_empty_summary_model_id_means_the_built_in_default_model() {
    let mut models = summary_models();
    models.push(summary_model_json(DEFAULT_SUMMARY_MODEL));
    let h = SummaryHarness::with(
        SummaryOptions::default()
            .with_models(models)
            .with_config(json!({"thread_summary_worker": {"summary_model_id": ""}})),
    )
    .await;
    let f = Fixture::on(h, 2).await;
    let task = f.task_to(2).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    assert_eq!(
        f.h.last_summary_request().model(),
        provider_model(DEFAULT_SUMMARY_MODEL)
    );
    f.h.summary_of(f.chat).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// the prompt
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_system_prompt_is_catalog_then_configuration_then_built_in() {
    // 1. the catalog entry of the summary model wins
    let mut models = summary_models();
    patch_model(
        &mut models,
        SUMMARY_MODEL,
        json!({"thread_summary_prompt": "CATALOG-SUMMARY-PROMPT"}),
    );
    let h = SummaryHarness::with(SummaryOptions::default().with_models(models).with_config(
        json!({"thread_summary_worker": {"summary_system_prompt": "CONFIG-SUMMARY-PROMPT"}}),
    ))
    .await;
    let f = Fixture::on(h, 2).await;
    assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));
    assert_eq!(
        f.h.last_summary_request().system_text(),
        "CATALOG-SUMMARY-PROMPT"
    );

    // 2. the configured prompt when the catalog one is empty
    let f = Fixture::on(
        SummaryHarness::with_config(
            json!({"thread_summary_worker": {"summary_system_prompt": "CONFIG-SUMMARY-PROMPT"}}),
        )
        .await,
        2,
    )
    .await;
    assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));
    assert_eq!(
        f.h.last_summary_request().system_text(),
        "CONFIG-SUMMARY-PROMPT"
    );

    // 3. the built-in text (the default of the configuration key) otherwise
    let f = Fixture::new(2).await;
    assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));
    let system = f.h.last_summary_request().system_text();
    assert!(system.starts_with(DEFAULT_SUMMARY_PROMPT_START), "{system}");
    assert_eq!(
        system,
        f.h.config.thread_summary_worker.summary_system_prompt
    );

    // 4. an emptied configuration key falls back to the built-in text as well
    let f = Fixture::on(
        SummaryHarness::with_config(
            json!({"thread_summary_worker": {"summary_system_prompt": ""}}),
        )
        .await,
        2,
    )
    .await;
    assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));
    assert!(
        f.h.last_summary_request()
            .system_text()
            .starts_with(DEFAULT_SUMMARY_PROMPT_START)
    );
}

#[tokio::test]
async fn the_first_summary_prompt_lists_the_messages_in_order() {
    let f = Fixture::new(3).await;
    let task = f.task_to(3).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    let prompt = f.h.last_summary_request().prompt();
    assert!(prompt.starts_with(OPENING_FRESH), "{prompt}");
    assert!(prompt.trim_end().ends_with(CLOSING), "{prompt}");
    assert!(!prompt.contains("<existing_summary>"), "{prompt}");
    assert_eq!(
        texts(&prompt_entries(&prompt)),
        [
            "User: question 1",
            "Assistant: answer 1",
            "User: question 2",
            "Assistant: answer 2",
            "User: question 3",
            "Assistant: answer 3"
        ]
    );
    assert!(
        prompt.contains("User: question 1\n\nAssistant: answer 1\n\nUser: question 2"),
        "entries are separated by a blank line: {prompt}"
    );
    // the analysis instruction asks for the two blocks and the fixed sections
    for needle in [
        "<analysis>",
        "<summary>",
        "Conversation Purpose",
        "Current Topic",
    ] {
        assert!(prompt.contains(needle), "{needle} in {prompt}");
    }
    let first_entry = prompt.find("User: question 1").unwrap();
    assert!(
        prompt.find("Conversation Purpose").unwrap() > first_entry,
        "the instruction follows the messages"
    );
}

#[tokio::test]
async fn an_update_prompt_carries_the_existing_summary_and_only_the_new_messages() {
    let f = Fixture::new(4).await;
    f.h.seed_summarized_up_to(f.chat, f.answer(2), "EARLIER-SUMMARY-TEXT")
        .await;
    let base = f.h.frontier_of(f.answer(2)).await;
    let task = f.task_to(4).await.based_on(base);
    f.h.summaries.push(summary_reply("MERGED-SUMMARY-TEXT"));

    assert!(is_ok(&f.h.deliver_task(&task).await));

    let prompt = f.h.last_summary_request().prompt();
    assert!(prompt.starts_with(OPENING_MERGE), "{prompt}");
    let at = |needle: &str| {
        prompt
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from {prompt}"))
    };
    assert!(at(OPENING_MERGE) < at("<existing_summary>"));
    assert!(at("<existing_summary>") < at("EARLIER-SUMMARY-TEXT"));
    assert!(at("EARLIER-SUMMARY-TEXT") < at("</existing_summary>"));
    assert!(at("</existing_summary>") < at("New messages to incorporate:"));
    assert!(at("New messages to incorporate:") < at("User: question 3"));
    assert_eq!(
        texts(&prompt_entries(&prompt)),
        [
            "User: question 3",
            "Assistant: answer 3",
            "User: question 4",
            "Assistant: answer 4"
        ],
        "the messages the summary already covers are not sent again"
    );
    assert!(prompt.trim_end().ends_with(CLOSING));

    let row = f.h.summary_of(f.chat).await;
    assert_eq!(
        row.summary_text.as_deref().map(str::trim),
        Some("MERGED-SUMMARY-TEXT")
    );
    assert_eq!(row.summarized_up_to_message_id, f.answer(4));
    assert_eq!(
        f.h.compressed_ids(f.chat).await.len(),
        8,
        "the old range stays compressed and the new range joins it"
    );
}

#[tokio::test]
async fn the_range_is_the_live_uncompressed_messages_between_the_frontiers() {
    let f = Fixture::new(5).await;
    let chat = f.chat;
    // a stray compressed message and a deleted one inside the range, a system message, a message
    // of another chat and the messages after the target
    f.h.mark_compressed(&[f.question(2)]).await;
    f.h.soft_delete_messages(&[f.answer(3)]).await;
    let system =
        f.h.chat
            .seed_message(
                MessageSeed::system(chat)
                    .content("system note")
                    .at(32)
                    .request(None),
            )
            .await;
    let other = f.h.seed_chat().await;
    let foreign =
        f.h.chat
            .seed_message(MessageSeed::user(other).content("another chat").at(15))
            .await;
    let task = f.task_to(4).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    assert_eq!(
        texts(&prompt_entries(&f.h.last_summary_request().prompt())),
        [
            "User: question 1",
            "Assistant: answer 1",
            "Assistant: answer 2",
            "User: question 3",
            "User: question 4",
            "Assistant: answer 4"
        ]
    );
    let compressed: BTreeSet<Uuid> = f.h.compressed_ids(chat).await.into_iter().collect();
    assert_eq!(
        compressed,
        BTreeSet::from([
            f.question(1),
            f.answer(1),
            f.question(2), // was flagged before
            f.answer(2),
            f.question(3),
            f.question(4),
            f.answer(4)
        ]),
        "the deleted message, the system message, the foreign message and the later turn stay as they are"
    );
    assert!(!compressed.contains(&system) && !compressed.contains(&foreign));
}

#[tokio::test]
async fn messages_are_ordered_by_created_at_then_id_and_the_target_is_inclusive() {
    let h = SummaryHarness::new().await;
    let chat = h.seed_chat().await;
    // four messages at the same instant, inserted out of id order; the target is the third id
    for (n, role, text) in [
        (3_u128, "user", "third"),
        (1, "user", "first"),
        (4, "user", "fourth"),
        (2, "assistant", "second"),
    ] {
        let seed = if role == "user" {
            MessageSeed::user(chat)
        } else {
            MessageSeed::assistant(chat)
        };
        h.chat
            .seed_message(seed.with_id(n).content(text).at(10))
            .await;
    }
    let task = TaskSpec::new(chat, summary_support::Frontier::at(ts(10), uid(3)));

    assert!(is_ok(&h.deliver_task(&task).await));

    assert_eq!(
        texts(&prompt_entries(&h.last_summary_request().prompt())),
        ["User: first", "Assistant: second", "User: third"],
        "`(created_at, id)` order; the message after the target id is outside the range"
    );
    let compressed: BTreeSet<Uuid> = h.compressed_ids(chat).await.into_iter().collect();
    assert_eq!(compressed, BTreeSet::from([uid(1), uid(2), uid(3)]));
    assert_eq!(h.summary_of(chat).await.summarized_up_to_message_id, uid(3));
}

#[tokio::test]
async fn message_content_is_cut_to_the_configured_limit() {
    type Check = Box<dyn Fn(&str)>;
    let cases: Vec<(Value, String, Check)> = vec![
        (
            // the default limit is 4000 characters
            json!({}),
            "x".repeat(5000),
            Box::new(|prompt: &str| {
                assert!(
                    prompt.contains(&format!("{}...", "x".repeat(3997))),
                    "cut and marked"
                );
                assert!(
                    !prompt.contains(&"x".repeat(4001)),
                    "at most 4000 characters"
                );
            }),
        ),
        (
            // 0 = no truncation
            json!({"thread_summary_worker": {"message_content_limit": 0}}),
            "x".repeat(5000),
            Box::new(|prompt: &str| {
                assert!(prompt.contains(&"x".repeat(5000)));
                assert!(!prompt.contains("..."));
            }),
        ),
        (
            // the limit counts characters, not bytes
            json!({"thread_summary_worker": {"message_content_limit": 10}}),
            "\u{e9}".repeat(20),
            Box::new(|prompt: &str| {
                assert!(prompt.contains(&format!("{}...", "\u{e9}".repeat(7))));
                assert!(!prompt.contains(&"\u{e9}".repeat(11)));
                assert!(
                    prompt.contains("Assistant: abcdefghij\n"),
                    "exactly at the limit is kept whole"
                );
            }),
        ),
    ];
    for (config, content, check) in cases {
        let h = SummaryHarness::with_config(config.clone()).await;
        let chat = h.seed_chat().await;
        let long = h
            .seed_pair(chat, PairSeed::completed(&content, "abcdefghij").at(10))
            .await;
        let task = TaskSpec::new(chat, h.frontier_of(long.assistant_message.unwrap()).await);
        assert!(is_ok(&h.deliver_task(&task).await), "{config}");
        check(&h.last_summary_request().prompt());
    }
}

// ── fitting the prompt to the input budget of the summary model ──────────────

/// The summary model with `window` / `max_input` set; `n` turns of 600-byte messages.
async fn fitting(window: u64, max_input: u64, n: usize) -> Fixture {
    let mut models = summary_models();
    patch_model(
        &mut models,
        SUMMARY_MODEL,
        json!({"context_window": window, "max_input_tokens": max_input}),
    );
    let h = SummaryHarness::with(SummaryOptions::default().with_models(models)).await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_sized(chat, n, 600).await;
    Fixture { h, chat, pairs }
}

fn entry_labels(prompt: &str) -> Vec<String> {
    prompt_entries(prompt)
        .iter()
        .map(|e| {
            let body = e.split_once(": ").unwrap().1;
            body.chars().take(2).collect()
        })
        .collect()
}

#[tokio::test]
async fn the_oldest_messages_are_dropped_until_the_prompt_fits() {
    // input budget = 3000 - 2048 = 952 tokens = 3808 bytes; ten messages of ~600 bytes do not fit
    let f = fitting(3000, 0, 5).await;
    let task = f.task_to(5).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    let request = f.h.last_summary_request();
    let prompt = request.prompt();
    let labels = entry_labels(&prompt);
    assert!(labels.len() >= 2 && labels.len() < 10, "{labels:?}");
    assert_eq!(
        labels.last().map(String::as_str),
        Some("A5"),
        "the newest message is kept"
    );
    assert!(
        !labels.contains(&"Q1".to_owned()),
        "the oldest message is dropped: {labels:?}"
    );
    let all = ["Q1", "A1", "Q2", "A2", "Q3", "A3", "Q4", "A4", "Q5", "A5"];
    assert_eq!(
        labels,
        all[all.len() - labels.len()..],
        "what is kept is a suffix of the range"
    );
    let bytes = request.system_text().len() + prompt.len();
    assert!(
        bytes.div_ceil(4) <= 952,
        "the prompt fits the budget: {bytes} bytes"
    );
}

#[tokio::test]
async fn at_least_two_messages_are_kept_whatever_the_budget() {
    // budget = 2100 - 2048 = 52 tokens: nothing fits
    let f = fitting(2100, 0, 5).await;
    let task = f.task_to(5).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    assert_eq!(
        entry_labels(&f.h.last_summary_request().prompt()),
        ["Q5", "A5"]
    );
    assert_eq!(f.h.summaries.request_count(), 1);
}

#[tokio::test]
async fn max_input_tokens_caps_the_budget_and_a_zero_window_disables_fitting() {
    // budget = min(100000 - 2048, 1000) = 1000 tokens
    let f = fitting(100_000, 1000, 5).await;
    assert!(is_ok(&f.h.deliver_task(&f.task_to(5).await).await));
    let kept = entry_labels(&f.h.last_summary_request().prompt()).len();
    assert!((2..10).contains(&kept), "{kept}");

    // a window of 0 means "unknown": nothing is fitted
    let f = fitting(0, 0, 5).await;
    assert!(is_ok(&f.h.deliver_task(&f.task_to(5).await).await));
    assert_eq!(entry_labels(&f.h.last_summary_request().prompt()).len(), 10);

    // a large window keeps everything as well
    let f = fitting(128_000, 100_000, 5).await;
    assert!(is_ok(&f.h.deliver_task(&f.task_to(5).await).await));
    assert_eq!(entry_labels(&f.h.last_summary_request().prompt()).len(), 10);
}

fn context_length_error() -> ProviderReply {
    ProviderReply::http_body(
        400,
        json!({"error": {
            "message": "This model's maximum context length is 8192 tokens. Your messages resulted in 9000 tokens.",
            "type": "invalid_request_error",
            "code": "context_length_exceeded"
        }}),
    )
}

#[tokio::test]
async fn a_context_length_error_retries_without_the_oldest_messages() {
    let f = Fixture::on(SummaryHarness::new().await, 0).await;
    let pairs = f.h.seed_sized(f.chat, 5, 100).await;
    let task = TaskSpec::new(
        f.chat,
        f.h.frontier_of(pairs[4].assistant_message.unwrap()).await,
    );
    f.h.summaries.push(context_length_error());

    let result = f.h.deliver_task(&task).await;

    assert!(is_ok(&result), "{result:?}");
    let requests = f.h.summary_requests();
    assert_eq!(requests.len(), 2, "one retry inside the same delivery");
    let first = entry_labels(&requests[0].prompt());
    let second = entry_labels(&requests[1].prompt());
    assert_eq!(first.len(), 10);
    assert!(
        second.len() < first.len() && second.len() >= 2,
        "{second:?}"
    );
    assert_eq!(
        second,
        first[first.len() - second.len()..],
        "the oldest are dropped"
    );
    assert_eq!(second.last().map(String::as_str), Some("A5"));
    f.h.summary_of(f.chat).await;
}

#[tokio::test]
async fn context_length_retries_stop_after_two() {
    let f = Fixture::on(SummaryHarness::new().await, 0).await;
    let pairs = f.h.seed_sized(f.chat, 5, 100).await;
    let task = TaskSpec::new(
        f.chat,
        f.h.frontier_of(pairs[4].assistant_message.unwrap()).await,
    );
    for _ in 0..4 {
        f.h.summaries.push(context_length_error());
    }
    let before = f.h.fingerprint().await;

    let result = f.h.deliver_task(&task).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(f.h.summaries.request_count(), 3, "the call and two retries");
    f.assert_untouched(&before, "a context that never fits")
        .await;
}

// ═════════════════════════════════════════════════════════════════════════════
// the answer
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_summary_block_is_extracted_from_the_answer() {
    let cases: [(&str, &str); 6] = [
        (
            "<analysis>thinking it over</analysis>\n<summary>\nFinal text\n</summary>",
            "Final text",
        ),
        ("<summary>Only the summary</summary>", "Only the summary"),
        (
            "Preface <analysis>x</analysis> <summary>Inside</summary> trailing words",
            "Inside",
        ),
        (
            "Plain summary without any tags.",
            "Plain summary without any tags.",
        ),
        (
            "<analysis>a\nb</analysis>\nWhat is left after the analysis",
            "What is left after the analysis",
        ),
        (
            "<analysis>x</analysis><summary>Para one.\n\n\n\n\nPara two.</summary>",
            "Para one.\n\nPara two.",
        ),
    ];
    for (answer, expected) in cases {
        let f = Fixture::new(2).await;
        f.h.summaries.push(plain_reply(answer));

        assert!(
            is_ok(&f.h.deliver_task(&f.task_to(2).await).await),
            "{answer}"
        );

        let stored = f.h.summary_of(f.chat).await.summary_text.unwrap();
        assert_eq!(stored.trim(), expected, "for {answer:?}");
        assert!(
            !stored.contains("\n\n\n"),
            "runs of blank lines are collapsed: {stored:?}"
        );
    }
}

#[tokio::test]
async fn the_answer_is_collected_from_every_delta() {
    let f = Fixture::new(2).await;
    f.h.summaries.push(ProviderReply::text_with(
        &[
            "<analysis>a</analysis><sum",
            "mary>part one, ",
            "part two</summ",
            "ary>",
        ],
        &Opts::default(),
    ));

    assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));

    assert_eq!(
        f.h.summary_of(f.chat).await.summary_text.unwrap().trim(),
        "part one, part two"
    );
}

#[tokio::test]
async fn an_answer_without_usable_text_is_an_empty_summary() {
    let empty: [&str; 8] = [
        "",
        "   \n  ",
        "<summary></summary>",
        "<summary>  \n </summary>",
        "<analysis>only an analysis</analysis>",
        "<summary>never closed",
        "<analysis>never closed",
        "No block, but a stray <summary tag",
    ];
    for answer in empty {
        let f = Fixture::new(2).await;
        f.h.summaries.push(plain_reply(answer));
        let before = f.h.fingerprint().await;

        let first = f.h.deliver(&f.task_to(2).await.json(), 0).await;
        assert!(is_retry(&first), "{answer:?}: {first:?}");
        f.assert_untouched(&before, answer).await;

        // the same bound as every other failure: the last delivery rejects
        f.h.summaries.push(plain_reply(answer));
        let last = f.h.deliver(&f.task_to(2).await.json(), 2).await;
        assert!(is_reject(&last), "{answer:?}: {last:?}");
        f.assert_untouched(&before, answer).await;
    }
}

#[tokio::test]
async fn the_token_estimate_is_output_minus_reasoning_or_a_byte_estimate() {
    // (output tokens, reasoning tokens, stored text) -> token_estimate
    let text = "S".repeat(41);
    let cases = [
        (100_u64, 30_u64, 70_i32),
        (100, 0, 100),
        (20, 20, 11), // difference 0: ceil(41 / 4)
        (0, 0, 11),   // the provider reports nothing
        (10, 30, 11), // reasoning above output
    ];
    for (output, reasoning, expected) in cases {
        let f = Fixture::new(2).await;
        f.h.summaries.push(summary_reply_with(
            &text,
            &Opts::default().usage(300, output).details(0, reasoning),
        ));
        assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));
        assert_eq!(
            f.h.summary_of(f.chat).await.token_estimate,
            Some(expected),
            "output {output}, reasoning {reasoning}"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// the usage event of the system task
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_committed_summary_emits_one_system_task_usage_event() {
    let f = Fixture::new(3).await;
    f.h.summaries.push(summary_reply_with(
        "text",
        &Opts::default().usage(500, 80).details(20, 30),
    ));
    let task = f.task_to(2).await;

    assert!(is_ok(&f.h.deliver_task(&task).await));

    let events = f.h.wait_system_usage(1).await;
    f.h.assert_system_usage_stays(1).await;
    let event = &events[0];
    assert_eq!(event.tenant_id, TENANT_A);
    assert_eq!(event.chat_id, f.chat);
    assert_eq!(event.user_id, None, "no user is billed");
    assert_eq!(event.turn_id, None, "there is no turn");
    assert_eq!(
        event.request_id, task.system_request_id,
        "the stable system request id"
    );
    assert_eq!(event.effective_model, SUMMARY_MODEL);
    assert_eq!(event.selected_model, SUMMARY_MODEL);
    assert_eq!(event.terminal_state, TerminalState::Completed);
    assert_eq!(event.settlement_method, SettlementMethod::None);
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.requester_type, RequesterType::System);
    assert_eq!(
        event.system_task_type,
        Some(SystemTaskType::ThreadSummaryUpdate)
    );
    assert_eq!(
        event.usage,
        Some(UsageTokens {
            input_tokens: 500,
            output_tokens: 80,
            cache_read_input_tokens: 20,
            cache_write_input_tokens: 0,
            reasoning_tokens: 30,
        })
    );
    assert_eq!(
        event.dedupe_key,
        system_task_dedupe_key(
            TENANT_A,
            SystemTaskType::ThreadSummaryUpdate,
            task.system_request_id
        )
    );
    assert_eq!(
        event.dedupe_key,
        format!(
            "{}/thread_summary_update/{}",
            hex(TENANT_A),
            hex(task.system_request_id)
        )
    );
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (0, 0, 0)
    );
    let wire = serde_json::to_value(event).unwrap();
    assert!(
        wire.get("user_id").is_none() && wire.get("turn_id").is_none(),
        "{wire}"
    );
    assert_eq!(wire["requester_type"], json!("system"));
    assert_eq!(wire["billing_outcome"], json!("system_task"));
    assert_eq!(wire["system_task_type"], json!("thread_summary_update"));
}

#[tokio::test]
async fn a_summary_never_touches_turns_quota_messages_or_other_chats() {
    let f = Fixture::new(3).await;
    let other = f.h.seed_chat().await;
    let other_pairs = f.h.seed_history(other, 2).await;
    f.h.seed_summarized_up_to(
        other,
        other_pairs[0].assistant_message.unwrap(),
        "other chat summary",
    )
    .await;
    let turns = f.h.all_turns().await.len();
    let messages = f.h.chat.all_messages().await.len();
    let other_flags = f.h.compressed_flags(other).await;
    let other_row = f.h.summary_of(other).await;

    assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));
    f.h.wait_system_usage(1).await;

    assert_eq!(
        f.h.all_turns().await.len(),
        turns,
        "no chat_turns row for a system task"
    );
    assert!(
        f.h.usage_rows().await.is_empty(),
        "no per-user quota is debited"
    );
    assert_eq!(
        f.h.chat.all_messages().await.len(),
        messages,
        "no message is created"
    );
    assert!(
        f.h.audit.events().is_empty(),
        "system tasks emit no audit events in P1"
    );
    assert_eq!(f.h.compressed_flags(other).await, other_flags);
    let again = f.h.summary_of(other).await;
    assert_eq!(
        (again.id, again.summary_text),
        (other_row.id, other_row.summary_text)
    );
}

#[tokio::test]
async fn the_usage_message_is_part_of_the_commit_transaction() {
    let f = Fixture::new(3).await;
    // a usage queue that is not registered: the enqueue of the commit transaction fails
    let handler = f.h.handler_with_usage_queue("mini-chat.not_registered");
    let before = f.h.fingerprint().await;
    let task = f.task_to(2).await;

    let result = handle_with(&handler, &task.json(), 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(
        f.h.fingerprint().await,
        before,
        "no summary row and no compressed flag without the usage message"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// failures: Retry up to max_attempts, then Reject
// ═════════════════════════════════════════════════════════════════════════════

type Failure = (&'static str, Box<dyn Fn() -> ProviderReply>);

fn failures() -> Vec<Failure> {
    vec![
        (
            "http 500",
            Box::new(|| ProviderReply::http_error(500, "provider down")),
        ),
        (
            "http 429",
            Box::new(|| ProviderReply::http_error(429, "slow down")),
        ),
        (
            "http 400",
            Box::new(|| ProviderReply::http_error(400, "bad request")),
        ),
        ("gateway timeout", Box::new(ProviderReply::gateway_timeout)),
        (
            "gateway unavailable",
            Box::new(ProviderReply::gateway_unavailable),
        ),
        (
            "gateway problem",
            Box::new(|| ProviderReply::gateway_problem(502)),
        ),
        (
            "response.failed",
            Box::new(|| {
                ProviderReply::sse(failed_stream(
                    "server_error",
                    "boom",
                    &["partial"],
                    Some((10, 2)),
                ))
            }),
        ),
        (
            "connection dropped mid-stream",
            Box::new(|| {
                ProviderReply::sse(vec![
                    ev(
                        "response.created",
                        json!({"type": "response.created", "response": {"id": "r1"}}),
                    ),
                    Piece::Fail("connection reset by peer".to_owned()),
                ])
            }),
        ),
        (
            "stream ends without a terminal event",
            Box::new(|| ProviderReply::text_with(&["partial"], &Opts::default().no_terminal())),
        ),
    ]
}

#[tokio::test]
async fn a_provider_failure_retries_and_the_last_attempt_rejects() {
    // max_attempts = 3: deliveries 0 and 1 retry, delivery 2 is the last one
    for (name, reply) in failures() {
        let f = Fixture::new(3).await;
        // a previous summary stays exactly as it was
        f.h.seed_summarized_up_to(f.chat, f.answer(1), "previous summary")
            .await;
        let base = f.h.frontier_of(f.answer(1)).await;
        let task = f.task_to(3).await.based_on(base);
        let before = f.h.fingerprint().await;

        for (attempts, retry) in [(0, true), (1, true), (2, false)] {
            f.h.summaries.push(reply());
            let result = f.h.deliver(&task.json(), attempts).await;
            if retry {
                assert!(is_retry(&result), "{name}, attempt {attempts}: {result:?}");
            } else {
                assert!(is_reject(&result), "{name}, attempt {attempts}: {result:?}");
            }
            f.assert_untouched(&before, name).await;
        }
        assert_eq!(f.h.summaries.request_count(), 3, "{name}");
    }
}

#[tokio::test]
async fn max_attempts_bounds_the_deliveries() {
    for (max_attempts, last) in [(1_u32, 0_i16), (2, 1), (5, 4)] {
        let f = Fixture::new(2).await;
        let handler = f
            .h
            .handler_with_config(json!({"thread_summary_worker": {"max_attempts": max_attempts}}));
        let task = f.task_to(2).await;
        for attempts in 0..=last {
            f.h.summaries.push(ProviderReply::http_error(500, "down"));
            let result = handle_with(&handler, &task.json(), attempts).await;
            if attempts < last {
                assert!(
                    is_retry(&result),
                    "max {max_attempts}, attempt {attempts}: {result:?}"
                );
            } else {
                assert!(
                    is_reject(&result),
                    "max {max_attempts}, attempt {attempts}: {result:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_late_success_after_failures_commits_normally() {
    let f = Fixture::new(2).await;
    let task = f.task_to(2).await;
    f.h.summaries
        .push(ProviderReply::http_error(503, "unavailable"));
    assert!(is_retry(&f.h.deliver(&task.json(), 0).await));
    assert!(f.h.summaries_of(f.chat).await.is_empty());

    f.h.summaries.push(summary_reply("recovered"));
    let result = f.h.deliver(&task.json(), 1).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(
        f.h.summary_of(f.chat).await.summary_text.unwrap().trim(),
        "recovered"
    );
    let events = f.h.wait_system_usage(1).await;
    assert_eq!(
        events[0].request_id, task.system_request_id,
        "the id of the message, not of the attempt"
    );
}

// ── a payload that is not a work item ────────────────────────────────────────

#[tokio::test]
async fn a_malformed_payload_is_rejected_without_a_provider_call() {
    let f = Fixture::new(2).await;
    let good = f.task_to(2).await.json();
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("not json", b"definitely not json".to_vec()),
        ("a json array", b"[1, 2]".to_vec()),
        ("an empty object", b"{}".to_vec()),
    ];
    for field in [
        "tenant_id",
        "chat_id",
        "system_request_id",
        "frozen_target_created_at",
        "frozen_target_message_id",
        "system_task_type",
    ] {
        let mut payload = good.clone();
        payload.as_object_mut().unwrap().remove(field);
        cases.push((field, payload.to_string().into_bytes()));
    }
    for (field, bad) in [
        ("chat_id", json!("not-a-uuid")),
        ("tenant_id", json!(42)),
        ("frozen_target_message_id", json!("nope")),
        ("frozen_target_created_at", json!("yesterday")),
        ("system_task_type", json!("doc_summary_generation")),
        ("system_task_type", json!("something_else")),
    ] {
        let mut payload = good.clone();
        payload[field] = bad;
        cases.push((field, payload.to_string().into_bytes()));
    }
    let before = f.h.fingerprint().await;

    for (what, raw) in cases {
        let result = f.h.deliver_raw(&raw, 0).await;
        assert!(is_reject(&result), "{what}: {result:?}");
    }

    assert_eq!(
        f.h.summaries.request_count(),
        0,
        "no provider call for garbage"
    );
    f.assert_untouched(&before, "malformed payloads").await;
}

// ── the summary model ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_disabled_or_missing_summary_model_rejects_without_retry() {
    for summary_model in ["gpt-off", "no-such-model"] {
        let f = Fixture::on(
            SummaryHarness::with_config(
                json!({"thread_summary_worker": {"summary_model_id": summary_model}}),
            )
            .await,
            2,
        )
        .await;
        let before = f.h.fingerprint().await;

        // the very first delivery is rejected: it is not a transient failure
        let result = f.h.deliver_task(&f.task_to(2).await).await;

        assert!(is_reject(&result), "{summary_model}: {result:?}");
        assert_eq!(f.h.summaries.request_count(), 0, "{summary_model}");
        assert_eq!(f.h.chat_requests().len(), 0, "{summary_model}");
        f.assert_untouched(&before, summary_model).await;
    }
}

#[tokio::test]
async fn a_failing_policy_plugin_is_a_transient_failure() {
    let f = Fixture::new(2).await;
    f.h.flaky.fail_with("plugin down");
    let before = f.h.fingerprint().await;

    let result = f.h.deliver_task(&f.task_to(2).await).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(f.h.summaries.request_count(), 0);
    f.assert_untouched(&before, "plugin failure").await;

    f.h.flaky.recover();
    assert!(is_ok(&f.h.deliver_task(&f.task_to(2).await).await));
}

#[tokio::test]
async fn a_disabled_worker_still_processes_a_message_that_is_already_queued() {
    // `thread_summary_worker.enabled` gates the trigger only (B.9.4)
    let f = Fixture::new(2).await;
    let handler =
        f.h.handler_with_config(json!({"thread_summary_worker": {"enabled": false}}));
    let task = f.task_to(2).await;

    let result = handle_with(&handler, &task.json(), 0).await;

    assert!(is_ok(&result), "{result:?}");
    f.h.summary_of(f.chat).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// skips: the message is acknowledged, nothing changes
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_deleted_chat_is_skipped() {
    let f = Fixture::new(2).await;
    let task = f.task_to(2).await;
    f.h.soft_delete_chat(f.chat).await;
    let before = f.h.fingerprint().await;

    let result = f.h.deliver_task(&task).await;

    assert!(is_ok(&result), "{result:?}");
    f.assert_untouched(&before, "deleted chat").await;
}

#[tokio::test]
async fn an_unknown_chat_or_a_foreign_tenant_is_skipped() {
    let f = Fixture::new(2).await;
    let before = f.h.fingerprint().await;

    // a chat that does not exist
    let mut gone = f.task_to(2).await;
    gone.chat = Uuid::new_v4();
    assert!(is_ok(&f.h.deliver_task(&gone).await));

    // the chat of tenant A under the id of tenant B
    let foreign = f.task_to(2).await.tenant(TENANT_B);
    assert!(is_ok(&f.h.deliver_task(&foreign).await));

    f.assert_untouched(&before, "unknown chat / foreign tenant")
        .await;
}

#[tokio::test]
async fn a_chat_deleted_while_the_provider_works_is_skipped_at_the_commit() {
    let f = Fixture::new(2).await;
    let task = f.task_to(2).await;
    f.h.summaries
        .push(summary_reply_with("late", &Opts::default().pause_after(1)));
    let delivery = f.h.spawn_deliver(&task.json(), 0);
    f.h.summaries.wait_paused(1).await;
    f.h.soft_delete_chat(f.chat).await;
    let before = f.h.fingerprint().await;

    f.h.summaries.release();
    let result = delivery.await.unwrap();

    assert!(is_ok(&result), "{result:?}");
    f.assert_untouched(&before, "chat deleted during the call")
        .await;
}

#[tokio::test]
async fn a_task_whose_base_is_not_the_stored_frontier_is_skipped_before_the_call() {
    // the chat already has a summary up to answer 2; a task built on an empty base is stale
    let f = Fixture::new(4).await;
    f.h.seed_summarized_up_to(f.chat, f.answer(2), "newer summary")
        .await;
    let before = f.h.fingerprint().await;

    let stale = f.task_to(3).await;
    let result = f.h.deliver_task(&stale).await;
    assert!(is_ok(&result), "{result:?}");

    // a task based on an older frontier than the stored one
    let older = f
        .task_to(4)
        .await
        .based_on(f.h.frontier_of(f.answer(1)).await);
    assert!(is_ok(&f.h.deliver_task(&older).await));

    assert_eq!(
        f.h.summaries.request_count(),
        0,
        "the pre-check skips before the provider call"
    );
    f.assert_untouched(&before, "stale base").await;
}

#[tokio::test]
async fn a_task_based_on_a_summary_that_no_longer_exists_is_skipped() {
    // a retry, edit or delete dropped the summary the task was based on
    let f = Fixture::new(4).await;
    let task = f
        .task_to(4)
        .await
        .based_on(f.h.frontier_of(f.answer(2)).await);
    let before = f.h.fingerprint().await;

    let result = f.h.deliver_task(&task).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(f.h.summaries.request_count(), 0);
    f.assert_untouched(&before, "missing base").await;
}

#[tokio::test]
async fn a_redelivered_message_makes_no_second_commit() {
    let f = Fixture::new(3).await;
    let task = f.task_to(2).await;
    assert!(is_ok(&f.h.deliver_task(&task).await));
    f.h.wait_system_usage(1).await;
    let committed = f.h.fingerprint().await;

    // outbox retry / replay of the same message
    let again = f.h.deliver_task(&task).await;

    assert!(is_ok(&again), "{again:?}");
    assert_eq!(f.h.fingerprint().await, committed);
    f.h.assert_system_usage_stays(1).await;
    assert_eq!(
        f.h.summaries.request_count(),
        1,
        "the frontier already moved: no second call"
    );
}

#[tokio::test]
async fn two_concurrent_deliveries_commit_once() {
    let f = Fixture::new(3).await;
    let task = f.task_to(2).await;
    f.h.summaries.set_default(|| summary_reply("one winner"));

    let payload = task.json();
    let (a, b) = tokio::join!(f.h.deliver(&payload, 0), f.h.deliver(&payload, 0));

    // a delivery that loses a database race may ask for a redelivery; it never rejects
    assert!(is_ok(&a) || is_ok(&b), "{a:?} {b:?}");
    assert!(!is_reject(&a) && !is_reject(&b), "{a:?} {b:?}");
    let row = f.h.summary_of(f.chat).await;
    assert_eq!(row.summarized_up_to_message_id, f.answer(2));
    assert_eq!(f.h.wait_system_usage(1).await.len(), 1);
    f.h.assert_system_usage_stays(1).await;
    assert!(
        f.h.summaries.request_count() <= 2,
        "a duplicate provider call is accepted"
    );
}

#[tokio::test]
async fn a_summary_committed_while_the_provider_works_wins_the_race() {
    let f = Fixture::new(4).await;
    let task = f.task_to(2).await;
    f.h.summaries
        .push(summary_reply_with("loser", &Opts::default().pause_after(1)));
    let delivery = f.h.spawn_deliver(&task.json(), 0);
    f.h.summaries.wait_paused(1).await;
    // another commit advances the frontier meanwhile
    f.h.seed_summarized_up_to(f.chat, f.answer(3), "winner")
        .await;
    let before = f.h.fingerprint().await;

    f.h.summaries.release();
    let result = delivery.await.unwrap();

    assert!(is_ok(&result), "{result:?}");
    f.assert_untouched(&before, "lost compare-and-set").await;
    assert_eq!(
        f.h.summary_of(f.chat).await.summary_text.as_deref(),
        Some("winner")
    );
}

#[tokio::test]
async fn a_summary_dropped_while_the_provider_works_is_not_resurrected() {
    let f = Fixture::new(4).await;
    f.h.seed_summarized_up_to(f.chat, f.answer(1), "old summary")
        .await;
    let task = f
        .task_to(3)
        .await
        .based_on(f.h.frontier_of(f.answer(1)).await);
    f.h.summaries.push(summary_reply_with(
        "stale result",
        &Opts::default().pause_after(1),
    ));
    let delivery = f.h.spawn_deliver(&task.json(), 0);
    f.h.summaries.wait_paused(1).await;
    // a retry / edit / delete dropped the summary and cleared the flags in its transaction
    f.h.delete_summary_row(f.chat).await;
    f.h.clear_compressed(f.chat).await;
    let before = f.h.fingerprint().await;

    f.h.summaries.release();
    let result = delivery.await.unwrap();

    assert!(is_ok(&result), "{result:?}");
    f.assert_untouched(&before, "summary invalidated during the call")
        .await;
    assert!(f.h.summaries_of(f.chat).await.is_empty());
}

#[tokio::test]
async fn a_target_that_was_soft_deleted_before_the_delivery_is_skipped() {
    let f = Fixture::new(3).await;
    let task = f.task_to(3).await;
    f.h.soft_delete_messages(&[f.question(3), f.answer(3)])
        .await;
    let before = f.h.fingerprint().await;

    let result = f.h.deliver_task(&task).await;

    assert!(is_ok(&result), "{result:?}");
    f.assert_untouched(&before, "deleted target").await;
}

#[tokio::test]
async fn a_target_removed_by_a_mutation_while_the_provider_works_is_skipped() {
    // the frozen target is the answer of the latest turn; retry, edit or delete soft-deletes it
    for op in [Op::Retry, Op::Edit, Op::Delete] {
        let f = Fixture::new(3).await;
        let task = f.task_to(3).await;
        f.h.summaries.push(summary_reply_with(
            "too late",
            &Opts::default().pause_after(1),
        ));
        let delivery = f.h.spawn_deliver(&task.json(), 0);
        f.h.summaries.wait_paused(1).await;

        f.h.mutate(op, f.chat, f.pairs[2].request_id).await;
        let before = f.h.fingerprint().await;
        assert!(
            f.h.messages_in(f.chat)
                .await
                .iter()
                .any(|m| m.id == f.answer(3) && m.deleted_at.is_some()),
            "{op:?}: the target is soft-deleted"
        );

        f.h.summaries.release();
        let result = delivery.await.unwrap();

        assert!(is_ok(&result), "{op:?}: {result:?}");
        f.assert_untouched(&before, &format!("{op:?} during the call"))
            .await;
        assert!(f.h.summaries_of(f.chat).await.is_empty(), "{op:?}");
        assert!(f.h.compressed_ids(f.chat).await.is_empty(), "{op:?}");
    }
}

#[tokio::test]
async fn the_target_lock_is_a_no_op_that_only_succeeds_for_a_live_message_of_the_chat() {
    // the commit transaction locks the frozen target (after the chat lock) instead of reading it:
    // `true` only for a live message of that chat, and it changes nothing
    use mini_chat::domain::ports::ThreadSummaryRepository;
    let f = Fixture::new(2).await;
    let scope = toolkit_security::AccessScope::for_tenant(TENANT_A);
    let conn = f.h.chat.db.conn().unwrap();
    let before = f.h.fingerprint().await;

    assert!(
        conn.lock_live_message(&scope, f.chat, f.answer(2))
            .await
            .unwrap()
    );
    assert!(
        !conn
            .lock_live_message(&scope, Uuid::new_v4(), f.answer(2))
            .await
            .unwrap(),
        "another chat"
    );
    assert!(
        !conn
            .lock_live_message(&scope, f.chat, Uuid::new_v4())
            .await
            .unwrap(),
        "unknown message"
    );
    let foreign = toolkit_security::AccessScope::for_tenant(TENANT_B);
    assert!(
        !conn
            .lock_live_message(&foreign, f.chat, f.answer(2))
            .await
            .unwrap(),
        "another tenant"
    );
    assert_eq!(f.h.fingerprint().await, before, "a lock changes nothing");

    f.h.soft_delete_messages(&[f.answer(2)]).await;
    assert!(
        !conn
            .lock_live_message(&scope, f.chat, f.answer(2))
            .await
            .unwrap(),
        "soft-deleted"
    );
}

#[tokio::test]
async fn a_delete_racing_the_summary_commit_never_leaves_a_summary_over_the_deleted_turn() {
    // whichever of the two commits first, the outcome is consistent: either the summary was
    // committed first and the delete dropped it with the compression flags, or the delete came
    // first and the worker skipped. A summary or a flag must never survive over a deleted target.
    for round in 0..8 {
        let f = Fixture::new(3).await;
        let task = f.task_to(3).await;
        f.h.summaries
            .set_default(|| summary_reply("racing summary"));

        let payload = task.json();
        let (result, ()) = tokio::join!(
            f.h.deliver(&payload, 0),
            f.h.mutate(Op::Delete, f.chat, f.pairs[2].request_id)
        );

        assert!(is_ok(&result), "round {round}: {result:?}");
        assert!(
            f.h.messages_in(f.chat)
                .await
                .iter()
                .any(|m| m.id == f.answer(3) && m.deleted_at.is_some()),
            "round {round}: the target is deleted"
        );
        assert!(
            f.h.summaries_of(f.chat).await.is_empty(),
            "round {round}: no summary over a deleted turn"
        );
        assert!(
            f.h.compressed_ids(f.chat).await.is_empty(),
            "round {round}: no compression flags over a deleted turn"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// the queue lease
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_lease_follows_claim_timeout_secs() {
    let f = Fixture::new(0).await;
    let lease = thread_summary_lease(&f.h.config);
    assert_eq!(lease.duration, Duration::from_secs(300), "default 300 s");
    assert!(lease.headroom < lease.duration);

    for secs in [30_u64, 45, 3600] {
        let h = SummaryHarness::with_config(
            json!({"thread_summary_worker": {"claim_timeout_secs": secs}}),
        )
        .await;
        let lease = thread_summary_lease(&h.config);
        assert_eq!(lease.duration, Duration::from_secs(secs));
        assert!(lease.headroom < lease.duration, "{secs}");
    }
}
