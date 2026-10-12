//! `SeaORM` entities of the gear's P1 tables (DESIGN section 3.7), one module
//! per table.
//!
//! Every entity derives the Secure ORM `Scopable` with exactly the scope
//! columns of its table's "Secure ORM" line: every table is tenant-scoped by
//! `tenant_id`; `chats`, `quota_usage` and `message_reactions` add the owner
//! column `user_id`; tables with their own identifier declare the resource
//! column `id`; the `message_attachments` join table and `chat_vector_stores`
//! are tenant-scoped only (owner isolation comes from the owner-scoped parent
//! chat query). No table has a type column.
//!
//! Enum-like columns (`role`, `state`, `status`, `reaction`, ...) are plain
//! `String`s; the repositories own the value sets (the database enforces the
//! value sets that carry a CHECK). Timestamps are [`DbTimestamp`],
//! `quota_usage.period_start` is a `Date`, `features_used` is JSON and
//! `img_thumbnail` is raw bytes.
//!
//! **Timestamp storage.** PostgreSQL stores timestamps as `TIMESTAMPTZ`;
//! `SQLite` stores them as `TEXT`, which it orders and compares byte-wise. Every
//! timestamp the gear writes on `SQLite` is therefore the fixed-width UTC text
//! `YYYY-MM-DDTHH:MM:SS.ffffffZ` (always six fractional digits, still RFC 3339,
//! see [`fixed_text`]), so byte order is chronological order. A
//! [`DbTimestamp`] carries the [`TimestampForm`] of the database it is written
//! to; a value to write is built with [`TimestampForm::stamp`] from the form of
//! the connected backend ([`TimestampForm::of_backend`]), and a value read from
//! a row keeps the form of that row's backend.

#![allow(
    clippy::struct_field_names,
    reason = "field names are the table's column names (`model`, `effective_model`, ...)"
)]

use sea_orm::sea_query::{ArrayType, ColumnType, Nullable, ValueType, ValueTypeErr};
use sea_orm::{ColIdx, DbBackend, QueryResult, TryGetError, TryGetable, Value};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

/// `at` as the fixed-width UTC text `YYYY-MM-DDTHH:MM:SS.ffffffZ` stored in
/// `SQLite` timestamp columns (sub-microsecond digits are truncated, as
/// PostgreSQL does for `TIMESTAMPTZ`).
#[must_use]
pub fn fixed_text(at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.microsecond()
    )
}

/// How the timestamp columns of one database are stored and bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimestampForm {
    /// PostgreSQL `TIMESTAMPTZ`: bound as a date-time value.
    Native,
    /// `SQLite` `TEXT`: bound as [`fixed_text`].
    FixedText,
}

impl TimestampForm {
    /// The form of the timestamp columns on `backend`.
    #[must_use]
    pub fn of_backend(backend: DbBackend) -> Self {
        match backend {
            DbBackend::Sqlite => Self::FixedText,
            _ => Self::Native,
        }
    }

    /// `at` as a value to write to (or compare with) a timestamp column.
    #[must_use]
    pub const fn stamp(self, at: OffsetDateTime) -> DbTimestamp {
        DbTimestamp { at, form: self }
    }

    /// `at` as the bound value of a timestamp column.
    #[must_use]
    pub fn value(self, at: OffsetDateTime) -> Value {
        self.stamp(at).into()
    }
}

/// A timestamp column value: the instant and the [`TimestampForm`] it is
/// bound in. Equality compares the instant only.
#[derive(Debug, Clone, Copy)]
pub struct DbTimestamp {
    at: OffsetDateTime,
    form: TimestampForm,
}

impl DbTimestamp {
    /// The stored instant.
    #[must_use]
    pub const fn at(self) -> OffsetDateTime {
        self.at
    }

    /// The form the value is bound in.
    #[must_use]
    pub const fn form(self) -> TimestampForm {
        self.form
    }
}

impl PartialEq for DbTimestamp {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at
    }
}

impl Eq for DbTimestamp {}

impl PartialEq<OffsetDateTime> for DbTimestamp {
    fn eq(&self, other: &OffsetDateTime) -> bool {
        self.at == *other
    }
}

