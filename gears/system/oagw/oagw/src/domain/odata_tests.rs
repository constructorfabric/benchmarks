//! Tests for [`crate::domain::odata`].

use serde_json::{Value, json};

use super::{
    DEFAULT_PAGE_SIZE, FieldCatalog, ListQuery, MAX_PAGE_SIZE, PLUGIN_FIELDS, ROUTE_FIELDS,
    UPSTREAM_FIELDS,
};
use crate::domain::error::OagwError;

/// Catalog covering every field the fixtures below carry. `server` is
/// deliberately selectable but not sortable so both rejections stay testable.
const CATALOG: FieldCatalog = FieldCatalog {
    filterable: &["id", "alias", "plugin_type", "enabled", "priority"],
    sortable: &["id", "alias", "plugin_type", "enabled", "priority"],
    selectable: &[
        "id",
        "alias",
        "plugin_type",
        "enabled",
        "priority",
        "server",
        "tags",
        "config",
    ],
    aliases: &[("type", "plugin_type")],
};

#[test]
fn the_plugin_catalog_resolves_the_type_alias() {
    assert_eq!(CATALOG.canonical("type"), Some("plugin_type"));
    assert_eq!(CATALOG.canonical("plugin_type"), Some("plugin_type"));
    assert_eq!(CATALOG.canonical("secret"), None);
    assert_eq!(PLUGIN_FIELDS.canonical("type"), Some("plugin_type"));
    assert_eq!(PLUGIN_FIELDS.canonical("config"), Some("config"));
    assert_eq!(ROUTE_FIELDS.canonical("upstream_id"), Some("upstream_id"));
    assert_eq!(UPSTREAM_FIELDS.canonical("server"), Some("server"));
    assert_eq!(UPSTREAM_FIELDS.canonical("upstream_id"), None);
}

fn rows() -> Vec<Value> {
    vec![
        json!({"id": "a", "alias": "api.openai.com", "enabled": true, "priority": 1}),
        json!({"id": "b", "alias": "vendor.com", "enabled": false, "priority": 3}),
        json!({"id": "c", "alias": "api.vendor.com", "enabled": true, "priority": 2}),
    ]
}

fn parsed(raw: Option<&str>) -> ListQuery {
    ListQuery::parse(raw, &CATALOG).expect("query parses")
}

fn applied(raw: Option<&str>) -> Vec<Value> {
    parsed(raw).apply_to_rows(rows())
}

fn error_of(raw: &str) -> OagwError {
    ListQuery::parse(Some(raw), &CATALOG).expect_err("query is rejected")
}

// -- defaults and paging --------------------------------------------------------

#[test]
fn an_absent_query_yields_the_default_page() {
    let query = parsed(None);
    assert_eq!(query.top, DEFAULT_PAGE_SIZE);
    assert_eq!(query.skip, 0);
    assert!(query.filter.is_none());
    assert!(query.select.is_empty());
    assert!(query.orderby.is_empty());
    assert_eq!(applied(None).len(), 3);
}

#[test]
fn top_and_skip_page_the_rows() {
    assert_eq!(applied(Some("$top=2")).len(), 2);
    assert_eq!(applied(Some("$skip=2")).len(), 1);
    assert_eq!(applied(Some("$top=2&$skip=1"))[0]["id"], json!("b"));
    assert!(applied(Some("$skip=9")).is_empty(), "skip past the end");
    assert!(applied(Some("$top=0")).is_empty());
}

#[test]
fn top_above_the_maximum_is_clamped() {
    let query = parsed(Some("$top=999"));
    assert_eq!(query.top, MAX_PAGE_SIZE);

    let error = error_of("$top=lots");
    assert!(error.detail().contains("$top"), "{error}");
    let error = error_of("$skip=-1");
    assert!(error.detail().contains("$skip"), "{error}");
}

#[test]
fn page_envelope_reports_the_limit_without_cursors() {
    let query = parsed(Some("$top=7"));
    let page = query.page(vec![1_u8, 2, 3]);
    assert_eq!(page.items, vec![1_u8, 2, 3]);
    assert_eq!(page.page_info.limit, 7);
    assert!(page.page_info.next_cursor.is_none());
    assert!(page.page_info.prev_cursor.is_none());
}

