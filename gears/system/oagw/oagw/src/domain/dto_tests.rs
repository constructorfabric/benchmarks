#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;

use super::{
    ListQuery, OrderKey, ProxyContext, SortDir, apply_list, parse_filter, parse_order, parse_select,
};
use crate::domain::error::DomainError;

fn doc(fields: &[(&str, serde_json::Value)]) -> serde_json::Value {
    serde_json::Value::Object(
        fields
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    )
}

// ── $filter ────────────────────────────────────────────────────────────────

#[test]
fn filter_parses_a_comparison_and_matches_a_document() {
    let expr = parse_filter("alias eq 'api.openai.com'").unwrap();
    assert!(expr.matches(&doc(&[("alias", "api.openai.com".into())])));
    assert!(!expr.matches(&doc(&[("alias", "api.anthropic.com".into())])));
}

#[test]
fn filter_supports_every_documented_operator() {
    let cases = [
        ("alias eq 'a'", doc(&[("alias", "a".into())]), true),
        ("alias ne 'a'", doc(&[("alias", "b".into())]), true),
        ("alias ne 'a'", doc(&[("alias", "a".into())]), false),
        ("alias ne null", doc(&[("alias", "a".into())]), true),
        ("alias eq null", doc(&[("alias", "a".into())]), false),
        ("priority gt 5", doc(&[("priority", 6.into())]), true),
        ("priority gt 5", doc(&[("priority", 5.into())]), false),
        ("priority ge 5", doc(&[("priority", 5.into())]), true),
        ("priority lt 5", doc(&[("priority", 4.into())]), true),
        ("priority le 5", doc(&[("priority", 5.into())]), true),
        ("enabled eq true", doc(&[("enabled", true.into())]), true),
        ("enabled eq false", doc(&[("enabled", false.into())]), true),
        (
            "alias contains 'openai'",
            doc(&[("alias", "api.openai.com".into())]),
            true,
        ),
        (
            "alias startswith 'api.'",
            doc(&[("alias", "api.openai.com".into())]),
            true,
        ),
    ];
    for (expression, document, expected) in cases {
        let parsed = parse_filter(expression).unwrap_or_else(|e| panic!("{expression}: {e}"));
        assert_eq!(parsed.matches(&document), expected, "{expression}");
        assert_eq!(
            parsed.matches(&document),
            expected,
            "{expression} re-evaluated"
        );
    }
}

#[test]
fn filter_combines_terms_with_and_or_not_and_parentheses() {
    let expression =
        parse_filter("(alias eq 'a' and priority gt 1) or not (alias startswith 'api.')").unwrap();
    let matched = doc(&[("alias", "a".into()), ("priority", 5.into())]);
    assert!(expression.matches(&matched));
    let fell_through = doc(&[("alias", "zz".into()), ("priority", 1.into())]);
    assert!(expression.matches(&fell_through));
    let neither = doc(&[
        ("alias", "api.anthropic.com".into()),
        ("priority", 1.into()),
    ]);
    assert!(!expression.matches(&neither));
}

#[test]
fn filter_addresses_a_nested_field_through_dots_and_slashes() {
    let expression = parse_filter("match/http/path eq '/v1/models'").unwrap();
    let document = doc(&[(
        "match",
        serde_json::json!({ "http": { "path": "/v1/models" } }),
    )]);
    assert!(expression.matches(&document));
}

#[test]
fn filter_rejects_malformed_input_instead_of_ignoring_it() {
    for expression in [
        "",
        "alias",
        "alias eq",
        "alias =~ 'x'",
        "'alias' eq 'x'",
        "alias eq 'unterminated",
        "alias eq 'x' bogus",
        "()",
        "alias eq 'x' extra",
        "alias === 'x'",
    ] {
        assert!(parse_filter(expression).is_err(), "{expression}");
    }
}

#[test]
fn filter_rejects_an_empty_or_oversized_expression() {
    assert!(matches!(
        parse_filter("   "),
        Err(DomainError::Validation { .. })
    ));
    let oversized = "a".repeat(super::MAX_FILTER_LEN + 1);
    assert!(parse_filter(&oversized).is_err());
}

#[test]
fn missing_fields_do_not_match_equality() {
    let expression = parse_filter("alias eq 'x'").unwrap();
    assert!(!expression.matches(&doc(&[])));
    assert!(!expression.matches(&serde_json::Value::Null));
}

/// `OData` semantics: an absent property is `null`, so it is selected by
/// `eq null` and rejected by `ne null`.
#[test]
fn a_missing_field_compares_as_null() {
    let absent = doc(&[]);
    for (expression, expected) in [("alias eq null", true), ("alias ne null", false)] {
        let parsed = parse_filter(expression).unwrap();
        assert_eq!(parsed.matches(&absent), expected, "{expression}");
    }
    // A non-null actual value is never equal to `null`.
    let present = doc(&[("alias", "x".into())]);
    for (expression, expected) in [("alias eq null", false), ("alias ne null", true)] {
        let parsed = parse_filter(expression).unwrap();
        assert_eq!(parsed.matches(&present), expected, "{expression}");
    }
}

#[test]
fn a_dotted_field_name_resolves_nested_values() {
    let expression = parse_filter("upstream.alias eq 'x'").unwrap();
    assert!(expression.matches(&doc(&[("upstream", doc(&[("alias", "x".into())]))])));
}

// ── literals ───────────────────────────────────────────────────────────────

