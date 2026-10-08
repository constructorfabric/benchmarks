//! `SeaORM` entity for the `attachments` table (DESIGN §3.7).
//!
//! Secure ORM: tenant `tenant_id`, resource `id`, no owner, no type column.
//! `status`, `attachment_kind`, `cleanup_status`, `secondary_status` and
//! `secondary_provider_kind` stay strings; the domain layer converts them.

#![allow(clippy::struct_field_names)]

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
    pub storage_backend: String,
    pub provider_file_id: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub attachment_kind: String,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    #[sea_orm(column_type = "Text", nullable)]
    pub doc_summary: Option<String>,
    pub img_thumbnail: Option<Vec<u8>>,
    pub img_thumbnail_width: Option<i32>,
    pub img_thumbnail_height: Option<i32>,
    pub summary_model: Option<String>,
    pub summary_updated_at: Option<DateTimeUtc>,
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
    #[sea_orm(column_type = "Text", nullable)]
    pub last_cleanup_error: Option<String>,
    pub cleanup_updated_at: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub deleted_at: Option<DateTimeUtc>,
    pub secondary_file_id: Option<String>,
    pub secondary_status: String,
    pub secondary_provider_kind: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
