#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The replacement transaction on a real (SQLite) database: the exact rows of
//! the DB writes table, the carry-forward, the summary invalidation, the audit
//! event of the same transaction, the statement order observable through the
//! rollbacks, and "nothing left behind" for every failure.

use mini_chat_sdk::TurnMutationAuditEvent;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::mutations::ports::{MutationRepository, ReplacementPlan};
use crate::domain::service::mutations::types::MutationOp;
use crate::mutation_test_env::{MutationEnv, SeedTurn};
use crate::test_doubles::USER;

/// The mutation time of the fixture clock as stored.
const NOW: &str = "2026-03-10T12:00:00.250001Z";

async fn msg(env: &MutationEnv, id: Uuid, column: &str) -> Option<String> {
    let quoted = env.message_col(id, column).await;
    (quoted != "NULL").then(|| quoted.trim_matches('\'').to_owned())
}

/// Every column of the old turn that a mutation must not change.
const UNCHANGED: [&str; 17] = [
    "state",
    "assistant_message_id",
    "error_code",
    "reserve_tokens",
    "max_output_tokens_applied",
    "reserved_credits_micro",
    "policy_version_applied",
    "effective_model",
    "minimal_generation_floor_applied",
    "provider_name",
    "provider_response_id",
    "error_detail",
    "started_at",
    "completed_at",
    "web_search_enabled",
    "requester_user_id",
    "web_search_completed_count",
];

async fn old_turn_columns(env: &MutationEnv, request: Uuid) -> Vec<String> {
    let mut out = Vec::new();
    for column in UNCHANGED {
        out.push(env.turn_col(request, column).await);
    }
    out
}

async fn replaced(
    env: &MutationEnv,
    old: &SeedTurn,
    op: MutationOp,
    content: &str,
) -> ReplacementPlan {
    let plan = env.replacement_plan(old, op, content);
    env.repo
        .replace_turn(&plan)
        .await
        .expect("the replacement commits");
    plan
}

// ==== the exact rows ================================================================================

#[tokio::test]
async fn the_new_turn_is_a_running_row_with_null_preflight_columns_and_the_old_web_search_flag() {
    for flag in [false, true] {
        let env = MutationEnv::new().await;
        let old = SeedTurn::completed(1, 10).web_search(flag);
        env.seed_turn(&old).await;

        let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;
        let new = plan.new_request_id;

        assert_eq!(env.turn_state_of(new).await, "running");
        assert_eq!(env.turn_id_of(new).await, plan.new_turn_id);
        for column in [
            "reserve_tokens",
            "max_output_tokens_applied",
            "reserved_credits_micro",
            "policy_version_applied",
            "effective_model",
            "minimal_generation_floor_applied",
            "provider_name",
            "provider_response_id",
            "assistant_message_id",
            "error_code",
            "error_detail",
            "deleted_at",
            "replaced_by_request_id",
            "completed_at",
        ] {
            assert_eq!(env.turn_text(new, column).await, None, "{column} is NULL");
        }
        assert_eq!(
            env.turn_text(new, "requester_type").await.as_deref(),
            Some("user")
        );
        assert_eq!(
            env.turn_col(new, "requester_user_id").await.to_lowercase(),
            format!("x'{}'", USER.simple())
        );
        for column in ["started_at", "last_progress_at", "updated_at"] {
            assert_eq!(
                env.turn_text(new, column).await.as_deref(),
                Some(NOW),
                "{column}"
            );
        }
        assert_eq!(
            env.turn_text(new, "web_search_enabled").await.as_deref(),
            Some(if flag { "1" } else { "0" })
        );
        for column in [
            "web_search_completed_count",
            "code_interpreter_completed_count",
            "file_search_completed_count",
        ] {
            assert_eq!(
                env.turn_text(new, column).await.as_deref(),
                Some("0"),
                "{column}"
            );
        }
    }
}

#[tokio::test]
async fn the_old_turn_is_soft_deleted_and_points_to_the_new_request_and_nothing_else_changes() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    let seeded = env.seed_turn(&old).await;
    let before = old_turn_columns(&env, seeded.request_id).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;

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
    assert_eq!(
        env.replaced_by_request(seeded.request_id).await,
        Some(plan.new_request_id)
    );
    assert_eq!(
        old_turn_columns(&env, seeded.request_id).await,
        before,
        "state, assistant_message_id, error_code, the preflight columns and the counts stay"
    );
}

