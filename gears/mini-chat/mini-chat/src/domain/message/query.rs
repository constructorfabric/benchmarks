//! `OData` query surface of the message list: `$filter` and `$orderby` over `created_at`, `id`
//! and `role`.

use toolkit_macros::domain_model;
use toolkit_odata::filter::{FieldKind, FilterField};

/// Filterable and orderable fields of `GET /v1/chats/{id}/messages`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageFilterField {
    /// Creation time.
    CreatedAt,
    /// Message id.
    Id,
    /// Role.
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
