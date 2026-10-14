//! The reserve transaction over in-memory `SQLite` and the real quota service: the rows of a
//! commit (reserve, user message, activity bump, attachment links, `running` turn), the monotonic
//! message time, the attachment validation, the key and running-index races and the rollback of
//! everything on any failure.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `reserve-transaction`, `attachment-validation`, `message-order`; state `turn-lifecycle`
//! (creation); definition of done `cpt-cf-mini-chat-dod-message-streaming-reserve-transaction`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::Duration;
use uuid::Uuid;

use crate::domain::error::{ConflictReason, DomainError, QuotaScope};
use crate::domain::messages::MessageRow;
use crate::domain::quota::{PeriodType, PreflightDecision};
use crate::domain::streaming::{ReserveCommand, ReservedTurn, SnapshotBoundary, StreamStore};
use crate::infra::storage::chat_page::TimeStorage;
use crate::infra::storage::message_repo::MessageRepo;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, at, message_rows, outbox_body_count, seed_chat};
use crate::test_support::quota::{
    FixedClock, PREMIUM, STANDARD, all_quota_rows, default_now, plain_facts, quota_row,
    reserved_of, row, seed_rows,
};
use crate::test_support::send::{
    AttachmentSeed, Footprint, MessageSeed, SendRig, SendRigOptions, TurnSeedFull, chat_model,
    footprint, link_rows, seed_att, seed_msg, seed_turn_full, send_rig, send_rig_with, turn_rows,
};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1, USER_A2, raw_exec};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";

/// A rig and a standard-model chat of user 1.
async fn fixture() -> (SendRig, ChatRecord) {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    (rig, chat)
}

/// The command of a new turn of `chat` built from a real quota preflight of `model`.
async fn command_for(rig: &SendRig, chat: &ChatRecord, model: &str) -> ReserveCommand {
    let decision = rig
        .quota
        .preflight(chat, model, plain_facts())
        .await
        .expect("preflight");
    command_with(chat, decision)
}

fn command_with(chat: &ChatRecord, decision: PreflightDecision) -> ReserveCommand {
    ReserveCommand {
        tenant_id: chat.tenant_id,
        user_id: chat.user_id,
        chat_id: chat.id,
        request_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        user_message_id: Uuid::new_v4(),
        content: "hello there".to_owned(),
        attachment_ids: Vec::new(),
        web_search_enabled: false,
        decision,
        now: default_now(),
    }
}

async fn reserve(rig: &SendRig, command: &ReserveCommand) -> Result<ReservedTurn, DomainError> {
    StreamStore::reserve(&*rig.quota.db.provider, rig.quota.service.as_ref(), command).await
}

fn conflict(result: Result<ReservedTurn, DomainError>) -> ConflictReason {
    match result {
        Err(DomainError::Conflict { reason }) => reason,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

async fn message_of(
    rig: &SendRig,
    chat: &ChatRecord,
    id: Uuid,
) -> crate::infra::storage::entity::message::Model {
    message_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("message {id} was not written"))
}

/// What a rejected reserve must leave unchanged.
async fn assert_nothing_left(rig: &SendRig, chat: &ChatRecord, before: &Footprint, what: &str) {
    let after = footprint(&rig.quota.db, chat).await;
    assert_eq!(&after, before, "{what}: rows were left behind");
    assert_eq!(
        chat_model(&rig.quota.db, chat.id).await.updated_at,
        at(0),
        "{what}: chats.updated_at moved"
    );
}

// ── the commit ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_commit_writes_the_user_message_with_the_columns_of_the_contract() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    let reserved = reserve(&rig, &command).await.unwrap();

    assert_eq!(reserved.turn_id, command.turn_id);
    assert_eq!(reserved.user_message_id, command.user_message_id);
    let message = message_of(&rig, &chat, command.user_message_id).await;
    assert_eq!(message.tenant_id, TENANT_A);
    assert_eq!(message.chat_id, chat.id);
    assert_eq!(message.request_id, Some(command.request_id));
    assert_eq!(message.role, "user");
    assert_eq!(message.content, "hello there");
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
    assert_eq!(message.created_at, reserved.user_message_created_at);
    assert_eq!(
        message_rows(&rig.quota.db, chat.id).await.len(),
        1,
        "no assistant row"
    );
}

