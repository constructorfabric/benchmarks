//! The mutation preview and the mutation preflight over in-memory `SQLite`, the real quota
//! service and the real stores: the check order of the preview against stored turns, the
//! read-only guarantee (no row, no reserve, no provider call, one PDP evaluation), the prepared
//! mutation (content, preflight instant, carried attachments and images, carried web search
//! flag, snapshot boundary) and every preflight rejection leaving the previous turn unchanged.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`,
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight` (`inst-mpf-resolve-model`,
//! `inst-mpf-model-missing`, `inst-mpf-policy-failed`, `inst-mpf-carry-forward`,
//! `inst-mpf-snapshot-boundary`, `inst-mpf-too-many-images`, `inst-mpf-read-facts`,
//! `inst-mpf-quota-preflight`, `inst-mpf-quota-rejected`, `inst-mpf-input-too-long`,
//! `inst-mpf-image-guards`, `inst-mpf-keep-decision`, `inst-mpf-return`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward` and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`
//! (`inst-tcf-web-search-flag`, `inst-tcf-limits`, `inst-tcf-return`); definitions of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-latest-terminal-turn`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-retry`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-edit` and
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-carry-forward`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;

use serde_json::json;
use uuid::Uuid;

use super::{MutationOp, MutationPreview, PreparedMutation};
use crate::domain::error::{ConflictReason, DomainError, QuotaScope, ResourceKind};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, at};
use crate::test_support::mutations::{SeededTurn, seed, snapshot, turn};
use crate::test_support::quota::{PREMIUM, STANDARD, default_now, row, seed_rows};
use crate::test_support::send::{
    AttachmentSeed, SendRig, SendRigOptions, TurnSeedFull, byte_budgets, catalog_of,
    default_catalog, prem_model, seed_att, seed_turn_full, send_rig, send_rig_with, std_model, t,
};
use crate::test_support::{USER_A1, USER_A2};

const DAY: &str = "2026-02-28";

async fn preview(
    rig: &SendRig,
    chat: &ChatRecord,
    op: MutationOp,
    request_id: Uuid,
) -> Result<MutationPreview, DomainError> {
    let detail = rig.mutations.admit(&a1(), op, chat.id).await?;
    rig.mutations.preview(&a1(), &detail, op, request_id).await
}

async fn prepared(
    rig: &SendRig,
    chat: &ChatRecord,
    op: MutationOp,
    request_id: Uuid,
    content: &str,
) -> Result<PreparedMutation, DomainError> {
    let detail = rig.mutations.admit(&a1(), op, chat.id).await?;
    let preview = rig
        .mutations
        .preview(&a1(), &detail, op, request_id)
        .await?;
    rig.mutations
        .preflight(&a1(), &detail, preview, content.to_owned())
        .await
}

fn reason_of<T: std::fmt::Debug>(result: Result<T, DomainError>) -> ConflictReason {
    match result {
        Err(DomainError::Conflict { reason }) => reason,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

async fn two_turns(rig: &SendRig, chat: &ChatRecord) -> (SeededTurn, SeededTurn) {
    let first = seed(&rig.quota.db, chat, turn("first question").at(10)).await;
    let second = seed(&rig.quota.db, chat, turn("second question").at(100)).await;
    (first, second)
}

// ── the preview ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_preview_returns_the_latest_terminal_turn_and_its_user_message() {
    // inst-mp-select-target, inst-mp-select-user-message, inst-mp-return
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, second) = two_turns(&rig, &chat).await;
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let preview = preview(&rig, &chat, op, second.request_id).await.unwrap();
        assert_eq!(preview.op, op);
        assert_eq!(preview.target.row.request_id, second.request_id);
        assert_eq!(preview.target.row.id, second.turn_id);
        assert_eq!(preview.user_message.id, second.user_message_id);
        assert_eq!(preview.user_message.content, "second question");
        assert_eq!(preview.user_message.role, "user");
    }
}

#[tokio::test]
async fn a_failed_and_a_cancelled_latest_turn_can_be_previewed_too() {
    // the allowed target states are completed, failed and cancelled
    let rig = send_rig().await;
    for plan in [
        turn("failed one").failed(),
        turn("cancelled with partial").cancelled(Some("part")),
        turn("cancelled without text").cancelled(None),
    ] {
        let chat = rig.std_chat().await;
        let seeded = seed(&rig.quota.db, &chat, plan).await;
        let preview = preview(&rig, &chat, MutationOp::Retry, seeded.request_id)
            .await
            .unwrap();
        assert_eq!(preview.target.row.request_id, seeded.request_id);
    }
}

#[tokio::test]
async fn an_unknown_request_id_is_a_not_found_of_the_turn_type() {
    // inst-mp-missing, inst-mp-return-404
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    two_turns(&rig, &chat).await;
    let error = preview(&rig, &chat, MutationOp::Retry, Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::NotFound {
                resource: ResourceKind::Turn,
                ..
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_turn_of_another_chat_is_a_not_found_of_the_turn_type() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let other = rig.std_chat().await;
    let foreign = seed(&rig.quota.db, &other, turn("elsewhere")).await;
    let error = preview(&rig, &chat, MutationOp::Delete, foreign.request_id)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::NotFound {
                resource: ResourceKind::Turn,
                ..
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_turn_of_another_requester_is_an_authz_denied() {
    // inst-mp-not-owner, inst-mp-return-403
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let foreign = seed(&rig.quota.db, &chat, turn("q").requester(Some(USER_A2))).await;
    let error = preview(&rig, &chat, MutationOp::Retry, foreign.request_id)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
}

#[tokio::test]
async fn a_turn_without_a_requester_is_an_authz_denied() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let orphan = seed(&rig.quota.db, &chat, turn("q").requester(None)).await;
    let error = preview(&rig, &chat, MutationOp::Edit, orphan.request_id)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
}

#[tokio::test]
async fn a_replaced_or_deleted_turn_is_not_latest() {
    // inst-mp-deleted, inst-mp-return-deleted
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (first, _) = two_turns(&rig, &chat).await;
    // delete the second turn directly, then the first becomes the latest; soft-delete the first
    // too and it is a deleted target
    crate::test_support::raw_exec(
        &rig.quota.db.raw,
        &format!(
            "UPDATE chat_turns SET deleted_at = '2026-01-01 00:00:00+00:00' WHERE id = {}",
            crate::test_support::chats::blob(first.turn_id)
        ),
    )
    .await
    .unwrap();
    let result = preview(&rig, &chat, MutationOp::Retry, first.request_id).await;
    assert_eq!(reason_of(result), ConflictReason::NotLatestTurn);
}

#[tokio::test]
async fn a_replaced_turn_with_a_replacement_pointer_is_not_latest() {
    // inst-tv-final: Replaced stays Replaced
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let replaced = Uuid::new_v4();
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("completed")
            .id(Uuid::new_v4())
            .request(replaced)
            .deleted(at(5))
            .replaced_by(Uuid::new_v4()),
    )
    .await;
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let result = preview(&rig, &chat, op, replaced).await;
        assert_eq!(reason_of(result), ConflictReason::NotLatestTurn, "{op:?}");
    }
}

#[tokio::test]
async fn a_running_target_is_a_turn_state_precondition() {
    // inst-mp-running, inst-mp-return-running
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let running = seed(&rig.quota.db, &chat, turn("q").running()).await;
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let error = preview(&rig, &chat, op, running.request_id)
            .await
            .unwrap_err();
        assert!(
            matches!(error, DomainError::TurnNotTerminal),
            "{op:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_target_behind_a_newer_completed_turn_is_not_latest() {
    // inst-mp-latest, inst-mp-not-latest, inst-mp-return-not-latest
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (first, _) = two_turns(&rig, &chat).await;
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let result = preview(&rig, &chat, op, first.request_id).await;
        assert_eq!(reason_of(result), ConflictReason::NotLatestTurn, "{op:?}");
    }
}

#[tokio::test]
async fn a_target_behind_a_newer_running_turn_is_not_latest_not_a_turn_state_error() {
    // DESIGN 3.9 rule 1: a newer `running` turn also makes the target non-latest
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("old").at(10)).await;
    seed(&rig.quota.db, &chat, turn("in flight").at(100).running()).await;
    let result = preview(&rig, &chat, MutationOp::Retry, first.request_id).await;
    assert_eq!(reason_of(result), ConflictReason::NotLatestTurn);
}

#[tokio::test]
async fn the_running_check_precedes_the_latest_check() {
    // a running target that is also not the latest answers `turn_state`, not `NOT_LATEST_TURN`
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let running = seed(&rig.quota.db, &chat, turn("stuck").at(10).running()).await;
    seed(&rig.quota.db, &chat, turn("newer").at(100)).await;
    let error = preview(&rig, &chat, MutationOp::Retry, running.request_id)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::TurnNotTerminal), "{error:?}");
}

#[tokio::test]
async fn after_deleting_the_latest_turn_the_previous_one_is_the_latest() {
    // acceptance: after deleting T2, T1 becomes the latest turn and can be retried
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (first, second) = two_turns(&rig, &chat).await;
    let detail = rig
        .mutations
        .admit(&a1(), MutationOp::Delete, chat.id)
        .await
        .unwrap();
    let delete_preview = rig
        .mutations
        .preview(&a1(), &detail, MutationOp::Delete, second.request_id)
        .await
        .unwrap();
    rig.mutations
        .commit_delete(&a1(), &detail, &delete_preview)
        .await
        .unwrap();
    let preview = preview(&rig, &chat, MutationOp::Retry, first.request_id)
        .await
        .unwrap();
    assert_eq!(preview.target.row.request_id, first.request_id);
}

#[tokio::test]
async fn a_missing_user_message_is_an_internal_error() {
    // inst-mp-user-message-missing, inst-mp-return-500
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    preview(&rig, &chat, MutationOp::Retry, seeded.request_id)
        .await
        .expect("the turn is previewable while its user message exists");
    crate::test_support::chats::soft_delete_message(&rig.quota.db, seeded.user_message_id).await;
    let error = preview(&rig, &chat, MutationOp::Retry, seeded.request_id)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Internal { .. }), "{error:?}");
}

#[tokio::test]
async fn the_preview_is_read_only_and_evaluates_the_pdp_exactly_once() {
    // inst-cr-no-early-lock: nothing is written and nothing is locked by the preview
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, second) = two_turns(&rig, &chat).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    preview(&rig, &chat, MutationOp::Retry, second.request_id)
        .await
        .unwrap();
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
    assert_eq!(rig.provider.call_count(), 0);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "retry_turn");
}

#[tokio::test]
async fn each_operation_evaluates_its_own_action_with_the_chat_id() {
    for (op, action) in [
        (MutationOp::Retry, "retry_turn"),
        (MutationOp::Edit, "edit_turn"),
        (MutationOp::Delete, "delete_turn"),
    ] {
        let rig = send_rig().await;
        let chat = rig.std_chat().await;
        let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
        preview(&rig, &chat, op, seeded.request_id).await.unwrap();
        let request = rig.pdp.only_request();
        assert_eq!(request.action.name, action);
        assert_eq!(
            request.resource.id,
            Some(chat.id),
            "the chat id is the resource id"
        );
    }
}

#[tokio::test]
async fn a_chat_of_another_user_is_a_not_found_of_the_chat_type() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let result = rig
        .mutations
        .admit(
            &crate::test_support::chats::a2(),
            MutationOp::Retry,
            chat.id,
        )
        .await;
    assert!(
        matches!(
            result,
            Err(DomainError::NotFound {
                resource: ResourceKind::Chat,
                ..
            })
        ),
        "{result:?} for {}",
        seeded.request_id
    );
}

// ── the preflight: the prepared mutation ─────────────────────────────────────

#[tokio::test]
async fn a_retry_prepares_the_original_content_and_the_preflight_instant() {
    // inst-mpf-quota-preflight, inst-mpf-keep-decision, inst-mpf-return
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, second) = two_turns(&rig, &chat).await;
    let prepared = prepared(
        &rig,
        &chat,
        MutationOp::Retry,
        second.request_id,
        "second question",
    )
    .await
    .unwrap();
    assert_eq!(prepared.content, "second question");
    assert_eq!(prepared.decision.started_at, default_now());
    assert_eq!(prepared.decision.effective_model, STANDARD);
    assert_eq!(prepared.model.id, STANDARD);
    assert_eq!(prepared.preview.target.row.request_id, second.request_id);
}

#[tokio::test]
async fn an_edit_prepares_the_content_exactly_as_given() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("old text")).await;
    let prepared = prepared(
        &rig,
        &chat,
        MutationOp::Edit,
        seeded.request_id,
        "  new text with spaces  ",
    )
    .await
    .unwrap();
    assert_eq!(prepared.content, "  new text with spaces  ");
}

#[tokio::test]
async fn the_preflight_charges_the_caller_as_the_requester_and_writes_nothing() {
    // inst-mpf-quota-preflight: `identity.user_id` is the caller; inst-mpf-return: no row, no
    // reserve, no provider request
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "q")
        .await
        .unwrap();
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
    assert_eq!(rig.provider.call_count(), 0);
    assert!(
        crate::test_support::quota::all_quota_rows(&rig.quota.db)
            .await
            .is_empty(),
        "the preflight takes no reserve"
    );
    let calls = rig.gateway.current_calls.lock().unwrap().clone();
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|user| *user == USER_A1), "{calls:?}");
}

#[tokio::test]
async fn the_snapshot_boundary_is_the_latest_message_of_another_turn() {
    // inst-mpf-snapshot-boundary: the history before the mutated turn
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (first, second) = two_turns(&rig, &chat).await;
    let prepared = prepared(&rig, &chat, MutationOp::Retry, second.request_id, "q")
        .await
        .unwrap();
    let boundary = prepared.boundary.expect("a boundary");
    assert_eq!(boundary.id, first.assistant_message_id.unwrap());
    assert_eq!(boundary.created_at, t(11));
}

#[tokio::test]
async fn the_first_turn_of_a_chat_has_no_snapshot_boundary() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let only = seed(&rig.quota.db, &chat, turn("q")).await;
    let prepared = prepared(&rig, &chat, MutationOp::Retry, only.request_id, "q")
        .await
        .unwrap();
    assert!(prepared.boundary.is_none());
}

#[tokio::test]
async fn the_web_search_flag_of_the_target_is_carried() {
    // inst-tcf-web-search-flag
    let rig = send_rig().await;
    for flag in [true, false] {
        let chat = rig.std_chat().await;
        let plan = if flag {
            turn("q").web_search()
        } else {
            turn("q")
        };
        let seeded = seed(&rig.quota.db, &chat, plan).await;
        let prepared = prepared(&rig, &chat, MutationOp::Edit, seeded.request_id, "changed")
            .await
            .unwrap();
        assert_eq!(prepared.web_search_enabled, flag);
    }
}

#[tokio::test]
async fn the_carried_attachments_skip_deleted_ones_and_keep_the_link_order() {
    // inst-mpf-carry-forward, inst-acf-select, inst-acf-exclude-deleted, inst-acf-order
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let db = &rig.quota.db;
    let late = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let early = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let gone = seed_att(db, &chat, AttachmentSeed::ready("image").deleted(at(1))).await;
    let document = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let seeded = seed(
        db,
        &chat,
        turn("look")
            .attach(late, 30)
            .attach(early, 20)
            .attach(gone, 25)
            .attach(document, 40),
    )
    .await;
    let prepared = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "look")
        .await
        .unwrap();
    assert_eq!(prepared.carried.attachment_ids, vec![early, late, document]);
    assert_eq!(
        prepared
            .carried
            .images
            .iter()
            .map(|f| f.id)
            .collect::<Vec<_>>(),
        vec![early, late]
    );
}

#[tokio::test]
async fn only_the_links_of_the_original_message_are_carried() {
    // inst-acf-only-original: never another message's links, never a later upload
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let db = &rig.quota.db;
    let old_image = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let _unlinked = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let first = seed(db, &chat, turn("first").at(10).attach(old_image, 10)).await;
    let second = seed(db, &chat, turn("second").at(100)).await;
    let prepared = prepared(&rig, &chat, MutationOp::Retry, second.request_id, "second")
        .await
        .unwrap();
    assert!(prepared.carried.attachment_ids.is_empty());
    assert!(prepared.carried.images.is_empty());
    assert_ne!(first.request_id, second.request_id);
}

// ── the preflight: rejections ────────────────────────────────────────────────

/// A rejected preflight leaves every row, the quota and the provider untouched.
async fn assert_rejected_without_trace<T: std::fmt::Debug>(
    rig: &SendRig,
    chat: &ChatRecord,
    before: &crate::test_support::mutations::Snapshot,
    result: Result<T, DomainError>,
    check: impl FnOnce(&DomainError) -> bool,
) {
    let error = result.expect_err("the preflight must reject");
    assert!(check(&error), "{error:?}");
    assert_eq!(&snapshot(&rig.quota.db, chat).await, before);
    assert_eq!(rig.provider.call_count(), 0);
}

#[tokio::test]
async fn a_model_missing_from_the_catalog_is_an_invalid_model() {
    // inst-mpf-model-missing, inst-mpf-return-invalid-model
    let rig = send_rig().await;
    let chat = rig.quota.chat_a1("ghost-model").await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "q").await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(e, DomainError::InvalidModel { .. })
    })
    .await;
}

#[tokio::test]
async fn a_failing_policy_plugin_is_an_error_not_a_rejection_of_the_content() {
    // inst-mpf-policy-failed, inst-mpf-return-policy-500
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "q")
        .await
        .expect("the same preflight succeeds while the plugin answers");
    rig.gateway.fail_current.store(true, Ordering::SeqCst);
    let result = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "q").await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(
            e,
            DomainError::PolicyUnavailable { .. } | DomainError::Internal { .. }
        )
    })
    .await;
}

#[tokio::test]
async fn an_exhausted_token_quota_rejects_before_any_write() {
    // inst-mpf-quota-rejected, inst-mpf-return-quota
    let rig = send_rig().await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "q").await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(
            e,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        )
    })
    .await;
}

#[tokio::test]
async fn an_exhausted_web_search_quota_rejects_a_turn_that_had_web_search() {
    // inst-tcf-limits: the daily web search check applies as to a new send
    let rig = send_rig().await;
    seed_rows(&rig.quota.db, &[row("daily", DAY, "total").web(75)]).await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q").web_search()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "q").await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(
            e,
            DomainError::QuotaExceeded {
                scope: QuotaScope::WebSearch
            }
        )
    })
    .await;
}

#[tokio::test]
async fn the_web_search_kill_switch_rejects_a_turn_that_had_web_search() {
    let mut snapshot_with_switch = default_catalog();
    snapshot_with_switch.kill_switches.disable_web_search = true;
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(snapshot_with_switch)).await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q").web_search()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = prepared(&rig, &chat, MutationOp::Edit, seeded.request_id, "again").await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(
            e,
            DomainError::FeatureDisabled {
                subject: "web_search"
            }
        )
    })
    .await;
}

#[tokio::test]
async fn more_carried_images_than_the_limit_are_too_many_images() {
    // inst-mpf-too-many-images, inst-mpf-return-too-many-images: `rag.max_images_per_message` 4
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let db = &rig.quota.db;
    let mut plan = turn("five images");
    for index in 0..5 {
        let id = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
        plan = plan.attach(id, 10 + index);
    }
    let seeded = seed(db, &chat, plan).await;
    let before = snapshot(db, &chat).await;
    let result = prepared(
        &rig,
        &chat,
        MutationOp::Retry,
        seeded.request_id,
        "five images",
    )
    .await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(e, DomainError::TooManyImages)
    })
    .await;
}

#[tokio::test]
async fn a_deleted_image_does_not_count_towards_the_image_limit() {
    // five links, one of them deleted since: four images are carried and the limit holds
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let db = &rig.quota.db;
    let mut plan = turn("five links");
    for index in 0..4 {
        let id = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
        plan = plan.attach(id, 10 + index);
    }
    let gone = seed_att(db, &chat, AttachmentSeed::ready("image").deleted(at(1))).await;
    plan = plan.attach(gone, 20);
    let seeded = seed(db, &chat, plan).await;
    let prepared = prepared(
        &rig,
        &chat,
        MutationOp::Retry,
        seeded.request_id,
        "five links",
    )
    .await
    .unwrap();
    assert_eq!(prepared.carried.images.len(), 4);
}

#[tokio::test]
async fn images_on_a_model_without_vision_are_a_vision_not_supported_rejection() {
    // inst-mpf-image-guards: images are never dropped to make the request pass
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let db = &rig.quota.db;
    let image = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let seeded = seed(db, &chat, turn("see").attach(image, 10)).await;
    let before = snapshot(db, &chat).await;
    let result = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "see").await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(e, DomainError::VisionNotSupported)
    })
    .await;
}

#[tokio::test]
async fn the_images_kill_switch_rejects_a_turn_with_carried_images() {
    let mut switched = default_catalog();
    switched.kill_switches.disable_images = true;
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(switched)).await;
    let chat = rig.prem_chat().await;
    let db = &rig.quota.db;
    let image = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let seeded = seed(db, &chat, turn("see").attach(image, 10)).await;
    let before = snapshot(db, &chat).await;
    let result = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "see").await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(e, DomainError::FeatureDisabled { subject: "images" })
    })
    .await;
}

#[tokio::test]
async fn a_replacement_above_the_input_limit_is_input_too_long() {
    // inst-mpf-input-too-long: evaluated in the preflight, before the commit
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(catalog_of(vec![
        prem_model(&json!({})),
        std_model(&json!({
            "max_input_tokens": 50, "estimation_budgets": byte_budgets(),
        })),
    ])))
    .await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("short")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = prepared(
        &rig,
        &chat,
        MutationOp::Edit,
        seeded.request_id,
        &"x".repeat(200),
    )
    .await;
    assert_rejected_without_trace(&rig, &chat, &before, result, |e| {
        matches!(e, DomainError::InputTooLong)
    })
    .await;
}

#[tokio::test]
async fn the_input_limit_does_not_reject_a_content_that_fits() {
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(catalog_of(vec![
        prem_model(&json!({})),
        std_model(&json!({
            "max_input_tokens": 50, "estimation_budgets": byte_budgets(),
        })),
    ])))
    .await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("short")).await;
    prepared(
        &rig,
        &chat,
        MutationOp::Edit,
        seeded.request_id,
        &"x".repeat(40),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_premium_chat_keeps_its_effective_model_in_the_decision() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let prepared = prepared(&rig, &chat, MutationOp::Retry, seeded.request_id, "q")
        .await
        .unwrap();
    assert_eq!(prepared.decision.selected_model, PREMIUM);
    assert_eq!(prepared.decision.effective_model, PREMIUM);
}
