#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The reads and the setup-failure mark of the mutation store on a real
//! (SQLite) database: the target with soft-deleted rows, the latest turn by
//! `(started_at, id)`, the live user message, the carried attachment ids, the
//! history boundary of a replacement and the guarded failed mark.

use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::chat_test_env::BASE_UNIX;
use crate::domain::service::mutations::ports::MutationRepository;
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::stream::types::TurnState;
use crate::mutation_test_env::{MutationEnv, SeedTurn, owner_scope};
use crate::test_doubles::{OTHER_TENANT, OTHER_USER, USER, USER_TENANT};

fn at_unix(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(BASE_UNIX + secs).unwrap()
}

// ---- find_target ----------------------------------------------------------------------------------

#[tokio::test]
async fn the_target_is_read_with_its_state_requester_start_and_web_search_flag() {
    let env = MutationEnv::new().await;
    let seeded = env
        .seed_turn(&SeedTurn::completed(1, 10).web_search(true))
        .await;

    let found = env
        .repo
        .find_target(&owner_scope(), env.chat, seeded.request_id)
        .await
        .unwrap()
        .expect("the turn exists");

    assert_eq!(found.turn.id, seeded.turn_id);
    assert_eq!(found.turn.tenant_id, USER_TENANT);
    assert_eq!(found.turn.chat_id, env.chat);
    assert_eq!(found.turn.request_id, seeded.request_id);
    assert_eq!(found.turn.requester_user_id, Some(USER));
    assert_eq!(found.turn.state, TurnState::Completed);
    assert_eq!(
        found.turn.assistant_message_id,
        Some(seeded.assistant_message)
    );
    assert_eq!(found.turn.started_at, at_unix(10));
    assert!(!found.turn.deleted);
    assert!(found.web_search_enabled);
}

#[tokio::test]
async fn the_web_search_flag_false_is_read_as_false() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;

    let found = env
        .repo
        .find_target(&owner_scope(), env.chat, seeded.request_id)
        .await
        .unwrap()
        .unwrap();

    assert!(!found.web_search_enabled);
}

#[tokio::test]
async fn a_soft_deleted_turn_is_still_found_and_flagged_deleted() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10).deleted()).await;

    let found = env
        .repo
        .find_target(&owner_scope(), env.chat, seeded.request_id)
        .await
        .unwrap()
        .unwrap();

    assert!(found.turn.deleted);
}

#[tokio::test]
async fn a_turn_without_a_requester_is_read_with_none() {
    let env = MutationEnv::new().await;
    let seeded = env
        .seed_turn(&SeedTurn::completed(1, 10).requester(None))
        .await;

    let found = env
        .repo
        .find_target(&owner_scope(), env.chat, seeded.request_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(found.turn.requester_user_id, None);
}

#[tokio::test]
async fn an_unknown_request_id_a_turn_of_another_chat_and_another_tenant_are_not_found() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let other_chat = env.seed_chat(2, "p").await;

    let unknown = env
        .repo
        .find_target(&owner_scope(), env.chat, Uuid::from_u128(0xDEAD))
        .await
        .unwrap();
    let wrong_chat = env
        .repo
        .find_target(&owner_scope(), other_chat, seeded.request_id)
        .await
        .unwrap();
    let wrong_tenant = env
        .repo
        .find_target(
            &AccessScope::for_tenant(OTHER_TENANT).ensure_owner(OTHER_USER),
            env.chat,
            seeded.request_id,
        )
        .await
        .unwrap();

    assert_eq!(unknown, None);
    assert_eq!(wrong_chat, None);
    assert_eq!(wrong_tenant, None);
}

// ---- latest_turn ----------------------------------------------------------------------------------

#[tokio::test]
async fn the_latest_turn_is_the_live_one_with_the_greatest_start() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let newest = env.seed_turn(&SeedTurn::completed(2, 20)).await;
    env.seed_turn(&SeedTurn::completed(3, 15)).await;

    let latest = env
        .repo
        .latest_turn(&owner_scope(), env.chat)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(latest.id, newest.turn_id);
    assert_eq!(latest.started_at, at_unix(20));
}

#[tokio::test]
async fn a_soft_deleted_newer_turn_is_not_the_latest() {
    let env = MutationEnv::new().await;
    let older = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.seed_turn(&SeedTurn::completed(2, 20).deleted()).await;

    let latest = env
        .repo
        .latest_turn(&owner_scope(), env.chat)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(latest.id, older.turn_id);
}

#[tokio::test]
async fn a_running_newer_turn_is_the_latest() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let running = env.seed_turn(&SeedTurn::running(2, 20)).await;

    let latest = env
        .repo
        .latest_turn(&owner_scope(), env.chat)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(latest.id, running.turn_id);
}

