#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use uuid::Uuid;

use crate::api::rest::dto::TurnStatusState;
use crate::domain::error::resource_types;
use crate::domain::service::chats::test_rows::{create_chat, env_with_pdp, insert_turn, problem};
use crate::domain::service::test_support::{
    DenyPdp, FailingPdp, TENANT_A, TENANT_B, TestEnv, USER_A1, USER_A2, USER_B, ctx, ctx_a1,
};

#[tokio::test]
async fn maps_states_and_optional_fields() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let msg = Uuid::new_v4();
    let cases = [
        ("running", None, None, TurnStatusState::Running),
        ("completed", None, Some(msg), TurnStatusState::Done),
        (
            "failed",
            Some("provider_error"),
            None,
            TurnStatusState::Error,
        ),
        ("cancelled", None, Some(msg), TurnStatusState::Cancelled),
        // error_code is only reported for failed turns.
        (
            "cancelled",
            Some("stream_interrupted"),
            None,
            TurnStatusState::Cancelled,
        ),
    ];
    for (state, code, am, expected) in cases {
        let rid = Uuid::new_v4();
        let row = insert_turn(&env, TENANT_A, chat, rid, state, code, am, false).await;
        let dto = env
            .services
            .turn_status
            .get(&ctx_a1(), chat, rid)
            .await
            .unwrap();
        assert_eq!(dto.request_id, rid);
        assert_eq!(dto.state, expected, "{state}");
        assert_eq!(dto.assistant_message_id, am);
        assert_eq!(dto.updated_at, row.updated_at);
        if state == "failed" {
            assert_eq!(dto.error_code.as_deref(), code);
        } else {
            assert!(dto.error_code.is_none());
        }
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("chat_id").is_none());
        if am.is_none() {
            assert!(json.get("assistant_message_id").is_none());
        }
        if dto.error_code.is_none() {
            assert!(json.get("error_code").is_none());
        }
    }
    env.shutdown().await;
}

#[tokio::test]
async fn deleted_missing_and_foreign_turns_are_404() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let other_chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let deleted = Uuid::new_v4();
    insert_turn(&env, TENANT_A, chat, deleted, "completed", None, None, true).await;
    let elsewhere = Uuid::new_v4();
    insert_turn(
        &env,
        TENANT_A,
        other_chat,
        elsewhere,
        "completed",
        None,
        None,
        false,
    )
    .await;
    for rid in [deleted, elsewhere, Uuid::new_v4()] {
        let p = problem(
            env.services
                .turn_status
                .get(&ctx_a1(), chat, rid)
                .await
                .unwrap_err(),
        );
        assert_eq!(p["status"], 404);
        assert_eq!(p["context"]["resource_type"], resource_types::TURN);
    }

    let live = Uuid::new_v4();
    insert_turn(&env, TENANT_A, chat, live, "running", None, None, false).await;
    for (user, tenant) in [(USER_A2, TENANT_A), (USER_B, TENANT_B)] {
        let p = problem(
            env.services
                .turn_status
                .get(&ctx(user, tenant), chat, live)
                .await
                .unwrap_err(),
        );
        assert_eq!(p["status"], 404);
        assert_eq!(p["context"]["resource_type"], resource_types::CHAT);
    }
    env.shutdown().await;
}

#[tokio::test]
async fn pep_denial_and_failure() {
    let env = env_with_pdp(Arc::new(DenyPdp)).await;
    let p = problem(
        env.services
            .turn_status
            .get(&ctx_a1(), Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap_err(),
    );
    assert_eq!(p["status"], 403);
    env.shutdown().await;
    let env = env_with_pdp(Arc::new(FailingPdp)).await;
    let p = problem(
        env.services
            .turn_status
            .get(&ctx_a1(), Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap_err(),
    );
    assert_eq!(p["status"], 503);
    env.shutdown().await;
}
