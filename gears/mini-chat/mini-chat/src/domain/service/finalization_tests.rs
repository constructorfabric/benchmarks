#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Instant;

use mini_chat_sdk::UsageTokens;
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::QuotaPeriods;
use crate::domain::service::stream::test_helpers::*;
use crate::domain::service::test_support::{TENANT_A, TestEnv, USER_A1, ctx_a1};

fn ctx_of(t: &chat_turn::Model) -> TurnContext {
    TurnContext {
        tenant_id: t.tenant_id,
        user_id: USER_A1,
        chat_id: t.chat_id,
        turn_id: t.id,
        request_id: t.request_id,
        selected_model: "gpt-premium".into(),
        effective_model: "gpt-premium".into(),
        downgrade_reason: None,
        periods: QuotaPeriods::of(OffsetDateTime::now_utc()),
        started: Instant::now(),
        summary_trigger: None,
    }
}

#[tokio::test]
async fn second_finalizer_loses_cas_and_writes_nothing() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let evs = collect(
        env.services
            .stream
            .send(&ctx_a1(), chat, input("x"))
            .await
            .unwrap(),
    )
    .await;
    let t = turn(&env, chat, started_request_id(&evs)).await;
    let usage_before = env
        .delivered_to(&env.deps.cfg.outbox.queue_name, 1)
        .await
        .len();

    let quota = env.services.quota.clone();
    let out = finalize(
        &env.deps,
        &quota,
        &ctx_of(&t),
        ToolCounters::default(),
        Terminal::Cancelled {
            assistant_message_id: Uuid::new_v4(),
            text: "late partial".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(out, FinalizeOutcome::Lost);
    let out = finalize(
        &env.deps,
        &quota,
        &ctx_of(&t),
        ToolCounters::default(),
        Terminal::Failed {
            code: "provider_error".into(),
            detail: "x".into(),
            usage: Some(UsageTokens::default()),
            response_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(out, FinalizeOutcome::Lost);
    let after = turn(&env, chat, t.request_id).await;
    assert_eq!(after.state, "completed");
    assert_eq!(after.assistant_message_id, t.assistant_message_id);
    assert_eq!(messages(&env, chat).await.len(), 2);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        env.delivered_to(&env.deps.cfg.outbox.queue_name, 0)
            .await
            .len(),
        usage_before
    );
    env.shutdown().await;
}

#[test]
fn message_builders() {
    let id = Uuid::new_v4();
    let am = assistant_message(
        TENANT_A,
        Uuid::nil(),
        id,
        Uuid::nil(),
        "text".into(),
        "m",
        Some(UsageTokens {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 4,
            reasoning_tokens: 5,
        }),
        Some("resp_x".into()),
    );
    assert_eq!(am.role, Set("assistant".to_owned()));
    assert_eq!(am.reasoning_tokens, Set(5));
    assert_eq!(am.features_used, Set(serde_json::json!([])));
    assert_eq!(am.model, Set(Some("m".to_owned())));
    let um = user_message(
        TENANT_A,
        Uuid::nil(),
        id,
        Uuid::nil(),
        "q".into(),
        OffsetDateTime::now_utc(),
    );
    assert_eq!(um.role, Set("user".to_owned()));
    assert_eq!(um.content_type, Set("text".to_owned()));
    assert_eq!(um.request_kind, Set("chat".to_owned()));
    assert_eq!(um.model, Set(None));
}
