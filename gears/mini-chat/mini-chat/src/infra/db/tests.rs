//! Schema and entity tests against in-memory SQLite.
#![allow(clippy::inconsistent_struct_constructor)] // fixtures list the interesting fields first

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::NotSet, ColumnTrait, EntityTrait, QueryFilter, Set};
use time::{Date, Month, OffsetDateTime};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, Outbox, OutboxMessage, Partitions};
use toolkit_db::secure::{
    AccessScope, ScopeError, SecureDeleteExt, SecureEntityExt, SecureUpdateExt,
    is_unique_violation, secure_insert,
};
use uuid::Uuid;

use super::entity::{
    attachments, chat_turns, chat_vector_stores, chats, message_attachments, message_reactions,
    messages, quota_usage, thread_summaries,
};
use super::{AttachmentKind, AttachmentStatus, CleanupStatus, MessageRole, TurnState};
use crate::test_support::db::test_db;

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000 + secs).unwrap()
}

fn scope() -> AccessScope {
    AccessScope::allow_all()
}

async fn insert<E>(db: &toolkit_db::Db, am: E::ActiveModel) -> Result<E::Model, ScopeError>
where
    E: toolkit_db::secure::ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
    E::ActiveModel: sea_orm::ActiveModelTrait<Entity = E> + Send,
    E::Model: sea_orm::IntoActiveModel<E::ActiveModel>,
{
    let conn = db.conn().expect("conn");
    secure_insert::<E>(am, &scope(), &conn).await
}

fn chat(tenant: Uuid) -> chats::ActiveModel {
    chats::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        user_id: Set(Uuid::new_v4()),
        model: Set("gpt-5".into()),
        title: Set(Some("hello".into())),
        is_temporary: NotSet,
        created_at: Set(ts(0)),
        updated_at: Set(ts(1)),
        deleted_at: Set(None),
    }
}

async fn make_chat(db: &toolkit_db::Db) -> chats::Model {
    insert::<chats::Entity>(db, chat(Uuid::new_v4()))
        .await
        .unwrap()
}

fn turn(chat: &chats::Model, state: TurnState) -> chat_turns::ActiveModel {
    chat_turns::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(Uuid::new_v4()),
        requester_type: Set("user".into()),
        requester_user_id: Set(Some(chat.user_id)),
        state: Set(state.as_str().into()),
        started_at: Set(ts(2)),
        last_progress_at: Set(Some(ts(2))),
        provider_name: NotSet,
        provider_response_id: NotSet,
        assistant_message_id: NotSet,
        error_code: NotSet,
        reserve_tokens: NotSet,
        max_output_tokens_applied: NotSet,
        reserved_credits_micro: NotSet,
        policy_version_applied: NotSet,
        effective_model: NotSet,
        minimal_generation_floor_applied: NotSet,
        error_detail: NotSet,
        deleted_at: NotSet,
        replaced_by_request_id: NotSet,
        web_search_enabled: NotSet,
        web_search_completed_count: NotSet,
        code_interpreter_completed_count: NotSet,
        file_search_completed_count: NotSet,
        completed_at: NotSet,
        updated_at: NotSet,
    }
}

fn message(
    chat: &chats::Model,
    request_id: Option<Uuid>,
    role: MessageRole,
) -> messages::ActiveModel {
    messages::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(request_id),
        role: Set(role.as_str().into()),
        content: Set("hi".into()),
        created_at: Set(ts(3)),
        content_type: NotSet,
        token_estimate: NotSet,
        provider_response_id: NotSet,
        request_kind: NotSet,
        features_used: NotSet,
        input_tokens: NotSet,
        output_tokens: NotSet,
        cache_read_input_tokens: NotSet,
        cache_write_input_tokens: NotSet,
        reasoning_tokens: NotSet,
        model: NotSet,
        is_compressed: NotSet,
        deleted_at: NotSet,
    }
}

