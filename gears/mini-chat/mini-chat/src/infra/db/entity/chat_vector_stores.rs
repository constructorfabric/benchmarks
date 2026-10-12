//! `chat_vector_stores`: tenant-scoped, no resource / owner column. Every
//! query must include `tenant_id` and a chat-scoped join (DESIGN 3.7).

use sea_orm::entity::prelude::*;
use toolkit_db::secure::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "chat_vector_stores")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// NULL while the provider vector store is being created.
    pub vector_store_id: Option<String>,
    pub provider: String,
    /// Reserved, always 0.
    pub file_count: i32,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
