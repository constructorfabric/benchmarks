//! `OData` filter fields and column mappings of the list endpoints.

use toolkit_db::odata::{FieldToColumn, ODataFieldMapping};
use toolkit_odata_macros::ODataFilterable;

use crate::infra::db::entities::{chat, message};

/// `OData` fields of `GET /v1/chats`.
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct ChatQuery {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: time::OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub id: uuid::Uuid,
    #[odata(filter(kind = "String"))]
    pub title: String,
}

pub struct ChatMapper;

impl FieldToColumn<ChatQueryFilterField> for ChatMapper {
    type Column = chat::Column;

    fn map_field(f: ChatQueryFilterField) -> chat::Column {
        match f {
            ChatQueryFilterField::UpdatedAt => chat::Column::UpdatedAt,
            ChatQueryFilterField::Id => chat::Column::Id,
            ChatQueryFilterField::Title => chat::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatQueryFilterField> for ChatMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(m: &chat::Model, f: ChatQueryFilterField) -> sea_orm::Value {
        match f {
            ChatQueryFilterField::UpdatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.updated_at))
            }
            ChatQueryFilterField::Id => sea_orm::Value::Uuid(Some(m.id)),
            ChatQueryFilterField::Title => sea_orm::Value::String(m.title.clone()),
        }
    }
}

/// `OData` fields of `GET /v1/chats/{id}/messages`.
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct MessageQuery {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: time::OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub id: uuid::Uuid,
    #[odata(filter(kind = "String"))]
    pub role: String,
}

pub struct MessageMapper;

impl FieldToColumn<MessageQueryFilterField> for MessageMapper {
    type Column = message::Column;

    fn map_field(f: MessageQueryFilterField) -> message::Column {
        match f {
            MessageQueryFilterField::CreatedAt => message::Column::CreatedAt,
            MessageQueryFilterField::Id => message::Column::Id,
            MessageQueryFilterField::Role => message::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageQueryFilterField> for MessageMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(m: &message::Model, f: MessageQueryFilterField) -> sea_orm::Value {
        match f {
            MessageQueryFilterField::CreatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.created_at))
            }
            MessageQueryFilterField::Id => sea_orm::Value::Uuid(Some(m.id)),
            MessageQueryFilterField::Role => sea_orm::Value::String(Some(m.role.clone())),
        }
    }
}
