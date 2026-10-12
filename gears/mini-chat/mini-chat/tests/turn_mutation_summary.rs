//! Summary interaction of the turn mutations (slice S5): the retry, edit and delete transaction
//! deletes the chat's `thread_summaries` row and clears `is_compressed` on every message of the
//! chat when the summary frontier is at or after the mutated turn's user message, and keeps both
//! otherwise. Summary generation itself belongs to slice S7; the rows are seeded here.
//!
//! Source of truth: DESIGN 3.9 "Summary Interaction on Turn Mutation" (the rule, "Until the next
//! summary, context assembly reads the uncompressed history", the frontier comparison on
//! `(created_at, id)`), 3.7 `thread_summaries` (frontier columns) and `messages.is_compressed`.

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
mod mutation_support;
mod quota_support;
mod stream_support;

use mutation_support::{
    ALL_OPS, MutationHarness, MutationOptions, Op, Pair, PairSeed, assert_not_latest, ts, uid,
};
use uuid::Uuid;

/// Three completed turns at 10 s, 20 s and 30 s whose first two are summarized (their messages
/// are `is_compressed`); the frontier is chosen by the test. The mutated turn is the third.
struct Setup {
    h: MutationHarness,
    chat: Uuid,
    pairs: Vec<Pair>,
}

async fn history() -> Setup {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 3).await;
    let covered: Vec<Uuid> = pairs[..2]
        .iter()
        .flat_map(|p| [p.user_message, p.assistant_message.unwrap()])
        .collect();
    h.mark_compressed(&covered).await;
    Setup { h, chat, pairs }
}

