//! `SeaORM` entity for the `messages` table (DESIGN §3.7).
//!
//! Secure ORM: tenant `tenant_id`, resource `id`, no owner, no type column (owner
//! isolation comes from the parent chat). `role`, `content_type` and
//! `request_kind` stay strings; `features_used` is `JSONB` on `PostgreSQL` and
//! `TEXT` on `SQLite` and is mapped as `serde_json::Value` (round-trips on both).

#![allow(clippy::struct_field_names)]

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
    pub role: String,
    #[sea_orm(column_type = "Text")]
    pub content: String,
    pub content_type: String,
    pub token_estimate: i32,
    pub provider_response_id: Option<String>,
    pub request_kind: String,
    pub features_used: Json,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
    pub model: Option<String>,
    pub is_compressed: bool,
    pub created_at: DateTimeUtc,
    pub deleted_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