#[tokio::test]
async fn a_failed_and_a_cancelled_old_turn_are_replaced_too_keeping_their_state() {
    for (old, state) in [
        (SeedTurn::failed(1, 10), "failed"),
        (SeedTurn::cancelled(1, 10), "cancelled"),
    ] {
        let env = MutationEnv::new().await;
        env.seed_turn(&old).await;

        replaced(&env, &old, MutationOp::Retry, "question 1").await;

        assert_eq!(env.turn_state_of(old.request_id()).await, state);
        assert!(env.turn_deleted_at(old.request_id()).await.is_some());
    }
}

#[tokio::test]
async fn the_old_user_and_assistant_messages_are_soft_deleted_with_the_mutation_time() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    let seeded = env.seed_turn(&old).await;

    replaced(&env, &old, MutationOp::Retry, "question 1").await;

    for id in [seeded.user_message, seeded.assistant_message] {
        assert_eq!(msg(&env, id, "deleted_at").await.as_deref(), Some(NOW));
    }
    let rows = env.messages_of(seeded.request_id).await;
    assert_eq!(rows.len(), 2, "the rows are kept");
    assert!(rows.iter().all(|row| row.deleted));
}

#[tokio::test]
async fn a_failed_turn_with_one_message_deletes_that_one_message() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::failed(1, 10);
    env.seed_turn(&old).await;

    replaced(&env, &old, MutationOp::Retry, "question 1").await;

    let rows = env.messages_of(old.request_id()).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].deleted);
}

#[tokio::test]
async fn a_retry_inserts_the_original_content_as_the_new_user_message_row() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10).question("  original \n text ");
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "  original \n text ").await;
    let id = plan.new_user_message_id;

    assert_eq!(msg(&env, id, "role").await.as_deref(), Some("user"));
    assert_eq!(
        msg(&env, id, "content").await.as_deref(),
        Some("  original \n text ")
    );
    assert_eq!(msg(&env, id, "content_type").await.as_deref(), Some("text"));
    assert_eq!(msg(&env, id, "token_estimate").await.as_deref(), Some("0"));
    assert_eq!(msg(&env, id, "request_kind").await.as_deref(), Some("chat"));
    assert_eq!(msg(&env, id, "features_used").await.as_deref(), Some("[]"));
    for column in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "reasoning_tokens",
    ] {
        assert_eq!(
            msg(&env, id, column).await.as_deref(),
            Some("0"),
            "{column}"
        );
    }
    for column in ["provider_response_id", "model", "deleted_at"] {
        assert_eq!(msg(&env, id, column).await, None, "{column}");
    }
    assert_eq!(msg(&env, id, "is_compressed").await.as_deref(), Some("0"));
    assert_eq!(msg(&env, id, "created_at").await.as_deref(), Some(NOW));
    assert_eq!(
        env.user_message_of(plan.new_request_id).await,
        id,
        "the message carries the new request id"
    );
}

#[tokio::test]
async fn an_edit_inserts_the_edited_content_as_sent() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let edited = "\n  edited with whitespace  \n";

    let plan = replaced(&env, &old, MutationOp::Edit, edited).await;

    assert_eq!(
        msg(&env, plan.new_user_message_id, "content")
            .await
            .as_deref(),
        Some(edited)
    );
}

#[tokio::test]
async fn the_replacement_inserts_no_assistant_message() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;

    let rows = env.messages_of(plan.new_request_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, "user");
}

#[tokio::test]
async fn only_the_new_turn_and_its_message_are_live_afterwards() {
    let env = MutationEnv::new().await;
    let previous = SeedTurn::completed(1, 10);
    env.seed_turn(&previous).await;
    let old = SeedTurn::completed(2, 20);
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "question 2").await;

    assert_eq!(
        env.live_turn_requests().await,
        vec![previous.request_id(), plan.new_request_id]
    );
    let live = env.messages(env.chat).await;
    assert_eq!(
        live.iter().map(|m| m.1.as_str()).collect::<Vec<_>>(),
        ["question 1", "answer 1", "question 2"]
    );
}

#[tokio::test]
async fn the_chat_updated_at_moves_to_the_mutation_time() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let before = env.chat_updated_at().await;

    replaced(&env, &old, MutationOp::Retry, "question 1").await;

    assert_ne!(before, NOW);
    assert_eq!(env.chat_updated_at().await, NOW);
}

