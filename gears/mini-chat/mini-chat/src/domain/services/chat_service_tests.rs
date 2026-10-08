#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::macros::datetime;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::sync::mpsc::UnboundedReceiver;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt};
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
use uuid::Uuid;

use super::{ChatService, CreateChat};
use crate::api::rest::dto::ChatDetailDto;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{ChatAction, PolicyProvider};
use crate::domain::services::model_service::ModelService;
use crate::domain::time::db_ts;
use crate::infra::db::entities::{attachment, chat};
use crate::test_support::{
    FakeAuthz, FakePolicy, PanicPolicy, catalog_entry, ctx_for, seed_attachment, seed_chat,
    seed_message, snapshot, test_ctx, test_file_db, test_outbox,
};

const CHAT_NOT_FOUND: DomainError = DomainError::NotFound {
    resource: ResourceKind::Chat,
};

struct Fixture {
    _dir: tempfile::TempDir,
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<FakeAuthz>,
    svc: ChatService,
    outbox_rx: UnboundedReceiver<(String, serde_json::Value)>,
}

/// Catalog `a` (disabled), `b` (enabled, first default), `c` (enabled).
fn catalog_policy() -> Arc<dyn PolicyProvider> {
    Arc::new(FakePolicy::new(snapshot(vec![
        catalog_entry("a", false),
        catalog_entry("b", true),
        catalog_entry("c", true),
    ])))
}

async fn fixture_with(authz: FakeAuthz, policy: Arc<dyn PolicyProvider>) -> Fixture {
    let (dir, raw) = test_file_db().await;
    let (outbox, outbox_rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    let authz = Arc::new(authz);
    let models = Arc::new(ModelService::new(authz.clone(), policy));
    let svc = ChatService::new(db.clone(), authz.clone(), models, outbox);
    Fixture {
        _dir: dir,
        db,
        authz,
        svc,
        outbox_rx,
    }
}

async fn fixture() -> Fixture {
    fixture_with(FakeAuthz::default(), catalog_policy()).await
}

fn create(title: Option<&str>, model: Option<&str>) -> CreateChat {
    CreateChat {
        title: title.map(str::to_owned),
        model: model.map(str::to_owned),
    }
}

fn base_time() -> OffsetDateTime {
    datetime!(2026-01-01 00:00:00 UTC)
}

fn query_err(err: DomainError) -> toolkit_odata::Error {
    match err {
        DomainError::Query(q) => q.0,
        other => panic!("expected an OData query error, got {other:?}"),
    }
}

fn filter(raw: &str) -> ODataQuery {
    let expr = toolkit_odata::parse_filter_string(raw).unwrap().into_expr();
    let hash = toolkit_odata::short_filter_hash(Some(&expr)).unwrap();
    ODataQuery::default()
        .with_filter(expr)
        .with_filter_hash(hash)
}

#[tokio::test]
async fn create_defaults_model_and_trims_title() {
    let f = fixture().await;
    let ctx = test_ctx();

    let chat = f
        .svc
        .create(&ctx, create(Some("  Hello world \n"), None))
        .await
        .unwrap();

    assert_eq!(chat.model, "b");
    assert_eq!(chat.title.as_deref(), Some("Hello world"));
    assert!(!chat.is_temporary);
    assert_eq!(chat.message_count, 0);
    assert_eq!(chat.created_at, chat.updated_at);
    assert_eq!(f.svc.get(&ctx, chat.id).await.unwrap(), chat);
    assert_eq!(f.authz.chat_actions()[0], ChatAction::Create);

    let explicit = f.svc.create(&ctx, create(None, Some("c"))).await.unwrap();
    assert_eq!(explicit.model, "c");
    assert_eq!(explicit.title, None);

    let long = format!(" {} ", "\u{e9}".repeat(255));
    let max = f.svc.create(&ctx, create(Some(&long), None)).await.unwrap();
    assert_eq!(max.title.unwrap().chars().count(), 255);
}

#[tokio::test]
async fn create_rejects_blank_and_long_title_before_model_lookup() {
    let f = fixture_with(FakeAuthz::denying(), Arc::new(PanicPolicy)).await;
    let ctx = test_ctx();

    for bad in ["", "   ", "\t\n", &"x".repeat(256)] {
        let err = f
            .svc
            .create(&ctx, create(Some(bad), Some("b")))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::InvalidTitle, "title {bad:?}");
    }
    assert!(f.authz.chat_actions().is_empty());
}

