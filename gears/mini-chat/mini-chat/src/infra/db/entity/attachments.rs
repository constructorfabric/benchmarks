//! `attachments` table. Tenant scoped; owner isolation comes from the parent chat.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "attachments")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
#[allow(clippy::struct_field_names)] // column names are fixed by DESIGN 3.7
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_backend: String,
    /// Internal only; never exposed through the API.
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
    pub summary_updated_at: Option<OffsetDateTime>,
    /// `pending`, `done` or `failed` (not enforced by a CHECK).
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
    pub last_cleanup_error: Option<String>,
    pub cleanup_updated_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
    pub secondary_file_id: Option<String>,
    /// `not_attempted`, `pending`, `uploaded` or `failed`.
    pub secondary_status: String,
    /// `anthropic` or NULL.
    pub secondary_provider_kind: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
