//! `SeaORM` entity for the `thread_summaries` table (DESIGN §3.7).
//!
//! Secure ORM: tenant `tenant_id`, resource `id`, no owner, no type column
//! (accessed through the parent chat). One row per chat (`UNIQUE(chat_id)`).

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
    pub chat_id: Uuid,
    #[sea_orm(column_type = "Text", nullable)]
    pub summary_text: Option<String>,
    pub summarized_up_to_created_at: DateTimeUtc,
    pub summarized_up_to_message_id: Uuid,
    pub token_estimate: Option<i32>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
