#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use super::*;
use crate::domain::llm::LlmEvent;
use crate::domain::stream::service::tests::{
    Fixture, completion, drain, names, request, standard_credits,
};
use crate::infra::db::repo::tests::message_rows;

#[test]
fn plans_follow_the_terminal_outcome() {
    let usage = LlmUsage {
        input_tokens: 10,
        output_tokens: 2,
        cache_read_input_tokens: 4,
        cache_write_input_tokens: 0,
        reasoning_tokens: 1,
    };
    let completed = Plan::of(
        &StreamOutcome::Completed {
            text: String::new(),
            usage: Some(usage),
            response_id: Some("resp_x".to_owned()),
        },
        true,
    );
    assert_eq!(completed.terminal, TerminalState::Completed);
    assert_eq!(completed.message.as_deref(), Some(""));
    assert_eq!(completed.usage, Some(usage_tokens(&usage)));
    assert!(completed.enqueue_summary);
    assert_eq!(completed.error_code, None);

    let failed = Plan::of(
        &StreamOutcome::Failed {
            code: "provider_error".to_owned(),
            message: "m".to_owned(),
            usage: None,
            request_sent: true,
        },
        true,
    );
    assert_eq!(failed.terminal, TerminalState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("provider_error"));
    assert_eq!(failed.message, None);
    assert!(!failed.enqueue_summary);

    let not_sent = Plan::of(
        &StreamOutcome::Failed {
            code: "provider_error".to_owned(),
            message: "m".to_owned(),
            usage: Some(usage),
            request_sent: false,
        },
        true,
    );
    assert_eq!(not_sent.terminal, TerminalState::Failed);
    assert_eq!(not_sent.error_code.as_deref(), Some("turn_setup_failed"));
    assert_eq!(not_sent.usage, None);
    assert_eq!(not_sent.message, None);
    assert!(!not_sent.enqueue_summary);

    let cancelled_empty = Plan::of(
        &StreamOutcome::Cancelled {
            text: String::new(),
        },
        true,
    );
    assert_eq!(cancelled_empty.message, None);
    let cancelled = Plan::of(
        &StreamOutcome::Cancelled {
            text: "partial".to_owned(),
        },
        false,
    );
    assert_eq!(cancelled.message.as_deref(), Some("partial"));
    assert_eq!(cancelled.error_code, None);
}

#[test]
fn plan_without_message_downgrades_completed_and_keeps_cancelled() {
    let completed = Plan::of(
        &StreamOutcome::Completed {
            text: "answer".to_owned(),
            usage: None,
            response_id: None,
        },
        true,
    )
    .without_message();
    assert_eq!(completed.terminal, TerminalState::Failed);
    assert_eq!(
        completed.error_code.as_deref(),
        Some(MESSAGE_PERSISTENCE_FAILED)
    );
    assert_eq!(completed.message, None);
    assert!(!completed.enqueue_summary);

    let cancelled = Plan::of(
        &StreamOutcome::Cancelled {
            text: "partial".to_owned(),
        },
        false,
    )
    .without_message();
    assert_eq!(cancelled.terminal, TerminalState::Cancelled);
    assert_eq!(cancelled.error_code, None);
    assert_eq!(cancelled.message, None);
}

#[test]
fn usage_tokens_keep_every_counter() {
    let usage = LlmUsage {
        input_tokens: 250,
        output_tokens: 40,
        cache_read_input_tokens: 64,
        cache_write_input_tokens: 3,
        reasoning_tokens: 12,
    };
    assert_eq!(
        usage_tokens(&usage),
        UsageTokens {
            input_tokens: 250,
            output_tokens: 40,
            cache_read_input_tokens: 64,
            cache_write_input_tokens: 3,
            reasoning_tokens: 12,
        }
    );
}

#[tokio::test]
async fn cas_loser_writes_nothing_and_sends_no_terminal_event() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let feed = fx.provider.channel();
    let live = fx.live(chat_id, request("slow answer")).await;
    let turn = fx.turn(chat_id).await;
    let reserved = fx.quota_rows().await;

    fx.finalize_externally(turn.id).await;
    feed.send(LlmEvent::TextDelta("too late".to_owned()))
        .unwrap();
    feed.send(completion(Some((50, 10)))).unwrap();
    let events = drain(live).await;

    assert_eq!(names(&events), ["stream_started", "delta"]);
    let after = fx.turn(chat_id).await;
    assert_eq!(after.state, "failed");
    assert_eq!(after.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(after.assistant_message_id, None);
    let roles: Vec<String> = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(roles, ["user"]);
    assert_eq!(
        fx.quota_rows().await,
        reserved,
        "a CAS loser must not settle"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(fx.store.delivered(&fx.queues.usage).is_empty());
    assert!(fx.store.delivered(&fx.queues.audit).is_empty());
}

#[tokio::test]
async fn second_finalization_of_the_same_turn_is_a_no_op() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let live = fx.live(chat_id, request("once")).await;
    let events = drain(live).await;
    assert_eq!(names(&events).last(), Some(&"done"));
    let turn = fx.turn(chat_id).await;
    let settled = fx.quota_rows().await;

    let finalizer = Finalizer::new(
        fx.store.db.clone(),
        fx.store.repo.repositories(),
        fx.service_quota(),
    );
    let context = Arc::new(fx.turn_context(&turn).await);
    let result = finalizer
        .finalize(
            &context,
            &StreamOutcome::Cancelled {
                text: "again".to_owned(),
            },
            TurnToolCalls::default(),
        )
        .await
        .unwrap();

    assert_eq!(result, Finalized::Lost);
    assert_eq!(fx.turn(chat_id).await, turn);
    assert_eq!(fx.quota_rows().await, settled);
    assert_eq!(
        settled
            .iter()
            .find(|row| row.bucket == "total")
            .unwrap()
            .spent_credits_micro,
        standard_credits(10, 5)
    );
}