#[tokio::test]
async fn equal_start_times_are_decided_by_the_greater_id() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let greater = env.seed_turn(&SeedTurn::completed(2, 10)).await;
    env.seed_turn(&SeedTurn::completed(0x0F, 5)).await;

    let latest = env
        .repo
        .latest_turn(&owner_scope(), env.chat)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(latest.id, greater.turn_id);
}

#[tokio::test]
async fn a_chat_without_a_live_turn_has_no_latest_turn() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10).deleted()).await;

    assert_eq!(
        env.repo
            .latest_turn(&owner_scope(), env.chat)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn the_latest_turn_of_another_chat_is_not_mixed_in() {
    let env = MutationEnv::new().await;
    let mine = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let other_chat = env.seed_chat(2, "p").await;
    env.seed_turn_in(other_chat, &SeedTurn::completed(2, 99))
        .await;

    let latest = env
        .repo
        .latest_turn(&owner_scope(), env.chat)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(latest.id, mine.turn_id);
}

// ---- user_message -----------------------------------------------------------------------------------

#[tokio::test]
async fn the_live_user_message_of_the_turn_is_read_not_the_answer() {
    let env = MutationEnv::new().await;
    let seeded = env
        .seed_turn(&SeedTurn::completed(1, 10).question("  the exact question \n"))
        .await;

    let message = env
        .repo
        .user_message(&owner_scope(), env.chat, seeded.request_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(message.id, seeded.user_message);
    assert_eq!(message.content, "  the exact question \n");
    assert_eq!(message.created_at, at_unix(10));
}

#[tokio::test]
async fn a_soft_deleted_user_message_is_not_found() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10).deleted()).await;

    assert_eq!(
        env.repo
            .user_message(&owner_scope(), env.chat, seeded.request_id)
            .await
            .unwrap(),
        None
    );
}

// ---- carried_attachment_ids -------------------------------------------------------------------------

#[tokio::test]
async fn the_carried_ids_are_the_linked_attachments_that_are_not_deleted() {
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let picture = env.seed_image(env.chat, 2).await;
    let gone = env.seed_attachment(env.chat, 3, |row| row).await;
    let seeded = env
        .seed_turn(&SeedTurn::completed(1, 10).attached(&[doc, picture, gone]))
        .await;
    env.soft_delete_attachment(gone).await;

    let mut carried = env
        .repo
        .carried_attachment_ids(&owner_scope(), env.chat, seeded.user_message)
        .await
        .unwrap();
    carried.sort();

    let mut expected = vec![doc, picture];
    expected.sort();
    assert_eq!(
        carried, expected,
        "the deleted attachment is silently left out"
    );
}

#[tokio::test]
async fn the_links_of_other_messages_are_not_carried() {
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let other = env.seed_attachment(env.chat, 2, |row| row).await;
    let older = env
        .seed_turn(&SeedTurn::completed(1, 10).attached(&[other]))
        .await;
    let target = env
        .seed_turn(&SeedTurn::completed(2, 20).attached(&[doc]))
        .await;

    let carried = env
        .repo
        .carried_attachment_ids(&owner_scope(), env.chat, target.user_message)
        .await
        .unwrap();
    let older_carried = env
        .repo
        .carried_attachment_ids(&owner_scope(), env.chat, older.user_message)
        .await
        .unwrap();

    assert_eq!(carried, vec![doc]);
    assert_eq!(older_carried, vec![other]);
}

#[tokio::test]
async fn a_message_without_links_carries_nothing() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;

    assert!(
        env.repo
            .carried_attachment_ids(&owner_scope(), env.chat, seeded.user_message)
            .await
            .unwrap()
            .is_empty()
    );
}

// ---- history_boundary -------------------------------------------------------------------------------

#[tokio::test]
async fn the_boundary_is_the_latest_live_message_of_another_request() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let previous = env.seed_turn(&SeedTurn::completed(2, 20)).await;
    // The replaced turn is soft-deleted, the new request has its own user message.
    env.seed_turn(&SeedTurn::completed(3, 30).deleted()).await;
    let new_request = Uuid::from_u128(0xAAAA_0001);
    env.seed_message_for(new_request, "user", "the new question", 40)
        .await;

    let boundary = env
        .repo
        .history_boundary(&owner_scope(), env.chat, new_request)
        .await
        .unwrap();

    assert_eq!(
        boundary,
        Some(MessageBoundary {
            created_at: at_unix(21),
            id: previous.assistant_message,
        })
    );
}

