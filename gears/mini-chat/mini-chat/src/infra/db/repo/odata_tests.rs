#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm::sea_query::{Query, SqliteQueryBuilder};
use time::OffsetDateTime;
use toolkit_odata::ast::{CompareOperator, Expr, Value as AstValue};
use toolkit_odata::filter::{FieldKind, FilterField, convert_expr_to_filter_node};
use toolkit_odata::{CursorV1, ODataQuery, SortDir};
use uuid::Uuid;

use super::*;

fn compare(field: &str, value: AstValue) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier(field.to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(value)),
    )
}

#[test]
fn field_sets_match_the_contract() {
    let chat: Vec<(&str, FieldKind)> = ChatField::FIELDS
        .iter()
        .map(|f| (f.name(), f.kind()))
        .collect();
    assert_eq!(
        chat,
        [
            ("updated_at", FieldKind::DateTimeUtc),
            ("id", FieldKind::Uuid),
            ("title", FieldKind::String)
        ]
    );
    assert!(ChatField::Title.nullable());
    assert!(!ChatField::Id.nullable());

    let message: Vec<(&str, FieldKind)> = MessageField::FIELDS
        .iter()
        .map(|f| (f.name(), f.kind()))
        .collect();
    assert_eq!(
        message,
        [
            ("created_at", FieldKind::DateTimeUtc),
            ("id", FieldKind::Uuid),
            ("role", FieldKind::String)
        ]
    );
    assert_eq!(MessageField::from_name("ROLE"), Some(MessageField::Role));
    assert_eq!(MessageField::from_name("content"), None);
    assert_eq!((LIMITS.default, LIMITS.max), (20, 100));
}

#[test]
fn default_order_applies_only_without_order_or_cursor() {
    let plain = with_default_order(&ODataQuery::new(), ("created_at", SortDir::Asc));
    assert_eq!(plain.order.to_signed_tokens(), "+created_at");

    let explicit = ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: "role".to_owned(),
        dir: SortDir::Desc,
    }]));
    assert_eq!(
        with_default_order(&explicit, ("created_at", SortDir::Asc))
            .order
            .to_signed_tokens(),
        "-role"
    );

    let cursor = CursorV1 {
        k: vec!["x".to_owned()],
        o: SortDir::Asc,
        s: "+id".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    };
    let continued = with_default_order(
        &ODataQuery::new().with_cursor(cursor),
        ("created_at", SortDir::Asc),
    );
    assert!(continued.order.is_empty());
}

#[test]
fn quoted_uuid_literals_on_id_become_uuid_literals() {
    let id = Uuid::new_v4();
    let filter = Expr::And(
        Box::new(compare("id", AstValue::String(id.to_string()))),
        Box::new(Expr::In(
            Box::new(Expr::Identifier("ID".to_owned())),
            vec![
                Expr::Value(AstValue::String(id.to_string())),
                Expr::Value(AstValue::Uuid(id)),
            ],
        )),
    );
    let query = ODataQuery::new()
        .with_filter(filter)
        .with_filter_hash("hash".to_owned());

    let coerced = with_uuid_literals(&query, &["id"]);

    assert_eq!(coerced.filter_hash.as_deref(), Some("hash"));
    let node = convert_expr_to_filter_node::<MessageField>(coerced.filter().unwrap());
    assert!(node.is_ok(), "{node:?}");
}

#[test]
fn other_strings_are_left_alone() {
    let query = ODataQuery::new().with_filter(Expr::Or(
        Box::new(compare(
            "role",
            AstValue::String(Uuid::new_v4().to_string()),
        )),
        Box::new(compare("id", AstValue::String("not-a-uuid".to_owned()))),
    ));

    let coerced = with_uuid_literals(&query, &["id"]);

    let Some(Expr::Or(left, right)) = coerced.filter() else {
        panic!("filter shape changed");
    };
    let Expr::Compare(_, _, value) = left.as_ref() else {
        panic!("compare expected");
    };
    assert!(matches!(value.as_ref(), Expr::Value(AstValue::String(_))));
    let Expr::Compare(_, _, value) = right.as_ref() else {
        panic!("compare expected");
    };
    assert!(matches!(value.as_ref(), Expr::Value(AstValue::String(s)) if s == "not-a-uuid"));
}

