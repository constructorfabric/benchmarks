//! `OData` field declarations and column mappings of the list endpoints.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, SecondsFormat, Utc};
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping};
use toolkit_odata::filter::FieldKind;
use uuid::Uuid;

use crate::infra::db::entities::{chats, messages};

/// On SQLite timestamps are TEXT. The `OData` cursor decoder binds datetime
/// keys as `time::OffsetDateTime` (`...Z`), while chrono values are stored as
/// `...+00:00`, so text comparisons at the page boundary would be off. With
/// SQLite the datetime cursor keys therefore carry the exact storage text and
/// are compared as strings.
static TEXT_TIMESTAMP_CURSORS: AtomicBool = AtomicBool::new(true);

/// Selects the cursor encoding of datetime keys for the active backend.
pub fn set_text_timestamp_cursors(enabled: bool) {
    TEXT_TIMESTAMP_CURSORS.store(enabled, Ordering::Relaxed);
}

fn text_cursors() -> bool {
    TEXT_TIMESTAMP_CURSORS.load(Ordering::Relaxed)
}

/// Storage text of a timestamp (the sqlx SQLite chrono encoding).
#[must_use]
pub fn storage_text(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::AutoSi, false)
}

fn ts_cursor(ts: DateTime<Utc>) -> sea_orm::Value {
    if text_cursors() {
        sea_orm::Value::String(Some(storage_text(ts)))
    } else {
        sea_orm::Value::ChronoDateTimeUtc(Some(ts))
    }
}

/// Filterable/orderable fields of `GET /v1/chats`.
#[derive(Debug, Clone, toolkit_odata_macros::ODataFilterable)]
pub struct ChatQueryFields {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: DateTime<Utc>,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub title: String,
}

/// Filterable/orderable fields of `GET /v1/chats/{id}/messages`.
#[derive(Debug, Clone, toolkit_odata_macros::ODataFilterable)]
pub struct MessageQueryFields {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: DateTime<Utc>,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub role: String,
}

/// Mapper of chat fields.
pub struct ChatMapper;

impl FieldToColumn<ChatQueryFieldsFilterField> for ChatMapper {
    type Column = chats::Column;
    fn map_field(f: ChatQueryFieldsFilterField) -> chats::Column {
        match f {
            ChatQueryFieldsFilterField::UpdatedAt => chats::Column::UpdatedAt,
            ChatQueryFieldsFilterField::Id => chats::Column::Id,
            ChatQueryFieldsFilterField::Title => chats::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatQueryFieldsFilterField> for ChatMapper {
    type Entity = chats::Entity;
    fn cursor_kind(f: ChatQueryFieldsFilterField) -> FieldKind {
        match f {
            ChatQueryFieldsFilterField::UpdatedAt if text_cursors() => FieldKind::String,
            other => toolkit_odata::filter::FilterField::kind(&other),
        }
    }
    fn extract_cursor_value(m: &chats::Model, f: ChatQueryFieldsFilterField) -> sea_orm::Value {
        match f {
            ChatQueryFieldsFilterField::UpdatedAt => ts_cursor(m.updated_at),
            ChatQueryFieldsFilterField::Id => sea_orm::Value::Uuid(Some(m.id)),
            ChatQueryFieldsFilterField::Title => {
                sea_orm::Value::String(Some(m.title.clone().unwrap_or_default()))
            }
        }
    }
}

/// Mapper of message fields.
pub struct MessageMapper;

impl FieldToColumn<MessageQueryFieldsFilterField> for MessageMapper {
    type Column = messages::Column;
    fn map_field(f: MessageQueryFieldsFilterField) -> messages::Column {
        match f {
            MessageQueryFieldsFilterField::CreatedAt => messages::Column::CreatedAt,
            MessageQueryFieldsFilterField::Id => messages::Column::Id,
            MessageQueryFieldsFilterField::Role => messages::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageQueryFieldsFilterField> for MessageMapper {
    type Entity = messages::Entity;
    fn cursor_kind(f: MessageQueryFieldsFilterField) -> FieldKind {
        match f {
            MessageQueryFieldsFilterField::CreatedAt if text_cursors() => FieldKind::String,
            other => toolkit_odata::filter::FilterField::kind(&other),
        }
    }
    fn extract_cursor_value(m: &messages::Model, f: MessageQueryFieldsFilterField) -> sea_orm::Value {
        match f {
            MessageQueryFieldsFilterField::CreatedAt => ts_cursor(m.created_at),
            MessageQueryFieldsFilterField::Id => sea_orm::Value::Uuid(Some(m.id)),
            MessageQueryFieldsFilterField::Role => sea_orm::Value::String(Some(m.role.clone())),
        }
    }
}
