//! The reaction persistence over in-memory `SQLite`: the upsert (one row per user and message,
//! idempotent, a replace updates `reaction` and `created_at` of the same row, `tenant_id` and
//! `user_id` from the arguments), concurrent upserts without a conflict error, the removal of the
//! caller's row only, the removal without a row, the absence of side effects, and the lookup of
//! the target message (non-deleted messages of the scoped chat).
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert` (`inst-ru-upsert`,
//! `inst-ru-one-row`, `inst-ru-idempotent`, `inst-ru-no-side-effects`, `inst-ru-return`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal` (`inst-rr-delete`,
//! `inst-rr-idempotent`, `inst-rr-owner-only`, `inst-rr-no-side-effects`, `inst-rr-return`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target` (`inst-rt-select`,
//! `inst-rt-missing`); state `cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction`
//! (`inst-rx-none-like`, `inst-rx-like-dislike`, `inst-rx-same`, `inst-rx-remove`,
//! `inst-rx-remove-none`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-reactions`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::Duration;
use uuid::Uuid;

use crate::domain::messages::Reaction;
use crate::domain::mutations::ReactionStore;
use crate::domain::streaming::StreamStore;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{message_rows, outbox_body_count};
use crate::test_support::mutations::{SeededTurn, reaction_rows, seed, turn};
use crate::test_support::quota::default_now;
use crate::test_support::send::{SendRig, chat_model, send_rig};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1, USER_A2};

async fn fixture() -> (SendRig, ChatRecord, SeededTurn, Uuid) {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    let assistant = seeded.assistant_message_id.unwrap();
    (rig, chat, seeded, assistant)
}

async fn upsert(
    rig: &SendRig,
    user: Uuid,
    message: Uuid,
    reaction: Reaction,
    secs: i64,
) -> crate::domain::mutations::StoredReaction {
    ReactionStore::upsert_reaction(
        &*rig.quota.db.provider,
        TENANT_A,
        user,
        message,
        reaction,
        default_now() + Duration::seconds(secs),
    )
    .await
    .expect("the upsert")
}

async fn remove(rig: &SendRig, tenant: Uuid, user: Uuid, message: Uuid) {
    ReactionStore::remove_reaction(&*rig.quota.db.provider, tenant, user, message)
        .await
        .expect("the removal");
}

// ── the upsert ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_first_put_creates_one_row_with_the_identity_of_the_caller() {
    // inst-ru-upsert, inst-rx-none-like, inst-ru-return
    let (rig, _, _, assistant) = fixture().await;
    let stored = upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    assert_eq!(stored.message_id, assistant);
    assert_eq!(stored.reaction, Reaction::Like);
    assert_eq!(stored.created_at, default_now());

    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message_id, assistant);
    assert_eq!(rows[0].user_id, USER_A1);
    assert_eq!(rows[0].tenant_id, TENANT_A);
    assert_eq!(rows[0].reaction, "like");
    assert_eq!(rows[0].created_at, default_now());
    assert_eq!(rows[0].id.get_version_num(), 4, "a new UUID v4 id");
}

#[tokio::test]
async fn a_repeated_put_with_the_same_value_leaves_one_row() {
    // inst-ru-idempotent, inst-ru-one-row, inst-rx-same
    let (rig, _, _, assistant) = fixture().await;
    upsert(&rig, USER_A1, assistant, Reaction::Dislike, 0).await;
    let first = reaction_rows(&rig.quota.db, assistant).await;
    let again = upsert(&rig, USER_A1, assistant, Reaction::Dislike, 5).await;
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reaction, "dislike");
    assert_eq!(rows[0].id, first[0].id, "the same row");
    assert_eq!(again.reaction, Reaction::Dislike);
}

#[tokio::test]
async fn switching_the_value_replaces_the_reaction_and_its_created_at_in_the_same_row() {
    // inst-rx-like-dislike, feature-local deviation: a replace updates `created_at`
    let (rig, _, _, assistant) = fixture().await;
    upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    let first = reaction_rows(&rig.quota.db, assistant).await;
    let replaced = upsert(&rig, USER_A1, assistant, Reaction::Dislike, 60).await;
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1, "one reaction per user and message");
    assert_eq!(rows[0].id, first[0].id);
    assert_eq!(rows[0].reaction, "dislike");
    assert_eq!(rows[0].created_at, default_now() + Duration::seconds(60));
    assert_eq!(replaced.created_at, rows[0].created_at);
    // and back
    upsert(&rig, USER_A1, assistant, Reaction::Like, 120).await;
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reaction, "like");
}

#[tokio::test]
async fn each_user_has_their_own_reaction_on_a_message() {
    let (rig, _, _, assistant) = fixture().await;
    upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    upsert(&rig, USER_A2, assistant, Reaction::Dislike, 0).await;
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 2);
    let of = |user: Uuid| {
        rows.iter()
            .find(|r| r.user_id == user)
            .unwrap()
            .reaction
            .clone()
    };
    assert_eq!(of(USER_A1), "like");
    assert_eq!(of(USER_A2), "dislike");
}

#[tokio::test]
async fn the_same_user_reacts_to_different_messages_independently() {
    let (rig, chat, _, first) = fixture().await;
    let other = seed(&rig.quota.db, &chat, turn("again").at(100)).await;
    let second = other.assistant_message_id.unwrap();
    upsert(&rig, USER_A1, first, Reaction::Like, 0).await;
    upsert(&rig, USER_A1, second, Reaction::Dislike, 0).await;
    assert_eq!(
        reaction_rows(&rig.quota.db, first).await[0].reaction,
        "like"
    );
    assert_eq!(
        reaction_rows(&rig.quota.db, second).await[0].reaction,
        "dislike"
    );
}

