//! Tests for the `OData` list options.

use crate::store::list::{ListQuery, MAX_TOP};

fn item(name: &str, enabled: bool) -> serde_json::Value {
    serde_json::json!({ "name": name, "enabled": enabled })
}

#[test]
fn an_empty_query_parses_to_the_default_page() {
    let query = ListQuery::parse("").expect("parses");
    assert_eq!(query.top, 50);
    assert_eq!(query.skip, 0);
    assert!(query.filter.is_none());
    assert!(query.select.is_none());
    assert!(query.orderby.is_empty());
}

#[test]
fn top_is_clamped_to_the_maximum() {
    let query = ListQuery::parse("$top=5000").expect("parses");
    assert_eq!(query.top, MAX_TOP);
}

#[test]
fn a_negative_top_is_a_validation_error() {
    let err = ListQuery::parse("$top=-1").expect_err("negative");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError);
}

#[test]
fn a_non_integer_top_is_a_validation_error() {
    assert!(ListQuery::parse("$top=many").is_err());
}

#[test]
fn a_negative_skip_is_a_validation_error() {
    assert!(ListQuery::parse("$skip=-1").is_err());
}

#[test]
fn skip_and_top_page_the_result() {
    let query = ListQuery::parse("$top=2&$skip=1").expect("parses");
    let items = vec![
        item("one", true),
        item("two", true),
        item("three", true),
        item("four", true),
    ];
    let paged = query.apply(items);
    assert_eq!(paged.len(), 2);
    assert_eq!(paged[0]["name"], "two");
    assert_eq!(paged[1]["name"], "three");
}

#[test]
fn orderby_sorts_ascending_and_descending() {
    let ascending = ListQuery::parse("$orderby=enabled").expect("parses");
    let items = vec![item("b", true), item("a", false), item("c", true)];
    let sorted = ascending.apply(items.clone());
    assert_eq!(sorted[0]["name"], "a", "false sorts before true");

    let descending = ListQuery::parse("$orderby=enabled desc").expect("parses");
    let sorted = descending.apply(items);
    assert_eq!(sorted[0]["name"], "b", "true sorts last ascending");
}

#[test]
fn a_filter_is_a_conjunction_of_equality_terms() {
    let query = ListQuery::parse("$filter=enabled eq true and name eq 'two'").expect("parses");
    let items = vec![item("one", true), item("two", false), item("two", true)];
    let filtered = query.filter(items);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["name"], "two");
    assert_eq!(filtered[0]["enabled"], true);
}

#[test]
fn string_filters_are_case_insensitive() {
    let query = ListQuery::parse("$filter=name eq 'TWO'").expect("parses");
    let filtered = query.filter(vec![item("two", true)]);
    assert_eq!(filtered.len(), 1);
}

#[test]
fn a_null_filter_matches_a_missing_field() {
    let query = ListQuery::parse("$filter=alias eq null").expect("parses");
    let filtered = query.filter(vec![item("one", true)]);
    assert_eq!(filtered.len(), 1, "a missing field is indistinguishable from null");
}

#[test]
fn an_unknown_system_option_is_rejected() {
    assert!(ListQuery::parse("$what=ever").is_err());
}

#[test]
fn select_is_recorded_as_the_field_list() {
    let query = ListQuery::parse("$select=name,enabled").expect("parses");
    assert_eq!(
        query.select,
        Some(vec!["name".to_owned(), "enabled".to_owned()])
    );
}
