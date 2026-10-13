#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slices 4 and 5 end to end: the thread-summary trigger evaluated in the
//! finalization transaction of the stream pipeline
//! (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`,
//! `cpt-cf-mini-chat-dod-background-workers-summary-trigger`; the hook of
//! `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`: the `ContextPlan`
//! outputs carried into the finalization input, the trigger inside the closure
//! after the audit enqueue, the queue woken and the trigger metric recorded after
//! the commit) and the whole long-conversation compression flow
//! (`cpt-cf-mini-chat-flow-background-workers-long-conversation-compression`).

use serde_json::Value;
use uuid::Uuid;

use crate::chat_test_env::blob;
use crate::config::ThreadSummaryWorkerConfig;
use crate::domain::service::stream::test_support::StreamMetric;
use crate::mutation_test_env::{MutationEnv, SeedTurn};
use crate::stream_test_env::{Streamed, body, oa, parse_frames, rid};
use crate::test_doubles::{ProxyReply, USER_TENANT, user_ctx};
use crate::worker_test_env::{WorkerEnv, WorkerOptions, boundary};

impl WorkerEnv {
    /// How often `mini_chat_thread_summary_trigger{result}` was recorded.
    fn metrics_stream_count(&self, result: &str) -> usize {
        self.m
            .stream
            .metrics
            .count(|metric| *metric == StreamMetric::SummaryTrigger(result.to_owned()))
    }
}

/// A chat on the `tiny` model (an input budget of 900 tokens, one token per
/// byte, the system prompt of 20 bytes) with `turns` completed turns whose
/// questions are `question_bytes` long.
async fn tiny_chat(
    env: &WorkerEnv,
    turns: u128,
    question_bytes: usize,
) -> (Uuid, Vec<crate::mutation_test_env::Seeded>) {
    let chat = env.m.stream.seed_chat(60, "tiny").await;
    let mut seeded = Vec::new();
    for n in 1..=turns {
        let secs = i64::try_from(n).unwrap() * 10;
        seeded.push(
            env.m
                .seed_turn_in(
                    chat,
                    &SeedTurn::completed(n, secs).question(&"q".repeat(question_bytes)),
                )
                .await,
        );
    }
    (chat, seeded)
}

async fn send(env: &WorkerEnv, chat: Uuid, content: &str, n: u128) -> Streamed {
    env.script(&[oa::delta("fine"), oa::completed(10, 5)]);
    env.m.stream.stream(chat, &body(content, rid(n))).await
}

// ================================================================================================
// the trigger in the finalization transaction
// ================================================================================================

#[tokio::test]
async fn a_completed_turn_reaching_the_threshold_without_a_summary_enqueues_one_work_item() {
    let env = WorkerEnv::new().await;
    // two prior turns of 300-byte questions: assembled ~736 tokens of 900 (80% is 720)
    let (chat, turns) = tiny_chat(&env, 2, 300).await;

    let streamed = send(&env, chat, &"n".repeat(100), 9).await;

    assert_eq!(streamed.names().last(), Some(&"done"));
    let items = env.summary_messages().await;
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["tenant_id"], USER_TENANT.to_string());
    assert_eq!(item["chat_id"], chat.to_string());
    assert!(Uuid::parse_str(item["system_request_id"].as_str().unwrap()).is_ok());
    assert!(
        item["base_frontier_created_at"].is_null() && item["base_frontier_message_id"].is_null()
    );
    assert_eq!(
        item["frozen_target_message_id"],
        turns[1].assistant_message.to_string(),
        "the latest message before the causing turn"
    );
    assert_eq!(
        item["frozen_target_created_at"],
        "2024-01-01T00:00:21.000000Z"
    );
    assert_eq!(item["system_task_type"], "thread_summary_update");
    assert_eq!(env.metrics_stream_count("scheduled"), 1);
    assert_eq!(env.metrics_stream_count("not_needed"), 0);
    assert!(
        streamed.of("stream_started")[0]
            .data
            .get("thread_summary_applied")
            .is_none()
    );
}

