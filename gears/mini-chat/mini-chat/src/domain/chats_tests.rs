//! Chat service unit tests: title validation, `OData` helpers, contract functions and SQLite
//! cursor boundaries on timestamps.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;

use sea_orm::ActiveValue::Set;
use toolkit_db::secure::secure_insert;
use toolkit_odata::filter::ODataValue;
use toolkit_odata::{CursorV1, ODataQuery, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::testing::{TENANT_A, TestApp, USER_A1, USER_A2, ctx_a1};

#[test]
fn title_validation_trims_and_bounds() {
    assert_eq!(validate_title("  Hello ").unwrap(), "Hello");
    assert_eq!(validate_title(&"a".repeat(255)).unwrap().len(), 255);
    assert_eq!(validate_title(&"\u{436}".repeat(255)).unwrap().chars().count(), 255);
    for bad in ["", "   ", "\t\n"] {
        assert!(matches!(validate_title(bad), Err(DomainError::InvalidArgument { ref reason, ref field, .. }) if reason == "INVALID_TITLE" && field == "title"));
    }
    assert!(validate_title(&"a".repeat(256)).is_err());
}

#[test]
fn timestamp_filter_values_use_the_stored_text_format_on_sqlite() {
    let dt = chrono::DateTime::parse_from_rfc3339("2026-10-03T12:00:00.5+00:00").unwrap().with_timezone(&chrono::Utc);
    let v = ODataValue::DateTime(dt);
    let text = |v: ODataValue| match v {
        ODataValue::String(s) => s,
        other => panic!("expected a string, got {other:?}"),
    };
    assert_eq!(text(map_timestamp_value(true, &v)), "2026-10-03T12:00:00.500000000Z");
    assert!(matches!(map_timestamp_value(false, &v), ODataValue::DateTime(d) if d == dt));
    assert_eq!(text(map_timestamp_value(true, &ODataValue::String("x".to_owned()))), "x");
    // Same text as a stored (clock-normalized) timestamp.
    let now = clock::now();
    let stored = now.format(&time::format_description::well_known::Rfc3339).unwrap();
    let chrono_now = chrono::DateTime::parse_from_rfc3339(&stored).unwrap().with_timezone(&chrono::Utc);
    assert_eq!(text(map_timestamp_value(true, &ODataValue::DateTime(chrono_now))), stored);
}

#[test]
fn default_order_applies_only_without_order_and_cursor() {
    let q = with_default_order(&ODataQuery::default(), &[("updated_at", SortDir::Desc), ("id", SortDir::Desc)]);
    assert_eq!(q.order.to_signed_tokens(), "-updated_at,-id");
    let explicit = ODataQuery::default().with_order(ODataOrderBy(vec![OrderKey { field: "title".into(), dir: SortDir::Asc }]));
    assert_eq!(with_default_order(&explicit, &[("id", SortDir::Desc)]).order.to_signed_tokens(), "+title");
}

async fn insert_chat(t: &TestApp, updated_at: OffsetDateTime, user: Uuid) -> Uuid {
    let am = chat::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        user_id: Set(user),
        model: Set("gpt-4.1".to_owned()),
        title: Set(None),
        is_temporary: Set(false),
        created_at: Set(updated_at),
        updated_at: Set(updated_at),
        deleted_at: Set(None),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<chat::Entity>(am, &AccessScope::for_tenant(TENANT_A).ensure_owner(user), &conn).await.unwrap().id
}

async fn all_pages(t: &TestApp, limit: u64, order: Option<ODataOrderBy>) -> Vec<Vec<Uuid>> {
    let mut query = ODataQuery::default().with_limit(limit);
    if let Some(o) = order {
        query = query.with_order(o);
    }
    let mut pages = Vec::new();
    loop {
        let page = list_chats(&t.app, &ctx_a1(), &query).await.unwrap();
        pages.push(page.items.iter().map(|v| v.chat.id).collect::<Vec<_>>());
        let Some(next) = page.page_info.next_cursor else { break };
        query = ODataQuery::default().with_limit(limit).with_cursor(CursorV1::decode(&next).unwrap());
        assert!(pages.len() < 50);
    }
    pages
}

#[tokio::test]
async fn cursor_pagination_handles_equal_and_close_timestamps_on_sqlite() {
    let t = TestApp::new().await;
    assert!(text_timestamps(&t.app));
    // Five chats share one timestamp (tiebreaker on id), others differ by 1 ns.
    let base = clock::now();
    let mut ids = HashSet::new();
    for _ in 0..5 {
        ids.insert(insert_chat(&t, base, USER_A1).await);
    }
    for i in 1..=4 {
        let ts = OffsetDateTime::from_unix_timestamp_nanos(base.unix_timestamp_nanos() + i * 2).unwrap();
        ids.insert(insert_chat(&t, clock::normalize(ts), USER_A1).await);
    }
    insert_chat(&t, base, USER_A2).await; // not visible to A1

    for limit in [1, 2, 3, 4, 9, 10] {
        let pages = all_pages(&t, limit, None).await;
        let flat: Vec<Uuid> = pages.concat();
        assert_eq!(flat.len(), 9, "limit {limit}: {pages:?}");
        assert_eq!(flat.iter().copied().collect::<HashSet<_>>(), ids, "limit {limit}");
        let asc = all_pages(
            &t,
            limit,
            Some(ODataOrderBy(vec![OrderKey { field: "updated_at".into(), dir: SortDir::Asc }])),
        )
        .await
        .concat();
        assert_eq!(asc.len(), 9);
        assert_eq!(asc.iter().copied().collect::<HashSet<_>>(), ids);
    }
    // Default order: newest first; the first page starts with the latest timestamp.
    let first = list_chats(&t.app, &ctx_a1(), &ODataQuery::default()).await.unwrap();
    assert!(first.items.windows(2).all(|w| w[0].chat.updated_at >= w[1].chat.updated_at));
}

#[tokio::test]
async fn cursor_round_trips_timestamp_text() {
    let t = TestApp::new().await;
    for _ in 0..3 {
        insert_chat(&t, clock::now(), USER_A1).await;
    }
    let page = list_chats(&t.app, &ctx_a1(), &ODataQuery::default().with_limit(2)).await.unwrap();
    let cursor = CursorV1::decode(page.page_info.next_cursor.as_deref().unwrap()).unwrap();
    let last = &page.items[1].chat;
    assert_eq!(cursor.s, "-updated_at,-id");
    assert_eq!(cursor.k[0], last.updated_at.format(&time::format_description::well_known::Rfc3339).unwrap());
    assert_eq!(cursor.k[1], last.id.to_string());
}

#[tokio::test]
async fn contract_functions() {
    let t = TestApp::new().await;
    let id = t.create_chat(&ctx_a1(), None).await;
    let scope = AccessScope::for_tenant(TENANT_A).ensure_owner(USER_A1);
    let chat = load_chat(&t.app, &scope, id).await.unwrap();
    assert_eq!(chat.id, id);
    let foreign = AccessScope::for_tenant(TENANT_A).ensure_owner(USER_A2);
    assert!(matches!(load_chat(&t.app, &foreign, id).await, Err(DomainError::NotFound { resource: Resource::Chat, .. })));
    let later = clock::now();
    let scope2 = scope.clone();
    let loaded = t
        .app
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                touch_chat(tx, TENANT_A, id, later).await?;
                load_chat_tx(tx, &scope2, id).await
            })
        })
        .await
        .unwrap();
    assert_eq!(loaded.updated_at, later);
    assert!(loaded.updated_at > chat.updated_at);
    let missing = Uuid::new_v4();
    let res = t
        .app
        .db
        .transaction(move |tx| Box::pin(async move { load_chat_tx(tx, &scope, missing).await }))
        .await;
    assert!(matches!(res, Err(DomainError::NotFound { .. })));
}
