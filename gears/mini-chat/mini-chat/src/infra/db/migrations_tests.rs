#![allow(clippy::unwrap_used, clippy::expect_used)]

use sea_orm::{
    ActiveValue::Set, ColumnTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder,
    QuerySelect, sea_query::Expr,
};
use sea_orm_migration::MigratorTrait;
use time::{Date, Duration, OffsetDateTime, macros::datetime};
use toolkit_db::DBProvider;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{
    AccessScope, ScopeError, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::time::{db_now, db_ts};
use crate::infra::db::entities::{
    attachment, chat, chat_turn, message, message_attachment, message_reaction, quota_usage,
};
use crate::infra::db::{all_migrations, migrations::Migrator};
use crate::test_support::{test_db, test_provider};

type Provider = DBProvider<DomainError>;

async fn provider() -> std::sync::Arc<Provider> {
    test_provider().await
}

fn scope() -> AccessScope {
    AccessScope::allow_all()
}

async fn insert_chat(p: &Provider) -> Uuid {
    let conn = p.conn().unwrap();
    let now = db_now();
    let id = Uuid::new_v4();
    secure_insert::<chat::Entity>(
        chat::ActiveModel {
            id: Set(id),
            tenant_id: Set(Uuid::new_v4()),
            user_id: Set(Uuid::new_v4()),
            model: Set("gpt-5".to_owned()),
            title: Set(None),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        },
        &scope(),
        &conn,
    )
    .await
    .unwrap();
    id
}

fn turn_model(chat_id: Uuid, request_id: Uuid, state: &str) -> chat_turn::ActiveModel {
    let now = db_now();
    chat_turn::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        request_id: Set(request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(Uuid::new_v4())),
        state: Set(state.to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(None),
        max_output_tokens_applied: Set(None),
        reserved_credits_micro: Set(None),
        policy_version_applied: Set(None),
        effective_model: Set(None),
        minimal_generation_floor_applied: Set(None),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(now),
        last_progress_at: Set(Some(now)),
        web_search_enabled: Set(false),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(now),
    }
}

async fn insert_turn(
    p: &Provider,
    chat_id: Uuid,
    request_id: Uuid,
    state: &str,
) -> Result<chat_turn::Model, ScopeError> {
    let conn = p.conn().unwrap();
    secure_insert::<chat_turn::Entity>(turn_model(chat_id, request_id, state), &scope(), &conn)
        .await
}

fn message_model(
    chat_id: Uuid,
    request_id: Option<Uuid>,
    role: &str,
    deleted: bool,
) -> message::ActiveModel {
    let now = db_now();
    message::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        request_id: Set(request_id),
        role: Set(role.to_owned()),
        content: Set("hello".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(deleted.then_some(now)),
    }
}

async fn insert_message(
    p: &Provider,
    chat_id: Uuid,
    request_id: Option<Uuid>,
    role: &str,
    deleted: bool,
) -> Result<message::Model, ScopeError> {
    let conn = p.conn().unwrap();
    secure_insert::<message::Entity>(
        message_model(chat_id, request_id, role, deleted),
        &scope(),
        &conn,
    )
    .await
}

fn attachment_model(chat_id: Uuid) -> attachment::ActiveModel {
    let now = db_now();
    attachment::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(Uuid::new_v4()),
        filename: Set("a.txt".to_owned()),
        content_type: Set("text/plain".to_owned()),
        size_bytes: Set(3),
        storage_backend: Set("azure".to_owned()),
        provider_file_id: Set(None),
        status: Set("pending".to_owned()),
        error_code: Set(None),
        attachment_kind: Set("document".to_owned()),
        for_file_search: Set(true),
        for_code_interpreter: Set(false),
        doc_summary: Set(None),
        img_thumbnail: Set(None),
        img_thumbnail_width: Set(None),
        img_thumbnail_height: Set(None),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(None),
        cleanup_attempts: Set(0),
        last_cleanup_error: Set(None),
        cleanup_updated_at: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
        secondary_file_id: Set(None),
        secondary_status: Set("not_attempted".to_owned()),
        secondary_provider_kind: Set(None),
    }
}

