//! Chat API DTOs (`ChatDetailDto`, `CreateChatReq`, `UpdateChatReq`) and the `OData` field enum of
//! the chat list.

use time::OffsetDateTime;
use toolkit_odata::filter::{FieldKind, FilterField};
use uuid::Uuid;

use crate::domain::chat_service::ChatDetail;

// Doc comments of these DTOs become the published schema descriptions (`docs/api/api.json`).

/// Request DTO for creating a new chat.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    pub title: Option<String>,
    pub model: Option<String>,
}

/// Request DTO for updating a chat title.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Response DTO for chat details.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    // Omitted (not `null`) when the chat has no title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    #[serde(with = "crate::api::dto::timestamp")]
    pub created_at: OffsetDateTime,
    #[serde(with = "crate::api::dto::timestamp")]
    pub updated_at: OffsetDateTime,
}

impl From<ChatDetail> for ChatDetailDto {
    fn from(c: ChatDetail) -> Self {
        Self {
            id: c.id,
            model: c.model,
            title: c.title,
            is_temporary: c.is_temporary,
            message_count: c.message_count,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// Fields of the chat list admitted in `$filter` and `$orderby`. The order of [`Self::FIELDS`]
/// is the order of the published `x-odata-*` extensions.
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
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
        }
    }
}
