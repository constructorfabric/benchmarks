#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{TimeZone, Utc};
use sea_orm::IntoActiveModel;
use toolkit_db::secure::{AccessScope, secure_insert};
use uuid::Uuid;

use super::*;
use crate::infra::db::entities::thread_summary;
use crate::infra::db::repos::chat::{ChatRepo, NewChat};
use crate::infra::db::test_db;
use crate::testing::seed;

#[tokio::test]
async fn get_for_chat_returns_the_chat_summary_within_the_tenant() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let tenant = Uuid::from_u128(1);
    let chat_id = Uuid::new_v4();
    let now = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap();
    ChatRepo::insert(
        &conn,
        &AccessScope::allow_all(),
        NewChat {
            id: chat_id,
            tenant_id: tenant,
            user_id: Uuid::from_u128(3),
            model: "m".to_owned(),
            title: None,
            now,
        },
    )
    .await
    .unwrap();
    let last = seed::insert_message(&db, chat_id, "user", "x", Some(Uuid::new_v4()), now).await;
    assert!(
        ThreadSummaryRepo::get_for_chat(&conn, tenant, chat_id)
            .await
            .unwrap()
            .is_none()
    );

    let row = thread_summary::Model {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        chat_id,
        summary_text: Some("sum".to_owned()),
        summarized_up_to_created_at: now,
        summarized_up_to_message_id: last,
        token_estimate: Some(7),
        created_at: now,
        updated_at: now,
    };
    secure_insert::<thread_summary::Entity>(
        row.clone().into_active_model(),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();

    let got = ThreadSummaryRepo::get_for_chat(&conn, tenant, chat_id)
        .await
        .unwrap();
    assert_eq!(got, Some(row));
    assert!(
        ThreadSummaryRepo::get_for_chat(&conn, Uuid::from_u128(2), chat_id)
            .await
            .unwrap()
            .is_none()
    );
}