#[tokio::test]
async fn the_trigger_changes_neither_the_context_nor_the_events_of_the_turn() {
    let with = WorkerEnv::new().await;
    let without = WorkerEnv::with(WorkerOptions {
        summary: ThreadSummaryWorkerConfig {
            enabled: false,
            summary_model_id: "sum".to_owned(),
            ..ThreadSummaryWorkerConfig::default()
        },
        ..WorkerOptions::default()
    })
    .await;
    let mut runs = Vec::new();
    for env in [&with, &without] {
        let (chat, _) = tiny_chat(env, 2, 300).await;
        let streamed = send(env, chat, &"n".repeat(100), 9).await;
        let request = env.oagw.proxied().last().unwrap().json();
        let mut done = streamed.of("done")[0].data.clone();
        done["quota_warnings"] = Value::Null;
        runs.push((
            streamed
                .names()
                .iter()
                .map(|n| (*n).to_owned())
                .collect::<Vec<_>>(),
            done,
            request,
        ));
    }

    assert_eq!(
        runs[0], runs[1],
        "the in-flight turn's context and its events are never changed by the trigger"
    );
    assert_eq!(with.summary_messages().await.len(), 1);
    assert!(without.summary_messages().await.is_empty());
}

#[tokio::test]
async fn a_truncating_turn_enqueues_a_work_item_based_on_the_stored_frontier() {
    let env = WorkerEnv::new().await;
    // four turns of 300-byte questions do not fit: history is truncated
    let (chat, turns) = tiny_chat(&env, 4, 300).await;
    env.seed_summary(
        chat,
        "short earlier summary",
        turns[0].assistant_message,
        11,
        12,
    )
    .await;

    let streamed = send(&env, chat, &"n".repeat(100), 9).await;

    assert_eq!(streamed.names().last(), Some(&"done"));
    assert_eq!(
        streamed.of("stream_started")[0].data["thread_summary_applied"]["token_estimate"],
        12,
        "the summary was applied to this turn"
    );
    let items = env.summary_messages().await;
    assert_eq!(
        items.len(),
        1,
        "an effective summary does not stop a truncating turn"
    );
    assert_eq!(
        items[0]["base_frontier_message_id"],
        turns[0].assistant_message.to_string()
    );
    assert_eq!(
        items[0]["base_frontier_created_at"],
        "2024-01-01T00:00:11.000000Z"
    );
    assert_eq!(
        items[0]["frozen_target_message_id"],
        turns[3].assistant_message.to_string()
    );
    assert_eq!(env.metrics_stream_count("scheduled"), 1);
}

#[tokio::test]
async fn with_a_summary_a_turn_at_the_threshold_without_truncation_is_not_needed() {
    let env = WorkerEnv::new().await;
    let (chat, turns) = tiny_chat(&env, 2, 500).await;
    env.seed_summary(chat, &"s".repeat(40), turns[0].assistant_message, 11, 12)
        .await;

    let streamed = send(&env, chat, &"n".repeat(100), 9).await;

    assert_eq!(streamed.names().last(), Some(&"done"));
    assert!(env.summary_messages().await.is_empty());
    assert_eq!(env.metrics_stream_count("not_needed"), 1);
    assert_eq!(env.metrics_stream_count("scheduled"), 0);
}

#[tokio::test]
async fn below_the_threshold_nothing_is_enqueued_and_nothing_is_recorded() {
    let env = WorkerEnv::new().await;
    let (chat, _) = tiny_chat(&env, 1, 50).await;

    send(&env, chat, "short", 9).await;

    assert!(env.summary_messages().await.is_empty());
    assert_eq!(
        env.metrics_stream_count("scheduled") + env.metrics_stream_count("not_needed"),
        0
    );
}

#[tokio::test]
async fn a_first_turn_has_no_earlier_message_to_summarize() {
    let env = WorkerEnv::new().await;
    let chat = env.m.stream.seed_chat(60, "tiny").await;

    let streamed = send(&env, chat, &"n".repeat(800), 9).await;

    assert_eq!(streamed.names().last(), Some(&"done"));
    assert!(env.summary_messages().await.is_empty());
    assert_eq!(env.metrics_stream_count("not_needed"), 1);
}

