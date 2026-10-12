//! Concurrent turn mutations resolve deterministically (slice S5): of two (or more) concurrent
//! retries / edits of the same latest turn exactly one gets the SSE stream and the others get a
//! 409 (`NOT_LATEST_TURN` or `GENERATION_IN_PROGRESS`), never a 500; at most one turn runs; a
//! lost insert race on the running-turn unique index is `GENERATION_IN_PROGRESS` and rolls the
//! whole mutation back.
//!
//! Source of truth: DESIGN 3.9 rule 1 (a newer running turn makes the target not the latest),
//! rule 7 ("Atomicity invariant (normative)": the latest-turn check, the terminal-state check,
//! the soft delete and the insert of the new running turn execute in one transaction; losing the
//! insert race is 409 `aborted` / `GENERATION_IN_PROGRESS`, not 500), 4 "Parallel Turn Policy"
//! (at most one running turn per chat), 3.3 "REST error mapping".

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
    MessageSeed, MutationHarness, Opts, PairSeed, ProviderReply, RawStream, Resp, Sent,
    TurnRowSeed, USER_A, assert_conflict, ctx_a, ts,
};
use serde_json::json;
use uuid::Uuid;

const Q: &str = "what is the capital of France";

/// The 409 reasons a losing mutation may report (rule 1 / rule 7).
const LOSS_REASONS: [&str; 2] = ["NOT_LATEST_TURN", "GENERATION_IN_PROGRESS"];

/// What one racing request got.
enum Outcome {
    Stream(RawStream),
    Rejected(Resp),
}

impl Outcome {
    async fn of(raw: RawStream) -> Self {
        if raw.is_sse() {
            Self::Stream(raw)
        } else {
            Self::Rejected(raw.into_resp().await)
        }
    }

    fn is_stream(&self) -> bool {
        matches!(self, Self::Stream(_))
    }
}

fn assert_loss(resp: &Resp, allowed: &[&str]) {
    assert_eq!(
        resp.status.as_u16(),
        409,
        "a lost race is a 409, never a 500: {}",
        resp.raw
    );
    let reason = resp.json()["context"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("no 409 reason: {}", resp.raw))
        .to_owned();
    assert!(
        allowed.contains(&reason.as_str()),
        "{reason} not in {allowed:?}: {}",
        resp.raw
    );
}

async fn finish(outcome: Outcome) -> Option<Sent> {
    match outcome {
        Outcome::Stream(raw) => Some(raw.collect().await),
        Outcome::Rejected(_) => None,
    }
}

/// The turns of `chat` that are running and not soft-deleted.
async fn running(h: &MutationHarness, chat: Uuid) -> usize {
    h.turns_in(chat)
        .await
        .iter()
        .filter(|t| t.state == "running" && t.deleted_at.is_none())
        .count()
}

// ═════════════════════════════════════════════════════════════════════════════
// the same turn, concurrently
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_retries_yield_one_stream_and_one_409() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    // the winner holds its running turn until released
    h.oagw.push(ProviderReply::text_with(
        &["slow"],
        &Opts::default().pause_after(0),
    ));

    let (a, b) = tokio::join!(
        h.retry(chat, pair.request_id),
        h.retry(chat, pair.request_id)
    );
    let (a, b) = (Outcome::of(a).await, Outcome::of(b).await);

    assert!(a.is_stream() != b.is_stream(), "exactly one stream");
    let (stream, loser) = if a.is_stream() { (a, b) } else { (b, a) };
    let Outcome::Rejected(loser) = loser else {
        unreachable!()
    };
    assert_loss(&loser, &LOSS_REASONS);
    assert_eq!(running(&h, chat).await, 1, "at most one running turn");
    assert_eq!(
        h.turns_in(chat).await.len(),
        2,
        "the loser inserted nothing"
    );

    h.oagw.release();
    let sent = finish(stream).await.unwrap();
    assert_eq!(sent.terminal().event, "done");
    let old = h.turn(pair.request_id).await;
    assert_eq!(old.replaced_by_request_id, Some(sent.request_id()));
    assert_eq!(
        h.messages_in(chat).await.len(),
        4,
        "the loser stored no message"
    );
    assert_eq!(h.live_texts(chat).await.len(), 2);
    assert_eq!(h.oagw.request_count(), 1, "one provider call");
    h.assert_nothing_reserved().await;
    h.policy.wait_published(1).await;
    h.policy.assert_published_stays(1).await;
    let audit = h.audit.wait_events(2).await;
    h.audit.assert_events_stay(2).await;
    assert_eq!(
        audit
            .iter()
            .filter(|e| e.event_type() == "turn_retry")
            .count(),
        1,
        "the loser emitted no mutation event: {audit:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_retries_never_produce_a_500_and_always_one_winner() {
    // the provider answers instantly: a winner often finalizes before the loser re-reads, and the
    // loser must still be a 409 about the replaced / running turn
    let h = MutationHarness::new().await;
    h.oagw.set_default(|| ProviderReply::text(&["quick"]));
    for _ in 0..12 {
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let rid = pair.request_id;
        let raws = futures::future::join_all((0..5).map(|_| h.retry(chat, rid))).await;
        let mut winners = Vec::new();
        for raw in raws {
            match Outcome::of(raw).await {
                Outcome::Stream(raw) => winners.push(raw.collect().await),
                Outcome::Rejected(resp) => assert_loss(&resp, &LOSS_REASONS),
            }
        }
        assert_eq!(winners.len(), 1, "exactly one winner");
        assert_eq!(winners[0].terminal().event, "done");
        let turns = h.turns_in(chat).await;
        assert_eq!(turns.len(), 2);
        assert_eq!(
            turns.iter().filter(|t| t.deleted_at.is_none()).count(),
            1,
            "one live turn"
        );
        assert_eq!(
            h.turn(rid).await.replaced_by_request_id,
            Some(winners[0].request_id())
        );
    }
}