#[tokio::test]
async fn create_rejects_disabled_model() {
    let f = fixture().await;
    let ctx = test_ctx();

    for model in ["a", "unknown"] {
        let err = f
            .svc
            .create(&ctx, create(None, Some(model)))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::InvalidModel, "model {model}");
    }
}

#[tokio::test]
async fn create_without_enabled_model_is_invalid_model() {
    let policy = Arc::new(FakePolicy::new(snapshot(vec![catalog_entry("a", false)])));
    let f = fixture_with(FakeAuthz::default(), policy).await;

    let err = f
        .svc
        .create(&test_ctx(), create(Some("t"), None))
        .await
        .unwrap_err();
    assert_eq!(err, DomainError::InvalidModel);
}

#[tokio::test]
async fn get_foreign_owner_is_not_found() {
    let f = fixture().await;
    let owner = test_ctx();
    let chat = f
        .svc
        .create(&owner, create(Some("mine"), None))
        .await
        .unwrap();
    let same_tenant_other_user = ctx_for(owner.subject_tenant_id(), Uuid::new_v4());
    let other_tenant = ctx_for(Uuid::new_v4(), owner.subject_id());

    for ctx in [&same_tenant_other_user, &other_tenant] {
        assert_eq!(f.svc.get(ctx, chat.id).await.unwrap_err(), CHAT_NOT_FOUND);
        assert_eq!(
            f.svc.update_title(ctx, chat.id, "x").await.unwrap_err(),
            CHAT_NOT_FOUND
        );
        assert_eq!(
            f.svc.delete(ctx, chat.id).await.unwrap_err(),
            CHAT_NOT_FOUND
        );
        assert!(
            f.svc
                .list(ctx, ODataQuery::default())
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }
    assert_eq!(f.svc.get(&owner, chat.id).await.unwrap().id, chat.id);
    assert_eq!(
        f.svc.get(&owner, Uuid::new_v4()).await.unwrap_err(),
        CHAT_NOT_FOUND
    );
}

#[tokio::test]
async fn denied_caller_gets_authz_error() {
    let f = fixture_with(FakeAuthz::denying(), catalog_policy()).await;
    let ctx = test_ctx();

    assert_eq!(
        f.svc.create(&ctx, create(None, None)).await.unwrap_err(),
        DomainError::AuthzDenied
    );
    assert_eq!(
        f.svc.get(&ctx, Uuid::new_v4()).await.unwrap_err(),
        DomainError::AuthzDenied
    );
}

#[tokio::test]
async fn list_orders_by_updated_at_desc_and_paginates() {
    let f = fixture().await;
    let ctx = test_ctx();
    let mut seeded = Vec::new();
    for i in 0..25 {
        let at = db_ts(base_time() + TimeDuration::seconds(i));
        seeded.push(
            seed_chat(&f.db, &ctx, Some(&format!("chat {i}")), at)
                .await
                .id,
        );
    }
    // Another user's chat in the same tenant is never listed.
    let other = ctx_for(ctx.subject_tenant_id(), Uuid::new_v4());
    seed_chat(&f.db, &other, Some("foreign"), db_ts(base_time())).await;
    let expected: Vec<Uuid> = seeded.iter().rev().copied().collect();

    let mut got = Vec::new();
    let mut query = ODataQuery::default().with_limit(10);
    let mut sizes = Vec::new();
    loop {
        let page = f.svc.list(&ctx, query.clone()).await.unwrap();
        assert_eq!(page.page_info.limit, 10);
        sizes.push(page.items.len());
        got.extend(page.items.iter().map(|c| c.id));
        match page.page_info.next_cursor {
            Some(next) => {
                query = ODataQuery::default()
                    .with_limit(10)
                    .with_cursor(CursorV1::decode(&next).unwrap());
            }
            None => break,
        }
    }

    assert_eq!(sizes, [10, 10, 5]);
    assert_eq!(got, expected);
    assert_eq!(got.iter().collect::<HashSet<_>>().len(), 25);
}

#[tokio::test]
async fn list_default_page_is_twenty_and_ties_break_by_id_desc() {
    let f = fixture().await;
    let ctx = test_ctx();
    let same = db_ts(base_time());
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(seed_chat(&f.db, &ctx, None, same).await.id);
    }
    ids.sort_unstable_by(|a, b| b.cmp(a));

    let page = f.svc.list(&ctx, ODataQuery::default()).await.unwrap();

    assert_eq!(page.page_info.limit, 20);
    assert_eq!(page.items.iter().map(|c| c.id).collect::<Vec<_>>(), ids);
    assert!(page.items.iter().all(|c| c.title.is_none()));
}