fn attachment(chat: &chats::Model) -> attachments::ActiveModel {
    attachments::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        uploaded_by_user_id: Set(chat.user_id),
        filename: Set("a.png".into()),
        content_type: Set("image/png".into()),
        size_bytes: Set(42),
        status: Set(AttachmentStatus::Pending.as_str().into()),
        attachment_kind: Set(AttachmentKind::Image.as_str().into()),
        img_thumbnail: Set(Some(vec![0, 1, 2, 255])),
        img_thumbnail_width: Set(Some(10)),
        created_at: Set(ts(4)),
        storage_backend: NotSet,
        provider_file_id: NotSet,
        error_code: NotSet,
        for_file_search: NotSet,
        for_code_interpreter: NotSet,
        doc_summary: NotSet,
        img_thumbnail_height: NotSet,
        summary_model: NotSet,
        summary_updated_at: NotSet,
        cleanup_status: NotSet,
        cleanup_attempts: NotSet,
        last_cleanup_error: NotSet,
        cleanup_updated_at: NotSet,
        updated_at: NotSet,
        deleted_at: NotSet,
        secondary_file_id: NotSet,
        secondary_status: NotSet,
        secondary_provider_kind: NotSet,
    }
}

fn quota(tenant: Uuid, user: Uuid, bucket: &str) -> quota_usage::ActiveModel {
    quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        user_id: Set(user),
        period_type: Set("daily".into()),
        period_start: Set(Date::from_calendar_date(2026, Month::October, 4).unwrap()),
        bucket: Set(bucket.into()),
        spent_credits_micro: NotSet,
        reserved_credits_micro: NotSet,
        calls: NotSet,
        input_tokens: NotSet,
        output_tokens: NotSet,
        file_search_calls: NotSet,
        web_search_calls: NotSet,
        code_interpreter_calls: NotSet,
        rag_retrieval_calls: NotSet,
        image_inputs: NotSet,
        image_upload_bytes: NotSet,
        updated_at: NotSet,
    }
}

fn is_unique(err: &ScopeError) -> bool {
    matches!(err, ScopeError::Db(e) if is_unique_violation(e))
}

#[tokio::test]
async fn migrations_apply_on_sqlite() {
    let db = test_db().await;
    let conn = db.conn().unwrap();

    let chat = make_chat(&db).await;
    assert_eq!(chat.title.as_deref(), Some("hello"));
    assert!(!chat.is_temporary);
    assert_eq!(chat.updated_at, ts(1));

    let msg =
        insert::<messages::Entity>(&db, message(&chat, Some(Uuid::new_v4()), MessageRole::User))
            .await
            .unwrap();
    assert_eq!(msg.content_type, "text");
    assert_eq!(msg.request_kind, "chat");
    assert_eq!(msg.features_used, serde_json::json!([]));
    assert_eq!(
        (msg.token_estimate, msg.input_tokens, msg.reasoning_tokens),
        (0, 0, 0)
    );
    assert!(!msg.is_compressed);

    let mut am = message(&chat, None, MessageRole::Assistant);
    am.features_used = Set(serde_json::json!(["a", {"b": 1}]));
    let amsg = insert::<messages::Entity>(&db, am).await.unwrap();
    assert_eq!(amsg.features_used, serde_json::json!(["a", {"b": 1}]));

    let t = insert::<chat_turns::Entity>(&db, turn(&chat, TurnState::Running))
        .await
        .unwrap();
    assert!(!t.web_search_enabled);
    assert_eq!(
        (
            t.web_search_completed_count,
            t.code_interpreter_completed_count,
            t.file_search_completed_count
        ),
        (0, 0, 0)
    );
    assert_eq!(t.last_progress_at, Some(ts(2)));
    assert!(t.reserve_tokens.is_none());

    let att = insert::<attachments::Entity>(&db, attachment(&chat))
        .await
        .unwrap();
    assert_eq!(att.storage_backend, "azure");
    assert_eq!(att.secondary_status, "not_attempted");
    assert_eq!(att.cleanup_attempts, 0);
    assert!(!att.for_file_search && !att.for_code_interpreter);
    assert_eq!(att.img_thumbnail.as_deref(), Some(&[0u8, 1, 2, 255][..]));
    assert_eq!(att.cleanup_status, None);

    let link = insert::<message_attachments::Entity>(
        &db,
        message_attachments::ActiveModel {
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            message_id: Set(msg.id),
            attachment_id: Set(att.id),
            created_at: Set(ts(5)),
        },
    )
    .await
    .unwrap();
    assert_eq!((link.message_id, link.attachment_id), (msg.id, att.id));

    let summary = insert::<thread_summaries::Entity>(
        &db,
        thread_summaries::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            summary_text: Set("sum".into()),
            summarized_up_to_created_at: Set(ts(3)),
            summarized_up_to_message_id: Set(msg.id),
            token_estimate: Set(7),
            created_at: Set(ts(6)),
            updated_at: Set(ts(6)),
        },
    )
    .await
    .unwrap();
    assert_eq!(summary.summarized_up_to_message_id, msg.id);

    let store = insert::<chat_vector_stores::Entity>(
        &db,
        chat_vector_stores::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            vector_store_id: Set(None),
            provider: Set("azure".into()),
            file_count: NotSet,
            created_at: Set(ts(7)),
        },
    )
    .await
    .unwrap();
    assert_eq!((store.file_count, store.vector_store_id), (0, None));

    let q = insert::<quota_usage::Entity>(&db, quota(chat.tenant_id, chat.user_id, "total"))
        .await
        .unwrap();
    assert_eq!(
        q.period_start,
        Date::from_calendar_date(2026, Month::October, 4).unwrap()
    );
    assert_eq!(
        (
            q.spent_credits_micro,
            q.reserved_credits_micro,
            q.calls,
            q.web_search_calls
        ),
        (0, 0, 0, 0)
    );

    let reaction = insert::<message_reactions::Entity>(
        &db,
        message_reactions::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(chat.tenant_id),
            message_id: Set(msg.id),
            user_id: Set(chat.user_id),
            reaction: Set("like".into()),
            created_at: Set(ts(8)),
        },
    )
    .await
    .unwrap();
    assert_eq!(reaction.reaction, "like");

    // Read back through the secure select.
    let found = chats::Entity::find()
        .secure()
        .scope_with(&scope())
        .and_id(chat.id)
        .unwrap()
        .one(&conn)
        .await
        .unwrap();
    assert_eq!(found, Some(chat));
}

