#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tests of the `FieldToColumn::bind_value` and `FieldToColumn::sort_expr`
//! hooks: the defaults keep the previous SQL, an override reaches the filter,
//! the order and the keyset predicate.

use sea_orm::sea_query::{Asterisk, Func, Query, SqliteQueryBuilder};

use super::*;

mod ent {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "hooks")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub title: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Field {
    Id,
    Title,
}

impl FilterField for Field {
    const FIELDS: &'static [Self] = &[Self::Id, Self::Title];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Title => "title",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Id => FieldKind::I64,
            Self::Title => FieldKind::String,
        }
    }
}

fn column(field: Field) -> ent::Column {
    match field {
        Field::Id => ent::Column::Id,
        Field::Title => ent::Column::Title,
    }
}

/// Mapper that overrides nothing.
struct Plain;

impl FieldToColumn<Field> for Plain {
    type Column = ent::Column;

    fn map_field(field: Field) -> ent::Column {
        column(field)
    }
}

/// Mapper that overrides both hooks for `title`.
struct Hooked;

impl FieldToColumn<Field> for Hooked {
    type Column = ent::Column;

    fn map_field(field: Field) -> ent::Column {
        column(field)
    }

    fn bind_value(field: Field, value: &ODataValue) -> Option<sea_orm::Value> {
        match (field, value) {
            (Field::Title, ODataValue::String(text)) => {
                Some(sea_orm::Value::String(Some(format!("bound:{text}"))))
            }
            _ => None,
        }
    }

    fn sort_expr(field: Field) -> SimpleExpr {
        match field {
            Field::Title => {
                Func::coalesce([ent::Column::Title.into_simple_expr(), Expr::val("")]).into()
            }
            Field::Id => ent::Column::Id.into_simple_expr(),
        }
    }
}

fn cursor_value(model: &ent::Model, field: Field) -> sea_orm::Value {
    match field {
        Field::Id => sea_orm::Value::BigInt(Some(model.id)),
        Field::Title => sea_orm::Value::String(Some(model.title.clone().unwrap_or_default())),
    }
}

impl ODataFieldMapping<Field> for Plain {
    type Entity = ent::Entity;

    fn extract_cursor_value(model: &ent::Model, field: Field) -> sea_orm::Value {
        cursor_value(model, field)
    }
}

impl ODataFieldMapping<Field> for Hooked {
    type Entity = ent::Entity;

    fn extract_cursor_value(model: &ent::Model, field: Field) -> sea_orm::Value {
        cursor_value(model, field)
    }
}

fn sql(condition: Condition) -> String {
    Query::select()
        .column(Asterisk)
        .from(ent::Entity)
        .cond_where(condition)
        .to_string(SqliteQueryBuilder)
}

fn title_filter(op: FilterOp, text: &str) -> FilterNode<Field> {
    FilterNode::Binary {
        field: Field::Title,
        op,
        value: ODataValue::String(text.to_owned()),
    }
}

fn cursor(token: &str) -> CursorV1 {
    CursorV1 {
        k: vec![token.to_owned(), "7".to_owned()],
        o: SortDir::Asc,
        s: "+title,+id".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    }
}

fn order() -> ODataOrderBy {
    ODataOrderBy::from_signed_tokens("+title,+id").unwrap()
}

#[test]
fn without_overrides_the_filter_binds_the_platform_value() {
    let cond = filter_node_to_condition::<Field, Plain>(&title_filter(FilterOp::Eq, "x")).unwrap();

    assert_eq!(sql(cond), r#"SELECT * FROM "hooks" WHERE "title" = 'x'"#);
}

#[test]
fn a_bind_value_override_replaces_the_bound_literal_of_comparisons_and_in_lists() {
    let eq = filter_node_to_condition::<Field, Hooked>(&title_filter(FilterOp::Gt, "x")).unwrap();
    let list = filter_node_to_condition::<Field, Hooked>(&FilterNode::InList {
        field: Field::Title,
        values: vec![ODataValue::String("a".to_owned())],
    })
    .unwrap();

    assert!(sql(eq).contains(r#""title" > 'bound:x'"#));
    assert!(sql(list).contains(r"IN ('bound:a')"));
}

#[test]
fn a_bind_value_override_that_declines_leaves_the_platform_value() {
    let cond = filter_node_to_condition::<Field, Hooked>(&FilterNode::Binary {
        field: Field::Id,
        op: FilterOp::Eq,
        value: ODataValue::Number(5.into()),
    })
    .unwrap();

    assert!(sql(cond).contains(r#""id" = 5"#));
}

#[test]
fn the_default_sort_expression_is_the_column_in_the_keyset_predicate() {
    let cond = build_cursor_predicate::<Field, Plain>(&cursor("m"), &order()).unwrap();

    assert_eq!(
        sql(cond),
        r#"SELECT * FROM "hooks" WHERE "hooks"."title" > 'm' OR ("hooks"."title" = 'm' AND "hooks"."id" > 7)"#
    );
}

#[test]
fn a_sort_expression_override_reaches_the_keyset_predicate() {
    let cond = build_cursor_predicate::<Field, Hooked>(&cursor(""), &order()).unwrap();

    let text = sql(cond);
    assert!(
        text.contains(r#"COALESCE("hooks"."title", '') > ''"#),
        "{text}"
    );
    assert!(
        text.contains(r#"COALESCE("hooks"."title", '') = ''"#),
        "{text}"
    );
}