impl PartialEq<DbTimestamp> for OffsetDateTime {
    fn eq(&self, other: &DbTimestamp) -> bool {
        *self == other.at
    }
}

impl From<DbTimestamp> for OffsetDateTime {
    fn from(value: DbTimestamp) -> Self {
        value.at
    }
}

impl From<DbTimestamp> for Value {
    fn from(value: DbTimestamp) -> Self {
        match value.form {
            TimestampForm::Native => Self::TimeDateTimeWithTimeZone(Some(value.at)),
            TimestampForm::FixedText => Self::String(Some(fixed_text(value.at))),
        }
    }
}

impl TryGetable for DbTimestamp {
    fn try_get_by<I: ColIdx>(res: &QueryResult, index: I) -> Result<Self, TryGetError> {
        let at = OffsetDateTime::try_get_by(res, index)?;
        let form = if res.try_as_sqlite_row().is_some() {
            TimestampForm::FixedText
        } else {
            TimestampForm::Native
        };
        Ok(Self { at, form })
    }
}

impl ValueType for DbTimestamp {
    fn try_from(v: Value) -> Result<Self, ValueTypeErr> {
        match v {
            Value::TimeDateTimeWithTimeZone(Some(at)) => Ok(TimestampForm::Native.stamp(at)),
            Value::String(Some(text)) => OffsetDateTime::parse(&text, &Rfc3339)
                .map(|at| TimestampForm::FixedText.stamp(at))
                .map_err(|_| ValueTypeErr),
            _ => Err(ValueTypeErr),
        }
    }

    fn type_name() -> String {
        "DbTimestamp".to_owned()
    }

    fn array_type() -> ArrayType {
        ArrayType::TimeDateTimeWithTimeZone
    }

    fn column_type() -> ColumnType {
        ColumnType::TimestampWithTimeZone
    }
}

impl Nullable for DbTimestamp {
    fn null() -> Value {
        Value::TimeDateTimeWithTimeZone(None)
    }
}

/// `chats` table (DESIGN section 3.7).
pub mod chats {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "chats")]
    #[secure(
        tenant_col = "tenant_id",
        owner_col = "user_id",
        resource_col = "id",
        no_type
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub user_id: Uuid,
        /// Selected model, immutable for the chat lifetime.
        pub model: String,
        pub title: Option<String>,
        pub is_temporary: bool,
        pub created_at: DbTimestamp,
        pub updated_at: DbTimestamp,
        pub deleted_at: Option<DbTimestamp>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(has_many = "super::messages::Entity")]
        Messages,
        #[sea_orm(has_many = "super::chat_turns::Entity")]
        ChatTurns,
        #[sea_orm(has_many = "super::attachments::Entity")]
        Attachments,
        #[sea_orm(has_one = "super::thread_summaries::Entity")]
        ThreadSummary,
    }

    impl Related<super::messages::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Messages.def()
        }
    }

    impl Related<super::chat_turns::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::ChatTurns.def()
        }
    }

    impl Related<super::attachments::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Attachments.def()
        }
    }

    impl Related<super::thread_summaries::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::ThreadSummary.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// `messages` table (DESIGN section 3.7).
pub mod messages {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "messages")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        pub request_id: Option<Uuid>,
        /// `user`, `assistant` or `system`.
        pub role: String,
        pub content: String,
        /// Always `text` at P1.
        pub content_type: String,
        /// Reserved; always 0.
        pub token_estimate: i32,
        pub provider_response_id: Option<String>,
        /// Reserved; always `chat` at P1.
        pub request_kind: String,
        /// Reserved; always `[]`.
        #[sea_orm(column_type = "JsonBinary")]
        pub features_used: Json,
        pub input_tokens: i64,
        pub output_tokens: i64,
        pub cache_read_input_tokens: i64,
        pub cache_write_input_tokens: i64,
        pub reasoning_tokens: i64,
        /// Effective model of an assistant message.
        pub model: Option<String>,
        pub is_compressed: bool,
        pub created_at: DbTimestamp,
        pub deleted_at: Option<DbTimestamp>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::chats::Entity",
            from = "Column::ChatId",
            to = "super::chats::Column::Id",
            on_delete = "Cascade"
        )]
        Chat,
        #[sea_orm(has_many = "super::message_attachments::Entity")]
        MessageAttachments,
        #[sea_orm(has_many = "super::message_reactions::Entity")]
        MessageReactions,
    }

    impl Related<super::chats::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Chat.def()
        }
    }

    impl Related<super::message_attachments::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::MessageAttachments.def()
        }
    }

    impl Related<super::message_reactions::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::MessageReactions.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// `chat_turns` table (DESIGN section 3.7).
