//! `OData` field mappings for `GET /chats` and `GET /chats/{id}/messages`.

use sea_orm::Value;
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping};
use toolkit_odata::filter::{FieldKind, FilterField};

use super::entities::{chats, messages};

/// Filterable / orderable chat fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatField {
    /// `updated_at`
    UpdatedAt,
    /// `id`
    Id,
    /// `title`
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

/// Chat mapper.
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
}

impl ODataFieldMapping<ChatField> for ChatMapper {
    type Entity = chats::Entity;

    fn extract_cursor_value(model: &chats::Model, field: ChatField) -> Value {
        match field {
            ChatField::UpdatedAt => Value::TimeDateTimeWithTimeZone(Some(model.updated_at)),
            ChatField::Id => Value::Uuid(Some(model.id)),
            ChatField::Title => Value::String(model.title.clone()),
        }
    }
}

/// Filterable / orderable message fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageField {
    /// `created_at`
    CreatedAt,
    /// `id`
    Id,
    /// `role`
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

/// Message mapper.
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
}

impl ODataFieldMapping<MessageField> for MessageMapper {
    type Entity = messages::Entity;

    fn extract_cursor_value(model: &messages::Model, field: MessageField) -> Value {
        match field {
            MessageField::CreatedAt => Value::TimeDateTimeWithTimeZone(Some(model.created_at)),
            MessageField::Id => Value::Uuid(Some(model.id)),
            MessageField::Role => Value::String(Some(model.role.clone())),
        }
    }
}

/// Rewrites timestamp comparisons into half-open ranges for SQLite.
///
/// SQLite stores timestamps as RFC 3339 text (`…Z`) while `OData` filter literals are bound
/// as chrono values (`…+00:00`); text comparison of the same instant then orders the stored
/// value after the literal, so `eq` never matches and `gt` matches the equal row. Expressed
/// only with `ge` / `lt` against `t` and `t + 1 ns`, every comparison is exact regardless of
/// the suffix.
#[must_use]
pub fn rewrite_timestamp_filter(expr: toolkit_odata::ast::Expr, fields: &[&str]) -> toolkit_odata::ast::Expr {
    use toolkit_odata::ast::{CompareOperator as Op, Expr, Value as V};

    let ident = |f: &str| Box::new(Expr::Identifier(f.to_owned()));
    let lit = |d: chrono::DateTime<chrono::Utc>| Box::new(Expr::Value(V::DateTime(d)));
    let next = |d: chrono::DateTime<chrono::Utc>| d + chrono::Duration::nanoseconds(1);
    let range = |f: &str, op: Op, d: chrono::DateTime<chrono::Utc>| -> Expr {
        match op {
            Op::Eq => Expr::And(
                Box::new(Expr::Compare(ident(f), Op::Ge, lit(d))),
                Box::new(Expr::Compare(ident(f), Op::Lt, lit(next(d)))),
            ),
            Op::Ne => Expr::Or(
                Box::new(Expr::Compare(ident(f), Op::Lt, lit(d))),
                Box::new(Expr::Compare(ident(f), Op::Ge, lit(next(d)))),
            ),
            Op::Gt => Expr::Compare(ident(f), Op::Ge, lit(next(d))),
            Op::Le => Expr::Compare(ident(f), Op::Lt, lit(next(d))),
            Op::Ge | Op::Lt => Expr::Compare(ident(f), op, lit(d)),
        }
    };
    let flip = |op: Op| match op {
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        o => o,
    };
    match expr {
        Expr::And(a, b) => Expr::And(
            Box::new(rewrite_timestamp_filter(*a, fields)),
            Box::new(rewrite_timestamp_filter(*b, fields)),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(rewrite_timestamp_filter(*a, fields)),
            Box::new(rewrite_timestamp_filter(*b, fields)),
        ),
        Expr::Not(a) => Expr::Not(Box::new(rewrite_timestamp_filter(*a, fields))),
        Expr::Compare(l, op, r) => match (*l, *r) {
            (Expr::Identifier(f), Expr::Value(V::DateTime(d))) if fields.contains(&f.as_str()) => range(&f, op, d),
            (Expr::Value(V::DateTime(d)), Expr::Identifier(f)) if fields.contains(&f.as_str()) => range(&f, flip(op), d),
            (l, r) => Expr::Compare(Box::new(l), op, Box::new(r)),
        },
        Expr::In(l, values) => match *l {
            Expr::Identifier(f)
                if fields.contains(&f.as_str()) && values.iter().all(|v| matches!(v, Expr::Value(V::DateTime(_)))) =>
            {
                let mut parts = values.into_iter().filter_map(|v| match v {
                    Expr::Value(V::DateTime(d)) => Some(range(&f, Op::Eq, d)),
                    _ => None,
                });
                let first = parts.next();
                parts.fold(first, |acc, p| acc.map(|a| Expr::Or(Box::new(a), Box::new(p)))).unwrap_or(Expr::In(Box::new(Expr::Identifier(f)), Vec::new()))
            }
            l => Expr::In(Box::new(l), values),
        },
        other => other,
    }
}

/// Applies [`rewrite_timestamp_filter`] to a query when the database is SQLite.
pub fn sqlite_safe_query(
    query: &toolkit_odata::ODataQuery,
    backend: sea_orm::DbBackend,
    fields: &[&str],
) -> toolkit_odata::ODataQuery {
    let mut q = query.clone();
    if backend == sea_orm::DbBackend::Sqlite
        && let Some(f) = q.filter.take()
    {
        q.filter = Some(Box::new(rewrite_timestamp_filter(*f, fields)));
    }
    q
}

#[cfg(test)]
#[path = "odata_tests.rs"]
mod odata_tests;