fn quota_model(tenant: Uuid, user: Uuid, bucket: &str) -> quota_usage::ActiveModel {
    quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        user_id: Set(user),
        period_type: Set("daily".to_owned()),
        period_start: Set(Date::from_calendar_date(2026, time::Month::October, 4).unwrap()),
        bucket: Set(bucket.to_owned()),
        spent_credits_micro: Set(0),
        reserved_credits_micro: Set(0),
        calls: Set(0),
        input_tokens: Set(0),
        output_tokens: Set(0),
        file_search_calls: Set(0),
        web_search_calls: Set(0),
        code_interpreter_calls: Set(0),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(db_now()),
    }
}

#[tokio::test]
async fn migrations_apply_and_are_idempotent() {
    let db = test_db().await;
    // `test_db` already applied everything once; applying again must be a no-op.
    let again = run_migrations_for_testing(&db, all_migrations())
        .await
        .unwrap();
    assert_eq!(again.applied, 0);
    assert!(again.skipped >= 1);

    let gear: Vec<String> = Migrator::migrations()
        .iter()
        .map(|m| m.name().to_owned())
        .collect();
    assert_eq!(gear, vec!["m20261004_000001_mini_chat_initial".to_owned()]);
}

#[tokio::test]
async fn one_running_turn_per_chat() {
    let p = provider().await;
    let chat_id = insert_chat(&p).await;

    let first = insert_turn(&p, chat_id, Uuid::new_v4(), "running")
        .await
        .unwrap();
    let err = insert_turn(&p, chat_id, Uuid::new_v4(), "running")
        .await
        .unwrap_err();
    assert!(err.is_unique_violation(), "{err:?}");

    // Another chat is independent.
    let other_chat = insert_chat(&p).await;
    insert_turn(&p, other_chat, Uuid::new_v4(), "running")
        .await
        .unwrap();

    // Completing the first turn frees the slot.
    let conn = p.conn().unwrap();
    let now = db_now();
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::State, Expr::value("completed"))
        .col_expr(chat_turn::Column::CompletedAt, Expr::value(now))
        .filter(chat_turn::Column::Id.eq(first.id))
        .secure()
        .scope_with(&scope())
        .exec(&conn)
        .await
        .unwrap();
    let second = insert_turn(&p, chat_id, Uuid::new_v4(), "running")
        .await
        .unwrap();

    // Soft-deleting the running turn also frees the slot.
    let conn = p.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(db_now()))
        .filter(chat_turn::Column::Id.eq(second.id))
        .secure()
        .scope_with(&scope())
        .exec(&conn)
        .await
        .unwrap();
    insert_turn(&p, chat_id, Uuid::new_v4(), "running")
        .await
        .unwrap();
}

#[tokio::test]
async fn request_id_unique_per_chat() {
    let p = provider().await;
    let chat_id = insert_chat(&p).await;
    let request_id = Uuid::new_v4();
    insert_turn(&p, chat_id, request_id, "completed")
        .await
        .unwrap();
    let err = insert_turn(&p, chat_id, request_id, "completed")
        .await
        .unwrap_err();
    assert!(err.is_unique_violation(), "{err:?}");

    // The same request id in another chat is fine.
    let other_chat = insert_chat(&p).await;
    insert_turn(&p, other_chat, request_id, "completed")
        .await
        .unwrap();
}