#[tokio::test]
async fn the_quota_of_the_old_turn_is_untouched_and_no_usage_event_is_enqueued() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let before = env.quota.dump("quota_usage").await;

    replaced(&env, &old, MutationOp::Retry, "question 1").await;

    assert_eq!(env.quota.dump("quota_usage").await, before);
    assert!(env.usage_events().await.is_empty());
}

// ==== carry-forward ==================================================================================

#[tokio::test]
async fn the_non_deleted_links_are_copied_and_the_old_links_stay() {
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let picture = env.seed_image(env.chat, 2).await;
    let gone = env.seed_attachment(env.chat, 3, |row| row).await;
    let old = SeedTurn::completed(1, 10).attached(&[doc, picture, gone]);
    let seeded = env.seed_turn(&old).await;
    env.soft_delete_attachment(gone).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;

    let mut carried = vec![doc, picture];
    carried.sort();
    assert_eq!(env.links_of(plan.new_user_message_id).await, carried);
    let mut all = vec![doc, picture, gone];
    all.sort();
    assert_eq!(
        env.links_of(seeded.user_message).await,
        all,
        "the old links stay on the soft-deleted message"
    );
}

#[tokio::test]
async fn a_carried_link_has_the_tenant_the_chat_the_new_message_and_the_mutation_time() {
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let old = SeedTurn::completed(1, 10).attached(&[doc]);
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Edit, "edited").await;

    let row = env
        .sql(&format!(
            "SELECT hex(tenant_id) || '|' || hex(chat_id) || '|' || hex(message_id) || '|' || hex(attachment_id) || '|' || created_at \
             FROM message_attachments WHERE message_id = {}",
            crate::chat_test_env::blob(plan.new_user_message_id)
        ))
        .await;
    assert_eq!(
        row.to_lowercase(),
        format!(
            "{}|{}|{}|{}|{NOW}",
            crate::test_doubles::USER_TENANT.simple(),
            env.chat.simple(),
            plan.new_user_message_id.simple(),
            doc.simple()
        )
        .to_lowercase()
    );
}

#[tokio::test]
async fn an_attachment_deleted_after_the_preview_is_left_out_not_an_error() {
    // The copy set is read inside the transaction: the attachment deleted
    // between the preview and the transaction is silently excluded.
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let old = SeedTurn::completed(1, 10).attached(&[doc]);
    env.seed_turn(&old).await;
    let plan = env.replacement_plan(&old, MutationOp::Retry, "question 1");
    env.soft_delete_attachment(doc).await;

    env.repo.replace_turn(&plan).await.unwrap();

    assert!(env.links_of(plan.new_user_message_id).await.is_empty());
}

#[tokio::test]
async fn a_message_without_links_gets_no_links() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;

    assert!(env.links_of(plan.new_user_message_id).await.is_empty());
    assert_eq!(env.count("message_attachments").await, 0);
}

// ==== summary invalidation ===========================================================================

async fn summary_world(frontier_is_after_target: bool) -> (MutationEnv, SeedTurn) {
    let env = MutationEnv::new().await;
    let older = SeedTurn::completed(1, 10).compressed();
    let seeded_older = env.seed_turn(&older).await;
    let old = SeedTurn::completed(2, 20);
    let seeded = env.seed_turn(&old).await;
    if frontier_is_after_target {
        env.seed_summary_at(seeded.assistant_message, 21).await;
    } else {
        env.seed_summary_at(seeded_older.assistant_message, 11)
            .await;
    }
    (env, old)
}

#[tokio::test]
async fn a_summary_covering_the_replaced_turn_is_deleted_and_every_compressed_flag_cleared() {
    let (env, old) = summary_world(true).await;
    assert_eq!(env.summary_count().await, 1);
    assert_eq!(
        env.compressed_count().await,
        2,
        "the older turn is compressed"
    );

    replaced(&env, &old, MutationOp::Retry, "question 2").await;

    assert_eq!(env.summary_count().await, 0);
    assert_eq!(env.compressed_count().await, 0);
}

