//! `OData` filter/order fields of the list endpoints and their column mappers.

use toolkit_db::odata::{FieldToColumn, ODataFieldMapping};
use toolkit_odata::filter::{FieldKind, FilterField};

use crate::infra::storage::entity::{chat, message};

/// `GET /v1/chats` fields: `updated_at`, `id`, `title`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
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
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
        }
    }

    fn nullable(&self) -> bool {
        matches!(self, Self::Title)
    }
}

pub struct ChatMapper;

impl FieldToColumn<ChatField> for ChatMapper {
    type Column = chat::Column;

    fn map_field(field: ChatField) -> chat::Column {
        match field {
            ChatField::UpdatedAt => chat::Column::UpdatedAt,
            ChatField::Id => chat::Column::Id,
            ChatField::Title => chat::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatField> for ChatMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(model: &chat::Model, field: ChatField) -> sea_orm::Value {
        match field {
            ChatField::UpdatedAt => sea_orm::Value::from(model.updated_at),
            ChatField::Id => sea_orm::Value::from(model.id),
            ChatField::Title => sea_orm::Value::from(model.title.clone()),
        }
    }
}

/// `GET /v1/chats/{id}/messages` fields: `created_at`, `id`, `role`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
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
            Self::Id => FieldKind::Uuid,
            Self::Role => FieldKind::String,
        }
    }
}

pub struct MessageMapper;

impl FieldToColumn<MessageField> for MessageMapper {
    type Column = message::Column;

    fn map_field(field: MessageField) -> message::Column {
        match field {
            MessageField::CreatedAt => message::Column::CreatedAt,
            MessageField::Id => message::Column::Id,
            MessageField::Role => message::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageField> for MessageMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(model: &message::Model, field: MessageField) -> sea_orm::Value {
        match field {
            MessageField::CreatedAt => sea_orm::Value::from(model.created_at),
            MessageField::Id => sea_orm::Value::from(model.id),
            MessageField::Role => sea_orm::Value::from(model.role.clone()),
        }
    }
}

fn ts_cursor(ts: crate::domain::clock::Timestamp) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Field map of `GET /v1/chats` (chrono-typed filter and cursor values, the
/// same encoding as the stored timestamps).
pub fn chat_field_map() -> toolkit_db::odata::FieldMap<chat::Entity> {
    toolkit_db::odata::FieldMap::new()
        .insert_with_extractor("updated_at", chat::Column::UpdatedAt, FieldKind::DateTimeUtc, |m: &chat::Model| ts_cursor(m.updated_at))
        .insert_with_extractor("id", chat::Column::Id, FieldKind::Uuid, |m: &chat::Model| m.id.to_string())
        .insert_with_extractor("title", chat::Column::Title, FieldKind::String, |m: &chat::Model| m.title.clone().unwrap_or_default())
}

/// Field map of `GET /v1/chats/{id}/messages`.
pub fn message_field_map() -> toolkit_db::odata::FieldMap<message::Entity> {
    toolkit_db::odata::FieldMap::new()
        .insert_with_extractor("created_at", message::Column::CreatedAt, FieldKind::DateTimeUtc, |m: &message::Model| ts_cursor(m.created_at))
        .insert_with_extractor("id", message::Column::Id, FieldKind::Uuid, |m: &message::Model| m.id.to_string())
        .insert_with_extractor("role", message::Column::Role, FieldKind::String, |m: &message::Model| m.role.clone())
}
