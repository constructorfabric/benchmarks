#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::DBProvider;
use toolkit_db::Db;
use toolkit_db::secure::{AccessScope, SecureUpdateExt};
use uuid::Uuid;

use super::*;
use crate::domain::error::DomainError;
use crate::domain::model::{TurnState, error_codes};
use crate::infra::db::entities::chat_turn::{ActiveModel, Column, Entity};
use crate::infra::db::repos::chat::{ChatRepo, NewChat};
use crate::infra::db::test_db;

const TENANT: Uuid = Uuid::from_u128(1);
const USER: Uuid = Uuid::from_u128(2);

fn t(secs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap() + Duration::seconds(secs)
}

async fn chat(db: &Db) -> Uuid {
    let id = Uuid::new_v4();
    ChatRepo::insert(
        &db.conn().unwrap(),
        &AccessScope::allow_all(),
        NewChat {
            id,
            tenant_id: TENANT,
            user_id: USER,
            model: "m".to_owned(),
            title: None,
            now: t(0),
        },
    )
    .await
    .unwrap();
    id
}

fn new_turn(chat_id: Uuid, request_id: Uuid, started_at: DateTime<Utc>) -> NewTurn {
    NewTurn {
        id: Uuid::new_v4(),
        tenant_id: TENANT,
        chat_id,
        request_id,
        requester_user_id: USER,
        web_search_enabled: false,
        preflight: None,
        now: started_at,
    }
}

fn preflight() -> PreflightFields {
    PreflightFields {
        reserve_tokens: 1000,
        max_output_tokens_applied: 400,
        reserved_credits_micro: 5000,
        policy_version_applied: 3,
        effective_model: "gpt-x".to_owned(),
        minimal_generation_floor_applied: 50,
    }
}

fn terminal(state: TurnState, now: DateTime<Utc>) -> TerminalUpdate {
    TerminalUpdate {
        state,
        error_code: None,
        error_detail: None,
        assistant_message_id: None,
        provider_response_id: None,
        counters: TurnCounters::default(),
        now,
    }
}

/// Overwrite `last_progress_at` (NULL allowed) of a turn.
async fn set_progress(db: &Db, id: Uuid, at: Option<DateTime<Utc>>) {
    Entity::update_many()
        .col_expr(Column::LastProgressAt, Expr::value(at))
        .filter(Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&db.conn().unwrap())
        .await
        .unwrap();
}

async fn get(db: &Db, id: Uuid) -> crate::infra::db::entities::chat_turn::Model {
    use toolkit_db::secure::SecureEntityExt;
    Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&db.conn().unwrap())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn insert_running_sets_initial_row() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let mut n = new_turn(chat_id, Uuid::new_v4(), t(10));
    n.web_search_enabled = true;
    n.preflight = Some(preflight());
    let row = TurnRepo::insert_running(&conn, n.clone()).await.unwrap();
    // The returned row (built without a read-back) equals what was stored.
    assert_eq!(row, get(&db, n.id).await);
    assert_eq!(row.id, n.id);
    assert_eq!(row.state, "running");
    assert_eq!(row.requester_type, "user");
    assert_eq!(row.requester_user_id, Some(USER));
    assert_eq!(row.started_at, t(10));
    assert_eq!(row.last_progress_at, Some(t(10)));
    assert_eq!(row.updated_at, Some(t(10)));
    assert_eq!(row.completed_at, None);
    assert!(row.web_search_enabled);
    assert_eq!(row.reserve_tokens, Some(1000));
    assert_eq!(row.max_output_tokens_applied, Some(400));
    assert_eq!(row.reserved_credits_micro, Some(5000));
    assert_eq!(row.policy_version_applied, Some(3));
    assert_eq!(row.effective_model.as_deref(), Some("gpt-x"));
    assert_eq!(row.minimal_generation_floor_applied, Some(50));
    assert_eq!(
        (
            row.web_search_completed_count,
            row.code_interpreter_completed_count,
            row.file_search_completed_count
        ),
        (0, 0, 0)
    );
}