/// `2026-01-01T00:00:00Z` with `nanos` nanoseconds.
fn instant(nanos: u32) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600)
        .unwrap()
        .replace_nanosecond(nanos)
        .unwrap()
}

fn chrono_literal(text: &str) -> ODataValue {
    ODataValue::DateTime(
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
}

#[test]
fn fixed_text_timestamps_are_fixed_width_and_sort_chronologically() {
    let texts: Vec<String> = [0, 500_000_000, 500_400_000, 999_999_999]
        .into_iter()
        .map(|nanos| fixed_text(instant(nanos)))
        .collect();

    assert_eq!(
        texts,
        [
            "2026-01-01T00:00:00.000000Z",
            "2026-01-01T00:00:00.500000Z",
            "2026-01-01T00:00:00.500400Z",
            "2026-01-01T00:00:00.999999Z",
        ]
    );
    let mut sorted = texts.clone();
    sorted.sort();
    assert_eq!(sorted, texts);
}

#[test]
fn date_time_literals_bind_in_the_timestamp_form() {
    let echoed = chrono_literal("2026-01-01T00:00:00.5Z");
    let text_of = |value: ODataValue| match value {
        ODataValue::String(text) => text,
        other => panic!("expected a text literal, got {other:?}"),
    };

    let text =
        ChatODataMapping::<TextTimestamps>::map_value(ChatField::UpdatedAt, FilterOp::Gt, &echoed)
            .unwrap();
    assert_eq!(text_of(text), "2026-01-01T00:00:00.500000Z");
    let text = MessageODataMapping::<TextTimestamps>::map_value(
        MessageField::CreatedAt,
        FilterOp::Eq,
        &chrono_literal("2026-01-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(text_of(text), "2026-01-01T00:00:00.000000Z");

    let native = ChatODataMapping::<NativeTimestamps>::map_value(
        ChatField::UpdatedAt,
        FilterOp::Gt,
        &echoed,
    )
    .unwrap();
    assert!(
        matches!((&native, &echoed), (ODataValue::DateTime(a), ODataValue::DateTime(b)) if a == b),
        "{native:?}"
    );
    let title = ChatODataMapping::<TextTimestamps>::map_value(
        ChatField::Title,
        FilterOp::Eq,
        &ODataValue::String("2026-01-01T00:00:00.5Z".to_owned()),
    )
    .unwrap();
    assert_eq!(text_of(title), "2026-01-01T00:00:00.5Z");
}

#[test]
fn timestamp_cursor_keys_use_the_storage_kind() {
    assert_eq!(
        ChatODataMapping::<TextTimestamps>::cursor_kind(ChatField::UpdatedAt),
        FieldKind::String
    );
    assert_eq!(
        ChatODataMapping::<NativeTimestamps>::cursor_kind(ChatField::UpdatedAt),
        FieldKind::DateTimeUtc
    );
    assert_eq!(
        MessageODataMapping::<TextTimestamps>::cursor_kind(MessageField::CreatedAt),
        FieldKind::String
    );
    assert_eq!(
        MessageODataMapping::<NativeTimestamps>::cursor_kind(MessageField::Role),
        FieldKind::String
    );
}

#[test]
fn title_sorts_by_its_null_safe_key() {
    let sql = |field| {
        Query::select()
            .expr(ChatODataMapping::<TextTimestamps>::sort_key(field))
            .to_string(SqliteQueryBuilder)
    };

    assert_eq!(sql(ChatField::Title), r#"SELECT COALESCE("title", '')"#);
    assert_eq!(sql(ChatField::UpdatedAt), r#"SELECT "updated_at""#);
}