#[tokio::test]
async fn message_role_request_unique_only_for_live_rows() {
    let p = provider().await;
    let chat_id = insert_chat(&p).await;
    let request_id = Uuid::new_v4();

    insert_message(&p, chat_id, Some(request_id), "user", false)
        .await
        .unwrap();
    // One assistant message per request id is allowed.
    insert_message(&p, chat_id, Some(request_id), "assistant", false)
        .await
        .unwrap();
    // A second live user message for the same request id is not.
    let err = insert_message(&p, chat_id, Some(request_id), "user", false)
        .await
        .unwrap_err();
    assert!(err.is_unique_violation(), "{err:?}");
    // Soft-deleted rows do not participate in the uniqueness.
    insert_message(&p, chat_id, Some(request_id), "user", true)
        .await
        .unwrap();
    insert_message(&p, chat_id, Some(request_id), "user", true)
        .await
        .unwrap();
    // Rows without a request id never collide.
    insert_message(&p, chat_id, None, "user", false)
        .await
        .unwrap();
    insert_message(&p, chat_id, None, "user", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn quota_usage_bucket_unique() {
    let p = provider().await;
    let conn = p.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    secure_insert::<quota_usage::Entity>(quota_model(tenant, user, "total"), &scope(), &conn)
        .await
        .unwrap();
    secure_insert::<quota_usage::Entity>(
        quota_model(tenant, user, "tier:premium"),
        &scope(),
        &conn,
    )
    .await
    .unwrap();
    let err =
        secure_insert::<quota_usage::Entity>(quota_model(tenant, user, "total"), &scope(), &conn)
            .await
            .unwrap_err();
    assert!(err.is_unique_violation(), "{err:?}");
    // Same bucket for another user is fine.
    secure_insert::<quota_usage::Entity>(
        quota_model(tenant, Uuid::new_v4(), "total"),
        &scope(),
        &conn,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn message_attachment_composite_fk_rejects_cross_chat() {
    let p = provider().await;
    let chat_a = insert_chat(&p).await;
    let chat_b = insert_chat(&p).await;
    let msg_a = insert_message(&p, chat_a, None, "user", false)
        .await
        .unwrap();
    let conn = p.conn().unwrap();
    let att_a = secure_insert::<attachment::Entity>(attachment_model(chat_a), &scope(), &conn)
        .await
        .unwrap();
    let att_b = secure_insert::<attachment::Entity>(attachment_model(chat_b), &scope(), &conn)
        .await
        .unwrap();

    let link = |chat_id: Uuid, attachment_id: Uuid| message_attachment::ActiveModel {
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        message_id: Set(msg_a.id),
        attachment_id: Set(attachment_id),
        created_at: Set(db_now()),
    };

    // Attachment belongs to another chat: composite FK (attachment_id, chat_id) fails.
    let err = secure_insert::<message_attachment::Entity>(link(chat_a, att_b.id), &scope(), &conn)
        .await
        .unwrap_err();
    assert!(err.is_foreign_key_violation(), "{err:?}");
    // Message belongs to another chat: composite FK (message_id, chat_id) fails.
    let err = secure_insert::<message_attachment::Entity>(link(chat_b, att_b.id), &scope(), &conn)
        .await
        .unwrap_err();
    assert!(err.is_foreign_key_violation(), "{err:?}");
    // Same chat succeeds.
    secure_insert::<message_attachment::Entity>(link(chat_a, att_a.id), &scope(), &conn)
        .await
        .unwrap();
}

#[derive(Debug, FromQueryResult)]
struct RawUuidCol {
    kind: String,
    len: i64,
}

#[derive(Debug, FromQueryResult)]
struct RawText {
    txt: String,
}

#[tokio::test]
async fn uuid_stored_as_16_byte_blob() {
    let p = provider().await;
    let id = insert_chat(&p).await;
    let conn = p.conn().unwrap();

    let row = chat::Entity::find()
        .filter(chat::Column::Id.eq(id))
        .secure()
        .scope_with(&scope())
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.id, id);

    let raw = chat::Entity::find()
        .filter(chat::Column::Id.eq(id))
        .secure()
        .scope_with(&scope())
        .project_all(&conn, |q| {
            q.select_only()
                .column_as(Expr::cust("typeof(id)"), "kind")
                .column_as(Expr::cust("length(id)"), "len")
                .into_model::<RawUuidCol>()
        })
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].kind, "blob");
    assert_eq!(raw[0].len, 16);

    // db_ts values round-trip exactly.
    let ts = db_ts(datetime!(2026-10-04 12:34:56.789_012_345 UTC));
    let id2 = Uuid::new_v4();
    let mut am = chat_am(id2);
    am.created_at = Set(ts);
    am.updated_at = Set(ts);
    secure_insert::<chat::Entity>(am, &scope(), &conn)
        .await
        .unwrap();
    let back = chat::Entity::find()
        .filter(chat::Column::Id.eq(id2))
        .secure()
        .scope_with(&scope())
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back.created_at, ts);
    assert_eq!(back.updated_at, ts);
}