#[test]
fn unknown_dollar_parameters_are_rejected_and_plain_ones_ignored() {
    let error = error_of("$select2=alias");
    assert!(error.detail().contains("$select2"), "{error}");
    assert_eq!(parsed(Some("trace_id=abc&$top=1")).top, 1);
}

// -- $filter ------------------------------------------------------------------

#[test]
fn filter_compares_strings_numbers_and_booleans() {
    assert_eq!(
        applied(Some("$filter=alias eq 'api.openai.com'"))[0]["id"],
        json!("a")
    );
    assert_eq!(applied(Some("$filter=alias ne 'api.openai.com'")).len(), 2);
    assert_eq!(applied(Some("$filter=enabled eq true")).len(), 2);
    assert_eq!(applied(Some("$filter=enabled eq false")).len(), 1);
    assert_eq!(applied(Some("$filter=priority eq 3"))[0]["id"], json!("b"));
    assert!(applied(Some("$filter=priority eq 99")).is_empty());
    assert!(
        applied(Some("$filter=alias ne null")).len() == 3,
        "absent fields only match null"
    );
    let error = error_of("$filter=missing eq null");
    assert!(error.detail().contains("missing"), "{error}");
}

#[test]
fn filter_combines_clauses_with_and_or_and_parentheses() {
    let raw = "$filter=enabled eq true and priority eq 1";
    assert_eq!(applied(Some(raw))[0]["id"], json!("a"));

    let raw = "$filter=alias eq 'vendor.com' or alias eq 'api.vendor.com'";
    assert_eq!(applied(Some(raw)).len(), 2);

    // `and` binds tighter than `or`.
    let raw = "$filter=enabled eq false or enabled eq true and priority eq 1";
    let rows = applied(Some(raw));
    let ids: Vec<&str> = rows.iter().filter_map(|row| row["id"].as_str()).collect();
    assert_eq!(ids, vec!["a", "b"], "rows keep their input order");

    let raw = "$filter=(alias eq 'vendor.com' or alias eq 'api.openai.com') and enabled eq false";
    let rows = applied(Some(raw));
    let ids: Vec<&str> = rows.iter().filter_map(|row| row["id"].as_str()).collect();
    assert_eq!(ids, vec!["b"]);
}

#[test]
fn filter_rejects_unknown_fields_and_operators() {
    let error = error_of("$filter=secret eq 'x'");
    assert!(error.detail().contains("`$filter`"), "{error}");
    assert!(error.detail().contains("alias"), "{error}");

    for raw in [
        "$filter=alias gt 'x'",
        "$filter=alias contains 'x'",
        "$filter=alias eq",
        "$filter=eq 'x'",
        "$filter=alias eq 'x' and",
        "$filter=(alias eq 'x'",
        "$filter=alias eq unquoted",
        "$filter=alias eq 'x' extra",
        "$filter=",
        "$filter=alias eq 'unclosed",
    ] {
        assert!(
            ListQuery::parse(Some(raw), &CATALOG).is_err(),
            "{raw} must be rejected"
        );
    }
}

#[test]
fn filter_escapes_and_compares_the_type_alias() {
    let rows = vec![json!({"id": "p", "plugin_type": "gts.cf.core.oagw.guard_plugin.v1"})];
    let query = ListQuery::parse(
        Some("$filter=type eq 'gts.cf.core.oagw.guard_plugin.v1'"),
        &CATALOG,
    )
    .expect("query parses");
    assert_eq!(query.apply_to_rows(rows.clone())[0]["id"], json!("p"));

    let query = ListQuery::parse(
        Some("$filter=type ne 'gts.cf.core.oagw.guard_plugin.v1'"),
        &CATALOG,
    )
    .expect("query parses");
    assert!(query.apply_to_rows(rows).is_empty());

    // `''` escapes a quote inside a string literal.
    let quoted = vec![json!({"id": "q", "alias": "o'brien.example"})];
    let query = ListQuery::parse(Some("$filter=alias eq 'o''brien.example'"), &CATALOG)
        .expect("query parses");
    assert_eq!(query.apply_to_rows(quoted)[0]["id"], json!("q"));
}

