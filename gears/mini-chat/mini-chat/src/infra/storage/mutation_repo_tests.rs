//! The turn mutation persistence over in-memory `SQLite` and the shared outbox: the latest-turn
//! query (fractional seconds of different lengths, the `id` tie-break, every state, soft-deleted
//! turns), the mutation transaction (the soft-delete of the turn and both messages,
//! `replaced_by_request_id` on retry and edit only, the replacement message after every existing
//! message, the copied links, the replacement turn with NULL preflight columns and `started_at`
//! equal to the preflight instant, the activity bump on retry and edit and none on delete, one
//! audit outbox row partitioned by the tenant), the rollback on each rejection with no row left,
//! the summary invalidation, the reserve write through `ReserveTurn::Existing` and the
//! unstarted-turn CAS.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn` (`inst-lt-select`,
//! `inst-lt-sort-key`, `inst-lt-every-state`, `inst-lt-skip-deleted`, `inst-lt-return`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck` (`inst-lr-reread`,
//! `inst-lr-deleted`, `inst-lr-return-deleted`, `inst-lr-not-terminal`,
//! `inst-lr-return-not-terminal`, `inst-lr-latest`, `inst-lr-not-latest`,
//! `inst-lr-return-not-latest`, `inst-lr-return`), `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit`
//! (`inst-mc-begin`, `inst-mc-recheck`, `inst-mc-soft-delete-turn`, `inst-mc-soft-delete-lost`,
//! `inst-mc-return-soft-delete-lost`, `inst-mc-soft-delete-messages`, `inst-mc-replace`,
//! `inst-mc-insert-user-message`, `inst-mc-copy-links`, `inst-mc-insert-turn`,
//! `inst-mc-running-race`, `inst-mc-return-generation-in-progress`, `inst-mc-key-collision`,
//! `inst-mc-return-key-collision`, `inst-mc-activity`, `inst-mc-delete-no-activity`,
//! `inst-mc-summary`, `inst-mc-audit`, `inst-mc-payload-too-large`, `inst-mc-return-payload-500`,
//! `inst-mc-db-error`, `inst-mc-return-500`, `inst-mc-commit`, `inst-mc-atomic`, `inst-mc-return`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation` (`inst-si-after-soft-delete`,
//! `inst-si-select`, `inst-si-no-summary`, `inst-si-covers`, `inst-si-delete`,
//! `inst-si-clear-compressed`, `inst-si-return-invalidated`, `inst-si-keep`, `inst-si-no-generation`,
//! `inst-si-return-kept`), `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order`
//! (`inst-mo-user-after-all`, `inst-mo-exclude-deleted`, `inst-mo-count`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward` (`inst-acf-select`,
//! `inst-acf-only-original`), `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`
//! (`inst-tcf-counters`), `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup`
//! (`inst-pcs-reserve`, `inst-pcs-over-limit`, `inst-pcs-reserve-failed`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure` (`inst-uf-cas`,
//! `inst-uf-cas-lost`, `inst-uf-return-cas-lost`, `inst-uf-no-settlement`); states
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility` (`inst-tv-to-replaced`,
//! `inst-tv-to-deleted`, `inst-tv-final`) and
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup`
//! (`inst-rts-to-reserved`, `inst-rts-to-failed-setup`, `inst-rts-immutable`); definitions of
//! done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-latest-terminal-turn`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-delete`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-mutation-transaction`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-carry-forward`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-summary-invalidation`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-audit-events` and
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-post-commit-setup`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::AuditEvent;
use time::Duration;
use uuid::Uuid;

use crate::domain::error::{ConflictReason, DomainError, EnqueueSite, QuotaScope};
use crate::domain::mutations::store::Replacement;
use crate::domain::mutations::{
    CommitCommand, CommittedMutation, ExistingReserve, MutationOp, MutationStore, UnstartedFailure,
};
use crate::infra::audit_gateway::AUDIT_PAYLOAD_TYPE;
use crate::infra::outbox::partition_for;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{
    a1, at, chat_row, fail_outbox_inserts, message_rows, outbox_body_count, outbox_rows, seed_chat,
};
use crate::test_support::mutations::{
    FailingMutationOutbox, SeededTurn, Snapshot, live_messages, mark_compressed, seed, snapshot,
    soft_delete_attachment, summary_row, turn, turn_by_request,
};
use crate::test_support::quota::{
    STANDARD, all_quota_rows, default_now, plain_facts, reserved_of, row, seed_rows,
};
use crate::test_support::send::{
    AttachmentSeed, MessageSeed, SendRig, SendRigOptions, TurnSeedFull, chat_model, link_rows,
    seed_att, seed_msg, seed_summary, seed_turn_full, send_rig, send_rig_with, t, turn_rows,
};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1, raw_exec};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";

// ── fixtures ─────────────────────────────────────────────────────────────────

async fn fixture() -> (SendRig, ChatRecord) {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    (rig, chat)
}

/// The replacement of a retry or edit with fresh ids; the preflight instant is five seconds
/// before the commit instant.
fn replacement(content: &str, attachments: &[Uuid]) -> Replacement {
    Replacement {
        request_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        user_message_id: Uuid::new_v4(),
        content: content.to_owned(),
        attachment_ids: attachments.to_vec(),
        web_search_enabled: false,
        started_at: default_now() - Duration::seconds(5),
    }
}

fn command(
    chat: &ChatRecord,
    op: MutationOp,
    target: &SeededTurn,
    replacement: Option<Replacement>,
) -> CommitCommand {
    CommitCommand {
        tenant_id: chat.tenant_id,
        user_id: chat.user_id,
        chat_id: chat.id,
        op,
        target_request_id: target.request_id,
        replacement,
        now: default_now(),
    }
}

async fn commit(rig: &SendRig, command: &CommitCommand) -> Result<CommittedMutation, DomainError> {
    MutationStore::commit_mutation(
        &*rig.quota.db.provider,
        Arc::clone(&rig.turn_outbox),
        command,
    )
    .await
}

async fn retry_commit(
    rig: &SendRig,
    chat: &ChatRecord,
    target: &SeededTurn,
) -> (Replacement, CommittedMutation) {
    let replacement = replacement("original text", &[]);
    let committed = commit(
        rig,
        &command(chat, MutationOp::Retry, target, Some(replacement.clone())),
    )
    .await
    .expect("the mutation commits");
    (replacement, committed)
}

fn conflict_of<T: std::fmt::Debug>(result: Result<T, DomainError>) -> ConflictReason {
    match result {
        Err(DomainError::Conflict { reason }) => reason,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

async fn latest(rig: &SendRig, chat: &ChatRecord) -> Option<Uuid> {
    MutationStore::latest_turn(&*rig.quota.db.provider, chat.tenant_id, chat.id)
        .await
        .unwrap()
        .map(|row| row.request_id)
}

async fn assert_rolled_back(rig: &SendRig, chat: &ChatRecord, before: &Snapshot, what: &str) {
    assert_eq!(
        &snapshot(&rig.quota.db, chat).await,
        before,
        "{what}: rows were left behind"
    );
}

// ── the latest-turn query ────────────────────────────────────────────────────

#[tokio::test]
async fn a_chat_without_turns_has_no_latest_turn() {
    // inst-lt-return
    let (rig, chat) = fixture().await;
    assert_eq!(latest(&rig, &chat).await, None);
}

#[tokio::test]
async fn the_latest_turn_has_the_greatest_started_at() {
    // inst-lt-select
    let (rig, chat) = fixture().await;
    seed(&rig.quota.db, &chat, turn("a").at(10)).await;
    let newest = seed(&rig.quota.db, &chat, turn("b").at(300)).await;
    seed(&rig.quota.db, &chat, turn("c").at(200)).await;
    assert_eq!(latest(&rig, &chat).await, Some(newest.request_id));
}

#[tokio::test]
async fn the_latest_turn_follows_started_at_not_the_time_of_the_messages() {
    // inst-lt-select: `(started_at, id)` of `chat_turns` is the key
    let (rig, chat) = fixture().await;
    let started_late = seed(&rig.quota.db, &chat, turn("a").at(100).started(900)).await;
    seed(&rig.quota.db, &chat, turn("b").at(500).started(20)).await;
    assert_eq!(latest(&rig, &chat).await, Some(started_late.request_id));
}

#[tokio::test]
async fn every_state_counts_and_a_newer_running_turn_is_the_latest() {
    // inst-lt-every-state
    let (rig, chat) = fixture().await;
    seed(&rig.quota.db, &chat, turn("done").at(10)).await;
    let running = seed(&rig.quota.db, &chat, turn("live").at(20).running()).await;
    assert_eq!(latest(&rig, &chat).await, Some(running.request_id));
    for (index, plan) in [
        turn("f").at(30).failed(),
        turn("c").at(40).cancelled(Some("partial")),
    ]
    .into_iter()
    .enumerate()
    {
        let chat = rig.std_chat().await;
        seed(&rig.quota.db, &chat, turn("base").at(5)).await;
        let newer = seed(&rig.quota.db, &chat, plan).await;
        assert_eq!(
            latest(&rig, &chat).await,
            Some(newer.request_id),
            "case {index}"
        );
    }
}

#[tokio::test]
async fn a_soft_deleted_turn_is_never_the_latest_turn() {
    // inst-lt-skip-deleted
    let (rig, chat) = fixture().await;
    let older = seed(&rig.quota.db, &chat, turn("older").at(10)).await;
    let newer = seed(&rig.quota.db, &chat, turn("newer").at(100)).await;
    assert_eq!(latest(&rig, &chat).await, Some(newer.request_id));
    raw_exec(
        &rig.quota.db.raw,
        &format!(
            "UPDATE chat_turns SET deleted_at = '2026-01-01 00:00:00+00:00' WHERE id = {}",
            crate::test_support::chats::blob(newer.turn_id)
        ),
    )
    .await
    .unwrap();
    assert_eq!(latest(&rig, &chat).await, Some(older.request_id));
}

#[tokio::test]
async fn started_at_values_with_fractions_of_different_lengths_are_ordered_chronologically() {
    // inst-lt-sort-key: the stored texts are `…:20Z`, `…:20.12Z`, `…:20.123Z`, `…:20.1234Z`,
    // `…:20.123456Z`, `…:20.5Z`, `…:20.999999Z` and `…:21Z`; raw text order would put `.12Z`
    // after `.123Z`
    let (rig, chat) = fixture().await;
    let offsets: [i64; 8] = [
        500_000, 0, 123_456, 1_000_000, 120_000, 999_999, 123_400, 123_000,
    ];
    let mut by_offset = Vec::new();
    for micros in offsets {
        let seeded = seed(&rig.quota.db, &chat, turn("x").at(micros)).await;
        by_offset.push((micros, seeded.request_id));
    }
    by_offset.sort_by_key(|(micros, _)| *micros);
    // delete the newest one by one: the latest turn walks back chronologically
    for (_, request_id) in by_offset.iter().rev() {
        assert_eq!(latest(&rig, &chat).await, Some(*request_id));
        raw_exec(
            &rig.quota.db.raw,
            &format!(
                "UPDATE chat_turns SET deleted_at = '2026-01-01 00:00:00+00:00' \
                 WHERE request_id = {}",
                crate::test_support::chats::blob(*request_id)
            ),
        )
        .await
        .unwrap();
    }
    assert_eq!(latest(&rig, &chat).await, None);
}

#[tokio::test]
async fn equal_started_at_values_are_broken_by_the_greatest_id() {
    // inst-lt-sort-key: `id` is the deterministic tie-break
    let (rig, chat) = fixture().await;
    let mut ids = Vec::new();
    for low in [
        0x0000_0000_0000_0000_0000_0000_0000_0001_u128,
        u128::MAX >> 1,
        u128::MAX,
    ] {
        let id = Uuid::from_u128(low);
        seed_turn_full(
            &rig.quota.db,
            &chat,
            TurnSeedFull::new("completed").id(id).started(t(77)),
        )
        .await;
        ids.push(id);
    }
    let row = MutationStore::latest_turn(&*rig.quota.db.provider, chat.tenant_id, chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.id, *ids.iter().max().unwrap());
}

#[tokio::test]
async fn the_latest_turn_query_is_scoped_to_the_tenant_and_the_chat() {
    let (rig, chat) = fixture().await;
    let other = rig.std_chat().await;
    seed(&rig.quota.db, &chat, turn("mine")).await;
    let foreign = seed(&rig.quota.db, &other, turn("theirs").at(500)).await;
    assert_ne!(latest(&rig, &chat).await, Some(foreign.request_id));
    let wrong_tenant = MutationStore::latest_turn(&*rig.quota.db.provider, TENANT_B, chat.id).await;
    assert!(
        !matches!(wrong_tenant, Ok(Some(_))),
        "another tenant must not see the turn: {wrong_tenant:?}"
    );
}

// ── the reads of the preview and of the preflight ────────────────────────────

#[tokio::test]
async fn the_target_lookup_returns_soft_deleted_turns_with_their_web_search_flag() {
    // inst-mp-select-target: a soft-deleted row is included
    let (rig, chat) = fixture().await;
    let live = seed(&rig.quota.db, &chat, turn("live").web_search()).await;
    let found = MutationStore::find_mutation_turn(
        &*rig.quota.db.provider,
        chat.tenant_id,
        chat.id,
        live.request_id,
    )
    .await
    .unwrap()
    .expect("the turn");
    assert!(found.web_search_enabled);
    assert_eq!(found.row.id, live.turn_id);
    assert!(found.row.deleted_at.is_none());

    commit(&rig, &command(&chat, MutationOp::Delete, &live, None))
        .await
        .unwrap();
    let deleted = MutationStore::find_mutation_turn(
        &*rig.quota.db.provider,
        chat.tenant_id,
        chat.id,
        live.request_id,
    )
    .await
    .unwrap()
    .expect("a soft-deleted turn is still found");
    assert_eq!(deleted.row.deleted_at, Some(default_now()));

    let unknown = MutationStore::find_mutation_turn(
        &*rig.quota.db.provider,
        chat.tenant_id,
        chat.id,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    assert!(unknown.is_none());
}

#[tokio::test]
async fn the_target_user_message_is_the_live_user_message_of_the_turn() {
    // inst-mp-select-user-message
    let (rig, chat) = fixture().await;
    let seeded = seed(&rig.quota.db, &chat, turn("the question")).await;
    let found = MutationStore::target_user_message(
        &*rig.quota.db.provider,
        chat.tenant_id,
        chat.id,
        seeded.request_id,
    )
    .await
    .unwrap()
    .expect("the user message");
    assert_eq!(found.id, seeded.user_message_id);
    assert_eq!(found.role, "user");
    assert_eq!(found.content, "the question");

    crate::test_support::chats::soft_delete_message(&rig.quota.db, seeded.user_message_id).await;
    let gone = MutationStore::target_user_message(
        &*rig.quota.db.provider,
        chat.tenant_id,
        chat.id,
        seeded.request_id,
    )
    .await
    .unwrap();
    assert!(
        gone.is_none(),
        "a soft-deleted message is not the target message"
    );
}

#[tokio::test]
async fn the_mutation_reads_exclude_the_target_messages_from_the_boundary_and_flag_deleted_links() {
    // inst-mpf-snapshot-boundary, inst-acf-select, inst-mpf-read-facts
    let (rig, chat) = fixture().await;
    let db = &rig.quota.db;
    let image = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let gone = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let first = seed(db, &chat, turn("first").at(10)).await;
    let second = seed(
        db,
        &chat,
        turn("second").at(100).attach(image, 105).attach(gone, 106),
    )
    .await;
    soft_delete_attachment(db, gone).await;
    let reads =
        MutationStore::read_mutation(&*db.provider, chat.tenant_id, chat.id, second.request_id)
            .await
            .unwrap();
    let boundary = reads.boundary.expect("the first turn bounds the history");
    assert_eq!(boundary.id, first.assistant_message_id.unwrap());
    assert_eq!(boundary.created_at, t(11));
    let by_id = |id: Uuid| reads.linked.iter().find(|l| l.fact.id == id).unwrap();
    assert_eq!(reads.linked.len(), 2);
    assert!(!by_id(image).deleted);
    assert!(by_id(gone).deleted);
    assert_eq!(by_id(image).link_created_at, t(105));
    assert_eq!(by_id(image).fact.kind, "image");
    assert!(by_id(image).fact.provider_file_id.is_some());
    assert_eq!(reads.ready_documents, 0, "the only document is deleted");
}

#[tokio::test]
async fn the_boundary_of_the_mutation_reads_follows_the_chronological_order_with_fractions() {
    let (rig, chat) = fixture().await;
    let db = &rig.quota.db;
    // the other turn's messages sit at `.12` and `.123`: the later instant bounds the history
    let other = Uuid::new_v4();
    seed_msg(
        db,
        &chat,
        MessageSeed::new("user", "u", t(120_000)).request(Some(other)),
    )
    .await;
    let late = seed_msg(
        db,
        &chat,
        MessageSeed::new("assistant", "a", t(123_000)).request(Some(other)),
    )
    .await;
    let target = seed(db, &chat, turn("target").at(5_000_000)).await;
    let reads =
        MutationStore::read_mutation(&*db.provider, chat.tenant_id, chat.id, target.request_id)
            .await
            .unwrap();
    let boundary = reads.boundary.unwrap();
    assert_eq!(boundary.id, late);
    assert_eq!(boundary.created_at, t(123_000));
}

// ── the mutation transaction: retry ──────────────────────────────────────────

#[tokio::test]
async fn a_retry_soft_deletes_the_turn_and_both_messages_and_points_to_the_replacement() {
    // inst-mc-soft-delete-turn, inst-mc-soft-delete-messages, inst-tv-to-replaced
    let (rig, chat) = fixture().await;
    let target = seed(
        &rig.quota.db,
        &chat,
        turn("question").answer("answer").counts(1, 2, 3),
    )
    .await;
    let before_turn = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    let (replacement, _) = retry_commit(&rig, &chat, &target).await;

    let after = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(after.deleted_at, Some(default_now()));
    assert_eq!(after.updated_at, default_now());
    assert_eq!(after.replaced_by_request_id, Some(replacement.request_id));
    assert_eq!(after.state, before_turn.state);
    assert_eq!(after.assistant_message_id, before_turn.assistant_message_id);
    // inst-tcf-counters: the replaced turn keeps its counters
    assert_eq!(
        (
            after.web_search_completed_count,
            after.code_interpreter_completed_count,
            after.file_search_completed_count
        ),
        (1, 2, 3)
    );
    for id in [Some(target.user_message_id), target.assistant_message_id]
        .into_iter()
        .flatten()
    {
        let message = message_rows(&rig.quota.db, chat.id)
            .await
            .into_iter()
            .find(|m| m.id == id)
            .unwrap();
        assert_eq!(message.deleted_at, Some(default_now()), "{id}");
    }
    let content: Vec<String> = message_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .filter(|m| m.request_id == Some(target.request_id))
        .map(|m| m.content)
        .collect();
    assert_eq!(content.len(), 2);
    assert!(content.contains(&"question".to_owned()) && content.contains(&"answer".to_owned()));
}

#[tokio::test]
async fn a_retry_inserts_the_replacement_user_message_with_the_columns_of_the_contract() {
    // inst-mc-insert-user-message
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let (replacement, committed) = retry_commit(&rig, &chat, &target).await;
    let message = message_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .find(|m| m.id == replacement.user_message_id)
        .expect("the replacement message");
    assert_eq!(message.tenant_id, TENANT_A);
    assert_eq!(message.chat_id, chat.id);
    assert_eq!(message.request_id, Some(replacement.request_id));
    assert_eq!(message.role, "user");
    assert_eq!(message.content, "original text");
    assert_eq!(message.content_type, "text");
    assert_eq!(message.token_estimate, 0);
    assert_eq!(message.request_kind, "chat");
    assert_eq!(message.features_used, serde_json::json!([]));
    assert_eq!(
        (
            message.input_tokens,
            message.output_tokens,
            message.cache_read_input_tokens
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (message.cache_write_input_tokens, message.reasoning_tokens),
        (0, 0)
    );
    assert_eq!(message.model, None);
    assert!(!message.is_compressed);
    assert_eq!(message.deleted_at, None);
    let committed = committed.replacement.expect("a replacement");
    assert_eq!(committed.user_message_id, replacement.user_message_id);
    assert_eq!(committed.request_id, replacement.request_id);
    assert_eq!(committed.turn_id, replacement.turn_id);
    assert_eq!(committed.user_message_created_at, message.created_at);
    assert_eq!(
        message_rows(&rig.quota.db, chat.id)
            .await
            .iter()
            .filter(|m| m.request_id == Some(replacement.request_id))
            .count(),
        1,
        "no assistant row exists for the replacement turn"
    );
}

#[tokio::test]
async fn an_edit_stores_the_validated_content_exactly_as_given() {
    // inst-mc-insert-user-message: the edited content, never trimmed
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("old")).await;
    let replacement = replacement("  edited \n text ", &[]);
    commit(
        &rig,
        &command(&chat, MutationOp::Edit, &target, Some(replacement.clone())),
    )
    .await
    .unwrap();
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].content, "  edited \n text ");
    let old = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(old.replaced_by_request_id, Some(replacement.request_id));
}

#[tokio::test]
async fn the_replacement_message_is_created_after_every_existing_message() {
    // inst-mo-user-after-all: the application clock when it is later
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let (replacement, _) = retry_commit(&rig, &chat, &target).await;
    let rows = message_rows(&rig.quota.db, chat.id).await;
    let new = rows
        .iter()
        .find(|m| m.id == replacement.user_message_id)
        .unwrap();
    assert_eq!(new.created_at, default_now());
    assert!(
        rows.iter()
            .filter(|m| m.id != new.id)
            .all(|m| m.created_at < new.created_at)
    );
}

#[tokio::test]
async fn a_clock_behind_the_latest_message_moves_the_replacement_one_microsecond_after_it() {
    // inst-mo-user-after-all: the just soft-deleted messages count too
    let (rig, chat) = fixture().await;
    let ahead = default_now() + Duration::hours(1);
    let user = Uuid::new_v4();
    let request = Uuid::new_v4();
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "q", ahead)
            .id(user)
            .request(Some(request)),
    )
    .await;
    let assistant = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "a", ahead + Duration::microseconds(1))
            .request(Some(request)),
    )
    .await;
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("completed")
            .request(request)
            .assistant(assistant)
            .started(t(10)),
    )
    .await;
    let target = SeededTurn {
        request_id: request,
        turn_id: Uuid::nil(),
        user_message_id: user,
        assistant_message_id: Some(assistant),
    };
    let (replacement, _) = retry_commit(&rig, &chat, &target).await;
    let rows = message_rows(&rig.quota.db, chat.id).await;
    let new = rows
        .iter()
        .find(|m| m.id == replacement.user_message_id)
        .unwrap();
    assert_eq!(new.created_at, ahead + Duration::microseconds(2));
}

#[tokio::test]
async fn the_replacement_time_follows_the_chronological_maximum_with_fractions_of_different_lengths()
 {
    // inst-mo-user-after-all, implementation note 1: `.12` is earlier than `.123`
    let (rig, chat) = fixture().await;
    let base = default_now() + Duration::days(1);
    let request = Uuid::new_v4();
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "q", base + Duration::microseconds(120_000))
            .request(Some(request)),
    )
    .await;
    let latest = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "a", base + Duration::microseconds(123_000))
            .request(Some(request)),
    )
    .await;
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("completed")
            .request(request)
            .started(t(10)),
    )
    .await;
    let target = SeededTurn {
        request_id: request,
        turn_id: Uuid::nil(),
        user_message_id: Uuid::nil(),
        assistant_message_id: Some(latest),
    };
    let (replacement, _) = retry_commit(&rig, &chat, &target).await;
    let rows = message_rows(&rig.quota.db, chat.id).await;
    let new = rows
        .iter()
        .find(|m| m.id == replacement.user_message_id)
        .unwrap();
    assert_eq!(new.created_at, base + Duration::microseconds(123_001));
}

#[tokio::test]
async fn the_attachment_links_are_copied_and_the_original_rows_stay() {
    // inst-mc-copy-links: tenant and chat from the message row, original rows kept
    let (rig, chat) = fixture().await;
    let db = &rig.quota.db;
    let a = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let b = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let target = seed(db, &chat, turn("files").attach(a, 20).attach(b, 30)).await;
    let replacement = replacement("files", &[a, b]);
    commit(
        &rig,
        &command(&chat, MutationOp::Retry, &target, Some(replacement.clone())),
    )
    .await
    .unwrap();
    let links = link_rows(db, chat.id).await;
    assert_eq!(links.len(), 4);
    for link in &links {
        assert_eq!(link.tenant_id, TENANT_A);
        assert_eq!(link.chat_id, chat.id);
    }
    let of = |message: Uuid| {
        let mut ids: Vec<Uuid> = links
            .iter()
            .filter(|l| l.message_id == message)
            .map(|l| l.attachment_id)
            .collect();
        ids.sort();
        ids
    };
    let mut wanted = vec![a, b];
    wanted.sort();
    assert_eq!(of(target.user_message_id), wanted, "the original rows stay");
    assert_eq!(
        of(replacement.user_message_id),
        wanted,
        "new rows on the replacement"
    );
}

#[tokio::test]
async fn only_the_carried_attachments_are_linked_to_the_replacement() {
    // inst-mc-copy-links: the carry-forward excluded the deleted one before the commit
    let (rig, chat) = fixture().await;
    let db = &rig.quota.db;
    let kept = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let dropped = seed_att(db, &chat, AttachmentSeed::ready("image").deleted(at(1))).await;
    let target = seed(
        db,
        &chat,
        turn("files").attach(kept, 20).attach(dropped, 30),
    )
    .await;
    let replacement = replacement("files", &[kept]);
    commit(
        &rig,
        &command(&chat, MutationOp::Retry, &target, Some(replacement.clone())),
    )
    .await
    .unwrap();
    let on_new: Vec<Uuid> = link_rows(db, chat.id)
        .await
        .into_iter()
        .filter(|l| l.message_id == replacement.user_message_id)
        .map(|l| l.attachment_id)
        .collect();
    assert_eq!(on_new, vec![kept]);
}

#[tokio::test]
async fn the_replacement_turn_is_running_with_null_preflight_columns_and_the_preflight_instant() {
    // inst-mc-insert-turn
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let mut replacement = replacement("q", &[]);
    replacement.web_search_enabled = true;
    commit(
        &rig,
        &command(&chat, MutationOp::Retry, &target, Some(replacement.clone())),
    )
    .await
    .unwrap();
    let new = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
    assert_eq!(new.id, replacement.turn_id);
    assert_eq!(new.tenant_id, TENANT_A);
    assert_eq!(new.chat_id, chat.id);
    assert_eq!(new.state, "running");
    assert_eq!(new.requester_type, "user");
    assert_eq!(new.requester_user_id, Some(USER_A1));
    assert_eq!(
        new.started_at, replacement.started_at,
        "the preflight instant"
    );
    assert_eq!(
        new.last_progress_at,
        Some(default_now()),
        "the insert instant"
    );
    assert!(new.web_search_enabled, "the carried flag");
    assert_eq!(new.reserve_tokens, None);
    assert_eq!(new.max_output_tokens_applied, None);
    assert_eq!(new.reserved_credits_micro, None);
    assert_eq!(new.policy_version_applied, None);
    assert_eq!(new.effective_model, None);
    assert_eq!(new.minimal_generation_floor_applied, None);
    assert_eq!(new.provider_name, None);
    assert_eq!(new.provider_response_id, None);
    assert_eq!(new.assistant_message_id, None);
    assert_eq!(new.error_code, None);
    assert_eq!(
        (
            new.web_search_completed_count,
            new.code_interpreter_completed_count,
            new.file_search_completed_count
        ),
        (0, 0, 0)
    );
    assert_eq!(new.completed_at, None);
    assert_eq!(new.deleted_at, None);
    assert_eq!(new.replaced_by_request_id, None);
}

#[tokio::test]
async fn the_replacement_turn_becomes_the_latest_turn_and_the_replaced_one_is_hidden() {
    // inst-tv-to-replaced, inst-lt-skip-deleted
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let (replacement, _) = retry_commit(&rig, &chat, &target).await;
    assert_eq!(latest(&rig, &chat).await, Some(replacement.request_id));
}

#[tokio::test]
async fn retry_and_edit_bump_the_chat_activity_and_delete_does_not() {
    // inst-mc-activity, inst-mc-delete-no-activity
    for (op, bumped) in [
        (MutationOp::Retry, true),
        (MutationOp::Edit, true),
        (MutationOp::Delete, false),
    ] {
        let (rig, chat) = fixture().await;
        let target = seed(&rig.quota.db, &chat, turn("q")).await;
        let replacement = op.replaces().then(|| replacement("q", &[]));
        commit(&rig, &command(&chat, op, &target, replacement))
            .await
            .unwrap();
        let updated = chat_model(&rig.quota.db, chat.id).await.updated_at;
        if bumped {
            assert!(
                updated > at(0),
                "{op:?}: updated_at did not move: {updated}"
            );
        } else {
            assert_eq!(updated, at(0), "{op:?}: updated_at moved");
        }
    }
}

#[tokio::test]
async fn a_mutation_adds_no_quota_row_and_no_usage_event() {
    // inst-tcf-no-refund, quota rows change only through the quota contracts
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    retry_commit(&rig, &chat, &target).await;
    assert!(all_quota_rows(&rig.quota.db).await.is_empty());
    assert!(rig.events().await.is_empty());
}

// ── the mutation transaction: delete ─────────────────────────────────────────

#[tokio::test]
async fn a_delete_soft_deletes_the_turn_and_its_messages_without_a_replacement() {
    // inst-tv-to-deleted, inst-mo-count
    let (rig, chat) = fixture().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    let before_turns = turn_rows(&rig.quota.db, chat.id).await.len();
    let committed = commit(&rig, &command(&chat, MutationOp::Delete, &second, None))
        .await
        .unwrap();
    assert_eq!(committed.op, MutationOp::Delete);
    assert!(committed.replacement.is_none());

    let turn = turn_by_request(&rig.quota.db, chat.id, second.request_id).await;
    assert_eq!(turn.deleted_at, Some(default_now()));
    assert_eq!(
        turn.replaced_by_request_id, None,
        "delete sets no replacement"
    );
    assert_eq!(
        turn_rows(&rig.quota.db, chat.id).await.len(),
        before_turns,
        "no new turn"
    );
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 2, "only the first turn's messages stay live");
    assert!(live.iter().all(|m| m.request_id == Some(first.request_id)));
    assert_eq!(latest(&rig, &chat).await, Some(first.request_id));
}

#[tokio::test]
async fn a_delete_of_a_failed_turn_without_an_assistant_message_removes_one_message() {
    // inst-mo-count: one message for a failed turn or a cancelled turn without text
    let (rig, chat) = fixture().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q").failed()).await;
    assert_eq!(live_messages(&rig.quota.db, chat.id).await.len(), 1);
    commit(&rig, &command(&chat, MutationOp::Delete, &seeded, None))
        .await
        .unwrap();
    assert!(live_messages(&rig.quota.db, chat.id).await.is_empty());
}

#[tokio::test]
async fn a_delete_changes_no_quota_row() {
    let (rig, chat) = fixture().await;
    seed_rows(&rig.quota.db, &[row("daily", DAY, "total").spent(42)]).await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = all_quota_rows(&rig.quota.db).await;
    commit(&rig, &command(&chat, MutationOp::Delete, &target, None))
        .await
        .unwrap();
    assert_eq!(all_quota_rows(&rig.quota.db).await, before);
}

#[tokio::test]
async fn the_reactions_and_links_of_a_deleted_turn_are_kept_for_audit() {
    // inst-mc-soft-delete-messages: message_attachments and message_reactions stay untouched
    let (rig, chat) = fixture().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let target = seed(&rig.quota.db, &chat, turn("q").attach(image, 10)).await;
    crate::test_support::send::seed_reaction_of(
        &rig.quota.db,
        TENANT_A,
        USER_A1,
        target.assistant_message_id.unwrap(),
        "like",
    )
    .await;
    commit(&rig, &command(&chat, MutationOp::Delete, &target, None))
        .await
        .unwrap();
    assert_eq!(link_rows(&rig.quota.db, chat.id).await.len(), 1);
    assert_eq!(
        crate::test_support::mutations::reaction_rows(
            &rig.quota.db,
            target.assistant_message_id.unwrap()
        )
        .await
        .len(),
        1
    );
}

// ── the audit event ──────────────────────────────────────────────────────────

/// A chat id whose audit partition by chat differs from the partition by tenant.
fn chat_id_off_the_tenant_partition(partitions: u32) -> Uuid {
    (1_u128..1_000)
        .map(Uuid::from_u128)
        .find(|id| partition_for(*id, partitions) != partition_for(TENANT_A, partitions))
        .unwrap()
}

#[tokio::test]
async fn one_audit_row_per_committed_mutation_partitioned_by_the_tenant() {
    // inst-mc-audit, inst-ae-enqueue (the chat's tenant, not the chat id), inst-ae-exactly-one
    let rig = send_rig().await;
    let partitions = rig.outbox.num_partitions;
    let chat_id = chat_id_off_the_tenant_partition(partitions);
    assert_ne!(
        partition_for(chat_id, partitions),
        partition_for(TENANT_A, partitions)
    );
    let chat = seed_chat(&rig.quota.db, &a1(), chat_id, Some("t"), STANDARD, at(0)).await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let (replacement, _) = retry_commit(&rig, &chat, &target).await;

    let rows = outbox_rows(&rig.quota.db).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let audit = &rows[0];
    assert_eq!(audit.queue, rig.quota.outbox.audit_queue_name);
    assert_eq!(audit.payload_type, AUDIT_PAYLOAD_TYPE);
    assert_eq!(
        audit.partition,
        i64::from(partition_for(TENANT_A, partitions)),
        "partitioned by the tenant of the chat"
    );
    let event: AuditEvent = serde_json::from_value(audit.payload.clone()).unwrap();
    let AuditEvent::TurnRetry(retry) = event else {
        panic!("expected turn_retry, got {event:?}");
    };
    assert_eq!(retry.actor_user_id, USER_A1);
    assert_eq!(retry.chat_id, chat.id);
    assert_eq!(retry.original_request_id, target.request_id);
    assert_eq!(retry.new_request_id, replacement.request_id);
    assert_eq!(retry.timestamp, default_now());
}

#[tokio::test]
async fn the_event_type_follows_the_operation() {
    // inst-ae-retry, inst-ae-edit, inst-ae-delete
    for (op, tag) in [
        (MutationOp::Retry, "turn_retry"),
        (MutationOp::Edit, "turn_edit"),
        (MutationOp::Delete, "turn_delete"),
    ] {
        let (rig, chat) = fixture().await;
        let target = seed(&rig.quota.db, &chat, turn("secret text")).await;
        let replacement = op.replaces().then(|| replacement("another secret", &[]));
        commit(&rig, &command(&chat, op, &target, replacement))
            .await
            .unwrap();
        let payloads = rig.audit_payloads().await;
        assert_eq!(payloads.len(), 1, "{op:?}");
        assert_eq!(payloads[0]["event_type"], tag);
        let wire = payloads[0].to_string();
        assert!(!wire.contains("secret"), "no content in the event: {wire}");
        assert!(!wire.contains("answer"), "no content in the event: {wire}");
    }
}

#[tokio::test]
async fn a_delete_event_carries_the_target_request_id() {
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    commit(&rig, &command(&chat, MutationOp::Delete, &target, None))
        .await
        .unwrap();
    let event = rig.audit_events().await.remove(0);
    let AuditEvent::TurnDelete(delete) = event else {
        panic!("expected turn_delete");
    };
    assert_eq!(delete.request_id, target.request_id);
    assert_eq!(delete.chat_id, chat.id);
    assert_eq!(delete.actor_user_id, USER_A1);
}

// ── rollbacks ────────────────────────────────────────────────────────────────

/// A chat with two turns and a summary covering neither, so a rollback has plenty to keep.
async fn populated(rig: &SendRig) -> (ChatRecord, SeededTurn, SeededTurn) {
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    seed_summary(
        &rig.quota.db,
        &chat,
        "summary",
        12,
        (t(11), first.assistant_message_id.unwrap()),
    )
    .await;
    (chat, first, second)
}

#[tokio::test]
async fn a_forced_audit_payload_overflow_rolls_everything_back() {
    // inst-mc-payload-too-large, inst-mc-return-payload-500, inst-mc-atomic
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let outbox = FailingMutationOutbox::payload_too_large();
        let rig = send_rig_with(SendRigOptions::new().with_turn_outbox(outbox.clone())).await;
        outbox.bind(rig.quota.enqueuer.clone());
        let (chat, _, second) = populated(&rig).await;
        let before = snapshot(&rig.quota.db, &chat).await;
        let replacement = op.replaces().then(|| replacement("second", &[]));
        let result = commit(&rig, &command(&chat, op, &second, replacement)).await;
        assert!(
            matches!(
                result,
                Err(DomainError::OutboxPayloadTooLarge {
                    site: EnqueueSite::TurnRetry | EnqueueSite::TurnEdit | EnqueueSite::TurnDelete
                })
            ),
            "{op:?}: {result:?}"
        );
        assert_eq!(
            outbox.attempts(),
            1,
            "{op:?}: the enqueue was attempted once"
        );
        assert_rolled_back(
            &rig,
            &chat,
            &before,
            &format!("{op:?} with an oversized payload"),
        )
        .await;
    }
}

#[tokio::test]
async fn a_forced_database_error_on_the_audit_enqueue_rolls_everything_back() {
    // inst-mc-db-error, inst-mc-return-500
    let rig = send_rig().await;
    let (chat, _, second) = populated(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    fail_outbox_inserts(&rig.quota.db).await;
    let result = commit(
        &rig,
        &command(
            &chat,
            MutationOp::Retry,
            &second,
            Some(replacement("second", &[])),
        ),
    )
    .await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    assert_rolled_back(&rig, &chat, &before, "a failing outbox insert").await;
}

#[tokio::test]
async fn an_internal_audit_enqueue_failure_rolls_everything_back() {
    let outbox = FailingMutationOutbox::internal();
    let rig = send_rig_with(SendRigOptions::new().with_turn_outbox(outbox.clone())).await;
    outbox.bind(rig.quota.enqueuer.clone());
    let (chat, _, second) = populated(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = commit(&rig, &command(&chat, MutationOp::Delete, &second, None)).await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    assert_rolled_back(&rig, &chat, &before, "an internal enqueue failure").await;
}

#[tokio::test]
async fn a_forced_database_error_in_the_middle_of_the_transaction_rolls_back_the_soft_deletes() {
    // inst-mc-atomic: the failing link insert comes after the soft-deletes
    let (rig, chat) = fixture().await;
    let db = &rig.quota.db;
    let image = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let target = seed(db, &chat, turn("q").attach(image, 10)).await;
    let before = snapshot(db, &chat).await;
    raw_exec(
        &db.raw,
        "CREATE TRIGGER fail_link_insert BEFORE INSERT ON message_attachments \
         BEGIN SELECT RAISE(ABORT, 'forced link failure'); END",
    )
    .await
    .unwrap();
    let result = commit(
        &rig,
        &command(
            &chat,
            MutationOp::Retry,
            &target,
            Some(replacement("q", &[image])),
        ),
    )
    .await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    assert_rolled_back(&rig, &chat, &before, "a failing link insert").await;
}

#[tokio::test]
async fn a_running_turn_index_violation_is_generation_in_progress_and_leaves_nothing() {
    // inst-mc-running-race, inst-mc-return-generation-in-progress: an older `running` turn holds
    // the one-running-turn index, so the replacement insert loses on it
    let (rig, chat) = fixture().await;
    seed(&rig.quota.db, &chat, turn("stuck").at(10).running()).await;
    let target = seed(&rig.quota.db, &chat, turn("target").at(100)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = commit(
        &rig,
        &command(
            &chat,
            MutationOp::Retry,
            &target,
            Some(replacement("target", &[])),
        ),
    )
    .await;
    assert_eq!(conflict_of(result), ConflictReason::GenerationInProgress);
    assert_rolled_back(&rig, &chat, &before, "the running index").await;
}

#[tokio::test]
async fn a_server_uuid_collision_is_an_internal_error_and_leaves_nothing() {
    // inst-mc-key-collision, inst-mc-return-key-collision: UNIQUE(chat_id, request_id)
    let (rig, chat) = fixture().await;
    let other = seed(&rig.quota.db, &chat, turn("older").at(10)).await;
    let target = seed(&rig.quota.db, &chat, turn("target").at(100)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let mut colliding = replacement("target", &[]);
    colliding.request_id = other.request_id;
    let result = commit(
        &rig,
        &command(&chat, MutationOp::Retry, &target, Some(colliding)),
    )
    .await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    assert_rolled_back(&rig, &chat, &before, "a request_id collision").await;
}

#[tokio::test]
async fn a_message_key_collision_is_an_internal_error_and_leaves_nothing() {
    // inst-mc-key-collision: the `(chat_id, request_id, role)` index of `messages`
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("target")).await;
    // an orphan message holds the `(request_id, user)` key the replacement would take
    let key = Uuid::new_v4();
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "squatter", t(5)).request(Some(key)),
    )
    .await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let mut colliding = replacement("target", &[]);
    colliding.request_id = key;
    let result = commit(
        &rig,
        &command(&chat, MutationOp::Edit, &target, Some(colliding)),
    )
    .await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    assert_rolled_back(&rig, &chat, &before, "a message key collision").await;
}

#[tokio::test]
async fn an_already_replaced_target_is_not_latest_and_leaves_nothing() {
    // inst-lr-reread, inst-lr-deleted, inst-tv-final: a second mutation of the same turn
    let rig = send_rig().await;
    let (chat, _, second) = populated(&rig).await;
    retry_commit(&rig, &chat, &second).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let replacement = op.replaces().then(|| replacement("again", &[]));
        let result = commit(&rig, &command(&chat, op, &second, replacement)).await;
        assert_eq!(conflict_of(result), ConflictReason::NotLatestTurn, "{op:?}");
        assert_rolled_back(&rig, &chat, &before, &format!("{op:?} of a replaced turn")).await;
    }
}

#[tokio::test]
async fn a_deleted_target_is_not_latest_for_every_operation() {
    // inst-tv-final: Deleted stays Deleted
    let rig = send_rig().await;
    let (chat, _, second) = populated(&rig).await;
    commit(&rig, &command(&chat, MutationOp::Delete, &second, None))
        .await
        .unwrap();
    let before = snapshot(&rig.quota.db, &chat).await;
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let replacement = op.replaces().then(|| replacement("again", &[]));
        let result = commit(&rig, &command(&chat, op, &second, replacement)).await;
        assert_eq!(conflict_of(result), ConflictReason::NotLatestTurn, "{op:?}");
    }
    assert_rolled_back(&rig, &chat, &before, "mutations of a deleted turn").await;
}

#[tokio::test]
async fn a_target_behind_a_newer_turn_is_not_latest_and_leaves_nothing() {
    // inst-lr-latest, inst-lr-not-latest: a newer turn committed after the preview
    let rig = send_rig().await;
    let (chat, first, _) = populated(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = commit(
        &rig,
        &command(
            &chat,
            MutationOp::Retry,
            &first,
            Some(replacement("first", &[])),
        ),
    )
    .await;
    assert_eq!(conflict_of(result), ConflictReason::NotLatestTurn);
    assert_rolled_back(&rig, &chat, &before, "a non-latest target").await;
}

#[tokio::test]
async fn a_target_behind_a_newer_running_turn_is_not_latest() {
    // a concurrent send committed a `running` turn after the preview
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("target").at(10)).await;
    seed(&rig.quota.db, &chat, turn("send").at(100).running()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = commit(&rig, &command(&chat, MutationOp::Delete, &target, None)).await;
    assert_eq!(conflict_of(result), ConflictReason::NotLatestTurn);
    assert_rolled_back(&rig, &chat, &before, "a newer running turn").await;
}

#[tokio::test]
async fn a_running_target_is_a_turn_state_rejection_and_leaves_nothing() {
    // inst-lr-not-terminal
    let (rig, chat) = fixture().await;
    let target = seed(&rig.quota.db, &chat, turn("q").running()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = commit(&rig, &command(&chat, MutationOp::Delete, &target, None)).await;
    assert!(
        matches!(result, Err(DomainError::TurnNotTerminal)),
        "{result:?}"
    );
    assert_rolled_back(&rig, &chat, &before, "a running target").await;
}

#[tokio::test]
async fn a_rejected_mutation_enqueues_no_audit_event() {
    // inst-ae-exactly-one: none for a rejected or rolled-back mutation
    let rig = send_rig().await;
    let (chat, first, _) = populated(&rig).await;
    let before = outbox_body_count(&rig.quota.db).await;
    let result = commit(&rig, &command(&chat, MutationOp::Delete, &first, None)).await;
    assert!(result.is_err());
    assert_eq!(outbox_body_count(&rig.quota.db).await, before);
}

// ── the summary invalidation ─────────────────────────────────────────────────

/// Two turns, `is_compressed` set on the first turn's messages, a summary with the given frontier.
async fn summarized(
    rig: &SendRig,
    frontier: impl FnOnce(&SeededTurn, &SeededTurn) -> (time::OffsetDateTime, Uuid),
) -> (ChatRecord, SeededTurn, SeededTurn) {
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    seed_summary(
        &rig.quota.db,
        &chat,
        "the summary",
        12,
        frontier(&first, &second),
    )
    .await;
    raw_exec(
        &rig.quota.db.raw,
        &format!(
            "UPDATE messages SET is_compressed = 1 WHERE request_id = {}",
            crate::test_support::chats::blob(first.request_id)
        ),
    )
    .await
    .unwrap();
    (chat, first, second)
}

async fn compressed_flags(rig: &SendRig, chat: &ChatRecord) -> Vec<bool> {
    let mut rows = message_rows(&rig.quota.db, chat.id).await;
    rows.sort_by_key(|m| (m.created_at, m.id));
    rows.into_iter().map(|m| m.is_compressed).collect()
}

#[tokio::test]
async fn a_summary_before_the_target_is_kept_after_a_retry() {
    // inst-si-keep, inst-si-return-kept
    let rig = send_rig().await;
    let (chat, first, second) = summarized(&rig, |first, _| {
        (t(11), first.assistant_message_id.unwrap())
    })
    .await;
    let before = summary_row(&rig.quota.db, chat.id).await.unwrap();
    let (_, committed) = retry_commit(&rig, &chat, &second).await;
    assert!(!committed.summary_invalidated);
    assert_eq!(summary_row(&rig.quota.db, chat.id).await, Some(before));
    let flags = compressed_flags(&rig, &chat).await;
    assert!(
        flags.iter().filter(|c| **c).count() == 2,
        "the first turn stays compressed: {flags:?} for {}",
        first.request_id
    );
}

#[tokio::test]
async fn a_summary_at_the_target_user_message_is_deleted_and_the_flags_are_cleared() {
    // inst-si-covers, inst-si-delete, inst-si-clear-compressed, inst-si-return-invalidated
    let rig = send_rig().await;
    let (chat, _, second) = summarized(&rig, |_, second| (t(100), second.user_message_id)).await;
    let (_, committed) = retry_commit(&rig, &chat, &second).await;
    assert!(committed.summary_invalidated);
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
    assert!(
        compressed_flags(&rig, &chat).await.iter().all(|c| !*c),
        "is_compressed is cleared on every message of the chat"
    );
}

#[tokio::test]
async fn a_summary_after_the_target_user_message_is_deleted() {
    // inst-si-covers: "at or after"
    let rig = send_rig().await;
    let (chat, _, second) = summarized(&rig, |_, second| {
        (t(101), second.assistant_message_id.unwrap())
    })
    .await;
    commit(
        &rig,
        &command(
            &chat,
            MutationOp::Edit,
            &second,
            Some(replacement("edited", &[])),
        ),
    )
    .await
    .unwrap();
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
    assert!(compressed_flags(&rig, &chat).await.iter().all(|c| !*c));
}

#[tokio::test]
async fn the_summary_rule_applies_to_a_delete_as_well() {
    let rig = send_rig().await;
    let (chat, _, second) = summarized(&rig, |_, second| {
        (t(101), second.assistant_message_id.unwrap())
    })
    .await;
    let committed = commit(&rig, &command(&chat, MutationOp::Delete, &second, None))
        .await
        .unwrap();
    assert!(committed.summary_invalidated);
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
}

#[tokio::test]
async fn a_summary_before_the_target_is_kept_after_a_delete() {
    let rig = send_rig().await;
    let (chat, _, second) = summarized(&rig, |first, _| {
        (t(11), first.assistant_message_id.unwrap())
    })
    .await;
    let committed = commit(&rig, &command(&chat, MutationOp::Delete, &second, None))
        .await
        .unwrap();
    assert!(!committed.summary_invalidated);
    assert!(summary_row(&rig.quota.db, chat.id).await.is_some());
}

#[tokio::test]
async fn the_frontier_comparison_goes_through_the_chronological_order_with_fractions() {
    // inst-si-covers: the frontier `.123` is after the target `.12`, though `.123Z` sorts before
    // `.12Z` as text
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("target").at(120_000)).await;
    seed_summary(
        &rig.quota.db,
        &chat,
        "s",
        5,
        (t(123_000), Uuid::from_u128(0xF00D)),
    )
    .await;
    commit(&rig, &command(&chat, MutationOp::Delete, &target, None))
        .await
        .unwrap();
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());

    let other = rig.std_chat().await;
    let target = seed(&rig.quota.db, &other, turn("target").at(123_000)).await;
    seed_summary(
        &rig.quota.db,
        &other,
        "s",
        5,
        (t(120_000), Uuid::from_u128(0xF00D)),
    )
    .await;
    commit(&rig, &command(&other, MutationOp::Delete, &target, None))
        .await
        .unwrap();
    assert!(
        summary_row(&rig.quota.db, other.id).await.is_some(),
        "a frontier `.12` is before a target `.123`"
    );
}

#[tokio::test]
async fn at_the_same_instant_the_message_id_breaks_the_frontier_tie() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("t").at(50)).await;
    // a frontier id below the target user message id at the same `created_at` is before it
    let low = Uuid::from_u128(0);
    assert!(low < first.user_message_id);
    seed_summary(&rig.quota.db, &chat, "s", 5, (t(50), low)).await;
    commit(&rig, &command(&chat, MutationOp::Delete, &first, None))
        .await
        .unwrap();
    assert!(summary_row(&rig.quota.db, chat.id).await.is_some());
}

#[tokio::test]
async fn a_chat_without_a_summary_has_nothing_to_invalidate_and_other_chats_are_untouched() {
    // inst-si-no-summary, inst-si-return-none
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed(&rig.quota.db, &chat, turn("a").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("b").at(100)).await;
    let other = rig.std_chat().await;
    let other_turn = seed(&rig.quota.db, &other, turn("o").at(10)).await;
    seed_summary(
        &rig.quota.db,
        &other,
        "other summary",
        5,
        (t(500), other_turn.user_message_id),
    )
    .await;
    mark_compressed(&rig.quota.db, other.id).await;
    let (_, committed) = retry_commit(&rig, &chat, &second).await;
    assert!(!committed.summary_invalidated);
    assert!(summary_row(&rig.quota.db, other.id).await.is_some());
    assert!(compressed_flags(&rig, &other).await.iter().all(|c| *c));
}

#[tokio::test]
async fn deleting_the_latest_turn_then_mutating_the_covered_previous_turn_invalidates_the_summary()
{
    // DESIGN 3.9 `Summary Interaction on Turn Mutation`: the summary triggered by the deleted turn
    // covers the previous one
    let rig = send_rig().await;
    let (chat, first, second) = summarized(&rig, |first, _| {
        (t(11), first.assistant_message_id.unwrap())
    })
    .await;
    let deleted = commit(&rig, &command(&chat, MutationOp::Delete, &second, None))
        .await
        .unwrap();
    assert!(
        !deleted.summary_invalidated,
        "a summary never covers the turn that triggered it"
    );
    assert!(summary_row(&rig.quota.db, chat.id).await.is_some());
    let (_, committed) = retry_commit(&rig, &chat, &first).await;
    assert!(committed.summary_invalidated);
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
    assert!(compressed_flags(&rig, &chat).await.iter().all(|c| !*c));
}

#[tokio::test]
async fn a_mutation_never_generates_or_enqueues_a_summary() {
    // inst-si-no-generation
    let rig = send_rig().await;
    let (chat, _, second) = summarized(&rig, |_, second| (t(100), second.user_message_id)).await;
    retry_commit(&rig, &chat, &second).await;
    assert!(rig.summary_rows().await.is_empty());
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
}

#[tokio::test]
async fn a_rolled_back_mutation_keeps_the_summary_and_the_compressed_flags() {
    let outbox = FailingMutationOutbox::internal();
    let rig = send_rig_with(SendRigOptions::new().with_turn_outbox(outbox.clone())).await;
    outbox.bind(rig.quota.enqueuer.clone());
    let (chat, _, second) = summarized(&rig, |_, second| (t(100), second.user_message_id)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = commit(&rig, &command(&chat, MutationOp::Delete, &second, None)).await;
    assert!(result.is_err());
    assert_rolled_back(&rig, &chat, &before, "the summary deletion").await;
    assert!(summary_row(&rig.quota.db, chat.id).await.is_some());
}

// ── the reserve through `ReserveTurn::Existing` ──────────────────────────────

async fn committed_replacement(rig: &SendRig, chat: &ChatRecord) -> (Replacement, SeededTurn) {
    let target = seed(&rig.quota.db, chat, turn("q")).await;
    let (replacement, _) = retry_commit(rig, chat, &target).await;
    (replacement, target)
}

async fn existing_reserve(
    rig: &SendRig,
    chat: &ChatRecord,
    replacement: &Replacement,
) -> ExistingReserve {
    let decision = rig
        .quota
        .preflight(chat, STANDARD, plain_facts())
        .await
        .expect("preflight");
    ExistingReserve {
        tenant_id: chat.tenant_id,
        user_id: chat.user_id,
        chat_id: chat.id,
        request_id: replacement.request_id,
        turn_id: replacement.turn_id,
        decision,
    }
}

async fn reserve_existing(
    rig: &SendRig,
    command: &ExistingReserve,
) -> Result<crate::domain::quota::ReserveReceipt, DomainError> {
    MutationStore::reserve_existing(&*rig.quota.db.provider, rig.quota.service.as_ref(), command)
        .await
}

#[tokio::test]
async fn the_existing_reserve_fills_the_preflight_columns_once_and_books_the_buckets() {
    // inst-pcs-reserve, inst-rts-to-reserved
    let (rig, chat) = fixture().await;
    let (replacement, _) = committed_replacement(&rig, &chat).await;
    let command = existing_reserve(&rig, &chat, &replacement).await;
    let decision = command.decision.clone();
    assert_ne!(
        replacement.started_at, decision.started_at,
        "the commit wrote another instant, so the reserve write visibly overwrites it"
    );
    let receipt = reserve_existing(&rig, &command).await.unwrap();
    assert_eq!(
        receipt.reserved_credits_micro,
        decision.reserved_credits_micro
    );

    let turn = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
    assert_eq!(turn.state, "running");
    assert_eq!(
        turn.started_at, decision.started_at,
        "started_at is the preflight instant"
    );
    assert_eq!(turn.reserve_tokens, Some(decision.reserve_tokens));
    assert_eq!(
        turn.reserved_credits_micro,
        Some(decision.reserved_credits_micro)
    );
    assert_eq!(
        turn.max_output_tokens_applied,
        Some(i32::try_from(decision.max_output_tokens_applied).unwrap())
    );
    assert_eq!(
        turn.policy_version_applied,
        Some(i64::try_from(decision.policy_version_applied).unwrap())
    );
    assert_eq!(
        turn.effective_model.as_deref(),
        Some(decision.effective_model.as_str())
    );
    assert_eq!(
        turn.minimal_generation_floor_applied,
        Some(i32::try_from(decision.minimal_generation_floor_applied).unwrap())
    );
    assert_eq!(
        reserved_of(&rig.quota.db, "daily", DAY, "total").await,
        decision.reserved_credits_micro
    );
    assert_eq!(
        reserved_of(&rig.quota.db, "monthly", MONTH, "total").await,
        decision.reserved_credits_micro
    );
}

#[tokio::test]
async fn a_second_existing_reserve_of_the_same_turn_fails_and_books_nothing() {
    // inst-pcs-reserve-failed: the turn already holds a reserve
    let (rig, chat) = fixture().await;
    let (replacement, _) = committed_replacement(&rig, &chat).await;
    let command = existing_reserve(&rig, &chat, &replacement).await;
    reserve_existing(&rig, &command).await.unwrap();
    let booked = all_quota_rows(&rig.quota.db).await;
    let result = reserve_existing(&rig, &command).await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(
        all_quota_rows(&rig.quota.db).await,
        booked,
        "no double booking"
    );
}

#[tokio::test]
async fn an_existing_reserve_of_a_turn_that_is_no_longer_running_fails() {
    // inst-pcs-reserve-failed: the turn is no longer `running`
    let (rig, chat) = fixture().await;
    let (replacement, _) = committed_replacement(&rig, &chat).await;
    let command = existing_reserve(&rig, &chat, &replacement).await;
    assert!(
        MutationStore::fail_unstarted(
            &*rig.quota.db.provider,
            chat.tenant_id,
            replacement.turn_id,
            UnstartedFailure::TurnSetupFailed
        )
        .await
        .unwrap()
    );
    let result = reserve_existing(&rig, &command).await;
    assert!(result.is_err(), "{result:?}");
    assert!(all_quota_rows(&rig.quota.db).await.is_empty());
    let turn = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
    assert_eq!(turn.reserve_tokens, None);
}

#[tokio::test]
async fn the_existing_reserve_re_check_over_the_limit_is_a_tokens_rejection_and_rolls_back() {
    // inst-pcs-over-limit
    let (rig, chat) = fixture().await;
    let (replacement, _) = committed_replacement(&rig, &chat).await;
    let command = existing_reserve(&rig, &chat, &replacement).await;
    let credits = command.decision.reserved_credits_micro;
    seed_rows(
        &rig.quota.db,
        &[row("daily", DAY, "total").reserved(60_000_000 - credits + 1)],
    )
    .await;
    let result = reserve_existing(&rig, &command).await;
    assert!(
        matches!(
            result,
            Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            })
        ),
        "{result:?}"
    );
    let turn = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
    assert_eq!(turn.reserve_tokens, None, "the columns stay NULL");
    assert_eq!(turn.state, "running");
    assert_eq!(
        reserved_of(&rig.quota.db, "daily", DAY, "total").await,
        60_000_000 - credits + 1,
        "the increment rolled back"
    );
}

// ── the unstarted replacement turn failure ───────────────────────────────────

#[tokio::test]
async fn the_unstarted_failure_is_a_plain_cas_to_failed_with_the_code_and_a_sanitized_detail() {
    // inst-uf-cas, inst-rts-to-failed-setup
    for code in [
        UnstartedFailure::ContextLengthExceeded,
        UnstartedFailure::TurnSetupFailed,
        UnstartedFailure::QuotaExceeded,
    ] {
        let (rig, chat) = fixture().await;
        let (replacement, target) = committed_replacement(&rig, &chat).await;
        let before_replaced = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
        let won = MutationStore::fail_unstarted(
            &*rig.quota.db.provider,
            chat.tenant_id,
            replacement.turn_id,
            code,
        )
        .await
        .unwrap();
        assert!(won, "{code:?}");
        let turn = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.error_code.as_deref(), Some(code.as_str()));
        assert!(!turn.error_detail.clone().unwrap_or_default().is_empty());
        assert!(turn.completed_at.is_some());
        assert!(turn.updated_at >= replacement.started_at);
        // the replaced turn stays replaced, the replacement message stays
        let replaced = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
        assert_eq!(replaced, before_replaced);
        assert_eq!(live_messages(&rig.quota.db, chat.id).await.len(), 1);
        // no settlement, no usage event, no turn audit event
        assert!(all_quota_rows(&rig.quota.db).await.is_empty());
        assert!(rig.events().await.is_empty());
        assert_eq!(
            rig.audit_payloads().await.len(),
            1,
            "only the mutation event"
        );
    }
}

#[tokio::test]
async fn the_unstarted_failure_codes_are_the_three_documented_values() {
    assert_eq!(
        UnstartedFailure::ContextLengthExceeded.as_str(),
        "context_length_exceeded"
    );
    assert_eq!(
        UnstartedFailure::TurnSetupFailed.as_str(),
        "turn_setup_failed"
    );
    assert_eq!(UnstartedFailure::QuotaExceeded.as_str(), "quota_exceeded");
}

#[tokio::test]
async fn a_lost_cas_changes_nothing() {
    // inst-uf-cas-lost, inst-uf-return-cas-lost, inst-rts-immutable
    let (rig, chat) = fixture().await;
    let (replacement, _) = committed_replacement(&rig, &chat).await;
    let provider = &*rig.quota.db.provider;
    assert!(
        MutationStore::fail_unstarted(
            provider,
            chat.tenant_id,
            replacement.turn_id,
            UnstartedFailure::ContextLengthExceeded
        )
        .await
        .unwrap()
    );
    let first = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
    let again = MutationStore::fail_unstarted(
        provider,
        chat.tenant_id,
        replacement.turn_id,
        UnstartedFailure::TurnSetupFailed,
    )
    .await
    .unwrap();
    assert!(!again, "another finalizer already made the turn terminal");
    assert_eq!(
        turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await,
        first
    );
    assert_eq!(first.error_code.as_deref(), Some("context_length_exceeded"));
}

#[tokio::test]
async fn a_terminal_turn_is_not_touched_by_the_unstarted_failure() {
    let (rig, chat) = fixture().await;
    let completed = seed(&rig.quota.db, &chat, turn("done")).await;
    let before = turn_by_request(&rig.quota.db, chat.id, completed.request_id).await;
    let won = MutationStore::fail_unstarted(
        &*rig.quota.db.provider,
        chat.tenant_id,
        completed.turn_id,
        UnstartedFailure::TurnSetupFailed,
    )
    .await
    .unwrap();
    assert!(!won);
    assert_eq!(
        turn_by_request(&rig.quota.db, chat.id, completed.request_id).await,
        before
    );
}

#[tokio::test]
async fn the_unstarted_failure_is_scoped_to_the_tenant_of_the_turn() {
    let (rig, chat) = fixture().await;
    let (replacement, _) = committed_replacement(&rig, &chat).await;
    let won = MutationStore::fail_unstarted(
        &*rig.quota.db.provider,
        TENANT_B,
        replacement.turn_id,
        UnstartedFailure::TurnSetupFailed,
    )
    .await;
    assert!(!matches!(won, Ok(true)), "{won:?}");
    let turn = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
    assert_eq!(turn.state, "running");
    assert!(chat_row(&rig.quota.db, chat.id).await.is_some());
}
