#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
use serde_json::{Value, json};
use time::macros::datetime;
use tokio::sync::mpsc::UnboundedReceiver;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{TurnService, TurnStatusState};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::ChatAction;
use crate::domain::time::db_ts;
use crate::infra::db::entities::{chat, chat_turn, message, thread_summary};
use crate::infra::outbox::AUDIT_PAYLOAD_TYPE;
use crate::test_support::{
    FakeAuthz, ctx_for, insert_message, insert_turn, seed_chat, test_ctx, test_file_db, test_outbox,
};

async fn service(
    authz: Arc<FakeAuthz>,
) -> (
    tempfile::TempDir,
    Arc<DBProvider<DomainError>>,
    TurnService,
    UnboundedReceiver<(String, Value)>,
) {
    let (dir, raw) = test_file_db().await;
    let (outbox, rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    let svc = TurnService::new(db.clone(), authz, outbox);
    (dir, db, svc, rx)
}

fn t0() -> time::OffsetDateTime {
    db_ts(datetime!(2026-10-04 12:00:00 UTC))
}

#[tokio::test]
async fn turn_status_maps_states_and_hides_deleted() {
    let authz = Arc::new(FakeAuthz::default());
    let (_dir, db, svc, _rx) = service(authz.clone()).await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let assistant = Uuid::new_v4();
    let updated = db_ts(datetime!(2026-10-04 12:05:00 UTC));

    // (state, error_code, assistant_message_id, deleted) -> expected
    let cases = [
        ("running", None, None, TurnStatusState::Running, None, None),
        (
            "completed",
            None,
            Some(assistant),
            TurnStatusState::Done,
            None,
            Some(assistant),
        ),
        (
            "failed",
            Some("provider_error"),
            Some(assistant),
            TurnStatusState::Error,
            Some("provider_error"),
            None,
        ),
        (
            "cancelled",
            Some("client_disconnect"),
            Some(assistant),
            TurnStatusState::Cancelled,
            None,
            Some(assistant),
        ),
    ];
    for (state, code, msg, want_state, want_code, want_msg) in cases {
        let request_id = Uuid::new_v4();
        let mut am = base_turn(&chat, request_id).into_active_model().reset_all();
        am.state = Set(state.to_owned());
        am.error_code = Set(code.map(str::to_owned));
        am.assistant_message_id = Set(msg);
        am.updated_at = Set(updated);
        insert_turn(&db, am).await;

        let view = svc.status(&ctx, chat.id, request_id).await.unwrap();

        assert_eq!(view.request_id, request_id, "{state}");
        assert_eq!(view.state, want_state, "{state}");
        assert_eq!(view.error_code.as_deref(), want_code, "{state}");
        assert_eq!(view.assistant_message_id, want_msg, "{state}");
        assert_eq!(view.updated_at, updated, "{state}");
    }

    let turn_not_found = DomainError::NotFound {
        resource: ResourceKind::Turn,
    };
    let deleted_request = Uuid::new_v4();
    let mut am = base_turn(&chat, deleted_request)
        .into_active_model()
        .reset_all();
    am.state = Set("completed".to_owned());
    am.deleted_at = Set(Some(updated));
    insert_turn(&db, am).await;
    assert_eq!(
        svc.status(&ctx, chat.id, deleted_request)
            .await
            .unwrap_err(),
        turn_not_found
    );
    assert_eq!(
        svc.status(&ctx, chat.id, Uuid::new_v4()).await.unwrap_err(),
        turn_not_found
    );

    let stranger = ctx_for(ctx.subject_tenant_id(), Uuid::new_v4());
    assert_eq!(
        svc.status(&stranger, chat.id, deleted_request)
            .await
            .unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Chat
        }
    );
    assert!(
        authz
            .chat_actions()
            .iter()
            .all(|a| *a == ChatAction::ReadTurn)
    );
}

#[tokio::test]
async fn turn_status_never_reads_another_chats_turn() {
    let (_dir, db, svc, _rx) = service(Arc::new(FakeAuthz::default())).await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let other = seed_chat(&db, &ctx, None, t0()).await;
    let request_id = Uuid::new_v4();
    insert_turn(
        &db,
        base_turn(&other, request_id)
            .into_active_model()
            .reset_all(),
    )
    .await;

    assert_eq!(
        svc.status(&ctx, chat.id, request_id).await.unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Turn
        }
    );
}