#[tokio::test]
async fn cas_finalize_only_once() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    let msg = Uuid::new_v4();

    let mut first = terminal(TurnState::Completed, t(5));
    first.assistant_message_id = Some(msg);
    first.provider_response_id = Some("resp_1".to_owned());
    first.counters = TurnCounters {
        web_search: 2,
        code_interpreter: 1,
        file_search: 3,
    };
    assert!(
        TurnRepo::cas_finalize(&conn, turn.id, &first)
            .await
            .unwrap()
    );

    let mut second = terminal(TurnState::Failed, t(9));
    second.error_code = Some(error_codes::PROVIDER_ERROR.to_owned());
    assert!(
        !TurnRepo::cas_finalize(&conn, turn.id, &second)
            .await
            .unwrap()
    );

    let row = get(&db, turn.id).await;
    assert_eq!(row.state, "completed");
    assert_eq!(row.completed_at, Some(t(5)));
    assert_eq!(row.updated_at, Some(t(5)));
    assert_eq!(row.assistant_message_id, Some(msg));
    assert_eq!(row.provider_response_id.as_deref(), Some("resp_1"));
    assert_eq!(row.error_code, None);
    assert_eq!(row.web_search_completed_count, 2);
    assert_eq!(row.code_interpreter_completed_count, 1);
    assert_eq!(row.file_search_completed_count, 3);
}

#[tokio::test]
async fn cas_finalize_failed_records_error_and_rejects_non_terminal_target() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();

    let err = TurnRepo::cas_finalize(&conn, turn.id, &terminal(TurnState::Running, t(1)))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Internal(_)));
    assert_eq!(get(&db, turn.id).await.state, "running");

    let mut f = terminal(TurnState::Failed, t(2));
    f.error_code = Some(error_codes::PROVIDER_ERROR.to_owned());
    f.error_detail = Some("stream broke".to_owned());
    assert!(TurnRepo::cas_finalize(&conn, turn.id, &f).await.unwrap());
    let row = get(&db, turn.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("provider_error"));
    assert_eq!(row.error_detail.as_deref(), Some("stream broke"));
    assert_eq!(row.completed_at, Some(t(2)));
}

#[tokio::test]
async fn cas_orphan_rechecks_staleness() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let cutoff = t(100);

    // Fresh progress although `started_at` is old: not an orphan.
    let fresh = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    set_progress(&db, fresh.id, Some(t(150))).await;
    assert!(
        !TurnRepo::cas_orphan(&conn, fresh.id, cutoff, t(200))
            .await
            .unwrap()
    );
    assert_eq!(get(&db, fresh.id).await.state, "running");

    // Progress exactly at the cutoff counts as stale (`<=`).
    set_progress(&db, fresh.id, Some(t(100))).await;
    assert!(
        TurnRepo::cas_orphan(&conn, fresh.id, cutoff, t(200))
            .await
            .unwrap()
    );
    let row = get(&db, fresh.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(row.completed_at, Some(t(200)));
    assert_eq!(row.updated_at, Some(t(200)));

    // Terminal now: a second CAS is a no-op.
    assert!(
        !TurnRepo::cas_orphan(&conn, fresh.id, cutoff, t(300))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn cas_orphan_null_progress_falls_back_to_started_at() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let cutoff = t(100);

    let recent_chat = chat(&db).await;
    let recent = TurnRepo::insert_running(&conn, new_turn(recent_chat, Uuid::new_v4(), t(150)))
        .await
        .unwrap();
    set_progress(&db, recent.id, None).await;
    assert!(
        !TurnRepo::cas_orphan(&conn, recent.id, cutoff, t(200))
            .await
            .unwrap()
    );

    let old_chat = chat(&db).await;
    let old = TurnRepo::insert_running(&conn, new_turn(old_chat, Uuid::new_v4(), t(50)))
        .await
        .unwrap();
    set_progress(&db, old.id, None).await;
    assert!(
        TurnRepo::cas_orphan(&conn, old.id, cutoff, t(200))
            .await
            .unwrap()
    );
    assert_eq!(get(&db, old.id).await.state, "failed");
}

#[tokio::test]
async fn cas_orphan_skips_soft_deleted_turns() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    assert!(
        TurnRepo::soft_delete(&conn, turn.id, None, t(10))
            .await
            .unwrap()
    );
    assert!(
        !TurnRepo::cas_orphan(&conn, turn.id, t(100), t(200))
            .await
            .unwrap()
    );
    assert_eq!(get(&db, turn.id).await.state, "running");
}