pub mod chat_turns {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "chat_turns")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        pub request_id: Uuid,
        /// `user` or `system`.
        pub requester_type: String,
        pub requester_user_id: Option<Uuid>,
        /// `running`, `completed`, `failed` or `cancelled`.
        pub state: String,
        /// Reserved; always NULL.
        pub provider_name: Option<String>,
        pub provider_response_id: Option<String>,
        pub assistant_message_id: Option<Uuid>,
        pub error_code: Option<String>,
        pub reserve_tokens: Option<i64>,
        pub max_output_tokens_applied: Option<i32>,
        pub reserved_credits_micro: Option<i64>,
        pub policy_version_applied: Option<i64>,
        pub effective_model: Option<String>,
        pub minimal_generation_floor_applied: Option<i32>,
        pub error_detail: Option<String>,
        pub deleted_at: Option<DbTimestamp>,
        pub replaced_by_request_id: Option<Uuid>,
        pub started_at: DbTimestamp,
        pub last_progress_at: Option<DbTimestamp>,
        pub web_search_enabled: bool,
        pub web_search_completed_count: i32,
        pub code_interpreter_completed_count: i32,
        pub file_search_completed_count: i32,
        pub completed_at: Option<DbTimestamp>,
        pub updated_at: DbTimestamp,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::chats::Entity",
            from = "Column::ChatId",
            to = "super::chats::Column::Id",
            on_delete = "Cascade"
        )]
        Chat,
    }

    impl Related<super::chats::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Chat.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// `attachments` table (DESIGN section 3.7).
pub mod attachments {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "attachments")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        pub uploaded_by_user_id: Uuid,
        pub filename: String,
        pub content_type: Option<String>,
        pub size_bytes: Option<i64>,
        /// Internal storage routing label; never exposed.
        pub storage_backend: String,
        /// Provider file id; internal only, never exposed.
        pub provider_file_id: Option<String>,
        /// `pending`, `uploaded`, `ready` or `failed`.
        pub status: String,
        pub error_code: Option<String>,
        /// `document` or `image`.
        pub attachment_kind: String,
        pub for_file_search: bool,
        pub for_code_interpreter: bool,
        /// Reserved; never populated.
        pub doc_summary: Option<String>,
        pub img_thumbnail: Option<Vec<u8>>,
        pub img_thumbnail_width: Option<i32>,
        pub img_thumbnail_height: Option<i32>,
        /// Reserved; never populated.
        pub summary_model: Option<String>,
        /// Reserved; never populated.
        pub summary_updated_at: Option<DbTimestamp>,
        /// `pending`, `done` or `failed` (no CHECK, ADR-0010).
        pub cleanup_status: Option<String>,
        pub cleanup_attempts: i32,
        pub last_cleanup_error: Option<String>,
        pub cleanup_updated_at: Option<DbTimestamp>,
        pub created_at: DbTimestamp,
        pub updated_at: DbTimestamp,
        pub deleted_at: Option<DbTimestamp>,
        /// Anthropic Files API copy; internal only.
        pub secondary_file_id: Option<String>,
        /// `not_attempted`, `pending`, `uploaded` or `failed`.
        pub secondary_status: String,
        /// `anthropic` or NULL.
        pub secondary_provider_kind: Option<String>,
        /// Provider entry the secondary copy was uploaded with (its delete
        /// resolves the upstream alias from this entry); NULL without an
        /// attempt and on rows written before the column existed.
        pub secondary_provider_id: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::chats::Entity",
            from = "Column::ChatId",
            to = "super::chats::Column::Id",
            on_delete = "Cascade"
        )]
        Chat,
        #[sea_orm(has_many = "super::message_attachments::Entity")]
        MessageAttachments,
    }

    impl Related<super::chats::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Chat.def()
        }
    }

    impl Related<super::message_attachments::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::MessageAttachments.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// `message_attachments` table (DESIGN section 3.7).