fn chat_am(id: Uuid) -> chat::ActiveModel {
    let now = db_now();
    chat::ActiveModel {
        id: Set(id),
        tenant_id: Set(Uuid::new_v4()),
        user_id: Set(Uuid::new_v4()),
        model: Set("gpt-5".to_owned()),
        title: Set(Some("t".to_owned())),
        is_temporary: Set(false),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
    }
}

#[tokio::test]
async fn db_ts_sorts_lexicographically() {
    let p = provider().await;
    let conn = p.conn().unwrap();
    let base = datetime!(2026-10-04 12:00:00 UTC);
    // Fractions whose trimmed RFC 3339 forms would sort wrongly (".1" > ".15"
    // lexicographically, ".1Z" > ".1000001Z" ...), inserted out of order.
    let offsets_ns: [i64; 7] = [
        150_000_000,
        100_000_000,
        0,
        100_000_100, // < 1 us after .1 -> same db_ts bucket as .1
        100_001_000,
        999_999_999,
        500_000_000,
    ];
    let mut expected: Vec<OffsetDateTime> = Vec::new();
    for ns in offsets_ns {
        let ts = db_ts(base + Duration::nanoseconds(ns));
        expected.push(ts);
        let mut am = chat_am(Uuid::new_v4());
        am.created_at = Set(ts);
        secure_insert::<chat::Entity>(am, &scope(), &conn)
            .await
            .unwrap();
    }
    expected.sort();

    let rows = chat::Entity::find()
        .secure()
        .scope_with(&scope())
        .project_all(&conn, |q| {
            q.select_only()
                .column_as(Expr::cust("created_at"), "txt")
                .order_by_asc(chat::Column::CreatedAt)
                .into_model::<RawText>()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), expected.len());
    for row in &rows {
        // `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`: always 9 fractional digits.
        assert_eq!(row.txt.len(), 30, "{}", row.txt);
    }
    let texts: Vec<&str> = rows.iter().map(|r| r.txt.as_str()).collect();
    let mut sorted = texts.clone();
    sorted.sort_unstable();
    assert_eq!(texts, sorted);

    let via_entity = chat::Entity::find()
        .order_by_asc(chat::Column::CreatedAt)
        .secure()
        .scope_with(&scope())
        .all(&conn)
        .await
        .unwrap();
    let got: Vec<OffsetDateTime> = via_entity.iter().map(|m| m.created_at).collect();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn check_constraints_reject_invalid_values() {
    let p = provider().await;
    let chat_id = insert_chat(&p).await;

    let err = insert_turn(&p, chat_id, Uuid::new_v4(), "exploded")
        .await
        .unwrap_err();
    assert!(!err.is_unique_violation(), "{err:?}");
    assert!(err.to_string().to_lowercase().contains("check"), "{err:?}");

    let err = insert_message(&p, chat_id, None, "robot", false)
        .await
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("check"), "{err:?}");

    let conn = p.conn().unwrap();
    let mut am = attachment_model(chat_id);
    am.status = Set("weird".to_owned());
    let err = secure_insert::<attachment::Entity>(am, &scope(), &conn)
        .await
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("check"), "{err:?}");

    // cleanup_status has no CHECK (ADR-0010).
    let mut am = attachment_model(chat_id);
    am.cleanup_status = Set(Some("whatever".to_owned()));
    secure_insert::<attachment::Entity>(am, &scope(), &conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn reaction_unique_per_user_and_value_checked() {
    let p = provider().await;
    let chat_id = insert_chat(&p).await;
    let msg = insert_message(&p, chat_id, None, "assistant", false)
        .await
        .unwrap();
    let conn = p.conn().unwrap();
    let user = Uuid::new_v4();
    let reaction = |u: Uuid, r: &str| message_reaction::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(msg.id),
        user_id: Set(u),
        tenant_id: Set(Uuid::new_v4()),
        reaction: Set(r.to_owned()),
        created_at: Set(db_now()),
    };
    secure_insert::<message_reaction::Entity>(reaction(user, "like"), &scope(), &conn)
        .await
        .unwrap();
    let err = secure_insert::<message_reaction::Entity>(reaction(user, "dislike"), &scope(), &conn)
        .await
        .unwrap_err();
    assert!(err.is_unique_violation(), "{err:?}");
    let err =
        secure_insert::<message_reaction::Entity>(reaction(Uuid::new_v4(), "meh"), &scope(), &conn)
            .await
            .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("check"), "{err:?}");
}