#[tokio::test]
async fn running_turn_unique_per_chat() {
    let db = test_db().await;
    let chat = make_chat(&db).await;

    let first = insert::<chat_turns::Entity>(&db, turn(&chat, TurnState::Running))
        .await
        .unwrap();
    let err = insert::<chat_turns::Entity>(&db, turn(&chat, TurnState::Running))
        .await
        .unwrap_err();
    assert!(is_unique(&err), "{err:?}");

    // A different chat is unaffected.
    let other = make_chat(&db).await;
    insert::<chat_turns::Entity>(&db, turn(&other, TurnState::Running))
        .await
        .unwrap();

    // Completing the first lets a new running turn start.
    let conn = db.conn().unwrap();
    chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::State, Expr::value("completed"))
        .filter(chat_turns::Column::Id.eq(first.id))
        .secure()
        .scope_with(&scope())
        .exec(&conn)
        .await
        .unwrap();
    let second = insert::<chat_turns::Entity>(&db, turn(&chat, TurnState::Running))
        .await
        .unwrap();

    // A soft-deleted running row does not block.
    let conn = db.conn().unwrap();
    chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(ts(9)))
        .filter(chat_turns::Column::Id.eq(second.id))
        .secure()
        .scope_with(&scope())
        .exec(&conn)
        .await
        .unwrap();
    insert::<chat_turns::Entity>(&db, turn(&chat, TurnState::Running))
        .await
        .unwrap();
}

#[tokio::test]
async fn turn_request_id_unique_per_chat() {
    let db = test_db().await;
    let chat = make_chat(&db).await;

    let mut first = turn(&chat, TurnState::Completed);
    first.deleted_at = Set(Some(ts(9)));
    let request_id = first.request_id.clone().unwrap();
    insert::<chat_turns::Entity>(&db, first).await.unwrap();

    let mut dup = turn(&chat, TurnState::Completed);
    dup.request_id = Set(request_id);
    let err = insert::<chat_turns::Entity>(&db, dup).await.unwrap_err();
    assert!(is_unique(&err), "{err:?}");

    // The same request id in another chat is fine.
    let other = make_chat(&db).await;
    let mut ok = turn(&other, TurnState::Completed);
    ok.request_id = Set(request_id);
    insert::<chat_turns::Entity>(&db, ok).await.unwrap();
}

