#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The delete transaction on a real (SQLite) database: the soft-deletes, the
//! summary invalidation, the `turn_delete` event of the same transaction, the
//! untouched `chats.updated_at`, links, reactions and quota, the guarded
//! first statement and "nothing left behind" for every failure.

use mini_chat_sdk::TurnMutationAuditEvent;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::mutations::ports::MutationRepository;
use crate::mutation_test_env::{MutationEnv, SeedTurn, owner_scope};
use crate::test_doubles::USER;

const NOW: &str = "2026-03-10T12:00:00.250001Z";

async fn msg(env: &MutationEnv, id: Uuid, column: &str) -> Option<String> {
    let quoted = env.message_col(id, column).await;
    (quoted != "NULL").then(|| quoted.trim_matches('\'').to_owned())
}

async fn deleted(env: &MutationEnv, old: &SeedTurn) {
    env.repo
        .delete_turn(&env.delete_plan(old))
        .await
        .expect("the delete commits");
}

#[tokio::test]
async fn the_turn_is_soft_deleted_without_a_replacement_and_keeps_its_state() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    let seeded = env.seed_turn(&old).await;
    let state_before = env.turn_col(seeded.request_id, "state").await;
    let reserve_before = env.turn_col(seeded.request_id, "reserve_tokens").await;

    deleted(&env, &old).await;

    assert_eq!(
        env.turn_deleted_at(seeded.request_id).await.as_deref(),
        Some(NOW)
    );
    assert_eq!(
        env.turn_text(seeded.request_id, "updated_at")
            .await
            .as_deref(),
        Some(NOW)
    );
    assert_eq!(env.replaced_by_request(seeded.request_id).await, None);
    assert_eq!(env.turn_col(seeded.request_id, "state").await, state_before);
    assert_eq!(
        env.turn_col(seeded.request_id, "reserve_tokens").await,
        reserve_before
    );
    assert!(
        env.turn_text(seeded.request_id, "assistant_message_id")
            .await
            .is_some(),
        "the assistant_message_id of the turn stays"
    );
}

#[tokio::test]
async fn the_user_and_assistant_messages_are_soft_deleted_with_the_mutation_time() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    let seeded = env.seed_turn(&old).await;

    deleted(&env, &old).await;

    for id in [seeded.user_message, seeded.assistant_message] {
        assert_eq!(msg(&env, id, "deleted_at").await.as_deref(), Some(NOW));
    }
    assert!(env.messages(env.chat).await.is_empty());
}

#[tokio::test]
async fn a_failed_turn_deletes_its_single_message() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::failed(1, 10);
    env.seed_turn(&old).await;

    deleted(&env, &old).await;

    let rows = env.messages_of(old.request_id()).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].deleted);
}

#[tokio::test]
async fn the_previous_turn_becomes_the_latest_and_stays_untouched() {
    let env = MutationEnv::new().await;
    let previous = SeedTurn::completed(1, 10);
    let seeded_previous = env.seed_turn(&previous).await;
    let old = SeedTurn::completed(2, 20);
    env.seed_turn(&old).await;

    deleted(&env, &old).await;

    let latest = env
        .repo
        .latest_turn(&owner_scope(), env.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, seeded_previous.turn_id);
    assert!(env.turn_deleted_at(previous.request_id()).await.is_none());
    assert_eq!(
        env.messages(env.chat)
            .await
            .iter()
            .map(|m| m.1.as_str())
            .collect::<Vec<_>>(),
        ["question 1", "answer 1"]
    );
}

#[tokio::test]
async fn the_chat_updated_at_is_not_bumped() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let before = env.chat_updated_at().await;

    deleted(&env, &old).await;

    assert_eq!(env.chat_updated_at().await, before);
}

#[tokio::test]
async fn links_reactions_and_quota_are_not_touched() {
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let old = SeedTurn::completed(1, 10).attached(&[doc]);
    let seeded = env.seed_turn(&old).await;
    env.quota
        .db
        .insert(
            "message_reactions",
            crate::infra::db::test_support::Row::default()
                .set("id", Uuid::new_v4())
                .set("tenant_id", crate::test_doubles::USER_TENANT)
                .set("message_id", seeded.assistant_message)
                .set("user_id", USER)
                .set("reaction", "like")
                .set("created_at", crate::chat_test_env::ts(12)),
        )
        .await
        .unwrap();
    let links = env.quota.dump("message_attachments").await;
    let reactions = env.quota.dump("message_reactions").await;
    let quota = env.quota.dump("quota_usage").await;

    deleted(&env, &old).await;

    assert_eq!(env.quota.dump("message_attachments").await, links);
    assert_eq!(env.quota.dump("message_reactions").await, reactions);
    assert_eq!(env.quota.dump("quota_usage").await, quota);
    assert!(env.usage_events().await.is_empty());
}