#[tokio::test]
async fn natural_key_only_raw_inserts_get_updated_at_defaults() {
    use sea_orm::{ConnectionTrait, Database, Statement};

    // Raw SQL is not reachable through `Db`, so use a plain single-connection
    // in-memory SQLite connection migrated with the gear migrator.
    let conn = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&conn, None).await.unwrap();
    let backend = conn.get_database_backend();

    let (id, tenant, user) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    conn.execute_raw(Statement::from_sql_and_values(
        backend,
        "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket) \
         VALUES (?, ?, ?, 'daily', '2026-10-04', 'total')",
        [id.into(), tenant.into(), user.into()],
    ))
    .await
    .unwrap();

    #[allow(clippy::disallowed_methods)]
    let row = quota_usage::Entity::find_by_id(id)
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.tenant_id, row.user_id), (tenant, user));
    assert_eq!(row.period_type, "daily");
    assert_eq!(row.bucket, "total");
    assert_eq!(
        row.period_start,
        Date::from_calendar_date(2026, time::Month::October, 4).unwrap()
    );
    assert_eq!(
        (
            row.spent_credits_micro,
            row.reserved_credits_micro,
            row.calls
        ),
        (0, 0, 0)
    );
    assert_eq!((row.input_tokens, row.output_tokens), (0, 0));
    assert_eq!(
        (
            row.file_search_calls,
            row.web_search_calls,
            row.code_interpreter_calls
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            row.rag_retrieval_calls,
            row.image_inputs,
            row.image_upload_bytes
        ),
        (0, 0, 0)
    );
    // The default is a UTC RFC 3339 timestamp close to now.
    assert!((db_now() - row.updated_at).abs() < Duration::minutes(1));

    // The default must have the nine-digit text shape that `db_ts` writes.
    let text = conn
        .query_one_raw(Statement::from_string(
            backend,
            "SELECT updated_at AS txt FROM quota_usage".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "txt")
        .unwrap();
    assert_eq!(text.len(), 30, "{text}");
    assert!(text.ends_with(".000000001Z"), "{text}");

    // attachments.updated_at has the same default.
    let chat_id = Uuid::new_v4();
    conn.execute_raw(Statement::from_sql_and_values(
        backend,
        "INSERT INTO chats (id, tenant_id, user_id, model, created_at, updated_at) \
         VALUES (?, ?, ?, 'm', '2026-10-04T00:00:00.000000001Z', '2026-10-04T00:00:00.000000001Z')",
        [chat_id.into(), tenant.into(), user.into()],
    ))
    .await
    .unwrap();
    let att_id = Uuid::new_v4();
    conn.execute_raw(Statement::from_sql_and_values(
        backend,
        "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
         content_type, status, attachment_kind, created_at) \
         VALUES (?, ?, ?, ?, 'a.txt', 'text/plain', 'pending', 'document', \
         '2026-10-04T00:00:00.000000001Z')",
        [att_id.into(), tenant.into(), chat_id.into(), user.into()],
    ))
    .await
    .unwrap();
    #[allow(clippy::disallowed_methods)]
    let att = attachment::Entity::find_by_id(att_id)
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    assert!((db_now() - att.updated_at).abs() < Duration::minutes(1));
}