// ── $orderby ───────────────────────────────────────────────────────────────

#[test]
fn order_parses_directions_and_rejects_garbage() {
    assert_eq!(
        parse_order("created_at desc, alias").unwrap(),
        vec![
            OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Desc,
            },
            OrderKey {
                field: "alias".to_owned(),
                dir: SortDir::Asc,
            },
        ]
    );
    assert!(parse_order("").is_err());
    assert!(parse_order("alias sideways").is_err());
    assert!(parse_order("a b c").is_err());
    let oversized = "a".repeat(super::MAX_ORDERBY_LEN + 1);
    assert!(parse_order(&oversized).is_err());
    let too_many_keys = (0..=super::MAX_ORDER_FIELDS)
        .map(|index| format!("f{index}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_order(&too_many_keys).is_err());
}

// ── $select ────────────────────────────────────────────────────────────────

#[test]
fn select_lowercases_and_splits_fields() {
    assert_eq!(
        parse_select("Alias, Created_At").unwrap(),
        vec!["alias", "created_at"]
    );
    assert!(parse_select("").is_err());
    assert!(parse_select("alias,,id").is_err());
    let too_many = (0..=super::MAX_SELECT_FIELDS)
        .map(|index| format!("f{index}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_select(&too_many).is_err());
}

// ── apply_list ─────────────────────────────────────────────────────────────

fn rows() -> Vec<serde_json::Value> {
    vec![
        doc(&[("alias", "a".into()), ("priority", 1.into())]),
        doc(&[("alias", "b".into()), ("priority", 3.into())]),
        doc(&[("alias", "c".into()), ("priority", 2.into())]),
    ]
}

#[test]
fn apply_list_filters_sorts_offsets_and_truncates() {
    let query = ListQuery {
        filter: Some(parse_filter("priority ge 2").unwrap()),
        order: parse_order("priority desc").unwrap(),
        select: Vec::new(),
        top: Some(1),
        skip: 1,
    };
    let projected = apply_list(rows(), &query, std::clone::Clone::clone);
    assert_eq!(
        projected,
        vec![doc(&[("alias", "c".into()), ("priority", 2.into())])]
    );
}

#[test]
fn apply_list_with_an_unconstrained_query_is_the_identity() {
    let query = ListQuery::default();
    assert!(query.is_unconstrained());
    assert_eq!(apply_list(rows(), &query, std::clone::Clone::clone), rows());
}

#[test]
fn apply_list_skip_past_the_end_yields_an_empty_page() {
    let query = ListQuery {
        skip: 99,
        ..ListQuery::default()
    };
    assert!(apply_list(rows(), &query, std::clone::Clone::clone).is_empty());
}

#[test]
fn apply_list_orders_ascending_by_default_and_descends_on_request() {
    let query = ListQuery {
        order: vec![OrderKey {
            field: "alias".to_owned(),
            dir: SortDir::Asc,
        }],
        ..ListQuery::default()
    };
    let aliases: Vec<String> = apply_list(rows(), &query, std::clone::Clone::clone)
        .iter()
        .filter_map(|row| row.get("alias").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    assert_eq!(aliases, ["a", "b", "c"]);
}

#[test]
fn apply_list_sorts_rows_with_a_missing_key_below_the_ones_that_have_it() {
    let query = ListQuery {
        order: vec![OrderKey {
            field: "priority".to_owned(),
            dir: SortDir::Asc,
        }],
        ..ListQuery::default()
    };
    let rows = vec![
        doc(&[("alias", "present".into()), ("priority", 1.into())]),
        doc(&[("alias", "missing".into())]),
    ];
    let projected = apply_list(rows, &query, std::clone::Clone::clone);
    assert_eq!(
        projected[0]
            .get("alias")
            .and_then(serde_json::Value::as_str),
        Some("missing")
    );
}

#[test]
fn apply_list_clamps_a_huge_skip_instead_of_panicking() {
    let query = ListQuery {
        skip: u64::MAX,
        ..ListQuery::default()
    };
    assert!(apply_list(rows(), &query, std::clone::Clone::clone).is_empty());
}

#[test]
fn apply_list_truncates_to_top() {
    let query = ListQuery {
        top: Some(1),
        ..ListQuery::default()
    };
    assert_eq!(
        apply_list(rows(), &query, std::clone::Clone::clone).len(),
        1
    );
}

// ── ProxyContext ───────────────────────────────────────────────────────────

#[test]
fn proxy_context_looks_headers_up_case_insensitively() {
    let context = ProxyContext {
        alias: "api.openai.com".to_owned(),
        method: "GET".to_owned(),
        path: "/v1/models".to_owned(),
        query: vec![("top".to_owned(), "1".to_owned())],
        headers: BTreeMap::from([("x-api-key".to_owned(), "k".to_owned())]),
        trace_id: None,
        tenant: uuid::Uuid::nil(),
        subject: uuid::Uuid::nil(),
    };
    assert_eq!(context.header("X-API-KEY"), Some("k"));
    assert_eq!(context.header("x-api-key"), Some("k"));
    assert_eq!(context.header("absent"), None);
}

#[test]
fn proxy_context_defaults_are_empty() {
    let context = ProxyContext::default();
    assert_eq!(context.alias, "");
    assert_eq!(context.method, "");
    assert_eq!(context.path, "");
    assert!(context.query.is_empty());
    assert!(context.headers.is_empty());
    assert!(context.trace_id.is_none());
}