async fn mutate(h: &MutationHarness, op: Op, chat: Uuid, rid: Uuid) {
    match op {
        Op::Retry => {
            assert_eq!(h.retried(chat, rid).await.terminal().event, "done");
        }
        Op::Edit => {
            assert_eq!(
                h.edited(chat, rid, "reworded question")
                    .await
                    .terminal()
                    .event,
                "done"
            );
        }
        Op::Delete => assert_eq!(h.delete(chat, rid).await.status.as_u16(), 204),
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// the summary covers the mutated turn: it is dropped
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_summary_that_covers_the_mutated_turn_is_deleted_and_the_messages_are_uncompressed() {
    for op in ALL_OPS {
        // frontier = the assistant message of the mutated turn (after its user message)
        let Setup { h, chat, pairs } = history().await;
        let last = pairs[2];
        h.seed_summary(chat, ts(31), last.assistant_message.unwrap())
            .await;
        assert_eq!(h.summaries_of(chat).await.len(), 1);

        mutate(&h, op, chat, last.request_id).await;

        assert!(
            h.summaries_of(chat).await.is_empty(),
            "{op:?}: the summary row is gone"
        );
        let flags = h.compressed_flags(chat).await;
        assert!(
            flags.iter().all(|(_, compressed)| !compressed),
            "{op:?}: is_compressed is cleared on every message of the chat: {flags:?}"
        );
    }
}

#[tokio::test]
async fn a_frontier_on_the_user_message_of_the_mutated_turn_counts_as_covering_it() {
    for op in ALL_OPS {
        let Setup { h, chat, pairs } = history().await;
        let last = pairs[2];
        h.seed_summary(chat, ts(30), last.user_message).await;

        mutate(&h, op, chat, last.request_id).await;

        assert!(
            h.summaries_of(chat).await.is_empty(),
            "{op:?}: at-or-after includes equal"
        );
        assert!(h.compressed_flags(chat).await.iter().all(|(_, c)| !c));
    }
}

#[tokio::test]
async fn the_frontier_comparison_is_on_created_at_then_message_id() {
    // same instant as the mutated user message: the id decides
    for (frontier_id, deleted) in [
        (uid(0x4fff), false),
        (uid(0x5000), true),
        (uid(0x5001), true),
    ] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pairs = {
            let mut pairs = h.seed_history(chat, 2).await;
            pairs.push(
                h.seed_pair(
                    chat,
                    PairSeed::completed("question 3", "answer 3")
                        .at(30)
                        .user_id(uid(0x5000)),
                )
                .await,
            );
            pairs
        };
        let covered = [pairs[0].user_message, pairs[0].assistant_message.unwrap()];
        h.mark_compressed(&covered).await;
        h.seed_summary(chat, ts(30), frontier_id).await;

        h.retried(chat, pairs[2].request_id).await;

        assert_eq!(
            h.summaries_of(chat).await.is_empty(),
            deleted,
            "frontier id {frontier_id}: deleted = {deleted}"
        );
        let still_compressed = h.compressed_flags(chat).await.iter().any(|(_, c)| *c);
        assert_eq!(still_compressed, !deleted, "frontier id {frontier_id}");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// the summary stops before the mutated turn: it is kept
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_summary_that_stops_before_the_mutated_turn_is_kept_untouched() {
    for op in ALL_OPS {
        // frontier = the assistant message of the previous turn
        let Setup { h, chat, pairs } = history().await;
        let summary = h
            .seed_summary(chat, ts(21), pairs[1].assistant_message.unwrap())
            .await;
        let flags_before = h.compressed_flags(chat).await;

        mutate(&h, op, chat, pairs[2].request_id).await;

        let rows = h.summaries_of(chat).await;
        assert_eq!(rows.len(), 1, "{op:?}");
        assert_eq!(rows[0].id, summary, "{op:?}: the same row");
        assert_eq!(
            rows[0].summarized_up_to_message_id,
            pairs[1].assistant_message.unwrap()
        );
        let flags_after = h.compressed_flags(chat).await;
        for (id, compressed) in flags_before {
            let after = flags_after.iter().find(|(i, _)| *i == id).unwrap().1;
            assert_eq!(
                after, compressed,
                "{op:?}: is_compressed of {id} is unchanged"
            );
        }
    }
}

#[tokio::test]
async fn a_kept_summary_keeps_its_messages_out_of_the_context() {
    let Setup { h, chat, pairs } = history().await;
    h.seed_summary(chat, ts(21), pairs[1].assistant_message.unwrap())
        .await;

    h.retried(chat, pairs[2].request_id).await;

    let conversation = h.oagw.last_request().conversation_texts();
    assert!(
        conversation.contains(&"user:question 3".to_owned()),
        "{conversation:?}"
    );
    for covered in ["question 1", "answer 1", "question 2", "answer 2"] {
        assert!(
            conversation.iter().all(|m| !m.ends_with(covered)),
            "{covered} is summarized and stays out of the context: {conversation:?}"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// context after the summary was dropped
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn after_the_summary_is_dropped_the_context_reads_the_uncompressed_history() {
    let Setup { h, chat, pairs } = history().await;
    h.seed_summary(chat, ts(31), pairs[2].assistant_message.unwrap())
        .await;

    h.retried(chat, pairs[2].request_id).await;

    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![
            "user:question 1".to_owned(),
            "assistant:answer 1".to_owned(),
            "user:question 2".to_owned(),
            "assistant:answer 2".to_owned(),
            "user:question 3".to_owned()
        ],
        "the formerly summarized messages are part of the context again"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// only a successful mutation touches the summary
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_rejected_mutation_keeps_the_summary_and_the_flags() {
    for op in ALL_OPS {
        let Setup { h, chat, pairs } = history().await;
        h.seed_summary(chat, ts(31), pairs[2].assistant_message.unwrap())
            .await;
        let before = h.snapshot().await;

        // the first turn is not the latest
        assert_not_latest(&h.reject(op, chat, pairs[0].request_id).await);

        assert_eq!(h.snapshot().await, before, "{op:?}");
        assert_eq!(h.summaries_of(chat).await.len(), 1);
    }
}

#[tokio::test]
async fn the_summary_of_another_chat_is_never_touched() {
    let Setup { h, chat, pairs } = history().await;
    let other = h.seed_chat().await;
    let other_pairs = h.seed_history(other, 1).await;
    h.mark_compressed(&[other_pairs[0].user_message]).await;
    h.seed_summary(other, ts(11), other_pairs[0].assistant_message.unwrap())
        .await;
    h.seed_summary(chat, ts(31), pairs[2].assistant_message.unwrap())
        .await;

    h.delete(chat, pairs[2].request_id).await;

    assert!(h.summaries_of(chat).await.is_empty());
    assert_eq!(h.summaries_of(other).await.len(), 1);
    let other_flags = h.compressed_flags(other).await;
    assert!(
        other_flags.iter().any(|(_, compressed)| *compressed),
        "another chat's messages stay compressed: {other_flags:?}"
    );
}

#[tokio::test]
async fn the_summary_is_dropped_in_the_mutation_transaction() {
    // the audit enqueue of the mutation fails, so the whole transaction - soft delete, summary
    // deletion and flag reset - rolls back
    let h = MutationHarness::with(MutationOptions {
        register_audit_queue: false,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 3).await;
    h.mark_compressed(&[pairs[0].user_message]).await;
    h.seed_summary(chat, ts(31), pairs[2].assistant_message.unwrap())
        .await;
    let before = h.snapshot().await;

    let resp = h.delete(chat, pairs[2].request_id).await;

    assert_eq!(resp.status.as_u16(), 500, "{}", resp.raw);
    assert_eq!(h.snapshot().await, before);
    assert_eq!(h.summaries_of(chat).await.len(), 1);
    assert!(h.compressed_flags(chat).await.iter().any(|(_, c)| *c));
}
