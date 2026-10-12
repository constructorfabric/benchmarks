//! `chats`: owner-scoped (`tenant_id`, `user_id`), resource column `id`.

use sea_orm::entity::prelude::*;
use toolkit_db::secure::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "chats")]
#[secure(
    tenant_col = "tenant_id",
    owner_col = "user_id",
    resource_col = "id",
    no_type
)]
#[allow(
    clippy::struct_field_names,
    reason = "column names are fixed by the DESIGN 3.7 schema"
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// `selected_model`, immutable for the chat lifetime.
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub deleted_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
