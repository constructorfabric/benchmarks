//! Direct database seeding for integration tests: valid rows (tenant ids, request
//! ids, timestamps) inserted without going through the services.

use chrono::{DateTime, NaiveDate, Utc};
use sea_orm::{EntityTrait, IntoActiveModel};
use toolkit_db::Db;
use toolkit_db::secure::{AccessScope, SecureEntityExt, secure_insert};
use uuid::Uuid;

use crate::domain::clock::now_utc;
use crate::domain::model::PeriodType;
use crate::infra::db::entities::{
    attachment, chat, message, message_attachment, message_reaction, quota_usage,
};

/// A message row to insert (everything else takes the column default).
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub chat_id: Uuid,
    pub role: String,
    pub content: String,
    pub request_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub is_compressed: bool,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl NewMessage {
    #[must_use]
    pub fn new(
        chat_id: Uuid,
        role: &str,
        content: &str,
        request_id: Option<Uuid>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            chat_id,
            role: role.to_owned(),
            content: content.to_owned(),
            request_id,
            created_at,
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            is_compressed: false,
            deleted_at: None,
        }
    }

    /// Model that produced the message (assistant messages).
    #[must_use]
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_owned());
        self
    }

    /// Provider-reported token counts.
    #[must_use]
    pub fn tokens(mut self, input: i64, output: i64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    /// Mark the message as covered by the thread summary (`is_compressed`).
    #[must_use]
    pub fn compressed(mut self) -> Self {
        self.is_compressed = true;
        self
    }

    /// Mark the message soft-deleted at `at`.
    #[must_use]
    pub fn deleted(mut self, at: DateTime<Utc>) -> Self {
        self.deleted_at = Some(at);
        self
    }
}

