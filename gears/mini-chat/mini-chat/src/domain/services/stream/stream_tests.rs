#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::http::Method;
use futures::StreamExt;
use sea_orm::EntityTrait;
use serde_json::json;
use toolkit_db::secure::{AccessScope, SecureEntityExt};

use super::*;
use crate::domain::model::QuotaScope;
use crate::infra::db::entities::chat_turn;
use crate::infra::db::repos::QuotaRepo;
use crate::testing::catalog::default_catalog;
use crate::testing::{TestApp, TestUser, seed};

const U: TestUser = TestUser::A1;

async fn chat_of(app: &TestApp) -> chat::Model {
    let r = app
        .call(U, Method::POST, "/mini-chat/v1/chats", Some(json!({})))
        .await;
    let id = Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap();
    let conn = app.db.conn().unwrap();
    ChatRepo::find_live(&conn, &AccessScope::allow_all(), id)
        .await
        .unwrap()
        .unwrap()
}

/// What a retry/edit mutation commit leaves behind: a running turn without
/// preflight columns and its user message.
async fn committed_turn(app: &TestApp, chat: &chat::Model, content: &str) -> chat_turn::Model {
    let request_id = Uuid::new_v4();
    let now = now_utc();
    let conn = app.db.conn().unwrap();
    let turn = TurnRepo::insert_running(
        &conn,
        NewTurn {
            id: Uuid::new_v4(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            request_id,
            requester_user_id: U.user_id,
            web_search_enabled: false,
            preflight: None,
            now,
        },
    )
    .await
    .unwrap();
    seed::insert_message(&app.db, chat.id, "user", content, Some(request_id), now).await;
    turn
}

async fn preflighted(app: &TestApp, chat: &chat::Model, content: &str) -> Preflighted {
    let svc = &app.services.stream;
    let model = app
        .services
        .models
        .resolve_chat_model(U.user_id, chat.model.as_deref().unwrap())
        .await
        .unwrap();
    svc.preflight(PreflightRequest {
        chat,
        user_id: U.user_id,
        model: &model,
        content,
        attachment_ids: &[],
        web_search: false,
    })
    .await
    .unwrap()
}

async fn stored_turn(app: &TestApp, id: Uuid) -> chat_turn::Model {
    let conn = app.db.conn().unwrap();
    chat_turn::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn existing_turn_mode_fills_preflight_reserves_and_streams() {
    let app = TestApp::builder().build().await;
    let chat = chat_of(&app).await;
    // The mutation preflight runs before the mutation commit.
    let pre = preflighted(&app, &chat, "again").await;
    let turn = committed_turn(&app, &chat, "again").await;
    assert!(turn.reserve_tokens.is_none());

    let svc = &app.services.stream;
    let prepared = svc
        .prepare_turn(TurnPrep {
            chat: chat.clone(),
            user_id: U.user_id,
            content: "again".to_owned(),
            request_id: turn.request_id,
            web_search_requested: false,
            pre,
            mode: TurnMode::Existing { turn: turn.clone() },
        })
        .await
        .unwrap();
    assert_eq!(prepared.turn.id, turn.id);
    assert_eq!(
        prepared.turn.effective_model.as_deref(),
        Some("gpt-premium")
    );
    assert_eq!(
        prepared.turn.reserved_credits_micro,
        Some(prepared.decision.reserved_credits_micro)
    );
    let stored = stored_turn(&app, turn.id).await;
    assert_eq!(stored.reserve_tokens, prepared.turn.reserve_tokens);
    assert_eq!(
        stored.policy_version_applied,
        prepared.turn.policy_version_applied
    );
    // The user message is only the current input, not history.
    assert_eq!(prepared.request.input.len(), 1);

    let conn = app.db.conn().unwrap();
    let d = &prepared.decision;
    let rows = QuotaRepo::rows_for_periods(
        &conn,
        U.tenant_id,
        U.user_id,
        d.daily_start,
        d.monthly_start,
    )
    .await
    .unwrap();
    assert!(
        rows.iter()
            .all(|r| r.reserved_credits_micro == d.reserved_credits_micro)
    );

    let live = svc.spawn_turn(prepared);
    let events: Vec<_> = live_events(live, Duration::from_secs(15)).collect().await;
    let names: Vec<_> = events.iter().map(SseEvent::name).collect();
    assert_eq!(names, ["stream_started", "delta", "done"]);
    let stored = stored_turn(&app, turn.id).await;
    assert_eq!(stored.state, "completed");
}

#[tokio::test]
async fn existing_turn_mode_reports_reserve_rejection() {
    let app = TestApp::builder().catalog(default_catalog()).build().await;
    let chat = chat_of(&app).await;
    let mut pre = preflighted(&app, &chat, "again").await;
    // Limits shrank after the preflight: the re-check rejects the reserve.
    pre.user_limits.standard.limit_daily_credits_micro = 1;
    let turn = committed_turn(&app, &chat, "again").await;
    let err = app
        .services
        .stream
        .prepare_turn(TurnPrep {
            chat,
            user_id: U.user_id,
            content: "again".to_owned(),
            request_id: turn.request_id,
            web_search_requested: false,
            pre,
            mode: TurnMode::Existing { turn: turn.clone() },
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ),
        "{err:?}"
    );
    let stored = stored_turn(&app, turn.id).await;
    assert!(stored.reserve_tokens.is_none(), "rolled back");
    assert_eq!(stored.state, "running");
}
