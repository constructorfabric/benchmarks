//! `OData` field mappings of the list endpoints.

use sea_orm::Value;
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping};
use toolkit_odata::filter::{FieldKind, FilterField};

use super::entities::{chat, message};

/// Filterable / orderable fields of `GET /chats`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChatFilterField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatFilterField {
    const FIELDS: &'static [Self] = &[Self::UpdatedAt, Self::Id, Self::Title];

    fn name(&self) -> &'static str {
        match self {
            Self::UpdatedAt => "updated_at",
            Self::Id => "id",
            Self::Title => "title",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::UpdatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
        }
    }

    fn nullable(&self) -> bool {
        matches!(self, Self::Title)
    }
}

pub struct ChatMapper;

impl FieldToColumn<ChatFilterField> for ChatMapper {
    type Column = chat::Column;

    fn map_field(field: ChatFilterField) -> chat::Column {
        match field {
            ChatFilterField::UpdatedAt => chat::Column::UpdatedAt,
            ChatFilterField::Id => chat::Column::Id,
            ChatFilterField::Title => chat::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatFilterField> for ChatMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(model: &chat::Model, field: ChatFilterField) -> Value {
        match field {
            ChatFilterField::UpdatedAt => Value::TimeDateTimeWithTimeZone(Some(model.updated_at)),
            ChatFilterField::Id => Value::Uuid(Some(model.id)),
            ChatFilterField::Title => Value::String(Some(model.title.clone().unwrap_or_default())),
        }
    }
}

/// Filterable / orderable fields of `GET /chats/{id}/messages`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MessageFilterField {
    CreatedAt,
    Id,
    Role,
}

impl FilterField for MessageFilterField {
    const FIELDS: &'static [Self] = &[Self::CreatedAt, Self::Id, Self::Role];

    fn name(&self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::Id => "id",
            Self::Role => "role",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::CreatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Role => FieldKind::String,
        }
    }
}

pub struct MessageMapper;

impl FieldToColumn<MessageFilterField> for MessageMapper {
    type Column = message::Column;

    fn map_field(field: MessageFilterField) -> message::Column {
        match field {
            MessageFilterField::CreatedAt => message::Column::CreatedAt,
            MessageFilterField::Id => message::Column::Id,
            MessageFilterField::Role => message::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageFilterField> for MessageMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(model: &message::Model, field: MessageFilterField) -> Value {
        match field {
            MessageFilterField::CreatedAt => {
                Value::TimeDateTimeWithTimeZone(Some(model.created_at))
            }
            MessageFilterField::Id => Value::Uuid(Some(model.id)),
            MessageFilterField::Role => Value::String(Some(model.role.clone())),
        }
    }
}
