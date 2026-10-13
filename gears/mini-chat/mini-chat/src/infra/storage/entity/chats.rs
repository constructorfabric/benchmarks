//! `chats`: tenant column `tenant_id`, owner column `user_id`, resource column `id`.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[allow(
    clippy::struct_field_names,
    reason = "field names are the column names of the schema"
)]
#[sea_orm(table_name = "chats")]
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
    /// The selected model; immutable for the lifetime of the chat.
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
