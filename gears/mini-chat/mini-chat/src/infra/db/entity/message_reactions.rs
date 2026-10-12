//! `message_reactions`: owner-scoped (`tenant_id`, `user_id`), resource column
//! `id`. Writers take `tenant_id` and `user_id` from the security context.

use sea_orm::entity::prelude::*;
use toolkit_db::secure::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "message_reactions")]
#[secure(
    tenant_col = "tenant_id",
    owner_col = "user_id",
    resource_col = "id",
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub message_id: Uuid,
    pub user_id: Uuid,
    pub tenant_id: Uuid,
    /// `like` or `dislike`.
    pub reaction: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