#[test]
fn deeply_nested_filter_groups_are_rejected_instead_of_overflowing_the_stack() {
    // 32 levels of nesting still parse…
    let deep = "$filter=".to_owned() + &"(".repeat(32) + "alias eq 'a'" + &")".repeat(32);
    ListQuery::parse(Some(&deep), &CATALOG).expect("the documented depth budget");

    // …33 levels are a 400 rather than a stack overflow inside a handler.
    let deeper = "$filter=".to_owned() + &"(".repeat(33) + "alias eq 'a'" + &")".repeat(33);
    let error = error_of(&deeper);
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(error.detail().contains("deeper than 32"), "{error}");
}

#[test]
fn an_oversized_filter_is_rejected_before_it_is_parsed() {
    let mut raw = String::from("$filter=alias eq '");
    raw.push_str(&"a".repeat(9 * 1024));
    raw.push('\'');
    let error = error_of(&raw);
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("8192 bytes"), "{error}");

    // Just inside the budget parses fine.
    let mut inside = String::from("$filter=alias eq '");
    inside.push_str(&"a".repeat(8 * 1024 - "$filter=alias eq ''".len()));
    inside.push('\'');
    ListQuery::parse(Some(&inside), &CATALOG).expect("inside the budget");
}

// -- $orderby -----------------------------------------------------------------

#[test]
fn orderby_sorts_multi_key_with_directions() {
    let sorted = applied(Some("$orderby=enabled desc,priority asc"));
    let ids: Vec<&str> = sorted.iter().filter_map(|row| row["id"].as_str()).collect();
    assert_eq!(ids, vec!["a", "c", "b"]);

    let sorted = applied(Some("$orderby=alias desc"));
    let ids: Vec<&str> = sorted.iter().filter_map(|row| row["id"].as_str()).collect();
    assert_eq!(ids, vec!["b", "c", "a"]);
}

#[test]
fn orderby_is_stable_for_equal_keys_and_keeps_the_input_order() {
    let sorted = applied(Some("$orderby=enabled asc"));
    let ids: Vec<&str> = sorted.iter().filter_map(|row| row["id"].as_str()).collect();
    assert_eq!(ids, vec!["b", "a", "c"], "equal keys keep the input order");
}

#[test]
fn orderby_puts_absent_keys_last_and_rejects_unknown_directions() {
    let sorted = applied(Some("$orderby=plugin_type asc"));
    assert_eq!(sorted.len(), 3, "absent keys still return every row");

    let error = error_of("$orderby=alias sideways");
    assert!(error.detail().contains("sideways"), "{error}");
    let error = error_of("$orderby=server asc");
    assert!(error.detail().contains("`$orderby`"), "{error}");
}

// -- $select ------------------------------------------------------------------

#[test]
fn select_projects_the_requested_fields() {
    let projected = applied(Some("$select=alias,enabled"));
    assert_eq!(projected.len(), 3);
    assert_eq!(
        projected[0],
        json!({"alias": "api.openai.com", "enabled": true})
    );
    assert_eq!(
        projected[2],
        json!({"alias": "api.vendor.com", "enabled": true})
    );
}

#[test]
fn select_can_repeat_fields_and_may_be_combined_with_filter() {
    let raw = "$filter=enabled eq false&$select=id";
    assert_eq!(applied(Some(raw)), vec![json!({"id": "b"})]);

    let raw = "$select=id,id,alias";
    let projected = applied(Some(raw));
    assert_eq!(projected[0], json!({"id": "a", "alias": "api.openai.com"}));
}

#[test]
fn select_rejects_unknown_fields() {
    let error = error_of("$select=secret");
    assert!(error.detail().contains("`$select`"), "{error}");
    assert!(error.detail().contains("server"), "{error}");
}

// -- the typed round trip --------------------------------------------------------

#[test]
fn apply_round_trips_typed_items_through_json() {
    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Row {
        id: String,
        enabled: bool,
    }

    let items = vec![
        Row {
            id: "a".to_owned(),
            enabled: true,
        },
        Row {
            id: "b".to_owned(),
            enabled: false,
        },
    ];
    let query =
        ListQuery::parse(Some("$filter=enabled eq false&$top=1"), &CATALOG).expect("query parses");
    let page = query.apply(items).expect("round trip");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "b");
    assert!(!page.items[0].enabled);
    assert_eq!(page.page_info.limit, 1);
}
