//! `thread_summaries`: tenant-scoped, resource column `id`; accessed through
//! the parent chat.

use sea_orm::entity::prelude::*;
use toolkit_db::secure::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "thread_summaries")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub summary_text: Option<String>,
    /// Inclusive frontier, `created_at` component.
    pub summarized_up_to_created_at: DateTimeUtc,
    /// Inclusive frontier, message id component.
    pub summarized_up_to_message_id: Uuid,
    pub token_estimate: Option<i32>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
