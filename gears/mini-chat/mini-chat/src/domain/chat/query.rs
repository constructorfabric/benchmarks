//! `OData` query surface of the chat list: `$filter` and `$orderby` over `updated_at`, `id` and
//! `title`.

use toolkit_macros::domain_model;
use toolkit_odata::filter::{FieldKind, FilterField};

/// Filterable and orderable fields of `GET /v1/chats`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatFilterField {
    /// Chat identifier.
    Id,
    /// Chat title.
    Title,
    /// Last activity time.
    UpdatedAt,
}

impl FilterField for ChatFilterField {
    const FIELDS: &'static [Self] = &[Self::Id, Self::Title, Self::UpdatedAt];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Title => "title",
            Self::UpdatedAt => "updated_at",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
            Self::UpdatedAt => FieldKind::DateTimeUtc,
        }
    }
}