#[tokio::test]
async fn a_commit_writes_the_running_turn_with_the_preflight_columns_and_instant() {
    let (rig, chat) = fixture().await;
    let mut command = command_for(&rig, &chat, STANDARD).await;
    command.web_search_enabled = true;
    let decision = command.decision.clone();
    let reserved = reserve(&rig, &command).await.unwrap();

    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns.len(), 1);
    let turn = &turns[0];
    assert_eq!(turn.id, command.turn_id);
    assert_eq!(turn.tenant_id, TENANT_A);
    assert_eq!(turn.chat_id, chat.id);
    assert_eq!(turn.request_id, command.request_id);
    assert_eq!(turn.state, "running");
    assert_eq!(turn.requester_type, "user");
    assert_eq!(turn.requester_user_id, Some(USER_A1));
    assert_eq!(
        turn.started_at, decision.started_at,
        "started_at is the preflight instant"
    );
    assert_eq!(reserved.started_at, decision.started_at);
    let progress = turn
        .last_progress_at
        .expect("running turns carry a progress timestamp");
    assert!(
        progress >= turn.started_at,
        "{progress} < {}",
        turn.started_at
    );
    assert!(turn.web_search_enabled, "the requested flag is stored");
    assert_eq!(turn.reserve_tokens, Some(decision.reserve_tokens));
    assert_eq!(
        turn.max_output_tokens_applied,
        Some(i32::try_from(decision.max_output_tokens_applied).unwrap())
    );
    assert_eq!(
        turn.reserved_credits_micro,
        Some(decision.reserved_credits_micro)
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
    assert_eq!(turn.provider_name, None);
    assert_eq!(turn.provider_response_id, None);
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(turn.error_code, None);
    assert_eq!(turn.error_detail, None);
    assert_eq!(turn.deleted_at, None);
    assert_eq!(turn.completed_at, None);
    assert_eq!(
        (
            turn.web_search_completed_count,
            turn.code_interpreter_completed_count,
            turn.file_search_completed_count
        ),
        (0, 0, 0)
    );
}

#[tokio::test]
async fn started_at_stays_the_preflight_instant_when_the_clock_moves_on() {
    let clock = FixedClock::at(default_now());
    let rig = send_rig_with(SendRigOptions {
        clock: clock.clone(),
        ..SendRigOptions::new()
    })
    .await;
    let chat = rig.std_chat().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    clock.set(default_now() + Duration::minutes(5));
    reserve(&rig, &command).await.unwrap();
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.started_at, default_now());
}

#[tokio::test]
async fn a_commit_bumps_the_chat_activity_and_enqueues_nothing() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    reserve(&rig, &command).await.unwrap();
    let updated = chat_model(&rig.quota.db, chat.id).await.updated_at;
    assert!(
        updated > at(0),
        "updated_at {updated} was not moved forward"
    );
    assert_eq!(outbox_body_count(&rig.quota.db).await, 0);
}

#[tokio::test]
async fn a_commit_books_the_reserve_through_the_quota_service() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    let credits = command.decision.reserved_credits_micro;
    let reserved = reserve(&rig, &command).await.unwrap();
    assert_eq!(reserved.receipt.reserved_credits_micro, credits);
    assert_eq!(
        reserved.receipt.periods,
        vec![PeriodType::Daily, PeriodType::Monthly]
    );
    assert_eq!(
        reserved_of(&rig.quota.db, "daily", DAY, "total").await,
        credits
    );
    assert_eq!(
        reserved_of(&rig.quota.db, "monthly", MONTH, "total").await,
        credits
    );
    assert_eq!(
        all_quota_rows(&rig.quota.db).await.len(),
        2,
        "a standard turn books `total` only"
    );
}

#[tokio::test]
async fn a_premium_commit_books_the_total_and_the_premium_bucket() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let command = command_for(&rig, &chat, PREMIUM).await;
    let credits = command.decision.reserved_credits_micro;
    reserve(&rig, &command).await.unwrap();
    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        for bucket in ["total", "tier:premium"] {
            let row = quota_row(&rig.quota.db, period, start, bucket)
                .await
                .unwrap();
            assert_eq!(row.reserved, credits, "{period} {bucket}");
        }
    }
}

// ── the user message time ────────────────────────────────────────────────────

#[tokio::test]
async fn the_user_message_takes_the_application_clock_when_the_chat_is_older() {
    let (rig, chat) = fixture().await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "old", at(5)),
    )
    .await;
    let command = command_for(&rig, &chat, STANDARD).await;
    let reserved = reserve(&rig, &command).await.unwrap();
    assert_eq!(reserved.user_message_created_at, command.now);
}

#[tokio::test]
async fn the_user_message_follows_the_latest_message_by_one_microsecond() {
    // the latest message is at, or after, the application clock: max(now, latest + 1 µs)
    for offset in [
        Duration::ZERO,
        Duration::microseconds(123_000),
        Duration::seconds(30),
    ] {
        let (rig, chat) = fixture().await;
        let latest = default_now() + offset;
        seed_msg(
            &rig.quota.db,
            &chat,
            MessageSeed::new("assistant", "future", latest),
        )
        .await;
        let command = command_for(&rig, &chat, STANDARD).await;
        let reserved = reserve(&rig, &command).await.unwrap();
        assert_eq!(
            reserved.user_message_created_at,
            latest + Duration::microseconds(1),
            "offset {offset}"
        );
        // and the chronological order (fixed-width keys) agrees
        let boundary = MessageRepo::snapshot_boundary(
            &rig.quota.db.provider.conn().unwrap(),
            &chat,
            TimeStorage::Rfc3339Text,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            boundary,
            SnapshotBoundary {
                created_at: reserved.user_message_created_at,
                id: command.user_message_id
            }
        );
    }
}

