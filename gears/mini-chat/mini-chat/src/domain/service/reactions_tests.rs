#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sea_orm::ActiveValue;
use time::OffsetDateTime;
use uuid::Uuid;

use super::parse_reaction;
use crate::api::rest::dto::ReactionKindDto;
use crate::domain::error::{reasons, resource_types};
use crate::domain::service::chats::test_rows::{
    create_chat, env_with_pdp, insert_message, insert_turn_messages, message_am, problem,
    reaction_rows,
};
use crate::domain::service::test_support::{
    DenyPdp, FailingPdp, TENANT_A, TENANT_B, TestEnv, USER_A1, USER_A2, USER_B, ctx, ctx_a1,
};

#[test]
fn reaction_values() {
    assert_eq!(parse_reaction("like").unwrap(), ReactionKindDto::Like);
    assert_eq!(parse_reaction("dislike").unwrap(), ReactionKindDto::Dislike);
    for bad in ["", "Like", "love", " like"] {
        let p = problem(parse_reaction(bad).unwrap_err());
        assert_eq!(p["status"], 400);
        assert_eq!(p["context"]["field_violations"][0]["field"], "reaction");
        assert_eq!(
            p["context"]["field_violations"][0]["reason"],
            reasons::INVALID_REACTION
        );
    }
}

#[tokio::test]
async fn put_upserts_and_delete_is_idempotent() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let (_, asst) = insert_turn_messages(&env, TENANT_A, chat, OffsetDateTime::now_utc()).await;
    let r = &env.services.reactions;

    let first = r.set(&ctx_a1(), chat, asst.id, "like").await.unwrap();
    assert_eq!(first.message_id, asst.id);
    assert_eq!(first.reaction, ReactionKindDto::Like);
    let second = r.set(&ctx_a1(), chat, asst.id, "dislike").await.unwrap();
    assert_eq!(second.reaction, ReactionKindDto::Dislike);
    assert!(second.created_at >= first.created_at);

    let rows = reaction_rows(&env, asst.id).await;
    assert_eq!(rows.len(), 1, "upsert keeps one row");
    assert_eq!(rows[0].reaction, "dislike");
    assert_eq!(rows[0].user_id, USER_A1);
    assert_eq!(rows[0].tenant_id, TENANT_A);

    let json = serde_json::to_value(&second).unwrap();
    assert_eq!(json["reaction"], "dislike");
    assert_eq!(json["message_id"], asst.id.to_string());

    r.delete(&ctx_a1(), chat, asst.id).await.unwrap();
    assert!(reaction_rows(&env, asst.id).await.is_empty());
    r.delete(&ctx_a1(), chat, asst.id).await.unwrap();
    env.shutdown().await;
}

#[tokio::test]
async fn non_assistant_targets_are_rejected() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let (user, _) = insert_turn_messages(&env, TENANT_A, chat, OffsetDateTime::now_utc()).await;
    let system = insert_message(
        &env,
        message_am(
            TENANT_A,
            chat,
            "system",
            Some(Uuid::new_v4()),
            OffsetDateTime::now_utc(),
        ),
    )
    .await;
    for msg in [user.id, system.id] {
        let errs = vec![
            env.services
                .reactions
                .set(&ctx_a1(), chat, msg, "like")
                .await
                .unwrap_err(),
            env.services
                .reactions
                .delete(&ctx_a1(), chat, msg)
                .await
                .unwrap_err(),
        ];
        for e in errs {
            let p = problem(e);
            assert_eq!(p["status"], 400, "{p}");
            assert_eq!(p["context"]["violations"][0]["subject"], "reaction_target");
            assert_eq!(p["context"]["violations"][0]["type"], "STATE");
        }
    }
    env.shutdown().await;
}

#[tokio::test]
async fn missing_deleted_or_foreign_messages_are_404_message() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let other_chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let (_, other_asst) =
        insert_turn_messages(&env, TENANT_A, other_chat, OffsetDateTime::now_utc()).await;
    let mut am = message_am(
        TENANT_A,
        chat,
        "assistant",
        Some(Uuid::new_v4()),
        OffsetDateTime::now_utc(),
    );
    am.deleted_at = ActiveValue::Set(Some(OffsetDateTime::now_utc()));
    let deleted = insert_message(&env, am).await;

    for msg in [Uuid::new_v4(), deleted.id, other_asst.id] {
        for e in [
            env.services
                .reactions
                .set(&ctx_a1(), chat, msg, "like")
                .await
                .unwrap_err(),
            env.services
                .reactions
                .delete(&ctx_a1(), chat, msg)
                .await
                .unwrap_err(),
        ] {
            let p = problem(e);
            assert_eq!(p["status"], 404);
            assert_eq!(p["context"]["resource_type"], resource_types::MESSAGE);
        }
    }
    env.shutdown().await;
}

#[tokio::test]
async fn foreign_chat_is_404_chat_and_value_checked_first() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let (_, asst) = insert_turn_messages(&env, TENANT_A, chat, OffsetDateTime::now_utc()).await;
    for (user, tenant) in [(USER_A2, TENANT_A), (USER_B, TENANT_B)] {
        let c = ctx(user, tenant);
        for e in [
            env.services
                .reactions
                .set(&c, chat, asst.id, "like")
                .await
                .unwrap_err(),
            env.services
                .reactions
                .delete(&c, chat, asst.id)
                .await
                .unwrap_err(),
        ] {
            let p = problem(e);
            assert_eq!(p["status"], 404);
            assert_eq!(p["context"]["resource_type"], resource_types::CHAT);
        }
        // Invalid value is reported before the chat lookup.
        let p = problem(
            env.services
                .reactions
                .set(&c, chat, asst.id, "x")
                .await
                .unwrap_err(),
        );
        assert_eq!(p["status"], 400);
    }
    assert!(reaction_rows(&env, asst.id).await.is_empty());

    // Reactions are per user: A1's reaction is invisible to... A1 only (A2 cannot reach the chat).
    env.services
        .reactions
        .set(&ctx_a1(), chat, asst.id, "like")
        .await
        .unwrap();
    assert_eq!(reaction_rows(&env, asst.id).await.len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn pep_denial_and_failure() {
    let env = env_with_pdp(Arc::new(DenyPdp)).await;
    let (c, m) = (Uuid::new_v4(), Uuid::new_v4());
    // Invalid value -> 400 even with a denying PDP.
    assert_eq!(
        problem(
            env.services
                .reactions
                .set(&ctx_a1(), c, m, "nah")
                .await
                .unwrap_err()
        )["status"],
        400
    );
    assert_eq!(
        problem(
            env.services
                .reactions
                .set(&ctx_a1(), c, m, "like")
                .await
                .unwrap_err()
        )["status"],
        403
    );
    assert_eq!(
        problem(
            env.services
                .reactions
                .delete(&ctx_a1(), c, m)
                .await
                .unwrap_err()
        )["status"],
        403
    );
    env.shutdown().await;

    let env = env_with_pdp(Arc::new(FailingPdp)).await;
    assert_eq!(
        problem(
            env.services
                .reactions
                .set(&ctx_a1(), c, m, "like")
                .await
                .unwrap_err()
        )["status"],
        503
    );
    assert_eq!(
        problem(
            env.services
                .reactions
                .delete(&ctx_a1(), c, m)
                .await
                .unwrap_err()
        )["status"],
        503
    );
    env.shutdown().await;
}