#[tokio::test]
async fn insert_running_conflicts_map_to_domain_errors() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let req = Uuid::new_v4();
    let first = TurnRepo::insert_running(&conn, new_turn(chat_id, req, t(0)))
        .await
        .unwrap();

    // Same `(chat_id, request_id)` (the running index is violated too).
    let err = TurnRepo::insert_running(&conn, new_turn(chat_id, req, t(1)))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::RequestIdConflict), "{err:?}");

    // Another request while one is running.
    let err = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(1)))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::TurnAlreadyRunning), "{err:?}");

    // The conflicting inserts left nothing behind.
    assert_eq!(
        TurnRepo::running_in_chat(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );

    // Terminal turn: the request key still conflicts, a new request is fine.
    assert!(
        TurnRepo::cas_finalize(&conn, first.id, &terminal(TurnState::Cancelled, t(2)))
            .await
            .unwrap()
    );
    let err = TurnRepo::insert_running(&conn, new_turn(chat_id, req, t(3)))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::RequestIdConflict), "{err:?}");
    TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(3)))
        .await
        .unwrap();
}

#[tokio::test]
async fn insert_running_ignores_soft_deleted_running_turn_but_keeps_request_key() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let req = Uuid::new_v4();
    let old = TurnRepo::insert_running(&conn, new_turn(chat_id, req, t(0)))
        .await
        .unwrap();
    TurnRepo::soft_delete(&conn, old.id, None, t(1))
        .await
        .unwrap();

    let err = TurnRepo::insert_running(&conn, new_turn(chat_id, req, t(2)))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::RequestIdConflict), "{err:?}");
    TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(2)))
        .await
        .unwrap();
}

#[tokio::test]
async fn insert_running_conflict_keeps_the_transaction_usable() {
    let db = test_db().await;
    let chat_id = chat(&db).await;
    let req = Uuid::new_v4();
    TurnRepo::insert_running(&db.conn().unwrap(), new_turn(chat_id, req, t(0)))
        .await
        .unwrap();

    let provider = DBProvider::<DomainError>::new(db.clone());
    let outcome = provider
        .transaction(move |tx| {
            Box::pin(async move {
                let err = TurnRepo::insert_running(tx, new_turn(chat_id, req, t(1)))
                    .await
                    .unwrap_err();
                // Still usable after the conflict (no aborted transaction).
                let found = TurnRepo::find_by_request(tx, TENANT, chat_id, req).await?;
                Ok((err, found.is_some()))
            })
        })
        .await
        .unwrap();
    assert!(matches!(outcome.0, DomainError::RequestIdConflict));
    assert!(outcome.1);
}

#[tokio::test]
async fn find_by_request_is_tenant_scoped_and_includes_deleted() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let req = Uuid::new_v4();
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, req, t(0)))
        .await
        .unwrap();
    assert_eq!(
        TurnRepo::find_by_request(&conn, TENANT, chat_id, req)
            .await
            .unwrap()
            .unwrap()
            .id,
        turn.id
    );
    assert!(
        TurnRepo::find_by_request(&conn, Uuid::from_u128(99), chat_id, req)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        TurnRepo::find_by_request(&conn, TENANT, chat_id, Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
    TurnRepo::soft_delete(&conn, turn.id, None, t(1))
        .await
        .unwrap();
    let deleted = TurnRepo::find_by_request(&conn, TENANT, chat_id, req)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.deleted_at, Some(t(1)));
}