#[tokio::test]
async fn a_failed_turn_enqueues_nothing_and_records_nothing() {
    let env = WorkerEnv::new().await;
    let (chat, _) = tiny_chat(&env, 2, 300).await;
    env.script(&[oa::failed("boom")]);

    let streamed = env
        .m
        .stream
        .stream(chat, &body(&"n".repeat(100), rid(9)))
        .await;

    assert_eq!(streamed.names().last(), Some(&"error"));
    assert!(env.summary_messages().await.is_empty());
    assert_eq!(
        env.metrics_stream_count("scheduled") + env.metrics_stream_count("not_needed"),
        0
    );
}

#[tokio::test]
async fn a_cancelled_turn_enqueues_nothing_and_records_nothing() {
    let env = WorkerEnv::with(WorkerOptions {
        ping: std::time::Duration::from_millis(40),
        ..WorkerOptions::default()
    })
    .await;
    let (chat, _) = tiny_chat(&env, 2, 300).await;
    let feed = env.gate();
    let mut stream = env
        .m
        .stream
        .open(chat, &body(&"n".repeat(100), rid(9)))
        .await;
    stream.next_frame().await.unwrap();

    drop(stream);
    env.wait_terminal(rid(9)).await;

    assert_eq!(env.turn_state_of(rid(9)).await, "cancelled");
    assert!(env.summary_messages().await.is_empty());
    assert_eq!(
        env.metrics_stream_count("scheduled") + env.metrics_stream_count("not_needed"),
        0
    );
    drop(feed);
}

#[tokio::test]
async fn a_disabled_trigger_enqueues_nothing_and_records_nothing() {
    let env = WorkerEnv::with(WorkerOptions {
        summary: ThreadSummaryWorkerConfig {
            enabled: false,
            summary_model_id: "sum".to_owned(),
            ..ThreadSummaryWorkerConfig::default()
        },
        ..WorkerOptions::default()
    })
    .await;
    let (chat, _) = tiny_chat(&env, 4, 300).await;

    send(&env, chat, &"n".repeat(100), 9).await;

    assert!(env.summary_messages().await.is_empty());
    assert_eq!(
        env.metrics_stream_count("scheduled") + env.metrics_stream_count("not_needed"),
        0
    );
}

#[tokio::test]
async fn the_compression_threshold_percentage_is_taken_from_the_configuration() {
    let env = WorkerEnv::with(WorkerOptions {
        summary: ThreadSummaryWorkerConfig {
            compression_threshold_pct: 99,
            summary_model_id: "sum".to_owned(),
            ..ThreadSummaryWorkerConfig::default()
        },
        ..WorkerOptions::default()
    })
    .await;
    let (chat, _) = tiny_chat(&env, 2, 300).await;

    send(&env, chat, &"n".repeat(100), 9).await;

    assert!(
        env.summary_messages().await.is_empty(),
        "736 of 900 tokens is below 99%"
    );
}

#[tokio::test]
async fn a_retry_and_an_edit_evaluate_the_trigger_through_the_shared_finalization() {
    for edit in [false, true] {
        let env = WorkerEnv::new().await;
        let (chat, turns) = tiny_chat(&env, 3, 300).await;
        let latest = SeedTurn::completed(3, 30).request_id();
        env.script(&[oa::delta("again"), oa::completed(10, 5)]);

        let reply = if edit {
            env.m
                .call(
                    "PATCH",
                    &MutationEnv::item_uri(chat, latest),
                    &user_ctx(),
                    Some((
                        "application/json",
                        serde_json::json!({ "content": "n".repeat(100) }).to_string(),
                    )),
                )
                .await
        } else {
            env.m
                .call(
                    "POST",
                    &MutationEnv::retry_uri(chat, latest),
                    &user_ctx(),
                    None,
                )
                .await
        };

        let frames = parse_frames(&reply.text);
        assert_eq!(
            frames.last().unwrap().name,
            "done",
            "edit={edit}: {}",
            reply.text
        );
        let items = env.summary_messages().await;
        assert_eq!(items.len(), 1, "edit={edit}");
        assert_eq!(
            items[0]["frozen_target_message_id"],
            turns[1].assistant_message.to_string(),
            "edit={edit}: the replaced turn's removed messages are never the target"
        );
        assert_eq!(env.metrics_stream_count("scheduled"), 1, "edit={edit}");
    }
}

