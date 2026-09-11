//! Unit tests for the OData list-query interpretation
//! (`cpt-cf-oagw-dod-upstream-management-odata-list`).

use uuid::Uuid;

use super::*;
use crate::domain::dto::{Endpoint, EndpointScheme, ServerConfig};
use crate::domain::gts_helpers::PROTOCOL_HTTP;

fn record(tenant_id: Uuid, alias: &str, enabled: bool) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id,
        alias: alias.to_owned(),
        protocol: PROTOCOL_HTTP.to_owned(),
        enabled,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: format!("backend.{alias}"),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn the_defaults_are_top_fifty_and_no_filter() {
    let query = ListQuery::parse(&pairs(&[])).expect("empty query parses");
    assert_eq!(query.top, 50, "`$top` defaults to 50");
    assert_eq!(query.skip, 0);
    assert!(query.filter.is_none());
    assert!(query.select.is_empty());
    assert!(query.orderby.is_none());
}

#[test]
fn top_is_capped_at_one_hundred_and_rejected_outside_the_range() {
    // `inst-um-lq-4`: out of range is rejected rather than clamped.
    let query = ListQuery::parse(&pairs(&[("$top", "100")])).expect("100 is the cap");
    assert_eq!(query.top, 100);
    let error = ListQuery::parse(&pairs(&[("$top", "101")])).expect_err("above the cap");
    assert!(error.to_string().contains("$top"), "`{error}` names the parameter");
    let error = ListQuery::parse(&pairs(&[("$top", "0")])).expect_err("below one");
    assert!(error.to_string().contains("$top"));
    let error = ListQuery::parse(&pairs(&[("$top", "ten")])).expect_err("not a number");
    assert!(error.to_string().contains("$top"));
}

#[test]
fn skip_is_a_non_negative_offset() {
    assert_eq!(
        ListQuery::parse(&pairs(&[("$skip", "7")])).expect("ok").skip,
        7
    );
    let error = ListQuery::parse(&pairs(&[("$skip", "-1")])).expect_err("negative");
    assert!(error.to_string().contains("$skip"), "`{error}` names the parameter");
    let error = ListQuery::parse(&pairs(&[("$skip", "x")])).expect_err("not a number");
    assert!(error.to_string().contains("$skip"));
}

#[test]
fn select_accepts_only_upstream_record_fields() {
    let query = ListQuery::parse(&pairs(&[("$select", "id, alias , enabled")])).expect("ok");
    assert_eq!(query.select, ["id", "alias", "enabled"]);
    let error = ListQuery::parse(&pairs(&[("$select", "secret")])).expect_err("unknown field");
    assert!(error.to_string().contains("$select"), "`{error}` names the parameter");
    let error = ListQuery::parse(&pairs(&[("$select", "id,,alias")])).expect_err("empty field");
    assert!(error.to_string().contains("$select"));
}

#[test]
fn orderby_takes_one_field_and_an_optional_direction() {
    let query = ListQuery::parse(&pairs(&[("$orderby", "alias desc")])).expect("ok");
    assert_eq!(
        query.orderby,
        Some(Ordering { field: "alias".to_owned(), direction: Direction::Descending })
    );
    let query = ListQuery::parse(&pairs(&[("$orderby", "alias")])).expect("ok");
    assert_eq!(query.orderby.as_ref().expect("ordering").direction, Direction::Ascending);
    for bad in ["secret", "alias sideways", "alias desc alias"] {
        let error = ListQuery::parse(&pairs(&[("$orderby", bad)]));
        assert!(error.is_err(), "`{bad}` is rejected");
        assert!(error.expect_err("named").to_string().contains("$orderby"));
    }
}

#[test]
fn a_filter_expression_compares_and_conjoins() {
    let query =
        ListQuery::parse(&pairs(&[("$filter", "alias eq 'api.vendor.com'")])).expect("ok");
    assert_eq!(
        query.filter,
        Some(Filter::Compare {
            field: "alias".to_owned(),
            comparison: Comparison::Equals,
            value: Literal::Text("api.vendor.com".to_owned()),
        })
    );
    let query = ListQuery::parse(&pairs(&[
        ("$filter", "alias eq 'a.vendor.com' and enabled eq true"),
    ]))
    .expect("conjunction");
    let Some(filter) = query.filter else { panic!("filter") };
    assert!(matches!(filter, Filter::And(_, _)));
}

#[test]
fn an_unparseable_filter_is_rejected_naming_the_parameter() {
    for bad in [
        "",
        "alias",
        "alias like 'x'",
        "secret eq 'x'",
        "startswith(alias)",
        "contains(alias, 5)",
        "enabled eq 'x'",
    ] {
        let error = ListQuery::parse(&pairs(&[("$filter", bad)]));
        assert!(error.is_err(), "`{bad}` is rejected");
        assert!(error.expect_err("named").to_string().contains("$filter"), "`{bad}`");
    }
}

#[test]
fn an_unknown_system_option_is_rejected() {
    let error = ListQuery::parse(&pairs(&[("$expand", "server")])).expect_err("unsupported");
    assert!(error.to_string().contains("$expand"), "`{error}` names the parameter");
}

#[test]
fn the_sequence_is_filter_order_offset_limit() {
    let tenant = Uuid::new_v4();
    let records = vec![
        record(tenant, "a.vendor.com", true),
        record(tenant, "b.vendor.com", false),
        record(tenant, "c.vendor.com", true),
        record(tenant, "d.vendor.com", true),
    ];
    let query = ListQuery::parse(&pairs(&[
        ("$filter", "enabled eq true"),
        ("$orderby", "alias asc"),
        ("$skip", "1"),
        ("$top", "2"),
    ]))
    .expect("ok");
    let applied: Vec<&str> = query.apply(&records).iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(applied, ["c.vendor.com", "d.vendor.com"], "filter, order, skip, top");
}

#[test]
fn a_descending_order_reverses_the_result() {
    let tenant = Uuid::new_v4();
    let records = vec![record(tenant, "a.vendor.com", true), record(tenant, "b.vendor.com", true)];
    let query = ListQuery::parse(&pairs(&[("$orderby", "alias desc")])).expect("ok");
    let applied: Vec<&str> = query.apply(&records).iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(applied, ["b.vendor.com", "a.vendor.com"]);
}

#[test]
fn a_ne_comparison_excludes_the_literal() {
    let tenant = Uuid::new_v4();
    let records = vec![record(tenant, "a.vendor.com", true), record(tenant, "b.vendor.com", true)];
    let query = ListQuery::parse(&pairs(&[("$filter", "alias ne 'a.vendor.com'")])).expect("ok");
    let applied: Vec<&str> = query.apply(&records).iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(applied, ["b.vendor.com"]);
}

#[test]
fn startswith_and_contains_match_the_alias() {
    let tenant = Uuid::new_v4();
    let records = vec![
        record(tenant, "api.vendor.com", true),
        record(tenant, "eu.vendor.com", true),
    ];
    let query = ListQuery::parse(&pairs(&[("$filter", "startswith(alias, 'api')")])).expect("ok");
    assert_eq!(query.apply(&records).len(), 1);
    let query = ListQuery::parse(&pairs(&[("$filter", "contains(alias, 'vendor')")])).expect("ok");
    assert_eq!(query.apply(&records).len(), 2);
}

#[test]
fn the_projection_names_are_kept_in_the_written_order() {
    let query = ListQuery::parse(&pairs(&[("$select", "alias,enabled")])).expect("ok");
    assert_eq!(query.select, ["alias", "enabled"]);
}