#[tokio::test]
async fn the_compressed_flags_of_soft_deleted_rows_are_cleared_too() {
    let env = MutationEnv::new().await;
    let gone = SeedTurn::completed(1, 5).compressed().deleted();
    env.seed_turn(&gone).await;
    let old = SeedTurn::completed(2, 20);
    let seeded = env.seed_turn(&old).await;
    env.seed_summary_at(seeded.assistant_message, 21).await;
    assert_eq!(env.compressed_count().await, 2);

    replaced(&env, &old, MutationOp::Edit, "edited").await;

    assert_eq!(env.compressed_count().await, 0);
}

#[tokio::test]
async fn a_frontier_exactly_at_the_replaced_user_message_covers_it() {
    let env = MutationEnv::new().await;
    let older = SeedTurn::completed(1, 10).compressed();
    env.seed_turn(&older).await;
    let old = SeedTurn::completed(2, 20);
    let seeded = env.seed_turn(&old).await;
    env.seed_summary_at(seeded.user_message, 20).await;

    replaced(&env, &old, MutationOp::Retry, "question 2").await;

    assert_eq!(env.summary_count().await, 0);
    assert_eq!(env.compressed_count().await, 0);
}

#[tokio::test]
async fn a_summary_before_the_replaced_turn_and_its_flags_are_kept() {
    let (env, old) = summary_world(false).await;

    replaced(&env, &old, MutationOp::Retry, "question 2").await;

    assert_eq!(env.summary_count().await, 1);
    assert_eq!(
        env.compressed_count().await,
        2,
        "the flags of the covered turn stay"
    );
}

#[tokio::test]
async fn a_chat_without_a_summary_is_replaced_without_touching_flags() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10).compressed();
    env.seed_turn(&old).await;

    replaced(&env, &old, MutationOp::Retry, "question 1").await;

    assert_eq!(env.summary_count().await, 0);
    assert_eq!(
        env.compressed_count().await,
        2,
        "no summary row: the flags are not cleared"
    );
}

// ==== the audit event of the same transaction ========================================================

#[tokio::test]
async fn exactly_one_turn_retry_event_is_enqueued_with_the_committed_replacement() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;

    assert_eq!(env.mutation_events().await, vec![plan.audit.clone()]);
    let payloads = env.audit_payloads().await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["event_type"], "turn_retry");
    assert_eq!(
        payloads[0]["original_request_id"],
        old.request_id().to_string()
    );
    assert_eq!(
        payloads[0]["new_request_id"],
        plan.new_request_id.to_string()
    );
}

#[tokio::test]
async fn an_edit_enqueues_a_turn_edit_event_without_any_content() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Edit, "secret edited text").await;

    assert!(matches!(
        env.mutation_events().await[0],
        TurnMutationAuditEvent::TurnEdit { .. }
    ));
    let raw = serde_json::to_string(&env.audit_payloads().await).unwrap();
    assert!(!raw.contains("secret edited text"));
    assert!(!raw.contains("question 1"));
    assert!(raw.contains(&plan.new_request_id.to_string()));
}

#[tokio::test]
async fn the_event_is_partitioned_by_the_tenant() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;

    replaced(&env, &old, MutationOp::Retry, "question 1").await;

    let rows = env.quota.outbox_rows("mini-chat.audit").await;
    assert_eq!(rows.len(), 1);
    let expected = env.quota.enqueuer.partition(
        crate::infra::outbox::queues::QueueKind::Audit,
        crate::test_doubles::USER_TENANT,
        env.chat,
    );
    assert_eq!(rows[0].partition, i64::from(expected));
}

// ==== the order of the statements and "nothing left behind" ==========================================

async fn assert_nothing_changed(env: &MutationEnv, before: &str) {
    assert_eq!(
        env.world().await,
        before,
        "the transaction rolled back completely"
    );
}

#[tokio::test]
async fn a_running_turn_of_the_chat_is_a_unique_violation_decided_by_the_first_statement() {
    // The target is already soft-deleted too: were the guarded UPDATE first,
    // the answer would be `NotLatestTurn`. The INSERT of the new running turn
    // is the first statement, so the one-running-turn index decides.
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10).deleted();
    env.seed_turn(&old).await;
    env.seed_turn(&SeedTurn::running(2, 20)).await;
    let before = env.world().await;

    let error = env
        .repo
        .replace_turn(&env.replacement_plan(&old, MutationOp::Retry, "q"))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::UniqueViolation), "{error:?}");
    assert_nothing_changed(&env, &before).await;
}

