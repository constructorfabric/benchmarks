//! The OData subset the list endpoints accept.

use super::*;

fn items() -> Vec<Value> {
    vec![
        serde_json::json!({"id": "1", "alias": "api.openai.com", "enabled": true, "priority": 10}),
        serde_json::json!({"id": "2", "alias": "api.stripe.com", "enabled": false, "priority": 5}),
        serde_json::json!({"id": "3", "alias": "vendor.com", "enabled": true, "priority": 20}),
    ]
}

fn aliases(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .map(|value| value["alias"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn an_empty_query_returns_everything() {
    let params = ListParams::from_query("").unwrap();
    let (page, total) = params.apply(items(), 50);
    assert_eq!(total, 3);
    assert_eq!(page.len(), 3);
}

#[test]
fn filter_eq_matches_a_string_field() {
    let params = ListParams::from_query("$filter=alias%20eq%20%27vendor.com%27").unwrap();
    let (page, total) = params.apply(items(), 50);
    assert_eq!(total, 1);
    assert_eq!(aliases(&page), ["vendor.com"]);
}

#[test]
fn filter_eq_matches_a_boolean_field() {
    let params = ListParams::from_query("$filter=enabled eq true").unwrap();
    let (page, _) = params.apply(items(), 50);
    assert_eq!(aliases(&page), ["api.openai.com", "vendor.com"]);
}

#[test]
fn filter_ne_excludes_matches_and_admits_absent_fields() {
    let params = ListParams::from_query("$filter=alias ne 'vendor.com'").unwrap();
    let (page, _) = params.apply(items(), 50);
    assert_eq!(aliases(&page), ["api.openai.com", "api.stripe.com"]);

    let params = ListParams::from_query("$filter=missing ne 'x'").unwrap();
    let (_, total) = params.apply(items(), 50);
    assert_eq!(total, 3);
}

#[test]
fn conjunctions_are_applied_together() {
    let params = ListParams::from_query("$filter=enabled eq true and alias ne 'vendor.com'").unwrap();
    let (page, _) = params.apply(items(), 50);
    assert_eq!(aliases(&page), ["api.openai.com"]);
}

#[test]
fn a_conjunction_separator_inside_a_literal_is_not_a_separator() {
    let values = vec![serde_json::json!({"alias": "a and b"})];
    let params = ListParams::from_query("$filter=alias eq 'a and b'").unwrap();
    let (_, total) = params.apply(values, 50);
    assert_eq!(total, 1);
}

#[test]
fn the_string_functions_are_supported() {
    for (query, expected) in [
        ("$filter=contains(alias,'openai')", vec!["api.openai.com"]),
        ("$filter=startswith(alias,'api.')", vec!["api.openai.com", "api.stripe.com"]),
        ("$filter=endswith(alias,'.com')", vec!["api.openai.com", "api.stripe.com", "vendor.com"]),
    ] {
        let params = ListParams::from_query(query).unwrap();
        let (page, _) = params.apply(items(), 50);
        assert_eq!(aliases(&page), expected, "{query}");
    }
}

#[test]
fn an_unsupported_operator_is_a_validation_error() {
    let err = ListParams::from_query("$filter=alias gt 'x'").unwrap_err();
    assert_eq!(err.status(), 400);
}

#[test]
fn select_projects_only_the_named_fields() {
    let params = ListParams::from_query("$select=id,alias").unwrap();
    let (page, _) = params.apply(items(), 50);
    let first = page[0].as_object().unwrap();
    assert_eq!(first.len(), 2);
    assert!(first.contains_key("id"));
    assert!(first.contains_key("alias"));
    assert!(!first.contains_key("enabled"));
}

#[test]
fn orderby_sorts_ascending_by_default_and_descending_on_request() {
    let params = ListParams::from_query("$orderby=priority").unwrap();
    let (page, _) = params.apply(items(), 50);
    assert_eq!(aliases(&page), ["api.stripe.com", "api.openai.com", "vendor.com"]);

    let params = ListParams::from_query("$orderby=priority desc").unwrap();
    let (page, _) = params.apply(items(), 50);
    assert_eq!(aliases(&page), ["vendor.com", "api.openai.com", "api.stripe.com"]);
}

#[test]
fn an_unsupported_sort_direction_is_a_validation_error() {
    assert_eq!(
        ListParams::from_query("$orderby=alias sideways").unwrap_err().status(),
        400
    );
}

#[test]
fn top_and_skip_page_the_result_and_total_counts_the_matches() {
    let params = ListParams::from_query("$orderby=alias&$top=1&$skip=1").unwrap();
    let (page, total) = params.apply(items(), 50);
    assert_eq!(total, 3, "total counts matches before pagination");
    assert_eq!(aliases(&page), ["api.stripe.com"]);
}

#[test]
fn a_nonnumeric_top_or_skip_is_a_validation_error() {
    assert_eq!(ListParams::from_query("$top=lots").unwrap_err().status(), 400);
    assert_eq!(ListParams::from_query("$skip=-1").unwrap_err().status(), 400);
}

#[test]
fn the_default_page_size_applies_when_top_is_absent() {
    let params = ListParams::from_query("").unwrap();
    let (page, total) = params.apply(items(), 2);
    assert_eq!(total, 3);
    assert_eq!(page.len(), 2);
}

#[test]
fn unknown_query_parameters_are_ignored() {
    // Callers routinely append tracing or cache-busting parameters.
    let params = ListParams::from_query("cacheBust=1&$top=1").unwrap();
    let (page, _) = params.apply(items(), 50);
    assert_eq!(page.len(), 1);
}

#[test]
fn quoted_literals_unescape_doubled_quotes() {
    let values = vec![serde_json::json!({"alias": "it's"})];
    let params = ListParams::from_query("$filter=alias eq 'it''s'").unwrap();
    let (_, total) = params.apply(values, 50);
    assert_eq!(total, 1);
}
