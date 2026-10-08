use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "attachments")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
#[allow(
    clippy::struct_field_names,
    reason = "names mirror the DB columns / wire contract"
)]
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
    pub provider_file_id: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub attachment_kind: String,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub doc_summary: Option<String>,
    pub img_thumbnail: Option<Vec<u8>>,
    pub img_thumbnail_width: Option<i32>,
    pub img_thumbnail_height: Option<i32>,
    pub summary_model: Option<String>,
    pub summary_updated_at: Option<OffsetDateTime>,
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
    pub last_cleanup_error: Option<String>,
    pub cleanup_updated_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
    pub secondary_file_id: Option<String>,
    pub secondary_status: String,
    pub secondary_provider_kind: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