/// An attachment row to insert.
#[derive(Debug, Clone)]
pub struct NewAttachment {
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    /// `document` or `image`.
    pub kind: String,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// WebP bytes with their pixel size.
    pub thumbnail: Option<(Vec<u8>, i32, i32)>,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl NewAttachment {
    /// A ready document `filename` uploaded by `uploaded_by_user_id`.
    #[must_use]
    pub fn document(chat_id: Uuid, uploaded_by_user_id: Uuid, filename: &str) -> Self {
        Self {
            chat_id,
            uploaded_by_user_id,
            filename: filename.to_owned(),
            kind: "document".to_owned(),
            status: "ready".to_owned(),
            thumbnail: None,
            deleted_at: None,
        }
    }

    /// A ready image `filename` with an optional thumbnail.
    #[must_use]
    pub fn image(
        chat_id: Uuid,
        uploaded_by_user_id: Uuid,
        filename: &str,
        thumbnail: Option<(Vec<u8>, i32, i32)>,
    ) -> Self {
        Self {
            kind: "image".to_owned(),
            thumbnail,
            ..Self::document(chat_id, uploaded_by_user_id, filename)
        }
    }

    #[must_use]
    pub fn status(mut self, status: &str) -> Self {
        status.clone_into(&mut self.status);
        self
    }

    /// Mark the attachment soft-deleted at `at`.
    #[must_use]
    pub fn deleted(mut self, at: DateTime<Utc>) -> Self {
        self.deleted_at = Some(at);
        self
    }
}

/// Tenant of `chat_id` (the seeded rows belong to the chat's tenant).
///
/// # Panics
/// When the chat does not exist.
#[allow(clippy::expect_used)]
async fn chat_tenant(db: &Db, chat_id: Uuid) -> Uuid {
    let conn = db.conn().expect("db connection");
    chat::Entity::find_by_id(chat_id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .expect("load chat")
        .expect("seed: chat must exist")
        .tenant_id
}

/// Insert a message and return its id.
///
/// # Panics
/// When the chat does not exist or the insert fails.
#[allow(clippy::expect_used)]
pub async fn insert_message_with(db: &Db, new: NewMessage) -> Uuid {
    let id = Uuid::new_v4();
    let row = message::Model {
        id,
        tenant_id: chat_tenant(db, new.chat_id).await,
        chat_id: new.chat_id,
        request_id: new.request_id,
        role: new.role,
        content: new.content,
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: None,
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!([]),
        input_tokens: new.input_tokens,
        output_tokens: new.output_tokens,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: new.model,
        is_compressed: new.is_compressed,
        created_at: new.created_at,
        deleted_at: new.deleted_at,
    };
    let conn = db.conn().expect("db connection");
    secure_insert::<message::Entity>(row.into_active_model(), &AccessScope::allow_all(), &conn)
        .await
        .expect("insert message");
    id
}

/// Insert a message with default token counts and no model; returns its id.
///
/// # Panics
/// See [`insert_message_with`].
pub async fn insert_message(
    db: &Db,
    chat_id: Uuid,
    role: &str,
    content: &str,
    request_id: Option<Uuid>,
    created_at: DateTime<Utc>,
) -> Uuid {
    insert_message_with(
        db,
        NewMessage::new(chat_id, role, content, request_id, created_at),
    )
    .await
}

/// Insert (or overwrite) the `quota_usage` row of `(tenant, user, bucket, period)`
/// with the given spent/reserved credits; returns its id. `bucket` is `total` or
/// `tier:premium`.
///
/// # Panics
/// When the insert fails (for example a duplicate bucket row).
#[allow(clippy::expect_used)]
#[allow(clippy::too_many_arguments)]
pub async fn insert_quota_row(
    db: &Db,
    tenant_id: Uuid,
    user_id: Uuid,
    bucket: &str,
    period: PeriodType,
    period_start: NaiveDate,
    spent_credits_micro: i64,
    reserved_credits_micro: i64,
) -> Uuid {
    let id = Uuid::new_v4();
    let row = quota_usage::Model {
        id,
        tenant_id,
        user_id,
        period_type: period.as_str().to_owned(),
        period_start,
        bucket: bucket.to_owned(),
        spent_credits_micro,
        reserved_credits_micro,
        calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        file_search_calls: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        rag_retrieval_calls: 0,
        image_inputs: 0,
        image_upload_bytes: 0,
        updated_at: Some(now_utc()),
    };
    let conn = db.conn().expect("db connection");
    secure_insert::<quota_usage::Entity>(row.into_active_model(), &AccessScope::allow_all(), &conn)
        .await
        .expect("insert quota row");
    id
}

/// Insert an attachment of `new.chat_id` (the chat's tenant); returns its id.
///
/// # Panics
/// When the chat does not exist or the insert fails.
#[allow(clippy::expect_used)]
pub async fn insert_attachment(db: &Db, new: NewAttachment) -> Uuid {
    let id = Uuid::new_v4();
    let now = now_utc();
    let (thumb, width, height) = match new.thumbnail {
        Some((bytes, w, h)) => (Some(bytes), Some(w), Some(h)),
        None => (None, None, None),
    };
    let is_image = new.kind == "image";
    let row = attachment::Model {
        id,
        tenant_id: chat_tenant(db, new.chat_id).await,
        chat_id: new.chat_id,
        uploaded_by_user_id: new.uploaded_by_user_id,
        filename: new.filename,
        content_type: Some(
            if is_image {
                "image/png"
            } else {
                "application/pdf"
            }
            .to_owned(),
        ),
        size_bytes: Some(10),
        storage_backend: "openai".to_owned(),
        provider_file_id: Some("file-seed".to_owned()),
        status: new.status,
        error_code: None,
        attachment_kind: new.kind,
        for_file_search: !is_image,
        for_code_interpreter: false,
        doc_summary: None,
        img_thumbnail: thumb,
        img_thumbnail_width: width,
        img_thumbnail_height: height,
        summary_model: None,
        summary_updated_at: None,
        cleanup_status: None,
        cleanup_attempts: 0,
        last_cleanup_error: None,
        cleanup_updated_at: None,
        created_at: now,
        updated_at: now,
        deleted_at: new.deleted_at,
        secondary_file_id: None,
        secondary_status: "not_attempted".to_owned(),
        secondary_provider_kind: None,
    };
    let conn = db.conn().expect("db connection");
    secure_insert::<attachment::Entity>(row.into_active_model(), &AccessScope::allow_all(), &conn)
        .await
        .expect("insert attachment");
    id
}

/// Link `attachment_id` to `message_id` (`message_attachments` row of the chat's tenant).
///
/// # Panics
/// When the chat, message or attachment does not exist.
#[allow(clippy::expect_used)]
pub async fn link_attachment(db: &Db, chat_id: Uuid, message_id: Uuid, attachment_id: Uuid) {
    let row = message_attachment::Model {
        tenant_id: chat_tenant(db, chat_id).await,
        chat_id,
        message_id,
        attachment_id,
        created_at: now_utc(),
    };
    let conn = db.conn().expect("db connection");
    secure_insert::<message_attachment::Entity>(
        row.into_active_model(),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("link attachment");
}

/// Insert the reaction of `user_id` on `message_id` (`like` or `dislike`, tenant of
/// the message); returns its id.
///
/// # Panics
/// When the message does not exist or the insert fails.
#[allow(clippy::expect_used)]
pub async fn insert_reaction(db: &Db, message_id: Uuid, user_id: Uuid, reaction: &str) -> Uuid {
    let conn = db.conn().expect("db connection");
    let tenant_id = message::Entity::find_by_id(message_id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .expect("load message")
        .expect("seed: message must exist")
        .tenant_id;
    let id = Uuid::new_v4();
    let row = message_reaction::Model {
        id,
        message_id,
        user_id,
        tenant_id,
        reaction: reaction.to_owned(),
        created_at: now_utc(),
    };
    secure_insert::<message_reaction::Entity>(
        row.into_active_model(),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("insert reaction");
    id
}