#[tokio::test]
async fn a_turn_whose_user_message_was_deleted_between_the_reads_is_409_not_a_500() {
    // The preview reads the turn, the latest turn and the user message without a transaction.
    // A winner that commits in between leaves the loser a turn that still looked live but whose
    // user message is gone (it is soft-deleted together with the turn). That is the lost race of
    // rule 7: 409 `NOT_LATEST_TURN`, never "turn has no user message" as a 500. The state is
    // forced here: a live latest turn whose user message is soft-deleted.
    for edit in [false, true] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let request_id = Uuid::new_v4();
        h.chat
            .seed_message(
                MessageSeed::user(chat)
                    .request(Some(request_id))
                    .content(Q)
                    .at(10)
                    .deleted(),
            )
            .await;
        h.insert_turn(
            TurnRowSeed::terminal(chat, "completed")
                .request(request_id)
                .started(ts(10))
                .requester(Some(USER_A)),
        )
        .await;
        let before = h.snapshot().await;

        let raw = if edit {
            h.edit(chat, request_id, "an edited question").await
        } else {
            h.retry(chat, request_id).await
        };
        assert!(!raw.is_sse(), "edit={edit}: no stream for a lost race");
        let resp = raw.into_resp().await;

        assert_conflict(&resp, "NOT_LATEST_TURN");
        assert_eq!(h.snapshot().await, before, "edit={edit}: nothing changed");
        h.oagw.assert_no_provider_call();
        h.audit.assert_events_stay(0).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_store_only_the_winners_content() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.oagw.push(ProviderReply::text_with(
        &["slow"],
        &Opts::default().pause_after(0),
    ));

    let (a, b) = tokio::join!(
        h.edit(chat, pair.request_id, "content of the first editor"),
        h.edit(chat, pair.request_id, "content of the second editor")
    );
    let (a, b) = (Outcome::of(a).await, Outcome::of(b).await);
    assert!(a.is_stream() != b.is_stream(), "exactly one stream");
    let (winner, a_won) = if a.is_stream() { (a, true) } else { (b, false) };

    h.oagw.release();
    let sent = finish(winner).await.unwrap();
    assert_eq!(sent.terminal().event, "done");
    let expected = if a_won {
        "content of the first editor"
    } else {
        "content of the second editor"
    };
    let texts = h.live_texts(chat).await;
    assert_eq!(texts[0], ("user".to_owned(), expected.to_owned()));
    assert_eq!(texts.len(), 2);
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![format!("user:{expected}")]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retry_racing_an_edit_yields_one_stream_and_one_409() {
    let h = MutationHarness::new().await;
    h.oagw.set_default(|| ProviderReply::text(&["quick"]));
    for _ in 0..8 {
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let (a, b) = tokio::join!(
            h.retry(chat, pair.request_id),
            h.edit(chat, pair.request_id, "an edited question")
        );
        let (a, b) = (Outcome::of(a).await, Outcome::of(b).await);
        assert!(a.is_stream() != b.is_stream(), "exactly one winner");
        for outcome in [a, b] {
            match outcome {
                Outcome::Stream(raw) => {
                    assert_eq!(raw.collect().await.terminal().event, "done");
                }
                Outcome::Rejected(resp) => assert_loss(&resp, &LOSS_REASONS),
            }
        }
        assert_eq!(h.turns_in(chat).await.len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_racing_a_retry_has_exactly_one_winner() {
    let h = MutationHarness::new().await;
    h.oagw.set_default(|| ProviderReply::text(&["quick"]));
    for _ in 0..8 {
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let (retry, delete) = tokio::join!(
            h.retry(chat, pair.request_id),
            h.delete(chat, pair.request_id)
        );
        let retry = Outcome::of(retry).await;
        let deleted = delete.status.as_u16() == 204;
        assert!(
            retry.is_stream() != deleted,
            "exactly one of the two succeeds (retry stream: {}, delete: {})",
            retry.is_stream(),
            delete.status
        );
        match retry {
            Outcome::Stream(raw) => {
                assert_eq!(raw.collect().await.terminal().event, "done");
                assert_conflict(&delete, "NOT_LATEST_TURN");
                assert_eq!(h.live_texts(chat).await.len(), 2);
            }
            Outcome::Rejected(resp) => {
                assert_conflict(&resp, "NOT_LATEST_TURN");
                assert!(h.live_texts(chat).await.is_empty());
                assert_eq!(
                    h.turns_in(chat).await.len(),
                    1,
                    "the retry inserted nothing"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_deletes_have_exactly_one_winner() {
    let h = MutationHarness::new().await;
    for _ in 0..8 {
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let (a, b) = tokio::join!(
            h.delete(chat, pair.request_id),
            h.delete(chat, pair.request_id)
        );
        let mut statuses = [a.status.as_u16(), b.status.as_u16()];
        statuses.sort_unstable();
        assert_eq!(statuses, [204, 409], "{} / {}", a.raw, b.raw);
        let loser = if a.status.as_u16() == 409 { &a } else { &b };
        assert_conflict(loser, "NOT_LATEST_TURN");
        assert!(h.turn(pair.request_id).await.deleted_at.is_some());
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// mutations racing sends and each other across chats
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retry_racing_a_send_leaves_at_most_one_running_turn() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.oagw.push(ProviderReply::text_with(
        &["slow"],
        &Opts::default().pause_after(0),
    ));
    let send_body = json!({"content": "a brand new question", "request_id": Uuid::new_v4()});
    let ctx = ctx_a();

    let (retry, send) = tokio::join!(
        h.retry(chat, pair.request_id),
        h.open(chat, &ctx, &send_body)
    );
    let (retry, send) = (Outcome::of(retry).await, Outcome::of(send).await);

    assert!(retry.is_stream() != send.is_stream(), "exactly one stream");
    assert_eq!(running(&h, chat).await, 1);
    match (&retry, &send) {
        (Outcome::Stream(_), Outcome::Rejected(resp)) => {
            assert_conflict(resp, "turn_already_running");
        }
        (Outcome::Rejected(resp), Outcome::Stream(_)) => assert_loss(resp, &LOSS_REASONS),
        _ => unreachable!(),
    }
    h.oagw.release();
    let (retry, send) = (finish(retry).await, finish(send).await);
    let winner = retry.or(send).unwrap();
    assert_eq!(winner.terminal().event, "done");
    assert_eq!(running(&h, chat).await, 0);
    h.assert_nothing_reserved().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutations_of_different_chats_do_not_interfere() {
    let h = MutationHarness::new().await;
    h.oagw.set_default(|| ProviderReply::text(&["quick"]));
    let mut chats = Vec::new();
    for _ in 0..4 {
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        chats.push((chat, pair.request_id));
    }
    let raws =
        futures::future::join_all(chats.iter().map(|(chat, rid)| h.retry(*chat, *rid))).await;
    for raw in raws {
        assert!(raw.is_sse(), "the guard is per chat");
        assert_eq!(raw.collect().await.terminal().event, "done");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// the unique-index race is classified, not surfaced
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_lost_running_turn_index_race_is_409_generation_in_progress_and_rolls_back() {
    // DESIGN 3.9 rule 7: the latest turn passes every check, but another non-deleted turn of the
    // chat is `running` (here an older stuck turn): the insert of the new running turn violates the
    // one-running-turn-per-chat index. The answer is 409 `GENERATION_IN_PROGRESS` and the
    // transaction - including the soft delete - rolls back.
    for edit in [false, true] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        h.insert_turn(TurnRowSeed::running(chat).started(ts(5)))
            .await;
        let latest = h.seed_pair(chat, PairSeed::completed(Q, "A").at(20)).await;
        let before = h.snapshot().await;

        let raw = if edit {
            h.edit(chat, latest.request_id, "an edited question").await
        } else {
            h.retry(chat, latest.request_id).await
        };
        assert!(!raw.is_sse(), "no stream for a lost race");
        let resp = raw.into_resp().await;

        assert_conflict(&resp, "GENERATION_IN_PROGRESS");
        assert_eq!(
            h.snapshot().await,
            before,
            "edit={edit}: nothing was committed"
        );
        assert!(h.turn(latest.request_id).await.deleted_at.is_none());
        h.oagw.assert_no_provider_call();
        h.audit.assert_events_stay(0).await;
    }
}