#[tokio::test]
async fn two_concurrent_puts_both_succeed_and_leave_one_row() {
    // inst-ru-one-row: no 409 and no 500 for the unique conflict; the last committed value stays
    for round in 0..5 {
        let (rig, _, _, assistant) = fixture().await;
        let provider = &*rig.quota.db.provider;
        let (a, b) = tokio::join!(
            ReactionStore::upsert_reaction(
                provider,
                TENANT_A,
                USER_A1,
                assistant,
                Reaction::Like,
                default_now()
            ),
            ReactionStore::upsert_reaction(
                provider,
                TENANT_A,
                USER_A1,
                assistant,
                Reaction::Dislike,
                default_now() + Duration::seconds(1)
            ),
        );
        let (a, b) = (a.expect("first put"), b.expect("second put"));
        let rows = reaction_rows(&rig.quota.db, assistant).await;
        assert_eq!(rows.len(), 1, "round {round}");
        assert!(
            rows[0].reaction == a.reaction.as_str() || rows[0].reaction == b.reaction.as_str(),
            "round {round}: {rows:?}"
        );
    }
}

#[tokio::test]
async fn a_put_has_no_side_effect_on_the_chat_the_message_or_the_outbox() {
    // inst-ru-no-side-effects: no audit event, no outbox message, no `updated_at` bump
    let (rig, chat, _, assistant) = fixture().await;
    let messages_before = message_rows(&rig.quota.db, chat.id).await;
    let chat_before = chat_model(&rig.quota.db, chat.id).await.updated_at;
    let outbox_before = outbox_body_count(&rig.quota.db).await;
    upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    upsert(&rig, USER_A1, assistant, Reaction::Dislike, 1).await;
    assert_eq!(message_rows(&rig.quota.db, chat.id).await, messages_before);
    assert_eq!(
        chat_model(&rig.quota.db, chat.id).await.updated_at,
        chat_before
    );
    assert_eq!(outbox_body_count(&rig.quota.db).await, outbox_before);
}

// ── the removal ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_delete_removes_the_row_of_the_caller() {
    // inst-rr-delete, inst-rx-remove
    let (rig, _, _, assistant) = fixture().await;
    upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    remove(&rig, TENANT_A, USER_A1, assistant).await;
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
}

#[tokio::test]
async fn a_delete_without_a_reaction_is_a_success_and_repeatable() {
    // inst-rr-idempotent, inst-rx-remove-none: zero and one deleted rows alike
    let (rig, _, _, assistant) = fixture().await;
    remove(&rig, TENANT_A, USER_A1, assistant).await;
    upsert(&rig, USER_A1, assistant, Reaction::Dislike, 0).await;
    remove(&rig, TENANT_A, USER_A1, assistant).await;
    remove(&rig, TENANT_A, USER_A1, assistant).await;
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
}

#[tokio::test]
async fn a_delete_never_removes_another_users_reaction() {
    // inst-rr-owner-only
    let (rig, _, _, assistant) = fixture().await;
    upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    upsert(&rig, USER_A2, assistant, Reaction::Like, 0).await;
    remove(&rig, TENANT_A, USER_A1, assistant).await;
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, USER_A2);
}

#[tokio::test]
async fn a_delete_is_scoped_to_the_tenant_of_the_caller() {
    // inst-rr-delete: the caller's tenant is part of the predicate
    let (rig, _, _, assistant) = fixture().await;
    upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    remove(&rig, TENANT_B, USER_A1, assistant).await;
    assert_eq!(reaction_rows(&rig.quota.db, assistant).await.len(), 1);
}

#[tokio::test]
async fn a_delete_has_no_side_effect_on_the_chat_or_the_outbox() {
    // inst-rr-no-side-effects
    let (rig, chat, _, assistant) = fixture().await;
    upsert(&rig, USER_A1, assistant, Reaction::Like, 0).await;
    let chat_before = chat_model(&rig.quota.db, chat.id).await.updated_at;
    let outbox_before = outbox_body_count(&rig.quota.db).await;
    remove(&rig, TENANT_A, USER_A1, assistant).await;
    assert_eq!(
        chat_model(&rig.quota.db, chat.id).await.updated_at,
        chat_before
    );
    assert_eq!(outbox_body_count(&rig.quota.db).await, outbox_before);
}

// ── the target message ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_target_lookup_finds_live_messages_of_the_scoped_chat_only() {
    // inst-rt-select, inst-rt-missing: unknown id, another chat, a soft-deleted message
    let (rig, chat, seeded, assistant) = fixture().await;
    let store = &*rig.quota.db.provider;
    let found = StreamStore::load_message(store, chat.tenant_id, chat.id, assistant)
        .await
        .unwrap()
        .expect("the assistant message");
    assert_eq!(found.role, "assistant");
    let user = StreamStore::load_message(store, chat.tenant_id, chat.id, seeded.user_message_id)
        .await
        .unwrap()
        .expect("the user message is found; the role check is the domain's");
    assert_eq!(user.role, "user");

    let unknown = StreamStore::load_message(store, chat.tenant_id, chat.id, Uuid::new_v4())
        .await
        .unwrap();
    assert!(unknown.is_none());

    let other = rig.std_chat().await;
    let foreign = StreamStore::load_message(store, other.tenant_id, other.id, assistant)
        .await
        .unwrap();
    assert!(foreign.is_none(), "a message of another chat is not found");

    crate::test_support::chats::soft_delete_message(&rig.quota.db, assistant).await;
    let deleted = StreamStore::load_message(store, chat.tenant_id, chat.id, assistant)
        .await
        .unwrap();
    assert!(deleted.is_none(), "a soft-deleted message is not found");
}
