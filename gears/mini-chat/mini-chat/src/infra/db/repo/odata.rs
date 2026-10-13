//! `OData` field mapping: filter / order fields of the chat and message lists
//! to their columns and to the values a pagination cursor carries.

use sea_orm::Value;
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping};

use crate::domain::chats::ChatFilterField;
use crate::domain::messages::MessageFilterField;
use crate::infra::db::entity::{chats, messages};

/// Mapper of the chat list.
pub struct ChatODataMapper;

impl FieldToColumn<ChatFilterField> for ChatODataMapper {
    type Column = chats::Column;

    fn map_field(field: ChatFilterField) -> chats::Column {
        match field {
            ChatFilterField::Id => chats::Column::Id,
            ChatFilterField::Title => chats::Column::Title,
            ChatFilterField::UpdatedAt => chats::Column::UpdatedAt,
        }
    }
}

impl ODataFieldMapping<ChatFilterField> for ChatODataMapper {
    type Entity = chats::Entity;

    fn extract_cursor_value(model: &chats::Model, field: ChatFilterField) -> Value {
        match field {
            ChatFilterField::Id => Value::Uuid(Some(model.id)),
            // Untitled chats are stored as the empty title, never `NULL`, so the
            // sort key, the cursor value and the keyset predicate agree.
            ChatFilterField::Title => Value::String(Some(model.title.clone().unwrap_or_default())),
            ChatFilterField::UpdatedAt => Value::ChronoDateTimeUtc(Some(model.updated_at)),
        }
    }
}

/// Mapper of the message list.
pub struct MessageODataMapper;

impl FieldToColumn<MessageFilterField> for MessageODataMapper {
    type Column = messages::Column;

    fn map_field(field: MessageFilterField) -> messages::Column {
        match field {
            MessageFilterField::CreatedAt => messages::Column::CreatedAt,
            MessageFilterField::Id => messages::Column::Id,
            MessageFilterField::Role => messages::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageFilterField> for MessageODataMapper {
    type Entity = messages::Entity;

    fn extract_cursor_value(model: &messages::Model, field: MessageFilterField) -> Value {
        match field {
            MessageFilterField::CreatedAt => Value::ChronoDateTimeUtc(Some(model.created_at)),
            MessageFilterField::Id => Value::Uuid(Some(model.id)),
            MessageFilterField::Role => Value::String(Some(model.role.clone())),
        }
    }
}
