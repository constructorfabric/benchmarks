//! Tests for the transport-only DTOs (list query parameters).

use std::collections::HashMap;

use super::{DEFAULT_TOP, ListParams, MAX_TOP};

fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn defaults_apply_when_the_parameters_are_absent() {
    let params = ListParams::from_query(&HashMap::new());
    assert_eq!(params, ListParams::new());
    assert_eq!(params.top, DEFAULT_TOP);
    assert_eq!(params.skip, 0);
    assert_eq!(ListParams::default(), ListParams::new());
}

#[test]
fn top_is_capped_and_skip_is_parsed() {
    assert_eq!(
        ListParams::from_query(&query(&[("$top", "2"), ("$skip", "3")])).top,
        2
    );
    assert_eq!(
        ListParams::from_query(&query(&[("$top", "2"), ("$skip", "3")])).skip,
        3
    );
    assert_eq!(
        ListParams::from_query(&query(&[("$top", "1000")])).top,
        MAX_TOP,
        "$top is capped at 100"
    );
    assert_eq!(ListParams::from_query(&query(&[("$top", "0")])).top, 0);
    assert_eq!(ListParams::from_query(&query(&[("$skip", "0")])).skip, 0);
}

#[test]
fn unsupported_options_and_bad_values_are_ignored() {
    let params = ListParams::from_query(&query(&[
        ("$filter", "alias eq 'api.openai.com'"),
        ("$select", "id,alias"),
        ("$orderby", "created_at desc"),
        ("$top", "not-a-number"),
        ("$skip", "-1"),
        ("ignored", "value"),
    ]));

    assert_eq!(
        params,
        ListParams::new(),
        "everything falls back to the default"
    );
}
