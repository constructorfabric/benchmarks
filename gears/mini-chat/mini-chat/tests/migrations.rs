//! T008: schema migrations apply cleanly (twice) and enforce the turn uniqueness invariants.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use mini_chat::domain::service::chats::tenant_scope;
use mini_chat::infra::db::entities::turn;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{SecureInsertExt, SecureUpdateExt};
use uuid::Uuid;

fn running_turn(tenant: Uuid, chat: Uuid, request_id: Uuid) -> turn::ActiveModel {
    let ts = time::OffsetDateTime::now_utc();
    turn::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant),
        chat_id: ActiveValue::Set(chat),
        request_id: ActiveValue::Set(request_id),
        requester_type: ActiveValue::Set("user".to_owned()),
        requester_user_id: ActiveValue::Set(None),
        state: ActiveValue::Set("running".to_owned()),
        provider_name: ActiveValue::Set(None),
        provider_response_id: ActiveValue::Set(None),
        assistant_message_id: ActiveValue::Set(None),
        error_code: ActiveValue::Set(None),
        reserve_tokens: ActiveValue::Set(None),
        max_output_tokens_applied: ActiveValue::Set(None),
        reserved_credits_micro: ActiveValue::Set(None),
        policy_version_applied: ActiveValue::Set(None),
        effective_model: ActiveValue::Set(None),
        minimal_generation_floor_applied: ActiveValue::Set(None),
        error_detail: ActiveValue::Set(None),
        deleted_at: ActiveValue::Set(None),
        replaced_by_request_id: ActiveValue::Set(None),
        started_at: ActiveValue::Set(ts),
        last_progress_at: ActiveValue::Set(None),
        web_search_enabled: ActiveValue::Set(false),
        web_search_completed_count: ActiveValue::Set(0),
        code_interpreter_completed_count: ActiveValue::Set(0),
        file_search_completed_count: ActiveValue::Set(0),
        completed_at: ActiveValue::Set(None),
        updated_at: ActiveValue::Set(ts),
    }
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let h = Harness::new().await;
    let again = toolkit_db::migration_runner::run_migrations_for_testing(
        &h.db,
        mini_chat::gear::all_migrations(),
    )
    .await
    .unwrap();
    assert_eq!(again.applied, 0);
}

#[tokio::test]
async fn turn_unique_indexes_are_enforced() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let scope = tenant_scope(h.tenant);
    let conn = h.core.db.conn().unwrap();
    let r1 = Uuid::new_v4();
    turn::Entity::insert(running_turn(h.tenant, chat, r1))
        .secure()
        .scope_unchecked(&scope)
        .unwrap()
        .exec(&conn)
        .await
        .unwrap();
    // a second running turn in the same chat violates the partial unique index
    let second = turn::Entity::insert(running_turn(h.tenant, chat, Uuid::new_v4()))
        .secure()
        .scope_unchecked(&scope)
        .unwrap()
        .exec(&conn)
        .await;
    assert!(second.is_err(), "one running turn per chat");
    // once terminal, a new turn may run, but the (chat_id, request_id) pair stays unique
    turn::Entity::update_many()
        .col_expr(turn::Column::State, Expr::value("completed"))
        .filter(turn::Column::RequestId.eq(r1))
        .secure()
        .scope_with(&scope)
        .exec(&conn)
        .await
        .unwrap();
    let dup = turn::Entity::insert(running_turn(h.tenant, chat, r1))
        .secure()
        .scope_unchecked(&scope)
        .unwrap()
        .exec(&conn)
        .await;
    assert!(dup.is_err(), "request_id unique per chat");
    turn::Entity::insert(running_turn(h.tenant, chat, Uuid::new_v4()))
        .secure()
        .scope_unchecked(&scope)
        .unwrap()
        .exec(&conn)
        .await
        .unwrap();
    // the same request id in another chat is fine
    let other = h.create_chat().await;
    turn::Entity::insert(running_turn(h.tenant, other, r1))
        .secure()
        .scope_unchecked(&scope)
        .unwrap()
        .exec(&conn)
        .await
        .unwrap();
}