// ── attachments ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn valid_attachments_are_linked_to_the_user_message_with_the_chat_of_the_message() {
    let (rig, chat) = fixture().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let document = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    let mut command = command_for(&rig, &chat, STANDARD).await;
    command.attachment_ids = vec![document, image];
    reserve(&rig, &command).await.unwrap();

    let links = link_rows(&rig.quota.db, chat.id).await;
    let mut wanted = vec![document, image];
    wanted.sort();
    assert_eq!(
        links.iter().map(|l| l.attachment_id).collect::<Vec<_>>(),
        wanted
    );
    for link in &links {
        assert_eq!(link.tenant_id, TENANT_A);
        assert_eq!(link.chat_id, chat.id);
        assert_eq!(link.message_id, command.user_message_id);
    }
}

#[tokio::test]
async fn an_invalid_attachment_rolls_back_the_reserve_the_message_and_the_turn() {
    let (rig, chat) = fixture().await;
    let other_chat = seed_chat(
        &rig.quota.db,
        &a1(),
        Uuid::new_v4(),
        Some("other"),
        "m",
        at(0),
    )
    .await;
    let db = &rig.quota.db;
    let cases: Vec<(&str, Uuid)> = vec![
        ("unknown id", Uuid::new_v4()),
        (
            "uploaded by another user",
            seed_att(
                db,
                &chat,
                AttachmentSeed::ready("document").uploaded_by(USER_A2),
            )
            .await,
        ),
        (
            "another tenant",
            seed_att(
                db,
                &chat,
                AttachmentSeed::ready("document").tenant(TENANT_B),
            )
            .await,
        ),
        (
            "another chat",
            seed_att(db, &other_chat, AttachmentSeed::ready("document")).await,
        ),
        (
            "pending",
            seed_att(
                db,
                &chat,
                AttachmentSeed::ready("document").status("pending"),
            )
            .await,
        ),
        (
            "uploaded, still indexing",
            seed_att(
                db,
                &chat,
                AttachmentSeed::ready("document").status("uploaded"),
            )
            .await,
        ),
        (
            "failed",
            seed_att(db, &chat, AttachmentSeed::ready("image").status("failed")).await,
        ),
        (
            "soft-deleted",
            seed_att(db, &chat, AttachmentSeed::ready("document").deleted(at(3))).await,
        ),
    ];
    let before = footprint(db, &chat).await;
    for (what, attachment) in cases {
        let mut command = command_for(&rig, &chat, STANDARD).await;
        command.attachment_ids = vec![attachment];
        let result = reserve(&rig, &command).await;
        assert!(
            matches!(result, Err(DomainError::InvalidAttachment)),
            "{what}: {result:?}"
        );
        assert_nothing_left(&rig, &chat, &before, what).await;
    }
}

#[tokio::test]
async fn one_bad_attachment_among_good_ones_leaves_no_partial_links() {
    let (rig, chat) = fixture().await;
    let good = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let bad = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").status("failed"),
    )
    .await;
    let before = footprint(&rig.quota.db, &chat).await;
    let mut command = command_for(&rig, &chat, STANDARD).await;
    command.attachment_ids = vec![good, bad];
    let result = reserve(&rig, &command).await;
    assert!(
        matches!(result, Err(DomainError::InvalidAttachment)),
        "{result:?}"
    );
    assert_nothing_left(&rig, &chat, &before, "good and bad").await;
    assert!(link_rows(&rig.quota.db, chat.id).await.is_empty());
}

// ── the re-check, the races and any other failure ────────────────────────────

#[tokio::test]
async fn a_failed_re_check_is_a_tokens_rejection_and_leaves_nothing_behind() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    let credits = command.decision.reserved_credits_micro;
    // another turn took the room between the preflight and the reserve
    seed_rows(
        &rig.quota.db,
        &[row("daily", DAY, "total").reserved(60_000_000 - credits + 1)],
    )
    .await;
    let before = footprint(&rig.quota.db, &chat).await;
    let result = reserve(&rig, &command).await;
    assert!(
        matches!(
            result,
            Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            })
        ),
        "{result:?}"
    );
    assert_nothing_left(&rig, &chat, &before, "re-check").await;
    assert_eq!(
        reserved_of(&rig.quota.db, "daily", DAY, "total").await,
        60_000_000 - credits + 1,
        "the increment rolled back"
    );
}