#[tokio::test]
async fn exactly_one_turn_delete_event_is_enqueued_in_the_same_transaction() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let plan = env.delete_plan(&old);

    env.repo.delete_turn(&plan).await.unwrap();

    assert_eq!(env.mutation_events().await, vec![plan.audit.clone()]);
    let payloads = env.audit_payloads().await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["event_type"], "turn_delete");
    assert_eq!(payloads[0]["request_id"], old.request_id().to_string());
    assert!(matches!(
        env.mutation_events().await[0],
        TurnMutationAuditEvent::TurnDelete { .. }
    ));
}

#[tokio::test]
async fn a_summary_covering_the_deleted_turn_is_deleted_and_the_flags_cleared() {
    let env = MutationEnv::new().await;
    let older = SeedTurn::completed(1, 10).compressed();
    env.seed_turn(&older).await;
    let old = SeedTurn::completed(2, 20);
    let seeded = env.seed_turn(&old).await;
    env.seed_summary_at(seeded.assistant_message, 21).await;

    deleted(&env, &old).await;

    assert_eq!(env.summary_count().await, 0);
    assert_eq!(env.compressed_count().await, 0);
}

#[tokio::test]
async fn a_frontier_exactly_at_the_user_message_of_the_deleted_turn_covers_it() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10).compressed();
    let seeded = env.seed_turn(&old).await;
    env.seed_summary_at(seeded.user_message, 10).await;

    deleted(&env, &old).await;

    assert_eq!(env.summary_count().await, 0);
    assert_eq!(env.compressed_count().await, 0);
}

#[tokio::test]
async fn a_summary_before_the_deleted_turn_is_kept_with_its_flags() {
    let env = MutationEnv::new().await;
    let older = SeedTurn::completed(1, 10).compressed();
    let seeded_older = env.seed_turn(&older).await;
    let old = SeedTurn::completed(2, 20);
    env.seed_turn(&old).await;
    env.seed_summary_at(seeded_older.assistant_message, 11)
        .await;

    deleted(&env, &old).await;

    assert_eq!(env.summary_count().await, 1);
    assert_eq!(env.compressed_count().await, 2);
}

// ---- the first statement and the rollbacks ------------------------------------------------------

#[tokio::test]
async fn an_already_deleted_turn_is_not_the_latest_and_nothing_changes() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10).deleted();
    env.seed_turn(&old).await;
    let before = env.world().await;

    let error = env
        .repo
        .delete_turn(&env.delete_plan(&old))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(env.world().await, before);
}

#[tokio::test]
async fn a_running_turn_is_not_deleted_by_the_guarded_first_statement() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::running(1, 10);
    env.seed_turn(&old).await;
    let before = env.world().await;

    let error = env
        .repo
        .delete_turn(&env.delete_plan(&old))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(env.world().await, before);
}

#[tokio::test]
async fn a_newer_live_turn_rolls_the_guarded_soft_delete_back() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    env.seed_turn(&SeedTurn::completed(2, 20)).await;
    let before = env.world().await;

    let error = env
        .repo
        .delete_turn(&env.delete_plan(&old))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(env.world().await, before, "the old turn is live again");
    assert!(env.mutation_events().await.is_empty());
}

#[tokio::test]
async fn a_newer_running_turn_makes_the_delete_not_the_latest_turn() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    env.seed_turn(&SeedTurn::running(2, 20)).await;
    let before = env.world().await;

    let error = env
        .repo
        .delete_turn(&env.delete_plan(&old))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(env.world().await, before);
}

#[tokio::test]
async fn with_equal_start_times_only_the_greater_id_can_be_deleted() {
    let env = MutationEnv::new().await;
    let lesser = SeedTurn::completed(1, 10);
    let greater = SeedTurn::completed(2, 10);
    env.seed_turn(&lesser).await;
    env.seed_turn(&greater).await;

    let refused = env
        .repo
        .delete_turn(&env.delete_plan(&lesser))
        .await
        .unwrap_err();
    env.repo
        .delete_turn(&env.delete_plan(&greater))
        .await
        .expect("the greater id is the latest");

    assert!(matches!(refused, DomainError::NotLatestTurn), "{refused:?}");
}

#[tokio::test]
async fn a_failing_audit_enqueue_rolls_the_delete_back() {
    let healthy = MutationEnv::new().await;
    let fine = SeedTurn::completed(1, 10);
    healthy.seed_turn(&fine).await;
    healthy
        .repo
        .delete_turn(&healthy.delete_plan(&fine))
        .await
        .expect("the same delete commits when the audit queue is healthy");
    let env = MutationEnv::with_failing_mutation_audit().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let before = env.world().await;

    let error = env
        .repo
        .delete_turn(&env.delete_plan(&old))
        .await
        .unwrap_err();

    assert!(
        matches!(error, DomainError::Internal(_) | DomainError::Database(_)),
        "{error:?}"
    );
    assert_eq!(env.world().await, before);
}

#[tokio::test]
async fn a_repeated_delete_is_not_the_latest_turn_and_adds_no_event() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    deleted(&env, &old).await;
    let after_first = env.world().await;

    let error = env
        .repo
        .delete_turn(&env.delete_plan(&old))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(env.world().await, after_first);
    assert_eq!(env.mutation_events().await.len(), 1);
}
