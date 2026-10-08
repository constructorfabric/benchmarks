#![allow(clippy::unwrap_used, clippy::expect_used)]

use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
use time::Duration as TimeDuration;
use time::macros::datetime;
use toolkit_db::secure::{AccessScope, SecureUpdateExt};
use uuid::Uuid;

use super::message_repo::NewMessage;
use super::{chat_repo, message_repo, turn_repo};
use crate::domain::enums::MessageRole;
use crate::domain::time::db_ts;
use crate::infra::db::entities::{chat, chat_turn, message};
use crate::test_support::{
    insert_turn, seed_chat, seed_message, seed_turn, test_ctx, test_provider,
};

fn t(secs: i64) -> time::OffsetDateTime {
    db_ts(datetime!(2026-10-04 12:00:00 UTC) + TimeDuration::seconds(secs))
}

fn owner_scope(ctx: &toolkit_security::SecurityContext) -> AccessScope {
    AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id())
}

#[tokio::test]
async fn find_scoped_hides_deleted_and_out_of_scope_chats() {
    let db = test_provider().await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, Some("t"), t(0)).await;
    let conn = db.conn().unwrap();

    let found = chat_repo::find_scoped(&conn, &owner_scope(&ctx), chat.id)
        .await
        .unwrap();
    assert_eq!(found.map(|c| c.id), Some(chat.id));
    let other = test_ctx();
    assert!(
        chat_repo::find_scoped(&conn, &owner_scope(&other), chat.id)
            .await
            .unwrap()
            .is_none()
    );

    chat::Entity::update_many()
        .col_expr(
            chat::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(t(1))),
        )
        .filter(chat::Column::Id.eq(chat.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    assert!(
        chat_repo::find_scoped(&conn, &owner_scope(&ctx), chat.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn touch_updated_at_sets_the_timestamp_within_the_tenant() {
    let db = test_provider().await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t(0)).await;
    let conn = db.conn().unwrap();

    chat_repo::touch_updated_at(&conn, Uuid::new_v4(), chat.id, t(50))
        .await
        .unwrap();
    let unchanged = chat_repo::find_scoped(&conn, &owner_scope(&ctx), chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.updated_at, t(0));

    chat_repo::touch_updated_at(&conn, chat.tenant_id, chat.id, t(60))
        .await
        .unwrap();
    let touched = chat_repo::find_scoped(&conn, &owner_scope(&ctx), chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(touched.updated_at, t(60));
    assert_eq!(touched.created_at, t(0));
}

#[tokio::test]
async fn insert_message_persists_every_field() {
    let db = test_provider().await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t(0)).await;
    let conn = db.conn().unwrap();
    let new = NewMessage {
        id: Uuid::new_v4(),
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        request_id: Uuid::new_v4(),
        role: MessageRole::Assistant,
        content: "hi".to_owned(),
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!(["web_search"]),
        provider_response_id: Some("resp_1".to_owned()),
        input_tokens: 10,
        output_tokens: 4,
        cache_read_input_tokens: 2,
        cache_write_input_tokens: 1,
        reasoning_tokens: 5,
        model: Some("b".to_owned()),
        created_at: t(3),
    };

    let row = message_repo::insert(&conn, new.clone()).await.unwrap();

    assert_eq!(row.id, new.id);
    assert_eq!(row.role, "assistant");
    assert_eq!(row.request_id, Some(new.request_id));
    assert_eq!(row.content_type, "text");
    assert_eq!(row.token_estimate, 0, "reserved column is always 0");
    assert_eq!(row.features_used, serde_json::json!(["web_search"]));
    assert_eq!(
        (
            row.input_tokens,
            row.output_tokens,
            row.cache_read_input_tokens,
            row.cache_write_input_tokens,
            row.reasoning_tokens
        ),
        (10, 4, 2, 1, 5)
    );
    assert_eq!(row.created_at, t(3));
    assert!(row.deleted_at.is_none());
    assert!(!row.is_compressed);
}

async fn set_usage(conn: &toolkit_db::secure::DbConn<'_>, id: Uuid, input: i64, output: i64) {
    message::Entity::update_many()
        .col_expr(
            message::Column::InputTokens,
            sea_orm::sea_query::Expr::value(input),
        )
        .col_expr(
            message::Column::OutputTokens,
            sea_orm::sea_query::Expr::value(output),
        )
        .filter(message::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(conn)
        .await
        .unwrap();
}

async fn soft_delete_message(conn: &toolkit_db::secure::DbConn<'_>, id: Uuid) {
    message::Entity::update_many()
        .col_expr(
            message::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(t(999))),
        )
        .filter(message::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn latest_assistant_with_usage_skips_users_unbilled_and_deleted() {
    let db = test_provider().await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t(0)).await;
    let conn = db.conn().unwrap();
    assert_eq!(
        message_repo::latest_assistant_with_usage(&conn, chat.tenant_id, chat.id, None)
            .await
            .unwrap(),
        None
    );
    let old = seed_message(&db, &chat, "assistant", t(1)).await;
    set_usage(&conn, old.id, 100, 20).await;
    let newer = seed_message(&db, &chat, "assistant", t(2)).await;
    set_usage(&conn, newer.id, 200, 30).await;
    seed_message(&db, &chat, "assistant", t(3)).await; // no usage
    let user = seed_message(&db, &chat, "user", t(4)).await;
    set_usage(&conn, user.id, 7, 7).await;
    let deleted = seed_message(&db, &chat, "assistant", t(5)).await;
    set_usage(&conn, deleted.id, 900, 90).await;
    soft_delete_message(&conn, deleted.id).await;

    assert_eq!(
        message_repo::latest_assistant_with_usage(&conn, chat.tenant_id, chat.id, None)
            .await
            .unwrap(),
        Some((200, 30))
    );
    assert_eq!(
        message_repo::latest_assistant_with_usage(&conn, Uuid::new_v4(), chat.id, None)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn snapshot_boundary_and_recent_for_context_window() {
    let db = test_provider().await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t(0)).await;
    let conn = db.conn().unwrap();
    assert_eq!(
        message_repo::snapshot_boundary(&conn, chat.tenant_id, chat.id)
            .await
            .unwrap(),
        None
    );
    let mut msgs = Vec::new();
    for i in 0..6 {
        msgs.push(
            seed_message(
                &db,
                &chat,
                if i % 2 == 0 { "user" } else { "assistant" },
                t(i),
            )
            .await,
        );
    }
    soft_delete_message(&conn, msgs[3].id).await;
    let late_deleted = seed_message(&db, &chat, "user", t(10)).await;
    soft_delete_message(&conn, late_deleted.id).await;

    let boundary = message_repo::snapshot_boundary(&conn, chat.tenant_id, chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(boundary, (msgs[5].created_at, msgs[5].id));

    // Messages created after the boundary are excluded.
    seed_message(&db, &chat, "user", t(20)).await;
    let ids = |v: Vec<message::Model>| v.into_iter().map(|m| m.id).collect::<Vec<_>>();

    let all = message_repo::recent_for_context(&conn, chat.tenant_id, chat.id, boundary, None, 100)
        .await
        .unwrap();
    assert_eq!(
        ids(all),
        [msgs[0].id, msgs[1].id, msgs[2].id, msgs[4].id, msgs[5].id]
    );

    let last_two =
        message_repo::recent_for_context(&conn, chat.tenant_id, chat.id, boundary, None, 2)
            .await
            .unwrap();
    assert_eq!(ids(last_two), [msgs[4].id, msgs[5].id]);

    let frontier = Some((msgs[1].created_at, msgs[1].id));
    let after =
        message_repo::recent_for_context(&conn, chat.tenant_id, chat.id, boundary, frontier, 100)
            .await
            .unwrap();
    assert_eq!(ids(after), [msgs[2].id, msgs[4].id, msgs[5].id]);
}

/// Sets one column of a message row (test seeding).
async fn set_message_col<V: Into<sea_orm::Value>>(
    conn: &impl toolkit_db::secure::DBRunner,
    id: Uuid,
    col: message::Column,
    v: V,
) {
    message::Entity::update_many()
        .col_expr(col, sea_orm::sea_query::Expr::value(v))
        .filter(message::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn recent_for_context_skips_messages_without_request_id() {
    let db = test_provider().await;
    let chat = seed_chat(&db, &test_ctx(), None, t(0)).await;
    let conn = db.conn().unwrap();
    let a = seed_message(&db, &chat, "user", t(1)).await;
    let legacy = seed_message(&db, &chat, "assistant", t(2)).await;
    let c = seed_message(&db, &chat, "user", t(3)).await;
    set_message_col(
        &conn,
        legacy.id,
        message::Column::RequestId,
        Option::<Uuid>::None,
    )
    .await;
    let rows = message_repo::recent_for_context(
        &conn,
        chat.tenant_id,
        chat.id,
        (c.created_at, c.id),
        None,
        100,
    )
    .await
    .unwrap();
    assert_eq!(rows.iter().map(|m| m.id).collect::<Vec<_>>(), [a.id, c.id]);
}

#[tokio::test]
async fn recent_for_context_skips_compressed_messages() {
    let db = test_provider().await;
    let chat = seed_chat(&db, &test_ctx(), None, t(0)).await;
    let conn = db.conn().unwrap();
    let a = seed_message(&db, &chat, "user", t(1)).await;
    let compressed = seed_message(&db, &chat, "assistant", t(2)).await;
    let c = seed_message(&db, &chat, "user", t(3)).await;
    set_message_col(&conn, compressed.id, message::Column::IsCompressed, true).await;
    let rows = message_repo::recent_for_context(
        &conn,
        chat.tenant_id,
        chat.id,
        (c.created_at, c.id),
        None,
        100,
    )
    .await
    .unwrap();
    assert_eq!(rows.iter().map(|m| m.id).collect::<Vec<_>>(), [a.id, c.id]);
}

#[tokio::test]
async fn turn_lookups() {
    let db = test_provider().await;
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t(0)).await;
    let conn = db.conn().unwrap();
    assert!(
        turn_repo::latest_live(&conn, chat.tenant_id, chat.id)
            .await
            .unwrap()
            .is_none()
    );

    let first = seed_turn(&db, &chat, Uuid::new_v4(), "completed", t(1)).await;
    let deleted_req = Uuid::new_v4();
    let deleted = seed_turn(&db, &chat, deleted_req, "completed", t(2)).await;
    chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(t(3))),
        )
        .filter(chat_turn::Column::Id.eq(deleted.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    // find_by_request returns deleted turns too (request-id reuse checks).
    let by_req = turn_repo::find_by_request(&conn, chat.tenant_id, chat.id, deleted_req)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_req.id, deleted.id);
    assert!(by_req.deleted_at.is_some());
    assert!(
        turn_repo::find_by_request(&conn, Uuid::new_v4(), chat.id, deleted_req)
            .await
            .unwrap()
            .is_none()
    );

    // latest_live ignores the (later) deleted turn.
    let latest = turn_repo::latest_live(&conn, chat.tenant_id, chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, first.id);
    assert!(
        turn_repo::find_running(&conn, chat.tenant_id, chat.id)
            .await
            .unwrap()
            .is_none()
    );

    let running = seed_turn(&db, &chat, Uuid::new_v4(), "running", t(4)).await;
    assert_eq!(
        turn_repo::find_running(&conn, chat.tenant_id, chat.id)
            .await
            .unwrap()
            .map(|r| r.id),
        Some(running.id)
    );
    assert_eq!(
        turn_repo::latest_live(&conn, chat.tenant_id, chat.id)
            .await
            .unwrap()
            .map(|r| r.id),
        Some(running.id)
    );

    // A deleted running turn is not "running".
    let other_chat = seed_chat(&db, &ctx, None, t(0)).await;
    let mut ghost = seed_turn(&db, &other_chat, Uuid::new_v4(), "completed", t(1))
        .await
        .into_active_model()
        .reset_all();
    ghost.id = Set(Uuid::new_v4());
    ghost.request_id = Set(Uuid::new_v4());
    ghost.state = Set("running".to_owned());
    ghost.deleted_at = Set(Some(t(2)));
    insert_turn(&db, ghost).await;
    assert!(
        turn_repo::find_running(&conn, other_chat.tenant_id, other_chat.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn unknown_stored_reaction_is_internal() {
    let row = |reaction: &str| crate::infra::db::entities::message_reaction::Model {
        id: Uuid::new_v4(),
        message_id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        reaction: reaction.to_owned(),
        created_at: t(0),
    };
    assert_eq!(
        super::reaction_repo::reaction_kind(&row("like")).unwrap(),
        crate::domain::enums::ReactionKind::Like
    );
    assert!(matches!(
        super::reaction_repo::reaction_kind(&row("love")),
        Err(crate::domain::error::DomainError::Internal(_))
    ));
}