///
/// `chat_id` is copied from the parent message; the composite foreign keys
/// `(message_id, chat_id)` and `(attachment_id, chat_id)` keep both rows in
/// the same chat.
pub mod message_attachments {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "message_attachments")]
    #[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
    pub struct Model {
        pub tenant_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub chat_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub message_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub attachment_id: Uuid,
        pub created_at: DbTimestamp,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::messages::Entity",
            from = "(Column::MessageId, Column::ChatId)",
            to = "(super::messages::Column::Id, super::messages::Column::ChatId)",
            on_delete = "Cascade"
        )]
        Message,
        #[sea_orm(
            belongs_to = "super::attachments::Entity",
            from = "(Column::AttachmentId, Column::ChatId)",
            to = "(super::attachments::Column::Id, super::attachments::Column::ChatId)",
            on_delete = "Cascade"
        )]
        Attachment,
    }

    impl Related<super::messages::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Message.def()
        }
    }

    impl Related<super::attachments::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Attachment.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// `thread_summaries` table (DESIGN section 3.7).
pub mod thread_summaries {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "thread_summaries")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        #[sea_orm(unique)]
        pub chat_id: Uuid,
        pub summary_text: String,
        /// Inclusive frontier, `created_at` component.
        pub summarized_up_to_created_at: DbTimestamp,
        /// Inclusive frontier, `id` component.
        pub summarized_up_to_message_id: Uuid,
        pub token_estimate: i32,
        pub created_at: DbTimestamp,
        pub updated_at: DbTimestamp,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::chats::Entity",
            from = "Column::ChatId",
            to = "super::chats::Column::Id",
            on_delete = "Cascade"
        )]
        Chat,
    }

    impl Related<super::chats::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Chat.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// `chat_vector_stores` table (DESIGN section 3.7).
///
/// Accessed only through chat-scoped repository methods filtering on
/// `(tenant_id, chat_id)`; never by `id` or `vector_store_id` alone.
pub mod chat_vector_stores {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "chat_vector_stores")]
    #[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        /// NULL while provider creation is in progress.
        pub vector_store_id: Option<String>,
        /// Storage backend label of the RAG provider.
        pub provider: String,
        /// Reserved; always 0.
        pub file_count: i32,
        pub created_at: DbTimestamp,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `quota_usage` table (DESIGN section 3.7).
pub mod quota_usage {
    use sea_orm::entity::prelude::*;
    use time::Date;

    use super::DbTimestamp;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "quota_usage")]
    #[secure(
        tenant_col = "tenant_id",
        owner_col = "user_id",
        resource_col = "id",
        no_type
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub user_id: Uuid,
        /// `daily` or `monthly`.
        pub period_type: String,
        pub period_start: Date,
        /// `total` or `tier:premium` (`tier:standard` analytics only).
        pub bucket: String,
        pub spent_credits_micro: i64,
        pub reserved_credits_micro: i64,
        pub calls: i32,
        pub input_tokens: i64,
        pub output_tokens: i64,
        /// Reserved; not populated.
        pub file_search_calls: i32,
        pub web_search_calls: i32,
        pub code_interpreter_calls: i32,
        pub rag_retrieval_calls: i32,
        /// Reserved; stays 0.
        pub image_inputs: i32,
        /// Reserved; stays 0.
        pub image_upload_bytes: i64,
        pub updated_at: DbTimestamp,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `message_reactions` table (DESIGN section 3.7).
pub mod message_reactions {
    use super::DbTimestamp;
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "message_reactions")]
    #[secure(
        tenant_col = "tenant_id",
        owner_col = "user_id",
        resource_col = "id",
        no_type
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub message_id: Uuid,
        pub user_id: Uuid,
        pub tenant_id: Uuid,
        /// `like` or `dislike`.
        pub reaction: String,
        pub created_at: DbTimestamp,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::messages::Entity",
            from = "Column::MessageId",
            to = "super::messages::Column::Id",
            on_delete = "Cascade"
        )]
        Message,
    }

    impl Related<super::messages::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Message.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}