#[tokio::test]
async fn a_request_id_that_already_has_a_turn_is_a_request_id_conflict() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("completed").request(command.request_id),
    )
    .await;
    let before = footprint(&rig.quota.db, &chat).await;
    assert_eq!(
        conflict(reserve(&rig, &command).await),
        ConflictReason::RequestIdConflict
    );
    assert_nothing_left(&rig, &chat, &before, "key race").await;
}

#[tokio::test]
async fn a_message_key_race_without_a_turn_is_turn_already_running() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    // the `(chat_id, request_id, role)` index is violated and no turn holds the key
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "racing", at(2)).request(Some(command.request_id)),
    )
    .await;
    let before = footprint(&rig.quota.db, &chat).await;
    assert_eq!(
        conflict(reserve(&rig, &command).await),
        ConflictReason::TurnAlreadyRunning
    );
    assert_nothing_left(&rig, &chat, &before, "message key race").await;
}

#[tokio::test]
async fn a_message_key_race_with_a_turn_is_a_request_id_conflict() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "racing", at(2)).request(Some(command.request_id)),
    )
    .await;
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("failed").request(command.request_id),
    )
    .await;
    let before = footprint(&rig.quota.db, &chat).await;
    assert_eq!(
        conflict(reserve(&rig, &command).await),
        ConflictReason::RequestIdConflict
    );
    assert_nothing_left(&rig, &chat, &before, "message and turn key race").await;
}

#[tokio::test]
async fn losing_the_running_index_race_is_turn_already_running() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new("running")).await;
    let before = footprint(&rig.quota.db, &chat).await;
    assert_eq!(
        conflict(reserve(&rig, &command).await),
        ConflictReason::TurnAlreadyRunning
    );
    assert_nothing_left(&rig, &chat, &before, "running index race").await;
}

#[tokio::test]
async fn a_deleted_or_terminal_turn_does_not_block_a_new_turn() {
    let (rig, chat) = fixture().await;
    for state in ["completed", "failed", "cancelled"] {
        seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new(state)).await;
    }
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("running").deleted(at(3)),
    )
    .await;
    let command = command_for(&rig, &chat, STANDARD).await;
    assert!(reserve(&rig, &command).await.is_ok());
}

#[tokio::test]
async fn any_other_database_failure_rolls_everything_back_as_an_internal_error() {
    for (table, event) in [
        ("messages", "INSERT"),
        ("chat_turns", "INSERT"),
        ("chats", "UPDATE"),
        ("message_attachments", "INSERT"),
    ] {
        let (rig, chat) = fixture().await;
        let attachment = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
        let mut command = command_for(&rig, &chat, STANDARD).await;
        command.attachment_ids = vec![attachment];
        raw_exec(
            &rig.quota.db.raw,
            &format!(
                "CREATE TRIGGER forced_failure BEFORE {event} ON {table} \
                 BEGIN SELECT RAISE(ABORT, 'forced {table} failure'); END"
            ),
        )
        .await
        .unwrap();
        let before = footprint(&rig.quota.db, &chat).await;
        let result = reserve(&rig, &command).await;
        assert!(
            matches!(result, Err(DomainError::Internal { .. })),
            "{table}: {result:?}"
        );
        assert_nothing_left(&rig, &chat, &before, table).await;
    }
}

// ── no cross-chat effects ────────────────────────────────────────────────────

#[tokio::test]
async fn a_commit_touches_only_its_own_chat() {
    let (rig, chat) = fixture().await;
    let other = seed_chat(
        &rig.quota.db,
        &a1(),
        Uuid::new_v4(),
        Some("other"),
        "m",
        at(0),
    )
    .await;
    let other_before = footprint(&rig.quota.db, &other).await;
    let command = command_for(&rig, &chat, STANDARD).await;
    reserve(&rig, &command).await.unwrap();
    assert_eq!(
        footprint(&rig.quota.db, &other).await.messages,
        other_before.messages
    );
    assert_eq!(
        footprint(&rig.quota.db, &other).await.turns,
        other_before.turns
    );
    assert_eq!(chat_model(&rig.quota.db, other.id).await.updated_at, at(0));
}

/// The message rows are `MessageRow`s through the repository too (documents the shape the
/// assembly reads).
#[tokio::test]
async fn the_committed_user_message_is_a_selectable_history_row() {
    let (rig, chat) = fixture().await;
    let command = command_for(&rig, &chat, STANDARD).await;
    reserve(&rig, &command).await.unwrap();
    let row: Option<MessageRow> = MessageRepo::find_message(
        &rig.quota.db.provider.conn().unwrap(),
        &chat,
        command.user_message_id,
    )
    .await
    .unwrap();
    let row = row.expect("the user message is readable");
    assert_eq!(row.request_id, Some(command.request_id));
    assert_eq!(row.role, "user");
}