fn base_turn(chat: &crate::infra::db::entities::chat::Model, request_id: Uuid) -> chat_turn::Model {
    chat_turn::Model {
        id: Uuid::new_v4(),
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        request_id,
        requester_type: "user".to_owned(),
        requester_user_id: Some(chat.user_id),
        state: "running".to_owned(),
        provider_name: None,
        provider_response_id: Some("resp_secret".to_owned()),
        assistant_message_id: None,
        error_code: None,
        reserve_tokens: None,
        max_output_tokens_applied: None,
        reserved_credits_micro: None,
        policy_version_applied: None,
        effective_model: None,
        minimal_generation_floor_applied: None,
        error_detail: None,
        deleted_at: None,
        replaced_by_request_id: None,
        started_at: t0(),
        last_progress_at: None,
        web_search_enabled: false,
        web_search_completed_count: 0,
        code_interpreter_completed_count: 0,
        file_search_completed_count: 0,
        completed_at: None,
        updated_at: t0(),
    }
}

// ── Mutations: delete, preview, summary rule ────────────────────────────────

fn at(second: u8) -> time::OffsetDateTime {
    db_ts(datetime!(2026-10-04 12:00:00 UTC) + time::Duration::seconds(i64::from(second)))
}

fn msg_am(
    chat: &chat::Model,
    request_id: Uuid,
    role: &str,
    created_at: time::OffsetDateTime,
) -> message::ActiveModel {
    message::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(Some(request_id)),
        role: Set(role.to_owned()),
        content: Set(format!("{role} text")),
        content_type: Set("text".to_owned()),
        token_estimate: Set(1),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(created_at),
        deleted_at: Set(None),
    }
}

struct SeededTurn {
    turn: chat_turn::Model,
    user: message::Model,
    assistant: message::Model,
}

/// A terminal turn started at second `s` with its user (`s`) and assistant
/// (`s + 1`) messages.
async fn seed_terminal(
    db: &DBProvider<DomainError>,
    chat: &chat::Model,
    s: u8,
    state: &str,
) -> SeededTurn {
    let rid = Uuid::new_v4();
    let user = insert_message(db, msg_am(chat, rid, "user", at(s))).await;
    let assistant = insert_message(db, msg_am(chat, rid, "assistant", at(s + 1))).await;
    let mut am = base_turn(chat, rid).into_active_model().reset_all();
    am.state = Set(state.to_owned());
    am.started_at = Set(at(s));
    let turn = insert_turn(db, am).await;
    SeededTurn {
        turn,
        user,
        assistant,
    }
}

