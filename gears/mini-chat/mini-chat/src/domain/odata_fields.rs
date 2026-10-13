//! OData filter/orderby fields of the list endpoints and their column
//! mappings. `id` is declared as a string field so that the documented
//! `$filter=id eq '<uuid>'` form works; the value is converted to a UUID.

use sea_orm::Value as SeaValue;
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping};
use toolkit_odata::filter::{FieldKind, FilterField, FilterOp, ODataValue};

use crate::infra::db::entities::{chats, messages};

fn uuid_value(value: &ODataValue) -> Result<ODataValue, String> {
    match value {
        ODataValue::String(s) => s
            .parse::<uuid::Uuid>()
            .map(ODataValue::Uuid)
            .map_err(|_| format!("'{s}' is not a valid UUID")),
        other => Ok(other.clone()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatField {
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
            Self::Id | Self::Title => FieldKind::String,
        }
    }
    fn nullable(&self) -> bool {
        matches!(self, Self::Title)
    }
}

pub struct ChatMapper;

impl FieldToColumn<ChatField> for ChatMapper {
    type Column = chats::Column;
    fn map_field(field: ChatField) -> chats::Column {
        match field {
            ChatField::UpdatedAt => chats::Column::UpdatedAt,
            ChatField::Id => chats::Column::Id,
            ChatField::Title => chats::Column::Title,
        }
    }
    fn map_value(field: ChatField, _op: FilterOp, value: &ODataValue) -> Result<ODataValue, String> {
        if field == ChatField::Id { uuid_value(value) } else { Ok(value.clone()) }
    }
}

impl ODataFieldMapping<ChatField> for ChatMapper {
    type Entity = chats::Entity;
    fn extract_cursor_value(model: &chats::Model, field: ChatField) -> SeaValue {
        match field {
            ChatField::UpdatedAt => SeaValue::TimeDateTimeWithTimeZone(Some(model.updated_at)),
            ChatField::Id => SeaValue::Uuid(Some(model.id)),
            ChatField::Title => SeaValue::String(model.title.clone()),
        }
    }
    fn cursor_kind(field: ChatField) -> FieldKind {
        match field {
            ChatField::Id => FieldKind::Uuid,
            other => other.kind(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageField {
    CreatedAt,
    Id,
    Role,
}

impl FilterField for MessageField {
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
            Self::Id | Self::Role => FieldKind::String,
        }
    }
}

pub struct MessageMapper;

impl FieldToColumn<MessageField> for MessageMapper {
    type Column = messages::Column;
    fn map_field(field: MessageField) -> messages::Column {
        match field {
            MessageField::CreatedAt => messages::Column::CreatedAt,
            MessageField::Id => messages::Column::Id,
            MessageField::Role => messages::Column::Role,
        }
    }
    fn map_value(field: MessageField, _op: FilterOp, value: &ODataValue) -> Result<ODataValue, String> {
        if field == MessageField::Id { uuid_value(value) } else { Ok(value.clone()) }
    }
}

impl ODataFieldMapping<MessageField> for MessageMapper {
    type Entity = messages::Entity;
    fn extract_cursor_value(model: &messages::Model, field: MessageField) -> SeaValue {
        match field {
            MessageField::CreatedAt => SeaValue::TimeDateTimeWithTimeZone(Some(model.created_at)),
            MessageField::Id => SeaValue::Uuid(Some(model.id)),
            MessageField::Role => SeaValue::String(Some(model.role.clone())),
        }
    }
    fn cursor_kind(field: MessageField) -> FieldKind {
        match field {
            MessageField::Id => FieldKind::Uuid,
            other => other.kind(),
        }
    }
}
