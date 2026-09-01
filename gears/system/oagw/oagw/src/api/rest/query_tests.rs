//! Tests for the `OData` list-query semantics (DESIGN §3.3 "List Query
//! Parameters"): `$filter`, `$orderby`, `$select`, `$top` (default 50, max 100)
//! and `$skip`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{ListQuery, parse_filter, CompareOperator, FilterExpr, FilterLiteral};
use serde_json::json;

fn items() -> Vec<serde_json::Value> {
    vec![
        json!({ "id": "gts.cf.core.oagw.upstream.v1~1", "alias": "api.openai.com", "enabled": true, "priority": 2 }),
        json!({ "id": "gts.cf.core.oagw.upstream.v1~2", "alias": "example.org", "enabled": false, "priority": 1 }),
        json!({ "id": "gts.cf.core.oagw.upstream.v1~3", "alias": "vendor.com", "enabled": true, "priority": 1 }),
    ]
}

fn aliases(items: &[serde_json::Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| item["alias"].as_str())
        .map(str::to_owned)
        .collect()
}

// --- parsing ----------------------------------------------------------------

#[test]
fn defaults_are_top_50_skip_0() {
    let query = ListQuery::parse(None).unwrap();
    assert_eq!(query.top, 50);
    assert_eq!(query.skip, 0);
    assert!(query.filter.is_none());
    assert!(query.select.is_none());
    assert!(query.orderby.is_empty());
}

#[test]
fn parses_every_supported_option() {
    let query = ListQuery::parse(Some(
        "%24filter=alias%20eq%20%27a%27&%24orderby=created_at%20desc&%24select=alias%2Cid&%24top=3&%24skip=2",
    ))
    .unwrap();
    assert_eq!(query.filter.as_deref(), Some("alias eq 'a'"));
    assert_eq!(query.select.as_deref(), Some(&["alias".to_owned(), "id".to_owned()][..]));
    assert_eq!(query.top, 3);
    assert_eq!(query.skip, 2);
    assert!(!query.orderby.is_empty());
}

#[test]
fn rejects_unknown_options_and_out_of_range_top() {
    let err = ListQuery::parse(Some("%24count=true")).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail().contains("unknown list query option"), "{err:?}");

    let err = ListQuery::parse(Some("%24top=101")).unwrap_err();
    assert!(err.detail().contains("$top must be between"), "{err:?}");

    let err = ListQuery::parse(Some("%24top=0")).unwrap_err();
    assert_eq!(err.status(), 400);

    // `$skip` is a plain non-negative offset.
    assert_eq!(ListQuery::parse(Some("%24skip=0")).unwrap().skip, 0);
    let err = ListQuery::parse(Some("%24skip=-1")).unwrap_err();
    assert_eq!(err.status(), 400);
}

// --- $filter -----------------------------------------------------------------

#[test]
fn filter_supports_equality_and_comparison() {
    let query = ListQuery::parse(Some("%24filter=alias%20eq%20%27example.org%27")).unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(aliases(&kept), vec!["example.org".to_owned()]);

    let query = ListQuery::parse(Some("%24filter=priority%20ge%202")).unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(aliases(&kept), vec!["api.openai.com".to_owned()]);

    let query = ListQuery::parse(Some("%24filter=enabled%20ne%20true")).unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(aliases(&kept), vec!["example.org".to_owned()]);
}

#[test]
fn filter_accepts_both_id_spellings() {
    let gts = "gts.cf.core.oagw.upstream.v1~2";
    let query = ListQuery::parse(Some(&format!("%24filter=id%20eq%20%27{gts}%27"))).unwrap();
    assert_eq!(aliases(&query.apply(items()).unwrap()), vec!["example.org".to_owned()]);

    // The bare UUID resolves to the same row.
    let query = ListQuery::parse(Some("%24filter=id%20eq%20%272%27")).unwrap();
    assert_eq!(aliases(&query.apply(items()).unwrap()), vec!["example.org".to_owned()]);
}

#[test]
fn filter_supports_and_or_not() {
    let query = ListQuery::parse(Some(
        "%24filter=priority%20eq%201%20and%20enabled%20eq%20false",
    ))
    .unwrap();
    assert_eq!(aliases(&query.apply(items()).unwrap()), vec!["example.org".to_owned()]);

    let query = ListQuery::parse(Some(
        "%24filter=alias%20eq%20%27example.org%27%20or%20alias%20eq%20%27vendor.com%27",
    ))
    .unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(aliases(&kept).len(), 2);

    let query = ListQuery::parse(Some("%24filter=not%20enabled%20eq%20true")).unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(aliases(&kept), vec!["example.org".to_owned()]);
}

#[test]
fn malformed_filters_are_rejected_with_400() {
    for text in ["", "alias eq", "alias eq 'x' and", "alias foo 'x'", "alias eq '"] {
        let err = parse_filter(text).unwrap_err();
        assert_eq!(err.status(), 400, "{text}: {err:?}");
    }
    let err = parse_filter("alias eq 'x' and").unwrap_err();
    assert!(err.detail().contains("$filter"), "{err:?}");
}

