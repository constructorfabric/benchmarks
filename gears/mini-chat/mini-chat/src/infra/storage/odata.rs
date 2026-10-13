//! `OData` field mapping of the chat list.

use sea_orm::Value;
use toolkit_db::odata::sea_orm_filter::{FieldToColumn, ODataFieldMapping};

use super::entity::chats;
use crate::domain::chat::ChatFilterField;

/// Stored `title` of an untitled chat.
///
/// The `OData` keyset predicate (`title > ?`, `title = ?`) and the `ne` filter never match a SQL
/// `NULL`, and `NULL` ordering differs between `SQLite` and `PostgreSQL`. Untitled chats are
/// therefore stored as the empty title, which no valid title can equal (titles are trimmed and
/// non-empty), and every read maps it back to `None`.
pub const UNTITLED_DB_TITLE: &str = "";

/// Maps the chat filter fields to `chats` columns and extracts cursor values from rows.
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
            // Untitled chats are stored as the empty title; a legacy NULL is encoded the same way.
            ChatFilterField::Title => Value::String(Some(model.title.clone().unwrap_or_default())),
            ChatFilterField::UpdatedAt => Value::TimeDateTimeWithTimeZone(Some(model.updated_at)),
        }
    }
}
