//! `SeaORM` entity for the `message_attachments` table (DESIGN §3.7).
//!
//! Secure ORM: tenant `tenant_id`; no resource, owner or type column (accessed
//! through the parent message/chat). Composite primary key
//! `(chat_id, message_id, attachment_id)`.

use sea_orm::entity::prelude::*;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "message_attachments")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub chat_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub message_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub attachment_id: Uuid,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