#[test]
fn filter_operators_cover_the_full_set() {
    let expr = |text: &str| parse_filter(text).unwrap();
    assert!(expr("priority gt 1").matches(&json!({"priority": 3})));
    assert!(!expr("priority gt 3").matches(&json!({"priority": 3})));
    assert!(expr("priority ge 3").matches(&json!({"priority": 3})));
    assert!(expr("priority lt 3").matches(&json!({"priority": 1})));
    assert!(expr("priority le 3").matches(&json!({"priority": 3})));

    // Missing fields never match an equality filter.
    assert!(!expr("alias eq 'x'").matches(&json!({})));
    assert!(expr("alias eq null").matches(&json!({"alias": null})));
}

// --- $orderby / paging --------------------------------------------------------

#[test]
fn orderby_sorts_ascending_and_descending() {
    let query = ListQuery::parse(Some("%24orderby=alias")).unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(
        aliases(&kept),
        vec!["api.openai.com".to_owned(), "example.org".to_owned(), "vendor.com".to_owned()]
    );

    let query = ListQuery::parse(Some("%24orderby=alias%20desc")).unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(
        aliases(&kept),
        vec!["vendor.com".to_owned(), "example.org".to_owned(), "api.openai.com".to_owned()]
    );
}

#[test]
fn orderby_is_stable_across_ties() {
    let query = ListQuery::parse(Some("%24orderby=priority")).unwrap();
    let kept = query.apply(items()).unwrap();
    // Both `example.org` and `vendor.com` have priority 1 and keep their input
    // order, so paging over a tie is repeatable.
    assert_eq!(aliases(&kept), vec!["example.org".to_owned(), "vendor.com".to_owned(), "api.openai.com".to_owned()]);
}

#[test]
fn top_and_skip_page_the_sorted_result() {
    let query = ListQuery::parse(Some("%24orderby=alias&%24top=2&%24skip=1")).unwrap();
    let kept = query.apply(items()).unwrap();
    assert_eq!(aliases(&kept), vec!["example.org".to_owned(), "vendor.com".to_owned()]);

    // The last page has no cursor, the offset is reported backwards.
    let page = query.page(kept, 3);
    assert_eq!(page.page_info.next_cursor, None);
    assert_eq!(page.page_info.prev_cursor.as_deref(), Some("0"));
    assert_eq!(page.page_info.limit, 2);
    assert_eq!(page.items.len(), 2);

    // A page that stops before the end carries the next offset.
    let first = ListQuery::parse(Some("%24orderby=alias&%24top=2")).unwrap();
    let page = first.page(first.apply(items()).unwrap(), 3);
    assert_eq!(page.page_info.next_cursor.as_deref(), Some("2"));
}

#[test]
fn last_page_has_no_next_cursor() {
    let query = ListQuery::parse(Some("%24top=10")).unwrap();
    let kept = query.apply(items()).unwrap();
    let page = query.page(kept, 3);
    assert!(page.page_info.next_cursor.is_none());
    assert!(page.page_info.prev_cursor.is_none());
}

// --- $select -----------------------------------------------------------------

#[test]
fn select_drops_unselected_members_but_keeps_filtering() {
    let query = ListQuery::parse(Some(
        "%24filter=alias%20eq%20%27example.org%27&%24select=alias",
    ))
    .unwrap();
    let kept = query.apply(items()).unwrap();
    let projected: Vec<serde_json::Value> = kept
        .iter()
        .map(|item| crate::api::rest::common::project_value(item, &query))
        .collect();
    assert_eq!(projected.len(), 1);
    assert!(projected[0].get("id").is_none(), "select dropped `id`");
    assert_eq!(projected[0]["alias"], "example.org");
}

// --- expression tree -----------------------------------------------------------

#[test]
fn filter_expressions_are_debug_and_clone_safe() {
    let expr = FilterExpr::Compare {
        path: "alias".to_owned(),
        operator: CompareOperator::Equal,
        literal: FilterLiteral::Text("example.org".to_owned()),
    };
    assert!(expr.matches(&json!({"alias": "example.org"})));
    assert!(!expr.matches(&json!({"alias": "other"})));
    assert!(!format!("{expr:?}").is_empty());
}

#[test]
fn validation_errors_carry_the_offending_option() {
    let err = ListQuery::parse(Some("%24unknown=1")).unwrap_err();
    let extensions = err.extensions();
    assert_eq!(extensions.invalid_value.as_deref(), Some("$unknown"));
    assert_eq!(err.status(), 400);
}

#[test]
fn list_query_rejects_malformed_orderby() {
    // An empty `$orderby` is tolerated (no ordering), a bogus direction is not.
    let empty = ListQuery::parse(Some("%24orderby=%20")).unwrap();
    assert!(empty.orderby.is_empty());
    assert!(ListQuery::parse(Some("%24orderby=alias%20sideways")).is_err());
    let ok = ListQuery::parse(Some("%24orderby=created_at")).unwrap();
    assert!(!ok.orderby.is_empty());
}