async fn all_turns(db: &DBProvider<DomainError>, chat_id: Uuid) -> Vec<chat_turn::Model> {
    let conn = db.conn().unwrap();
    chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

async fn all_messages(db: &DBProvider<DomainError>, chat_id: Uuid) -> Vec<message::Model> {
    let conn = db.conn().unwrap();
    message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

async fn chat_row(db: &DBProvider<DomainError>, chat_id: Uuid) -> chat::Model {
    let conn = db.conn().unwrap();
    chat::Entity::find()
        .filter(chat::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
}

async fn summaries(db: &DBProvider<DomainError>, chat_id: Uuid) -> Vec<thread_summary::Model> {
    let conn = db.conn().unwrap();
    thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

async fn seed_summary(db: &DBProvider<DomainError>, chat: &chat::Model, frontier: &message::Model) {
    let conn = db.conn().unwrap();
    secure_insert::<thread_summary::Entity>(
        thread_summary::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            summary_text: Set("summary".to_owned()),
            summarized_up_to_created_at: Set(frontier.created_at),
            summarized_up_to_message_id: Set(frontier.id),
            token_estimate: Set(9),
            created_at: Set(at(50)),
            updated_at: Set(at(50)),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
}

async fn compress_all(db: &DBProvider<DomainError>, chat_id: Uuid) {
    let conn = db.conn().unwrap();
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(message::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

/// Collects outbox messages for `window`.
async fn drain(rx: &mut UnboundedReceiver<(String, Value)>, window: Duration) -> Vec<Value> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some((ty, body))) = tokio::time::timeout_at(deadline, rx.recv()).await {
        assert_eq!(ty, AUDIT_PAYLOAD_TYPE, "only audit events expected: {body}");
        out.push(body);
    }
    out
}

fn not_latest() -> DomainError {
    DomainError::NotLatestTurn
}

#[tokio::test]
async fn delete_soft_deletes_and_status_404() {
    let authz = Arc::new(FakeAuthz::default());
    let (_dir, db, svc, mut rx) = service(authz.clone()).await;
    let ctx = test_ctx();
    let long_ago = db_ts(datetime!(2020-01-01 00:00:00 UTC));
    let chat = seed_chat(&db, &ctx, None, long_ago).await;
    let older = seed_terminal(&db, &chat, 1, "completed").await;
    let target = seed_terminal(&db, &chat, 10, "failed").await;

    svc.delete(&ctx, chat.id, target.turn.request_id)
        .await
        .unwrap();

    assert_eq!(
        svc.status(&ctx, chat.id, target.turn.request_id)
            .await
            .unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Turn
        }
    );
    let turns = all_turns(&db, chat.id).await;
    let deleted = turns.iter().find(|t| t.id == target.turn.id).unwrap();
    assert!(deleted.deleted_at.is_some());
    assert_eq!(deleted.replaced_by_request_id, None);
    assert_eq!(deleted.state, "failed");
    let kept = turns.iter().find(|t| t.id == older.turn.id).unwrap();
    assert_eq!(kept.deleted_at, None);
    for m in all_messages(&db, chat.id).await {
        let of_target = m.request_id == Some(target.turn.request_id);
        assert_eq!(m.deleted_at.is_some(), of_target, "message {}", m.role);
    }
    assert!(chat_row(&db, chat.id).await.updated_at > long_ago);

    let events = drain(&mut rx, Duration::from_millis(1500)).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let ev = &events[0];
    assert_eq!(ev["kind"], "mutation");
    assert_eq!(ev["event_type"], "turn_delete");
    assert_eq!(ev["tenant_id"], json!(chat.tenant_id));
    assert_eq!(ev["chat_id"], json!(chat.id));
    assert_eq!(ev["actor_user_id"], json!(ctx.subject_id()));
    assert_eq!(ev["request_id"], json!(target.turn.request_id));
    assert!(ev["original_request_id"].is_null());
    assert!(ev["new_request_id"].is_null());

    // The deleted turn is no longer the latest; the previous turn now is.
    assert_eq!(
        svc.delete(&ctx, chat.id, target.turn.request_id)
            .await
            .unwrap_err(),
        not_latest()
    );
    svc.delete(&ctx, chat.id, older.turn.request_id)
        .await
        .unwrap();
    assert!(
        authz
            .chat_actions()
            .iter()
            .filter(|a| **a != ChatAction::ReadTurn)
            .all(|a| *a == ChatAction::DeleteTurn)
    );
    assert_eq!(
        authz
            .chat_actions()
            .iter()
            .filter(|a| **a == ChatAction::DeleteTurn)
            .count(),
        3
    );
}

#[tokio::test]
async fn delete_preview_rejections_change_nothing() {
    let (_dir, db, svc, mut rx) = service(Arc::new(FakeAuthz::default())).await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let older = seed_terminal(&db, &chat, 1, "completed").await;
    let running = seed_terminal(&db, &chat, 10, "running").await;

    // Running is checked before the latest check.
    assert_eq!(
        svc.delete(&ctx, chat.id, running.turn.request_id)
            .await
            .unwrap_err(),
        DomainError::TurnNotTerminal
    );
    // A newer running turn makes the older one non-latest.
    assert_eq!(
        svc.delete(&ctx, chat.id, older.turn.request_id)
            .await
            .unwrap_err(),
        not_latest()
    );
    assert_eq!(
        svc.delete(&ctx, chat.id, Uuid::new_v4()).await.unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Turn
        }
    );
    let stranger = ctx_for(ctx.subject_tenant_id(), Uuid::new_v4());
    assert_eq!(
        svc.delete(&stranger, chat.id, older.turn.request_id)
            .await
            .unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Chat
        }
    );

    // Another user's turn in the caller's chat.
    let other_chat = seed_chat(&db, &ctx, None, t0()).await;
    let foreign = seed_terminal(&db, &other_chat, 1, "completed").await;
    let conn = db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::RequesterUserId,
            Expr::value(Some(Uuid::new_v4())),
        )
        .filter(chat_turn::Column::Id.eq(foreign.turn.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    assert_eq!(
        svc.delete(&ctx, other_chat.id, foreign.turn.request_id)
            .await
            .unwrap_err(),
        DomainError::NotRequester
    );

    assert!(
        all_turns(&db, chat.id)
            .await
            .iter()
            .all(|t| t.deleted_at.is_none())
    );
    assert!(
        all_messages(&db, chat.id)
            .await
            .iter()
            .all(|m| m.deleted_at.is_none())
    );
    assert_eq!(chat_row(&db, chat.id).await.updated_at, t0());
    assert!(drain(&mut rx, Duration::from_millis(1500)).await.is_empty());
}

#[tokio::test]
async fn latest_turn_is_ordered_by_started_at_then_id() {
    let (_dir, db, svc, _rx) = service(Arc::new(FakeAuthz::default())).await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let (low, high) = (Uuid::from_u128(1), Uuid::from_u128(2));
    let mut rids = Vec::new();
    for id in [high, low] {
        let rid = Uuid::new_v4();
        let mut am = base_turn(&chat, rid).into_active_model().reset_all();
        am.id = Set(id);
        am.state = Set("completed".to_owned());
        insert_turn(&db, am).await;
        rids.push(rid);
    }
    let (high_rid, low_rid) = (rids[0], rids[1]);
    assert_eq!(
        svc.delete(&ctx, chat.id, low_rid).await.unwrap_err(),
        not_latest()
    );
    svc.delete(&ctx, chat.id, high_rid).await.unwrap();
}

#[tokio::test]
async fn mutation_deletes_covering_summary_and_clears_compressed() {
    let (_dir, db, svc, _rx) = service(Arc::new(FakeAuthz::default())).await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let first = seed_terminal(&db, &chat, 1, "completed").await;
    let latest = seed_terminal(&db, &chat, 10, "completed").await;
    // Frontier exactly at the latest turn's user message: "at or after".
    seed_summary(&db, &chat, &latest.user).await;
    compress_all(&db, chat.id).await;

    svc.delete(&ctx, chat.id, latest.turn.request_id)
        .await
        .unwrap();

    assert!(summaries(&db, chat.id).await.is_empty());
    let msgs = all_messages(&db, chat.id).await;
    assert_eq!(msgs.len(), 4);
    assert!(msgs.iter().all(|m| !m.is_compressed), "{msgs:?}");
    assert!(
        msgs.iter()
            .filter(|m| m.request_id == Some(first.turn.request_id))
            .all(|m| m.deleted_at.is_none())
    );
}

#[tokio::test]
async fn mutation_keeps_summary_that_does_not_cover_the_turn() {
    let (_dir, db, svc, _rx) = service(Arc::new(FakeAuthz::default())).await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let first = seed_terminal(&db, &chat, 1, "completed").await;
    let latest = seed_terminal(&db, &chat, 10, "completed").await;
    seed_summary(&db, &chat, &first.assistant).await;
    compress_all(&db, chat.id).await;

    svc.delete(&ctx, chat.id, latest.turn.request_id)
        .await
        .unwrap();

    let rows = summaries(&db, chat.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].summarized_up_to_message_id, first.assistant.id);
    assert!(
        all_messages(&db, chat.id)
            .await
            .iter()
            .all(|m| m.is_compressed)
    );
}

#[tokio::test]
async fn stale_delete_commit_loses_guarded_update() {
    // A concurrent delete whose preview passed before the other delete
    // committed loses the guarded update and commits nothing (no second
    // audit event).
    let (_dir, db, svc, mut rx) = service(Arc::new(FakeAuthz::default())).await;
    let ctx: SecurityContext = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let target = seed_terminal(&db, &chat, 1, "completed").await;
    let preview = svc
        .preview(
            &ctx,
            chat.id,
            target.turn.request_id,
            super::MutationKind::Delete,
        )
        .await
        .unwrap();
    svc.delete(&ctx, chat.id, target.turn.request_id)
        .await
        .unwrap();
    // Replaying the commit of the stale preview loses the guarded update.
    assert_eq!(svc.commit_delete(&preview).await.unwrap_err(), not_latest());
    assert_eq!(drain(&mut rx, Duration::from_millis(1500)).await.len(), 1);
}