#[tokio::test]
async fn latest_live_ignores_deleted() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    assert!(
        TurnRepo::latest_live(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .is_none()
    );

    let mut ids = Vec::new();
    for secs in [10, 20, 30] {
        let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(secs)))
            .await
            .unwrap();
        TurnRepo::cas_finalize(&conn, turn.id, &terminal(TurnState::Completed, t(secs + 1)))
            .await
            .unwrap();
        ids.push(turn.id);
    }
    assert_eq!(
        TurnRepo::latest_live(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        ids[2]
    );

    TurnRepo::soft_delete(&conn, ids[2], None, t(40))
        .await
        .unwrap();
    assert_eq!(
        TurnRepo::latest_live(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        ids[1]
    );
    TurnRepo::soft_delete(&conn, ids[1], None, t(41))
        .await
        .unwrap();
    TurnRepo::soft_delete(&conn, ids[0], None, t(42))
        .await
        .unwrap();
    assert!(
        TurnRepo::latest_live(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn latest_live_breaks_started_at_ties_by_id() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(5)))
            .await
            .unwrap();
        TurnRepo::cas_finalize(&conn, turn.id, &terminal(TurnState::Completed, t(6)))
            .await
            .unwrap();
        ids.push(turn.id);
    }
    let max = *ids.iter().max().unwrap();
    assert_eq!(
        TurnRepo::latest_live(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        max
    );
}

#[tokio::test]
async fn running_in_chat_ignores_terminal_and_deleted() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    assert!(
        TurnRepo::running_in_chat(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .is_none()
    );
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    assert!(
        TurnRepo::running_in_chat(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        TurnRepo::running_in_chat(&conn, Uuid::from_u128(99), chat_id)
            .await
            .unwrap()
            .is_none()
    );
    TurnRepo::soft_delete(&conn, turn.id, None, t(1))
        .await
        .unwrap();
    assert!(
        TurnRepo::running_in_chat(&conn, TENANT, chat_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn fill_preflight_writes_only_null_columns() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    assert_eq!(turn.reserve_tokens, None);
    assert_eq!(turn.effective_model, None);

    TurnRepo::fill_preflight(&conn, turn.id, &preflight())
        .await
        .unwrap();
    let row = get(&db, turn.id).await;
    assert_eq!(row.reserve_tokens, Some(1000));
    assert_eq!(row.max_output_tokens_applied, Some(400));
    assert_eq!(row.reserved_credits_micro, Some(5000));
    assert_eq!(row.policy_version_applied, Some(3));
    assert_eq!(row.effective_model.as_deref(), Some("gpt-x"));
    assert_eq!(row.minimal_generation_floor_applied, Some(50));

    // Immutable once set.
    let other = PreflightFields {
        reserve_tokens: 1,
        max_output_tokens_applied: 2,
        reserved_credits_micro: 3,
        policy_version_applied: 4,
        effective_model: "other".to_owned(),
        minimal_generation_floor_applied: 6,
    };
    TurnRepo::fill_preflight(&conn, turn.id, &other)
        .await
        .unwrap();
    assert_eq!(get(&db, turn.id).await, row);

    let err = TurnRepo::fill_preflight(&conn, Uuid::new_v4(), &other)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::TurnNotFound), "{err:?}");
}

#[tokio::test]
async fn touch_progress_refreshes_running_turns_only() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    let counters = TurnCounters {
        web_search: 1,
        code_interpreter: 2,
        file_search: 3,
    };
    TurnRepo::touch_progress(&conn, turn.id, counters)
        .await
        .unwrap();
    let row = get(&db, turn.id).await;
    assert!(row.last_progress_at.unwrap() > t(0));
    assert_eq!(row.updated_at, row.last_progress_at);
    assert_eq!(
        (
            row.web_search_completed_count,
            row.code_interpreter_completed_count,
            row.file_search_completed_count
        ),
        (1, 2, 3)
    );
    assert_eq!(row.state, "running");

    TurnRepo::cas_finalize(&conn, turn.id, &terminal(TurnState::Completed, t(5)))
        .await
        .unwrap();
    let finalized = get(&db, turn.id).await;
    TurnRepo::touch_progress(&conn, turn.id, TurnCounters::default())
        .await
        .unwrap();
    assert_eq!(get(&db, turn.id).await, finalized);
}

#[tokio::test]
async fn stale_running_selects_stale_live_running_turns_up_to_limit() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let cutoff = t(100);

    let mut stale = Vec::new();
    for (i, secs) in [10, 20, 30].into_iter().enumerate() {
        let c = chat(&db).await;
        let turn = TurnRepo::insert_running(&conn, new_turn(c, Uuid::new_v4(), t(secs)))
            .await
            .unwrap();
        if i == 2 {
            // NULL progress falls back to `started_at`.
            set_progress(&db, turn.id, None).await;
        }
        stale.push(turn.id);
    }
    // Fresh progress, terminal and soft-deleted turns are excluded.
    let c = chat(&db).await;
    let fresh = TurnRepo::insert_running(&conn, new_turn(c, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    set_progress(&db, fresh.id, Some(t(500))).await;
    let c = chat(&db).await;
    let done = TurnRepo::insert_running(&conn, new_turn(c, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    TurnRepo::cas_finalize(&conn, done.id, &terminal(TurnState::Completed, t(1)))
        .await
        .unwrap();
    let c = chat(&db).await;
    let deleted = TurnRepo::insert_running(&conn, new_turn(c, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    TurnRepo::soft_delete(&conn, deleted.id, None, t(1))
        .await
        .unwrap();

    let found = TurnRepo::stale_running(&conn, cutoff, None, 100)
        .await
        .unwrap();
    let mut found_ids: Vec<Uuid> = found.iter().map(|m| m.id).collect();
    found_ids.sort();
    stale.sort();
    assert_eq!(found_ids, stale);

    let first = TurnRepo::stale_running(&conn, cutoff, None, 2)
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    // The next page starts after the last `(started_at, id)` of the previous one.
    let last = first.last().unwrap();
    let next = TurnRepo::stale_running(&conn, cutoff, Some((last.started_at, last.id)), 2)
        .await
        .unwrap();
    let mut paged: Vec<Uuid> = first.iter().chain(&next).map(|m| m.id).collect();
    paged.sort();
    assert_eq!(paged, stale);
}

#[tokio::test]
async fn soft_delete_marks_turn_once_and_records_replacement() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let chat_id = chat(&db).await;
    let turn = TurnRepo::insert_running(&conn, new_turn(chat_id, Uuid::new_v4(), t(0)))
        .await
        .unwrap();
    TurnRepo::cas_finalize(&conn, turn.id, &terminal(TurnState::Completed, t(1)))
        .await
        .unwrap();
    let replacement = Uuid::new_v4();
    assert!(
        TurnRepo::soft_delete(&conn, turn.id, Some(replacement), t(7))
            .await
            .unwrap()
    );
    let row = get(&db, turn.id).await;
    assert_eq!(row.deleted_at, Some(t(7)));
    assert_eq!(row.updated_at, Some(t(7)));
    assert_eq!(row.replaced_by_request_id, Some(replacement));
    assert_eq!(row.state, "completed");

    // Already deleted: untouched.
    assert!(
        !TurnRepo::soft_delete(&conn, turn.id, None, t(9))
            .await
            .unwrap()
    );
    assert_eq!(get(&db, turn.id).await.deleted_at, Some(t(7)));
}

#[test]
fn insert_absorbs_conflicts_on_every_unique_key() {
    use sea_orm::{DbBackend, QueryTrait, Set};
    for backend in [DbBackend::Postgres, DbBackend::Sqlite] {
        let sql = Entity::insert(ActiveModel {
            id: Set(Uuid::nil()),
            ..Default::default()
        })
        .on_conflict(absorb_any_conflict())
        .build(backend)
        .to_string();
        assert!(
            sql.split("ON CONFLICT").nth(1).map(str::trim) == Some("DO NOTHING"),
            "{backend:?} must not target a single constraint: {sql}"
        );
    }
}
