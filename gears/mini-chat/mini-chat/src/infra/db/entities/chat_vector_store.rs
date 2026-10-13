//! `SeaORM` entity for the `chat_vector_stores` table (DESIGN §3.7).
//!
//! Secure ORM: tenant `tenant_id`; no resource, owner or type column. Always
//! query with `tenant_id` and a chat obtained from an owner-scoped chat query.
//! `vector_store_id` is NULL while provider creation is in progress.

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
    pub vector_store_id: Option<String>,
    pub provider: String,
    pub file_count: i32,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