#[tokio::test]
async fn only_the_new_user_message_live_means_no_boundary() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10).deleted()).await;
    let new_request = Uuid::from_u128(0xAAAA_0001);
    env.seed_message_for(new_request, "user", "the new question", 40)
        .await;

    assert_eq!(
        env.repo
            .history_boundary(&owner_scope(), env.chat, new_request)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn an_empty_chat_has_no_boundary() {
    let env = MutationEnv::new().await;

    assert_eq!(
        env.repo
            .history_boundary(&owner_scope(), env.chat, Uuid::new_v4())
            .await
            .unwrap(),
        None
    );
}

// ---- mark_setup_failed ------------------------------------------------------------------------------

async fn running_unreserved(env: &MutationEnv, n: u128) -> crate::mutation_test_env::Seeded {
    let mut seed = SeedTurn::running(n, 10);
    seed.reserved = false;
    env.seed_turn(&seed).await
}

#[tokio::test]
async fn an_unreserved_running_turn_is_marked_failed_with_the_code_and_the_time() {
    let env = MutationEnv::new().await;
    let seeded = running_unreserved(&env, 1).await;
    let at = env.now();

    let updated = env
        .repo
        .mark_setup_failed(USER_TENANT, seeded.turn_id, "context_length_exceeded", at)
        .await
        .unwrap();

    assert_eq!(updated, 1);
    assert_eq!(env.turn_state_of(seeded.request_id).await, "failed");
    assert_eq!(
        env.turn_text(seeded.request_id, "error_code")
            .await
            .as_deref(),
        Some("context_length_exceeded")
    );
    let stamp = env.turn_text(seeded.request_id, "completed_at").await;
    assert_eq!(stamp.as_deref(), Some("2026-03-10T12:00:00.250001Z"));
    assert_eq!(env.turn_text(seeded.request_id, "updated_at").await, stamp);
    assert_eq!(env.turn_text(seeded.request_id, "error_detail").await, None);
    assert_eq!(
        env.turn_text(seeded.request_id, "assistant_message_id")
            .await,
        None
    );
    assert_eq!(
        env.turn_text(seeded.request_id, "reserve_tokens").await,
        None
    );
}

#[tokio::test]
async fn each_of_the_three_codes_is_stored() {
    for code in [
        "context_length_exceeded",
        "turn_setup_failed",
        "quota_exceeded",
    ] {
        let env = MutationEnv::new().await;
        let seeded = running_unreserved(&env, 1).await;

        env.repo
            .mark_setup_failed(USER_TENANT, seeded.turn_id, code, env.now())
            .await
            .unwrap();

        assert_eq!(
            env.turn_text(seeded.request_id, "error_code")
                .await
                .as_deref(),
            Some(code)
        );
    }
}

#[tokio::test]
async fn a_turn_that_already_holds_a_reserve_is_not_marked() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::running(1, 10)).await; // six columns set
    let before = env.quota.dump("chat_turns").await;

    let updated = env
        .repo
        .mark_setup_failed(USER_TENANT, seeded.turn_id, "turn_setup_failed", env.now())
        .await
        .unwrap();

    assert_eq!(
        updated, 0,
        "the guard `reserve_tokens IS NULL` keeps the reserve settled by the finalization"
    );
    assert_eq!(env.quota.dump("chat_turns").await, before);
}

#[tokio::test]
async fn a_terminal_turn_is_not_marked() {
    let env = MutationEnv::new().await;
    let mut seed = SeedTurn::completed(1, 10);
    seed.reserved = false;
    let seeded = env.seed_turn(&seed).await;
    let before = env.quota.dump("chat_turns").await;

    let updated = env
        .repo
        .mark_setup_failed(USER_TENANT, seeded.turn_id, "turn_setup_failed", env.now())
        .await
        .unwrap();

    assert_eq!(updated, 0);
    assert_eq!(env.quota.dump("chat_turns").await, before);
}

#[tokio::test]
async fn a_turn_of_another_tenant_is_not_marked() {
    let env = MutationEnv::new().await;
    let seeded = running_unreserved(&env, 1).await;

    let updated = env
        .repo
        .mark_setup_failed(OTHER_TENANT, seeded.turn_id, "turn_setup_failed", env.now())
        .await
        .unwrap();

    assert_eq!(updated, 0);
    assert_eq!(env.turn_state_of(seeded.request_id).await, "running");
}

#[tokio::test]
async fn an_unknown_turn_updates_no_row() {
    let env = MutationEnv::new().await;

    let updated = env
        .repo
        .mark_setup_failed(
            USER_TENANT,
            Uuid::from_u128(0xBEEF),
            "turn_setup_failed",
            env.now(),
        )
        .await
        .unwrap();

    assert_eq!(updated, 0);
}

#[tokio::test]
async fn a_marked_turn_no_longer_blocks_the_chat() {
    let env = MutationEnv::new().await;
    let seeded = running_unreserved(&env, 1).await;
    env.repo
        .mark_setup_failed(USER_TENANT, seeded.turn_id, "turn_setup_failed", env.now())
        .await
        .unwrap();

    // The one-running-turn index is free again: a second running turn inserts.
    env.seed_turn(&SeedTurn::running(2, 20)).await;

    assert_eq!(env.turn_state_of(seeded.request_id).await, "failed");
}