#[tokio::test]
async fn a_running_target_is_a_unique_violation_and_nothing_is_left_behind() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::running(1, 10);
    env.seed_turn(&old).await;
    let before = env.world().await;

    let error = env
        .repo
        .replace_turn(&env.replacement_plan(&old, MutationOp::Retry, "q"))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::UniqueViolation), "{error:?}");
    assert_nothing_changed(&env, &before).await;
}

#[tokio::test]
async fn a_target_that_is_already_soft_deleted_is_not_the_latest_turn_and_the_insert_rolls_back() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10).deleted();
    env.seed_turn(&old).await;
    let before = env.world().await;

    let error = env
        .repo
        .replace_turn(&env.replacement_plan(&old, MutationOp::Retry, "q"))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_nothing_changed(&env, &before).await;
}

#[tokio::test]
async fn a_newer_live_turn_found_by_the_recheck_rolls_the_whole_transaction_back() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    env.seed_turn(&SeedTurn::completed(2, 20)).await;
    let before = env.world().await;

    let error = env
        .repo
        .replace_turn(&env.replacement_plan(&old, MutationOp::Retry, "q"))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_nothing_changed(&env, &before).await;
    assert!(
        env.turn_deleted_at(old.request_id()).await.is_none(),
        "the guarded soft-delete rolled back"
    );
    assert!(env.mutation_events().await.is_empty());
}

#[tokio::test]
async fn with_equal_start_times_the_turn_with_the_greater_id_is_the_latest() {
    // Turn 2 has the greater id: turn 1 is not the latest, turn 2 is.
    let env = MutationEnv::new().await;
    let lesser = SeedTurn::completed(1, 10);
    let greater = SeedTurn::completed(2, 10);
    env.seed_turn(&lesser).await;
    env.seed_turn(&greater).await;
    let before = env.world().await;

    let refused = env
        .repo
        .replace_turn(&env.replacement_plan(&lesser, MutationOp::Retry, "q"))
        .await
        .unwrap_err();
    assert!(matches!(refused, DomainError::NotLatestTurn), "{refused:?}");
    assert_nothing_changed(&env, &before).await;

    env.repo
        .replace_turn(&env.replacement_plan(&greater, MutationOp::Retry, "q"))
        .await
        .expect("the turn with the greater id is mutable");
    assert!(env.turn_deleted_at(greater.request_id()).await.is_some());
}

#[tokio::test]
async fn a_failing_audit_enqueue_rolls_back_everything_the_transaction_wrote() {
    let healthy = MutationEnv::new().await;
    let fine = SeedTurn::completed(1, 10);
    healthy.seed_turn(&fine).await;
    replaced(&healthy, &fine, MutationOp::Retry, "q").await;
    let env = MutationEnv::with_failing_mutation_audit().await;
    let old = SeedTurn::completed(1, 10).attached(&[]);
    env.seed_turn(&old).await;
    let before = env.world().await;

    let error = env
        .repo
        .replace_turn(&env.replacement_plan(&old, MutationOp::Retry, "q"))
        .await
        .unwrap_err();

    assert!(
        matches!(error, DomainError::Internal(_) | DomainError::Database(_)),
        "{error:?}"
    );
    assert_nothing_changed(&env, &before).await;
}

#[tokio::test]
async fn a_second_replacement_of_the_same_turn_while_the_first_runs_is_a_unique_violation() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let first = replaced(&env, &old, MutationOp::Retry, "q").await;
    let after_first = env.world().await;

    let error = env
        .repo
        .replace_turn(&env.replacement_plan(&old, MutationOp::Retry, "q"))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::UniqueViolation), "{error:?}");
    assert_nothing_changed(&env, &after_first).await;
    assert_eq!(
        env.replaced_by_request(old.request_id()).await,
        Some(first.new_request_id)
    );
}

#[tokio::test]
async fn a_second_replacement_after_the_first_finished_is_not_the_latest_turn() {
    let env = MutationEnv::new().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let first = replaced(&env, &old, MutationOp::Retry, "q").await;
    env.exec(&format!(
        "UPDATE chat_turns SET state = 'completed', completed_at = updated_at WHERE request_id = {}",
        crate::chat_test_env::blob(first.new_request_id)
    ))
    .await;
    let after_first = env.world().await;

    let error = env
        .repo
        .replace_turn(&env.replacement_plan(&old, MutationOp::Edit, "q2"))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_nothing_changed(&env, &after_first).await;
}

