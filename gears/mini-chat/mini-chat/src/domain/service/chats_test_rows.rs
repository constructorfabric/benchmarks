//! Direct row insertion helpers for the REST CRUD tests (tests only).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    dead_code
)]

use sea_orm::{ActiveValue, EntityTrait};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, secure_insert};
use uuid::Uuid;

use crate::api::rest::dto::CreateChatReq;
use crate::domain::service::test_support::{TestEnv, ctx};
use crate::infra::db::entity::{
    attachment, chat, chat_turn, message, message_attachment, message_reaction,
};

/// Creates a chat through the service as `user`/`tenant`.
pub async fn create_chat(env: &TestEnv, user: Uuid, tenant: Uuid, title: Option<&str>) -> Uuid {
    env.services
        .chats
        .create(
            &ctx(user, tenant),
            CreateChatReq {
                model: None,
                title: title.map(str::to_owned),
            },
        )
        .await
        .expect("create chat")
        .id
}

/// Loads a chat row regardless of scope.
pub async fn chat_row(env: &TestEnv, id: Uuid) -> chat::Model {
    let conn = env.deps.db.conn().unwrap();
    toolkit_db::secure::SecureEntityExt::secure(chat::Entity::find_by_id(id))
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("chat row")
}

/// A message row template.
pub fn message_am(
    tenant: Uuid,
    chat_id: Uuid,
    role: &str,
    request_id: Option<Uuid>,
    created_at: OffsetDateTime,
) -> message::ActiveModel {
    message::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant),
        chat_id: ActiveValue::Set(chat_id),
        request_id: ActiveValue::Set(request_id),
        role: ActiveValue::Set(role.to_owned()),
        content: ActiveValue::Set(format!("{role} content")),
        content_type: ActiveValue::Set("text".to_owned()),
        token_estimate: ActiveValue::Set(0),
        provider_response_id: ActiveValue::Set(None),
        request_kind: ActiveValue::Set("chat".to_owned()),
        features_used: ActiveValue::Set(serde_json::json!([])),
        input_tokens: ActiveValue::Set(0),
        output_tokens: ActiveValue::Set(0),
        cache_read_input_tokens: ActiveValue::Set(0),
        cache_write_input_tokens: ActiveValue::Set(0),
        reasoning_tokens: ActiveValue::Set(0),
        model: ActiveValue::Set(None),
        is_compressed: ActiveValue::Set(false),
        created_at: ActiveValue::Set(created_at),
        deleted_at: ActiveValue::Set(None),
    }
}

/// Inserts a message row.
pub async fn insert_message(env: &TestEnv, am: message::ActiveModel) -> message::Model {
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<message::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .expect("insert message")
}

/// Inserts a completed turn: user + assistant message sharing `request_id`.
/// Returns `(user_msg, assistant_msg)`.
pub async fn insert_turn_messages(
    env: &TestEnv,
    tenant: Uuid,
    chat_id: Uuid,
    base: OffsetDateTime,
) -> (message::Model, message::Model) {
    let rid = Uuid::new_v4();
    let user = insert_message(env, message_am(tenant, chat_id, "user", Some(rid), base)).await;
    let mut am = message_am(
        tenant,
        chat_id,
        "assistant",
        Some(rid),
        base + time::Duration::milliseconds(1),
    );
    am.model = ActiveValue::Set(Some("gpt-premium".to_owned()));
    am.input_tokens = ActiveValue::Set(100);
    am.output_tokens = ActiveValue::Set(50);
    let asst = insert_message(env, am).await;
    (user, asst)
}

/// Inserts a turn row.
pub async fn insert_turn(
    env: &TestEnv,
    tenant: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    state: &str,
    error_code: Option<&str>,
    assistant_message_id: Option<Uuid>,
    deleted: bool,
) -> chat_turn::Model {
    let now = OffsetDateTime::now_utc();
    let am = chat_turn::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant),
        chat_id: ActiveValue::Set(chat_id),
        request_id: ActiveValue::Set(request_id),
        requester_type: ActiveValue::Set("user".to_owned()),
        requester_user_id: ActiveValue::Set(None),
        state: ActiveValue::Set(state.to_owned()),
        provider_name: ActiveValue::Set(None),
        provider_response_id: ActiveValue::Set(None),
        assistant_message_id: ActiveValue::Set(assistant_message_id),
        error_code: ActiveValue::Set(error_code.map(str::to_owned)),
        reserve_tokens: ActiveValue::Set(None),
        max_output_tokens_applied: ActiveValue::Set(None),
        reserved_credits_micro: ActiveValue::Set(None),
        policy_version_applied: ActiveValue::Set(None),
        effective_model: ActiveValue::Set(None),
        minimal_generation_floor_applied: ActiveValue::Set(None),
        error_detail: ActiveValue::Set(None),
        deleted_at: ActiveValue::Set(deleted.then_some(now)),
        replaced_by_request_id: ActiveValue::Set(None),
        started_at: ActiveValue::Set(now),
        last_progress_at: ActiveValue::Set(None),
        web_search_enabled: ActiveValue::Set(false),
        web_search_completed_count: ActiveValue::Set(0),
        code_interpreter_completed_count: ActiveValue::Set(0),
        file_search_completed_count: ActiveValue::Set(0),
        completed_at: ActiveValue::Set(None),
        updated_at: ActiveValue::Set(now),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<chat_turn::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .expect("insert turn")
}

