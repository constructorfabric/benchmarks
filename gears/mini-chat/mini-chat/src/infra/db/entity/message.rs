//! `messages` table.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

// Field names are the column names (D§3.7).
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "messages")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Option<Uuid>,
    /// `user` | `assistant` | `system`.
    pub role: String,
    #[sea_orm(column_type = "Text")]
    pub content: String,
    pub content_type: String,
    pub token_estimate: i32,
    pub provider_response_id: Option<String>,
    pub request_kind: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub features_used: Json,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
    /// Effective model of the turn (assistant messages).
    pub model: Option<String>,
    pub is_compressed: bool,
    pub created_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
