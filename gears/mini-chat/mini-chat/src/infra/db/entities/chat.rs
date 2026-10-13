//! `SeaORM` entity for the `chats` table (DESIGN §3.7).
//!
//! Secure ORM: tenant `tenant_id`, owner `user_id`, resource `id`, no type column.
//! `model` is the chat's `selected_model` (immutable after creation).

#![allow(clippy::struct_field_names)]

use sea_orm::entity::prelude::*;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "chats")]
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
    pub model: Option<String>,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub deleted_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
