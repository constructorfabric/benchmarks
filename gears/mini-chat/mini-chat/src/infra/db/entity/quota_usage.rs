//! `quota_usage` table: per user, period and bucket credit counters.

use sea_orm::entity::prelude::*;
use time::{Date, OffsetDateTime};
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "quota_usage")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "id",
    owner_col = "user_id",
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
    /// `total` or `tier:premium`.
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
    /// Reserved; not populated.
    pub image_inputs: i32,
    /// Reserved; not populated.
    pub image_upload_bytes: i64,
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