#[tokio::test]
async fn list_filter_contains_title() {
    let f = fixture().await;
    let ctx = test_ctx();
    let a1 = seed_chat(&f.db, &ctx, Some("alpha one"), db_ts(base_time())).await;
    seed_chat(
        &f.db,
        &ctx,
        Some("beta"),
        db_ts(base_time() + TimeDuration::seconds(1)),
    )
    .await;
    let a2 = seed_chat(
        &f.db,
        &ctx,
        Some("alpha two"),
        db_ts(base_time() + TimeDuration::seconds(2)),
    )
    .await;
    seed_chat(
        &f.db,
        &ctx,
        None,
        db_ts(base_time() + TimeDuration::seconds(3)),
    )
    .await;

    let page = f
        .svc
        .list(&ctx, filter("contains(title,'alpha')"))
        .await
        .unwrap();

    let ids: Vec<_> = page.items.iter().map(|c| c.id).collect();
    assert_eq!(ids, [a2.id, a1.id]);
}

/// `$orderby=title asc` with untitled chats (stored NULL, sorted first by
/// SQLite): page 1 has the expected order and its cursor continues the walk.
/// Known limitation (accepted): a NULL-title row exactly at a page boundary
/// pages as `""`, so keyset continuation can skip other NULL-title rows.
#[tokio::test]
async fn list_orderby_title_asc_with_untitled_chats() {
    let f = fixture().await;
    let ctx = test_ctx();
    let untitled = seed_chat(&f.db, &ctx, None, db_ts(base_time())).await;
    let b = seed_chat(&f.db, &ctx, Some("b"), db_ts(base_time())).await;
    let a = seed_chat(&f.db, &ctx, Some("a"), db_ts(base_time())).await;
    let order = ODataOrderBy(vec![OrderKey {
        field: "title".to_owned(),
        dir: SortDir::Asc,
    }]);

    let page = f
        .svc
        .list(&ctx, ODataQuery::default().with_order(order).with_limit(2))
        .await
        .unwrap();

    let ids: Vec<_> = page.items.iter().map(|c| c.id).collect();
    assert_eq!(ids, [untitled.id, a.id]);
    assert!(page.items[0].title.is_none());
    let next = page.page_info.next_cursor.expect("a third chat exists");
    let rest = f
        .svc
        .list(
            &ctx,
            ODataQuery::default()
                .with_limit(2)
                .with_cursor(CursorV1::decode(&next).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(rest.items.iter().map(|c| c.id).collect::<Vec<_>>(), [b.id]);
    assert!(rest.page_info.next_cursor.is_none());
}

#[tokio::test]
async fn default_order_pages_every_chat_once_including_untitled() {
    let f = fixture().await;
    let ctx = test_ctx();
    let mut seeded = Vec::new();
    for i in 0..7 {
        let title = (i % 2 == 0).then(|| format!("chat {i}"));
        let at = db_ts(base_time() + TimeDuration::seconds(i));
        seeded.push(seed_chat(&f.db, &ctx, title.as_deref(), at).await.id);
    }
    let expected: Vec<Uuid> = seeded.iter().rev().copied().collect();

    let mut got = Vec::new();
    let mut query = ODataQuery::default().with_limit(2);
    loop {
        let page = f.svc.list(&ctx, query).await.unwrap();
        got.extend(page.items.iter().map(|c| c.id));
        let Some(next) = page.page_info.next_cursor else {
            break;
        };
        query = ODataQuery::default()
            .with_limit(2)
            .with_cursor(CursorV1::decode(&next).unwrap());
    }

    assert_eq!(got, expected);
}

#[tokio::test]
async fn list_unknown_field_is_invalid_filter() {
    let f = fixture().await;
    let ctx = test_ctx();
    seed_chat(&f.db, &ctx, Some("t"), db_ts(base_time())).await;

    let err = f.svc.list(&ctx, filter("model eq 'b'")).await.unwrap_err();
    assert!(
        matches!(query_err(err), toolkit_odata::Error::InvalidFilter(_)),
        "unknown filter field"
    );

    let order = ODataOrderBy(vec![OrderKey {
        field: "created_at".to_owned(),
        dir: SortDir::Asc,
    }]);
    let err = f
        .svc
        .list(&ctx, ODataQuery::default().with_order(order))
        .await
        .unwrap_err();
    assert!(matches!(
        query_err(err),
        toolkit_odata::Error::InvalidOrderByField(_)
    ));
}

#[tokio::test]
async fn list_limit_over_100_clamped() {
    let f = fixture().await;
    let ctx = test_ctx();
    for i in 0..3 {
        seed_chat(
            &f.db,
            &ctx,
            None,
            db_ts(base_time() + TimeDuration::seconds(i)),
        )
        .await;
    }

    let page = f
        .svc
        .list(&ctx, ODataQuery::default().with_limit(500))
        .await
        .unwrap();

    assert_eq!(page.page_info.limit, 100);
    assert_eq!(page.items.len(), 3);
    assert!(page.page_info.next_cursor.is_none());
}

#[tokio::test]
async fn message_count_counts_live_messages_only() {
    let f = fixture().await;
    let ctx = test_ctx();
    let chat = seed_chat(&f.db, &ctx, None, db_ts(base_time())).await;
    let empty = seed_chat(&f.db, &ctx, None, db_ts(base_time())).await;
    seed_message(&f.db, &chat, "user", db_ts(base_time())).await;
    seed_message(&f.db, &chat, "assistant", db_ts(base_time())).await;
    seed_message(&f.db, &chat, "system", db_ts(base_time())).await;
    let deleted = seed_message(&f.db, &chat, "user", db_ts(base_time())).await;
    let conn = f.db.conn().unwrap();
    crate::infra::db::entities::message::Entity::update_many()
        .col_expr(
            crate::infra::db::entities::message::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(db_ts(base_time()))),
        )
        .filter(crate::infra::db::entities::message::Column::Id.eq(deleted.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    assert_eq!(f.svc.get(&ctx, chat.id).await.unwrap().message_count, 3);
    let page = f.svc.list(&ctx, ODataQuery::default()).await.unwrap();
    let counts: Vec<_> = page.items.iter().map(|c| (c.id, c.message_count)).collect();
    assert!(counts.contains(&(chat.id, 3)), "{counts:?}");
    assert!(counts.contains(&(empty.id, 0)), "{counts:?}");
}

#[tokio::test]
async fn update_title_ignores_other_fields_and_bumps_updated_at() {
    let f = fixture().await;
    let ctx = test_ctx();
    let old = db_ts(base_time());
    let seeded = seed_chat(&f.db, &ctx, Some("old"), old).await;

    let updated = f
        .svc
        .update_title(&ctx, seeded.id, "  Renamed  ")
        .await
        .unwrap();

    assert_eq!(updated.title.as_deref(), Some("Renamed"));
    assert_eq!(updated.model, seeded.model);
    assert_eq!(updated.is_temporary, seeded.is_temporary);
    assert_eq!(updated.created_at, seeded.created_at);
    assert!(updated.updated_at > old);
    assert_eq!(f.svc.get(&ctx, seeded.id).await.unwrap(), updated);

    for bad in ["", "  ", &"y".repeat(256)] {
        assert_eq!(
            f.svc.update_title(&ctx, seeded.id, bad).await.unwrap_err(),
            DomainError::InvalidTitle
        );
    }
    assert_eq!(
        f.svc.get(&ctx, seeded.id).await.unwrap().title.as_deref(),
        Some("Renamed")
    );
}

#[tokio::test]
async fn rename_moves_chat_to_the_top_of_the_list() {
    let f = fixture().await;
    let ctx = test_ctx();
    let first = seed_chat(&f.db, &ctx, Some("first"), db_ts(base_time())).await;
    let second = seed_chat(
        &f.db,
        &ctx,
        Some("second"),
        db_ts(base_time() + TimeDuration::seconds(1)),
    )
    .await;

    f.svc.update_title(&ctx, first.id, "renamed").await.unwrap();

    let ids: Vec<_> = f
        .svc
        .list(&ctx, ODataQuery::default())
        .await
        .unwrap()
        .items
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, [first.id, second.id]);
}

#[tokio::test]
async fn delete_sets_attachment_cleanup_pending_and_enqueues_chat_cleanup() {
    let mut f = fixture().await;
    let ctx = test_ctx();
    let old = db_ts(base_time());
    let chat = seed_chat(&f.db, &ctx, Some("doomed"), old).await;
    let live = seed_attachment(&f.db, &chat, "document", "ready", None).await;
    let done = seed_attachment(&f.db, &chat, "image", "ready", None).await;
    let gone = seed_attachment(&f.db, &chat, "document", "ready", None).await;
    let conn = f.db.conn().unwrap();
    attachment::Entity::update_many()
        .col_expr(
            attachment::Column::CleanupStatus,
            sea_orm::sea_query::Expr::value(Some("done".to_owned())),
        )
        .filter(attachment::Column::Id.eq(done.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    attachment::Entity::update_many()
        .col_expr(
            attachment::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(old)),
        )
        .filter(attachment::Column::Id.eq(gone.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    f.svc.delete(&ctx, chat.id).await.unwrap();

    let conn = f.db.conn().unwrap();
    let row = chat::Entity::find()
        .filter(chat::Column::Id.eq(chat.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    let deleted_at = row.deleted_at.expect("soft-deleted");
    assert!(deleted_at > old);
    assert_eq!(row.updated_at, deleted_at);

    let cleanup = |id: Uuid| {
        let conn = f.db.conn().unwrap();
        async move {
            attachment::Entity::find()
                .filter(attachment::Column::Id.eq(id))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .one(&conn)
                .await
                .unwrap()
                .unwrap()
                .cleanup_status
        }
    };
    assert_eq!(cleanup(live.id).await.as_deref(), Some("pending"));
    assert_eq!(cleanup(done.id).await.as_deref(), Some("done"));
    assert_eq!(cleanup(gone.id).await, None);

    let (payload_type, body) = tokio::time::timeout(Duration::from_secs(20), f.outbox_rx.recv())
        .await
        .expect("chat cleanup delivered")
        .unwrap();
    assert_eq!(payload_type, "mini-chat.chat_cleanup.v1");
    assert_eq!(body["reason"], "chat_soft_delete");
    assert_eq!(body["chat_id"], chat.id.to_string());
    assert_eq!(body["tenant_id"], ctx.subject_tenant_id().to_string());
    let system_request_id: Uuid = body["system_request_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(system_request_id, Uuid::nil());
    assert_eq!(f.svc.get(&ctx, chat.id).await.unwrap_err(), CHAT_NOT_FOUND);
}

#[tokio::test]
async fn delete_twice_is_not_found() {
    let f = fixture().await;
    let ctx = test_ctx();
    let chat = f.svc.create(&ctx, create(None, None)).await.unwrap();

    f.svc.delete(&ctx, chat.id).await.unwrap();

    assert_eq!(
        f.svc.delete(&ctx, chat.id).await.unwrap_err(),
        CHAT_NOT_FOUND
    );
    assert_eq!(
        f.svc.update_title(&ctx, chat.id, "x").await.unwrap_err(),
        CHAT_NOT_FOUND
    );
    assert!(
        f.svc
            .list(&ctx, ODataQuery::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn chat_dto_omits_absent_title() {
    let f = fixture().await;
    let ctx = test_ctx();
    let untitled = f.svc.create(&ctx, create(None, None)).await.unwrap();
    let titled = f.svc.create(&ctx, create(Some("T"), None)).await.unwrap();

    let json = serde_json::to_value(ChatDetailDto::from(untitled.clone())).unwrap();
    let keys: HashSet<_> = json.as_object().unwrap().keys().cloned().collect();
    let expected: HashSet<String> = [
        "id",
        "model",
        "is_temporary",
        "message_count",
        "created_at",
        "updated_at",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys, expected);
    assert_eq!(json["id"], untitled.id.to_string());
    assert_eq!(json["message_count"], 0);

    let json = serde_json::to_value(ChatDetailDto::from(titled)).unwrap();
    assert_eq!(json["title"], "T");
}
