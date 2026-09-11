//! List-page assembly over the platform's parsed `OData` query.
//!
//! The repositories hand back whole resource vectors; this module narrows them
//! to a page: evaluate `$filter`, sort by `$orderby`, project `$select`, take
//! `$top`. The returned body is the documented `{ items, total_count }` shape,
//! where `total_count` counts the matches before the page was cut.

use serde_json::Value;
use toolkit_odata::ODataQuery;

use crate::api::rest::odata;

/// Narrow `records` into the page the query asks for.
///
/// The order of operations is the `OData` one: filter, order, project, page.
#[must_use]
pub fn page(records: Vec<Value>, query: &ODataQuery) -> (Vec<Value>, u64) {
    let mut matching: Vec<Value> = records
        .into_iter()
        .filter(|record| filter_matches(query, record))
        .collect();
    let total_count = u64::try_from(matching.len()).unwrap_or(u64::MAX);
    odata::order(&mut matching, &query.order);
    let records: Vec<Value> = matching
        .into_iter()
        .map(|record| odata::project(&record, query.select.as_deref()))
        .collect();
    let take = usize::try_from(odata::page_size(query.limit)).unwrap_or(records.len());
    (records.into_iter().take(take).collect(), total_count)
}

fn filter_matches(query: &ODataQuery, record: &Value) -> bool {
    match query.filter() {
        Some(expr) => odata::evaluate(expr, record),
        None => true,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod records_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    fn records() -> Vec<Value> {
        vec![
            json!({"alias": "a.partner.com", "name": "A", "priority": 2}),
            json!({"alias": "b.partner.com", "name": "B", "priority": 1}),
        ]
    }

    fn filter(raw: &str) -> ODataQuery {
        let parsed = toolkit_odata::parse_filter_string(raw).expect("parsable filter");
        ODataQuery::new().with_filter(parsed.into_expr())
    }

    #[test]
    fn without_options_everything_is_returned() {
        let (items, total) = page(records(), &ODataQuery::new());
        assert_eq!(total, 2);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn a_filter_narrows_both_counts() {
        let (items, total) = page(records(), &filter("alias eq 'a.partner.com'"));
        assert_eq!(total, 1);
        assert_eq!(items[0]["name"], "A");
    }

    #[test]
    fn orderby_sorts_and_top_cuts() {
        let order = toolkit::api::odata::parse_orderby("priority asc").expect("parsable order");
        let query = ODataQuery::new().with_order(order).with_limit(1);
        let (items, total) = page(records(), &query);
        assert_eq!(total, 2);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "B");
    }

    #[test]
    fn select_projects_items_only() {
        let query = ODataQuery::new().with_select(vec!["alias".to_owned()]);
        let (items, _) = page(records(), &query);
        assert_eq!(items[0], json!({"alias": "a.partner.com"}));
    }
}
