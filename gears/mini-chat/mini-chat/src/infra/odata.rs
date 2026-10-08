//! `OData` field mappings (filter fields -> entity columns, cursor values).

use toolkit_db::odata::sea_orm_filter::{FieldToColumn, ODataFieldMapping};

use crate::api::rest::odata::{
    ChatQueryFieldsFilterField as CF, MessageQueryFieldsFilterField as MF,
};
use crate::infra::db::entity::{chat, message};

pub struct ChatODataMapper;

impl FieldToColumn<CF> for ChatODataMapper {
    type Column = chat::Column;

    fn map_field(field: CF) -> chat::Column {
        match field {
            CF::UpdatedAt => chat::Column::UpdatedAt,
            CF::Id => chat::Column::Id,
            CF::Title => chat::Column::Title,
        }
    }
}

impl ODataFieldMapping<CF> for ChatODataMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(model: &chat::Model, field: CF) -> sea_orm::Value {
        match field {
            CF::UpdatedAt => sea_orm::Value::from(model.updated_at),
            CF::Id => sea_orm::Value::from(model.id),
            CF::Title => sea_orm::Value::from(model.title.clone().unwrap_or_default()),
        }
    }
}

pub struct MessageODataMapper;

impl FieldToColumn<MF> for MessageODataMapper {
    type Column = message::Column;

    fn map_field(field: MF) -> message::Column {
        match field {
            MF::CreatedAt => message::Column::CreatedAt,
            MF::Id => message::Column::Id,
            MF::Role => message::Column::Role,
        }
    }
}

impl ODataFieldMapping<MF> for MessageODataMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(model: &message::Model, field: MF) -> sea_orm::Value {
        match field {
            MF::CreatedAt => sea_orm::Value::from(model.created_at),
            MF::Id => sea_orm::Value::from(model.id),
            MF::Role => sea_orm::Value::from(model.role.clone()),
        }
    }
}
