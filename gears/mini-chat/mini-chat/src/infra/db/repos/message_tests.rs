#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use toolkit_db::Db;
use toolkit_db::secure::AccessScope;
use uuid::Uuid;

use super::*;
use crate::infra::db::repos::chat::{ChatRepo, NewChat};
use crate::infra::db::test_db;
use crate::testing::seed::{self, NewMessage};

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

#[allow(
    clippy::unnecessary_wraps,
    reason = "matches the nullable request_id column"
)]
fn req() -> Option<Uuid> {
    Some(Uuid::new_v4())
}

async fn add(db: &Db, new: NewMessage) -> Uuid {
    seed::insert_message_with(db, new).await
}

fn ids(rows: &[message::Model]) -> Vec<Uuid> {
    rows.iter().map(|m| m.id).collect()
}

#[tokio::test]
async fn recent_query_excludes_compressed_deleted_after_boundary_and_before_frontier() {
    let db = test_db().await;
    let chat_id = chat(&db).await;
    let m = |secs: i64, role: &str, request_id: Option<Uuid>| {
        NewMessage::new(chat_id, role, "x", request_id, t(secs))
    };

    let before_frontier = add(&db, m(1, "user", req())).await;
    let frontier = add(&db, m(2, "assistant", req())).await;
    let kept_a = add(&db, m(3, "user", req())).await;
    let _compressed = add(&db, m(4, "assistant", req()).compressed()).await;
    let _deleted = add(&db, m(5, "user", req()).deleted(t(50))).await;
    let _no_request = add(&db, m(6, "user", None)).await;
    let _system = add(&db, m(6, "system", req())).await;
    let kept_b = add(&db, m(7, "assistant", req())).await;
    let boundary = add(&db, m(8, "user", req())).await;
    let _after = add(&db, m(9, "assistant", req())).await;

    let conn = db.conn().unwrap();
    let rows = MessageRepo::recent_for_context(
        &conn,
        TENANT,
        chat_id,
        (t(8), boundary),
        Some((t(2), frontier)),
        10,
    )
    .await
    .unwrap();
    // Newest first (the D§4 query order).
    assert_eq!(ids(&rows), vec![boundary, kept_b, kept_a]);

    // Without a summary the frontier predicate is omitted.
    let rows = MessageRepo::recent_for_context(&conn, TENANT, chat_id, (t(8), boundary), None, 10)
        .await
        .unwrap();
    assert_eq!(
        ids(&rows),
        vec![boundary, kept_b, kept_a, frontier, before_frontier]
    );

    // LIMIT keeps the newest messages.
    let rows = MessageRepo::recent_for_context(&conn, TENANT, chat_id, (t(8), boundary), None, 2)
        .await
        .unwrap();
    assert_eq!(ids(&rows), vec![boundary, kept_b]);
    let rows = MessageRepo::recent_for_context(&conn, TENANT, chat_id, (t(8), boundary), None, 0)
        .await
        .unwrap();
    assert!(rows.is_empty());

    // Another tenant sees nothing.
    let rows = MessageRepo::recent_for_context(
        &conn,
        Uuid::from_u128(99),
        chat_id,
        (t(8), boundary),
        None,
        10,
    )
    .await
    .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn recent_query_orders_equal_timestamps_by_id_and_cuts_on_the_tuple() {
    let db = test_db().await;
    let chat_id = chat(&db).await;
    let a = add(&db, NewMessage::new(chat_id, "user", "a", req(), t(5))).await;
    let b = add(&db, NewMessage::new(chat_id, "assistant", "b", req(), t(5))).await;
    let (low, high) = if a < b { (a, b) } else { (b, a) };

    let conn = db.conn().unwrap();
    // Boundary on the higher id includes both, newest (higher id) first.
    let rows = MessageRepo::recent_for_context(&conn, TENANT, chat_id, (t(5), high), None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&rows), vec![high, low]);
    // Boundary on the lower id excludes the higher id at the same instant.
    let rows = MessageRepo::recent_for_context(&conn, TENANT, chat_id, (t(5), low), None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&rows), vec![low]);
    // A frontier on the lower id keeps only the higher one.
    let rows = MessageRepo::recent_for_context(
        &conn,
        TENANT,
        chat_id,
        (t(5), high),
        Some((t(5), low)),
        10,
    )
    .await
    .unwrap();
    assert_eq!(ids(&rows), vec![high]);
}

#[tokio::test]
async fn snapshot_boundary_is_latest_non_deleted_message() {
    let db = test_db().await;
    let chat_id = chat(&db).await;
    let conn = db.conn().unwrap();
    assert_eq!(
        MessageRepo::snapshot_boundary(&conn, TENANT, chat_id)
            .await
            .unwrap(),
        None
    );

    let first = add(&db, NewMessage::new(chat_id, "user", "a", req(), t(1))).await;
    assert_eq!(
        MessageRepo::snapshot_boundary(&conn, TENANT, chat_id)
            .await
            .unwrap(),
        Some((t(1), first))
    );
    let _deleted = add(
        &db,
        NewMessage::new(chat_id, "user", "b", req(), t(2)).deleted(t(9)),
    )
    .await;
    assert_eq!(
        MessageRepo::snapshot_boundary(&conn, TENANT, chat_id)
            .await
            .unwrap(),
        Some((t(1), first))
    );
    let x = add(&db, NewMessage::new(chat_id, "user", "c", req(), t(3))).await;
    let y = add(&db, NewMessage::new(chat_id, "assistant", "d", req(), t(3))).await;
    assert_eq!(
        MessageRepo::snapshot_boundary(&conn, TENANT, chat_id)
            .await
            .unwrap(),
        Some((t(3), Ord::max(x, y)))
    );
    // Scoped to the chat.
    let other = chat(&db).await;
    assert_eq!(
        MessageRepo::snapshot_boundary(&conn, TENANT, other)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn prior_context_tokens_uses_latest_assistant_with_usage() {
    let db = test_db().await;
    let chat_id = chat(&db).await;
    let conn = db.conn().unwrap();
    let prior = || MessageRepo::prior_context_tokens(&conn, TENANT, chat_id);
    assert_eq!(prior().await.unwrap(), 0);

    let a = |secs: i64| NewMessage::new(chat_id, "assistant", "a", req(), t(secs));
    add(&db, a(1).tokens(10, 5)).await;
    assert_eq!(prior().await.unwrap(), 15);

    // Newer assistant message with usage wins (input + output).
    add(&db, a(2).tokens(100, 20)).await;
    assert_eq!(prior().await.unwrap(), 120);

    // Output-only usage counts as non-zero usage.
    add(&db, a(3).tokens(0, 7)).await;
    assert_eq!(prior().await.unwrap(), 7);

    // Newer messages without usage, deleted ones and user messages are skipped.
    add(&db, a(4)).await;
    add(&db, a(5).tokens(900, 90).deleted(t(60))).await;
    add(
        &db,
        NewMessage::new(chat_id, "user", "u", req(), t(6)).tokens(500, 50),
    )
    .await;
    assert_eq!(prior().await.unwrap(), 7);
}