#[tokio::test]
async fn a_failing_summary_enqueue_fails_the_whole_finalization_like_any_statement_of_the_closure()
{
    let env = WorkerEnv::new().await;
    let (chat, _) = tiny_chat(&env, 2, 300).await;
    env.exec(
        "CREATE TRIGGER fail_summary_enqueue BEFORE INSERT ON mini_chat__outbox_body \
         WHEN CAST(NEW.payload AS TEXT) LIKE '%thread_summary_update%' \
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
    )
    .await;
    env.script(&[oa::delta("fine"), oa::completed(10, 5)]);

    let reply = env
        .m
        .stream
        .post(chat, &body(&"n".repeat(100), rid(9)))
        .await;

    let frames = parse_frames(&reply.text);
    assert_eq!(frames.last().unwrap().name, "error", "{}", reply.text);
    assert_eq!(frames.last().unwrap().data["code"], "finalization_failed");
    assert_eq!(
        env.turn_state_of(rid(9)).await,
        "running",
        "the CAS rolled back with the failed enqueue"
    );
    assert!(env.usage().await.is_empty());
    assert!(env.audits().await.is_empty());
    assert!(env.summary_messages().await.is_empty());
}

// ================================================================================================
// the whole compression flow through the running outbox
// ================================================================================================

#[tokio::test]
async fn a_long_conversation_is_compressed_in_the_background_and_the_next_turn_uses_the_summary() {
    let env = WorkerEnv::with(WorkerOptions {
        real_pipeline: true,
        ..WorkerOptions::default()
    })
    .await;
    let (chat, turns) = tiny_chat(&env, 2, 300).await;
    // the turn's own answer, then the summary model's answer
    env.script(&[oa::delta("fine"), oa::completed(10, 5)]);
    env.oagw.push_proxy(ProxyReply::json(
        200,
        &serde_json::json!({
            "output": [ { "type": "message", "content": [ { "type": "output_text",
                "text": "<analysis>x</analysis><summary>Compressed history.</summary>" } ] } ],
            "usage": { "input_tokens": 300, "output_tokens": 20 }
        }),
    ));

    // 1. the turn completes; `done` is sent as usual and the work item is committed with the turn
    let streamed = env
        .m
        .stream
        .stream(chat, &body(&"n".repeat(100), rid(9)))
        .await;
    assert_eq!(streamed.names().last(), Some(&"done"));

    // 2. the handler summarizes the older messages, marks them compressed and advances the frontier
    env.wait_for("the summary is committed", || async {
        env.summary(chat).await.is_some()
    })
    .await;
    let summary = env.summary(chat).await.unwrap();
    assert_eq!(summary.text, "Compressed history.");
    assert_eq!(summary.frontier_id, turns[1].assistant_message);
    assert_eq!(summary.token_estimate, 20);
    assert_eq!(
        env.compressed_ids(chat).await.len(),
        4,
        "the messages before the causing turn"
    );

    // 3. the next turn starts with the summary and only the uncompressed messages
    env.script(&[oa::delta("next"), oa::completed(10, 5)]);
    let next = env
        .m
        .stream
        .stream(chat, &body("one more question", rid(10)))
        .await;
    assert_eq!(next.names().last(), Some(&"done"));
    assert_eq!(
        next.of("stream_started")[0].data["thread_summary_applied"]["token_estimate"],
        20
    );
    let request = env
        .oagw
        .proxied()
        .into_iter()
        .last()
        .unwrap()
        .json()
        .to_string();
    assert!(request.contains("Compressed history."), "{request}");
    assert!(
        !request.contains(&"q".repeat(300)),
        "a compressed message reached the provider"
    );
    assert!(request.contains("one more question"));
    // the compressed messages stay visible in the message list
    let page = env
        .get(&format!("/mini-chat/v1/chats/{chat}/messages"))
        .await;
    assert!(page.text.contains(&"q".repeat(300)));
    let _ = (blob(chat), boundary(chat, 0));
}