/// Inserts an attachment row.
pub async fn insert_attachment(
    env: &TestEnv,
    tenant: Uuid,
    chat_id: Uuid,
    kind: &str,
    status: &str,
    thumbnail: Option<Vec<u8>>,
    deleted: bool,
) -> attachment::Model {
    let now = OffsetDateTime::now_utc();
    let am = attachment::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant),
        chat_id: ActiveValue::Set(chat_id),
        uploaded_by_user_id: ActiveValue::Set(Uuid::nil()),
        filename: ActiveValue::Set(format!("{kind}.bin")),
        content_type: ActiveValue::Set("application/pdf".to_owned()),
        size_bytes: ActiveValue::Set(10),
        storage_backend: ActiveValue::Set("openai".to_owned()),
        provider_file_id: ActiveValue::Set(Some("file-x".to_owned())),
        status: ActiveValue::Set(status.to_owned()),
        error_code: ActiveValue::Set(None),
        attachment_kind: ActiveValue::Set(kind.to_owned()),
        for_file_search: ActiveValue::Set(false),
        for_code_interpreter: ActiveValue::Set(false),
        doc_summary: ActiveValue::Set(None),
        img_thumbnail: ActiveValue::Set(thumbnail),
        img_thumbnail_width: ActiveValue::Set(Some(64)),
        img_thumbnail_height: ActiveValue::Set(Some(32)),
        summary_model: ActiveValue::Set(None),
        summary_updated_at: ActiveValue::Set(None),
        cleanup_status: ActiveValue::Set(None),
        cleanup_attempts: ActiveValue::Set(0),
        last_cleanup_error: ActiveValue::Set(None),
        cleanup_updated_at: ActiveValue::Set(None),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
        deleted_at: ActiveValue::Set(deleted.then_some(now)),
        secondary_file_id: ActiveValue::Set(None),
        secondary_status: ActiveValue::Set("not_attempted".to_owned()),
        secondary_provider_kind: ActiveValue::Set(None),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<attachment::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .expect("insert attachment")
}

/// Links an attachment to a message.
pub async fn link_attachment(
    env: &TestEnv,
    tenant: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
) {
    let am = message_attachment::ActiveModel {
        tenant_id: ActiveValue::Set(tenant),
        chat_id: ActiveValue::Set(chat_id),
        message_id: ActiveValue::Set(message_id),
        attachment_id: ActiveValue::Set(attachment_id),
        created_at: ActiveValue::Set(OffsetDateTime::now_utc()),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<message_attachment::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .expect("link attachment");
}

/// All attachment rows of a chat (unscoped).
pub async fn attachment_rows(env: &TestEnv, chat_id: Uuid) -> Vec<attachment::Model> {
    use sea_orm::{ColumnTrait, QueryFilter};
    let conn = env.deps.db.conn().unwrap();
    toolkit_db::secure::SecureEntityExt::secure(
        attachment::Entity::find().filter(attachment::Column::ChatId.eq(chat_id)),
    )
    .scope_with(&AccessScope::allow_all())
    .all(&conn)
    .await
    .unwrap()
}

/// All reaction rows of a message (unscoped).
pub async fn reaction_rows(env: &TestEnv, message_id: Uuid) -> Vec<message_reaction::Model> {
    use sea_orm::{ColumnTrait, QueryFilter};
    let conn = env.deps.db.conn().unwrap();
    toolkit_db::secure::SecureEntityExt::secure(
        message_reaction::Entity::find().filter(message_reaction::Column::MessageId.eq(message_id)),
    )
    .scope_with(&AccessScope::allow_all())
    .all(&conn)
    .await
    .unwrap()
}

/// Parses an OData query string the way the REST extractor does.
pub async fn odata(
    qs: &str,
) -> Result<toolkit_odata::ODataQuery, toolkit_canonical_errors::CanonicalError> {
    let uri = if qs.is_empty() {
        "/x".to_owned()
    } else {
        format!("/x?{qs}")
    };
    let (mut parts, ()) = http::Request::builder()
        .uri(uri)
        .body(())
        .unwrap()
        .into_parts();
    toolkit::api::odata::extract_odata_query(&mut parts, &()).await
}

/// URL-encodes a query-string value (spaces, quotes, `$`).
#[must_use]
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Wire `Problem` JSON of a domain error.
#[must_use]
pub fn problem(e: crate::domain::error::DomainError) -> serde_json::Value {
    let ce = toolkit_canonical_errors::CanonicalError::from(e);
    serde_json::to_value(toolkit_canonical_errors::Problem::from(ce)).unwrap()
}

/// Wire `Problem` JSON of a canonical error.
#[must_use]
pub fn canonical_problem(ce: toolkit_canonical_errors::CanonicalError) -> serde_json::Value {
    serde_json::to_value(toolkit_canonical_errors::Problem::from(ce)).unwrap()
}

/// Test env with a custom PDP.
pub async fn env_with_pdp(
    pdp: std::sync::Arc<dyn authz_resolver_sdk::AuthZResolverApi>,
) -> TestEnv {
    TestEnv::new(crate::domain::service::test_support::TestOptions {
        pdp,
        ..Default::default()
    })
    .await
}
