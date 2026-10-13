//! `messages` table. Tenant scoped; owner isolation comes from the parent chat.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "messages")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
#[allow(clippy::struct_field_names)] // column names are fixed by DESIGN 3.7
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Option<Uuid>,
    /// `user`, `assistant` or `system`.
    pub role: String,
    pub content: String,
    /// Reserved; always `text`.
    pub content_type: String,
    /// Reserved; always 0.
    pub token_estimate: i32,
    pub provider_response_id: Option<String>,
    /// Reserved; always `chat`.
    pub request_kind: String,
    /// Reserved; always `[]`.
    pub features_used: serde_json::Value,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
    /// `effective_model` of the turn (assistant messages).
    pub model: Option<String>,
    pub is_compressed: bool,
    pub created_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