#[tokio::test]
async fn message_request_role_unique() {
    let db = test_db().await;
    let chat = make_chat(&db).await;
    let request_id = Uuid::new_v4();

    insert::<messages::Entity>(&db, message(&chat, Some(request_id), MessageRole::User))
        .await
        .unwrap();
    let err = insert::<messages::Entity>(&db, message(&chat, Some(request_id), MessageRole::User))
        .await
        .unwrap_err();
    assert!(is_unique(&err), "{err:?}");

    insert::<messages::Entity>(
        &db,
        message(&chat, Some(request_id), MessageRole::Assistant),
    )
    .await
    .unwrap();

    // NULL request ids are never constrained.
    insert::<messages::Entity>(&db, message(&chat, None, MessageRole::User))
        .await
        .unwrap();
    insert::<messages::Entity>(&db, message(&chat, None, MessageRole::User))
        .await
        .unwrap();
}

#[tokio::test]
async fn quota_usage_bucket_unique() {
    let db = test_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());

    insert::<quota_usage::Entity>(&db, quota(tenant, user, "total"))
        .await
        .unwrap();
    let err = insert::<quota_usage::Entity>(&db, quota(tenant, user, "total"))
        .await
        .unwrap_err();
    assert!(is_unique(&err), "{err:?}");

    insert::<quota_usage::Entity>(&db, quota(tenant, user, "tier:premium"))
        .await
        .unwrap();
}

#[tokio::test]
async fn foreign_keys_and_checks_are_enforced() {
    let db = test_db().await;
    let chat = make_chat(&db).await;

    // Unknown chat -> FK violation.
    let mut orphan = message(&chat, None, MessageRole::User);
    orphan.chat_id = Set(Uuid::new_v4());
    assert!(insert::<messages::Entity>(&db, orphan).await.is_err());

    // Invalid state / reaction values -> CHECK violation.
    let mut bad = turn(&chat, TurnState::Running);
    bad.state = Set("bogus".into());
    assert!(insert::<chat_turns::Entity>(&db, bad).await.is_err());

    // Attachment link must stay inside one chat (composite FK).
    let other = make_chat(&db).await;
    let msg = insert::<messages::Entity>(&db, message(&chat, None, MessageRole::User))
        .await
        .unwrap();
    let att = insert::<attachments::Entity>(&db, attachment(&other))
        .await
        .unwrap();
    let cross = message_attachments::ActiveModel {
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        message_id: Set(msg.id),
        attachment_id: Set(att.id),
        created_at: Set(ts(5)),
    };
    assert!(
        insert::<message_attachments::Entity>(&db, cross)
            .await
            .is_err()
    );

    // Deleting the chat cascades to its messages.
    let conn = db.conn().unwrap();
    chats::Entity::delete_many()
        .filter(chats::Column::Id.eq(chat.id))
        .secure()
        .scope_with(&scope())
        .exec(&conn)
        .await
        .unwrap();
    let left = messages::Entity::find()
        .secure()
        .scope_with(&scope())
        .all(&conn)
        .await
        .unwrap();
    assert!(left.is_empty());
}

struct Noop;

#[async_trait::async_trait]
impl LeasedMessageHandler for Noop {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Ok
    }
}

#[tokio::test]
async fn outbox_tables_exist() {
    let db = test_db().await;
    let handle = Outbox::builder(db.clone())
        .queue("mini-chat.audit", Partitions::of(4))
        .leased(Noop)
        .start()
        .await
        .expect("outbox starts against the migrated DB");
    handle.stop().await;
}

