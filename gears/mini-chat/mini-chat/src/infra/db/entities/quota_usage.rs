//! `SeaORM` entity for the `quota_usage` table (DESIGN §3.7).
//!
//! Secure ORM: tenant `tenant_id`, owner `user_id`, resource `id`, no type column.
//! `period_start` is a `DATE` (`chrono::NaiveDate`, `YYYY-MM-DD` on `SQLite`).
//! Timestamps are `DateTimeUtc` (`chrono::DateTime<Utc>`) on every table.

use sea_orm::entity::prelude::*;
use chrono::NaiveDate;
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
    pub period_type: String,
    pub period_start: NaiveDate,
    pub bucket: String,
    pub spent_credits_micro: i64,
    pub reserved_credits_micro: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub file_search_calls: i32,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
    pub rag_retrieval_calls: i32,
    pub image_inputs: i32,
    pub image_upload_bytes: i64,
    pub updated_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