#[tokio::test]
async fn the_new_turn_sorts_after_the_replaced_turn_and_the_new_message_after_the_history() {
    let env = MutationEnv::new().await;
    let previous = SeedTurn::completed(1, 10);
    env.seed_turn(&previous).await;
    let old = SeedTurn::completed(2, 20);
    env.seed_turn(&old).await;

    let plan = replaced(&env, &old, MutationOp::Retry, "question 2").await;

    let latest = env
        .repo
        .latest_turn(&crate::mutation_test_env::owner_scope(), env.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, plan.new_turn_id);
    let boundary = env
        .repo
        .history_boundary(
            &crate::mutation_test_env::owner_scope(),
            env.chat,
            plan.new_request_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(boundary.id, previous.assistant_message_id());
}

// ==== the fill-mode reserve of the new turn ==========================================================

mod fill {
    use super::{MutationEnv, MutationOp, SeedTurn, replaced};
    use crate::domain::error::DomainError;
    use crate::domain::service::quota::ports::QuotaRepository;
    use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest};
    use crate::domain::service::quota::test_support::premium_decision;
    use crate::test_doubles::{USER, USER_TENANT};

    #[tokio::test]
    async fn the_fill_writes_the_six_columns_of_the_new_unreserved_turn_with_the_buckets() {
        let env = MutationEnv::new().await;
        let old = SeedTurn::completed(1, 10);
        env.seed_turn(&old).await;
        let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;
        let decision = premium_decision();

        let columns = env
            .quota
            .repo
            .reserve(&ReserveRequest {
                tenant_id: USER_TENANT,
                user_id: USER,
                mode: ReserveMode::Fill {
                    turn_id: plan.new_turn_id,
                },
                decision: &decision,
                now: env.now(),
            })
            .await
            .expect("the fill commits");

        let new = plan.new_request_id;
        assert_eq!(
            env.turn_text(new, "reserve_tokens").await,
            Some(columns.reserve_tokens.to_string())
        );
        assert_eq!(
            env.turn_text(new, "effective_model").await.as_deref(),
            Some("p")
        );
        assert_eq!(
            env.turn_text(new, "policy_version_applied")
                .await
                .as_deref(),
            Some("7")
        );
        assert_eq!(env.turn_state_of(new).await, "running");
        assert_eq!(
            env.quota.total("daily").await.unwrap().reserved,
            columns.reserved_credits_micro
        );
    }

    #[tokio::test]
    async fn a_second_fill_of_the_same_turn_updates_no_row_and_rolls_the_buckets_back() {
        let env = MutationEnv::new().await;
        let old = SeedTurn::completed(1, 10);
        env.seed_turn(&old).await;
        let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;
        let decision = premium_decision();
        let request = ReserveRequest {
            tenant_id: USER_TENANT,
            user_id: USER,
            mode: ReserveMode::Fill {
                turn_id: plan.new_turn_id,
            },
            decision: &decision,
            now: env.now(),
        };
        env.quota.repo.reserve(&request).await.unwrap();
        let quota = env.quota.dump("quota_usage").await;

        let error = env.quota.repo.reserve(&request).await.unwrap_err();

        assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
        assert_eq!(env.quota.dump("quota_usage").await, quota);
    }

    #[tokio::test]
    async fn a_turn_that_is_no_longer_running_is_not_filled() {
        let env = MutationEnv::new().await;
        let old = SeedTurn::completed(1, 10);
        env.seed_turn(&old).await;
        let plan = replaced(&env, &old, MutationOp::Retry, "question 1").await;
        env.exec(&format!(
            "UPDATE chat_turns SET state = 'failed' WHERE request_id = {}",
            crate::chat_test_env::blob(plan.new_request_id)
        ))
        .await;
        let decision = premium_decision();

        let error = env
            .quota
            .repo
            .reserve(&ReserveRequest {
                tenant_id: USER_TENANT,
                user_id: USER,
                mode: ReserveMode::Fill {
                    turn_id: plan.new_turn_id,
                },
                decision: &decision,
                now: env.now(),
            })
            .await
            .unwrap_err();

        assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
        assert_eq!(
            env.turn_text(plan.new_request_id, "reserve_tokens").await,
            None
        );
    }
}