#[test]
fn text_enums_round_trip() {
    assert_eq!(TurnState::Running.as_str(), "running");
    assert_eq!(TurnState::parse("cancelled"), Some(TurnState::Cancelled));
    assert_eq!(TurnState::parse("done"), None);
    assert_eq!(
        AttachmentStatus::parse("uploaded"),
        Some(AttachmentStatus::Uploaded)
    );
    assert_eq!(AttachmentKind::Image.as_str(), "image");
    assert_eq!(MessageRole::parse("system"), Some(MessageRole::System));
    assert_eq!(CleanupStatus::parse("done"), Some(CleanupStatus::Done));
    assert_eq!(CleanupStatus::Failed.to_string(), "failed");
}

/// Rows that rely on the `updated_at` default must compare correctly against RFC 3339 cutoffs
/// bound by the entity layer (the upload reaper scan, externally seeded rows).
#[tokio::test]
async fn default_updated_at_compares_against_rfc3339_cutoffs() {
    let db = test_db().await;
    let chat = make_chat(&db).await;
    let conn = db.conn().unwrap();
    let now = super::ts::db_now();
    let (past, future) = (
        now - time::Duration::hours(1),
        now + time::Duration::hours(1),
    );

    let att = insert::<attachments::Entity>(&db, attachment(&chat))
        .await
        .unwrap();
    let turn = insert::<chat_turns::Entity>(&db, turn(&chat, TurnState::Running))
        .await
        .unwrap();
    let quota = insert::<quota_usage::Entity>(&db, quota(chat.tenant_id, chat.user_id, "total"))
        .await
        .unwrap();

    // Decodes through the entity and lies within the test's time window.
    for stamp in [att.updated_at, turn.updated_at, quota.updated_at] {
        assert!(stamp > past && stamp < future, "{stamp}");
        // Same nine-digit shape as `ts::normalize`d values: milliseconds + 1 ns.
        assert_eq!(stamp.nanosecond() % 1_000_000, 1, "{stamp}");
    }

    macro_rules! count {
        ($entity:ident, $col:expr, $op:ident, $bound:expr) => {
            $entity::Entity::find()
                .filter($col.$op($bound))
                .secure()
                .scope_with(&scope())
                .count(&conn)
                .await
                .unwrap()
        };
    }
    assert_eq!(
        count!(attachments, attachments::Column::UpdatedAt, lt, past),
        0
    );
    assert_eq!(
        count!(attachments, attachments::Column::UpdatedAt, gt, past),
        1
    );
    assert_eq!(
        count!(attachments, attachments::Column::UpdatedAt, lt, future),
        1
    );
    assert_eq!(
        count!(chat_turns, chat_turns::Column::UpdatedAt, lt, past),
        0
    );
    assert_eq!(
        count!(chat_turns, chat_turns::Column::UpdatedAt, gt, past),
        1
    );
    assert_eq!(
        count!(quota_usage, quota_usage::Column::UpdatedAt, lt, past),
        0
    );
    assert_eq!(
        count!(quota_usage, quota_usage::Column::UpdatedAt, gt, past),
        1
    );
}

/// A row stamped by the column default and rows written with `ts::normalize`d values order
/// consistently as text, whatever the fractional digits (`.5`, `.55`, `.05`, ...).
#[tokio::test]
async fn default_stamped_rows_sort_consistently_with_written_rows() {
    let db = test_db().await;
    let chat = make_chat(&db).await;
    let conn = db.conn().unwrap();
    let att = insert::<attachments::Entity>(&db, attachment(&chat))
        .await
        .unwrap();
    let stamped = att.updated_at;

    for delta_ms in [-999, -550, -500, -50, -5, -1, 1, 5, 50, 500, 550, 999] {
        let cutoff = super::ts::normalize(stamped + time::Duration::milliseconds(delta_ms));
        let older = attachments::Entity::find()
            .filter(attachments::Column::UpdatedAt.gt(cutoff))
            .secure()
            .scope_with(&scope())
            .count(&conn)
            .await
            .unwrap();
        // The default row is later than the cutoff exactly when the cutoff is earlier.
        assert_eq!(
            older,
            u64::from(delta_ms < 0),
            "cutoff {delta_ms} ms: {stamped} vs {cutoff}"
        );
    }
}
