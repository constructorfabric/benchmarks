//! Route List OData Query Parameters (`cpt-cf-oagw-dod-route-list-query`).
//!
//! A minimal `$filter`/`$select`/`$orderby`/`$top`/`$skip` implementation
//! against the tenant-scoped, already-filtered `Vec<Route>` the list
//! handler assembles -- applied strictly *after* tenant scoping, per
//! `cpt-cf-oagw-dod-route-tenant-scope`.
//!
//! `$top`/`$skip` parsing itself is delegated to the shared
//! `super::super::page_params` module so this endpoint and
//! `GET /oagw/v1/upstreams` cannot re-diverge on malformed input (RF-005):
//! both now reject a present-but-unparsable value with `400` rather than
//! this endpoint's previous silent fall-back to the default.

use serde::Deserialize;
use serde_json::Value;

use crate::api::rest::page_params::{self, PageParams};
use crate::model::route::Route;

/// `GET /oagw/v1/routes` query-string bindings.
#[derive(Debug, Default, Deserialize)]
pub struct RouteListQuery {
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    #[serde(rename = "$select")]
    pub select: Option<String>,
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    #[serde(rename = "$top")]
    pub top: Option<String>,
    #[serde(rename = "$skip")]
    pub skip: Option<String>,
}

/// `$top`/`$skip`, defaulted/capped/rejected per the shared contract
/// (`cpt-cf-oagw-dod-route-list-query`). A present-but-unparsable value is
/// rejected with an `Err` naming the offending parameter -- the caller
/// renders this as `400`, matching the upstreams endpoint's behaviour.
pub fn resolve_page(top: Option<&str>, skip: Option<&str>) -> Result<PageParams, String> {
    page_params::parse_page_params(top, skip)
}

/// One parsed `field eq value` `$filter` clause (the only grammar this
/// minimal implementation supports).
struct EqClause {
    field: String,
    value: String,
}

fn parse_eq_clause(raw: &str) -> Option<EqClause> {
    let mut parts = raw.trim().splitn(3, ' ');
    let field = parts.next()?.trim();
    let op = parts.next()?.trim();
    let value = parts.next()?.trim();
    if !op.eq_ignore_ascii_case("eq") {
        return None;
    }
    let value = value.trim_matches('\'').to_owned();
    Some(EqClause {
        field: field.to_owned(),
        value,
    })
}

fn route_field_as_string(route: &Route, field: &str) -> Option<String> {
    match field {
        "id" => route.id.map(|id| id.to_string()),
        "upstream_id" => Some(route.upstream_id.to_string()),
        "enabled" => Some(route.enabled.to_string()),
        "priority" => route.priority.map(|p| p.to_string()),
        _ => None,
    }
}

/// Apply a single supported `field eq 'value'` clause. An unrecognized
/// field or grammar leaves the input set unfiltered rather than failing the
/// request -- this feature's testable surface is tenant scoping surviving
/// `$filter`, not a full `OData` filter grammar.
#[must_use]
pub fn apply_filter(routes: Vec<Route>, filter: Option<&str>) -> Vec<Route> {
    let Some(filter) = filter else {
        return routes;
    };
    let Some(clause) = parse_eq_clause(filter) else {
        return routes;
    };
    routes
        .into_iter()
        .filter(|route| {
            route_field_as_string(route, &clause.field).as_deref() == Some(clause.value.as_str())
        })
        .collect()
}

/// One parsed `field [asc|desc]` `$orderby` clause.
#[must_use]
pub fn apply_orderby(mut routes: Vec<Route>, orderby: Option<&str>) -> Vec<Route> {
    let Some(orderby) = orderby else {
        return routes;
    };
    let mut parts = orderby.split_whitespace();
    let Some(field) = parts.next() else {
        return routes;
    };
    let descending = parts.next().is_some_and(|d| d.eq_ignore_ascii_case("desc"));

    routes.sort_by(|a, b| {
        let ka = route_field_as_string(a, field).unwrap_or_default();
        let kb = route_field_as_string(b, field).unwrap_or_default();
        if descending { kb.cmp(&ka) } else { ka.cmp(&kb) }
    });
    routes
}

/// `$skip`/`$top` pagination, applied last.
#[must_use]
pub fn paginate(routes: Vec<Route>, skip: usize, top: usize) -> Vec<Route> {
    routes.into_iter().skip(skip).take(top).collect()
}

/// `$select` projection, delegated to `toolkit::api::select` so this
/// module's own logic stays limited to `$filter`/`$orderby`/`$top`/`$skip`.
#[must_use]
pub fn apply_select(route: &Route, select: Option<&[String]>) -> Value {
    toolkit::api::select::apply_select(route, select)
}

/// Parse a comma-separated `$select` field list.
#[must_use]
pub fn parse_select_fields(select: Option<&str>) -> Option<Vec<String>> {
    select.map(|s| s.split(',').map(|f| f.trim().to_owned()).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::route::{HttpMatch, PathSuffixMode, RouteMatch};
    use uuid::Uuid;

    fn route(upstream_id: uuid::Uuid, priority: i64, enabled: bool) -> Route {
        Route {
            id: Some(Uuid::new_v4()),
            tenant_id: Uuid::new_v4(),
            tags: Vec::new(),
            upstream_id,
            route_match: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![crate::model::route::HttpMethod::Get],
                    path: "/p".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled,
            priority: Some(priority),
        }
    }

    #[test]
    fn resolve_page_top_defaults_to_50() {
        assert_eq!(resolve_page(None, None).unwrap().top, 50);
    }

    #[test]
    fn resolve_page_top_clamps_to_100() {
        assert_eq!(resolve_page(Some("500"), None).unwrap().top, 100);
    }

    #[test]
    fn resolve_page_rejects_a_malformed_top() {
        assert!(resolve_page(Some("not-a-number"), None).is_err());
    }

    #[test]
    fn resolve_page_skip_defaults_to_zero() {
        assert_eq!(resolve_page(None, None).unwrap().skip, 0);
    }

    #[test]
    fn resolve_page_skip_parses_a_valid_value() {
        assert_eq!(resolve_page(None, Some("5")).unwrap().skip, 5);
    }

    #[test]
    fn resolve_page_rejects_a_malformed_skip() {
        assert!(resolve_page(None, Some("not-a-number")).is_err());
    }

    #[test]
    fn apply_filter_matches_an_eq_clause_on_enabled() {
        let routes = vec![
            route(Uuid::new_v4(), 1, true),
            route(Uuid::new_v4(), 2, false),
        ];
        let filtered = apply_filter(routes, Some("enabled eq true"));
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].enabled);
    }

    #[test]
    fn apply_filter_matches_upstream_id() {
        let upstream_id = Uuid::new_v4();
        let routes = vec![route(upstream_id, 1, true), route(Uuid::new_v4(), 2, true)];
        let filtered = apply_filter(routes, Some(&format!("upstream_id eq '{upstream_id}'")));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].upstream_id, upstream_id);
    }

    #[test]
    fn apply_filter_is_a_noop_when_absent() {
        let routes = vec![route(Uuid::new_v4(), 1, true)];
        assert_eq!(apply_filter(routes.clone(), None).len(), routes.len());
    }

    #[test]
    fn apply_orderby_sorts_by_priority_descending() {
        let routes = vec![
            route(Uuid::new_v4(), 1, true),
            route(Uuid::new_v4(), 5, true),
        ];
        let ordered = apply_orderby(routes, Some("priority desc"));
        assert_eq!(ordered[0].priority, Some(5));
    }

    #[test]
    fn paginate_honors_skip_and_top() {
        let routes: Vec<Route> = (0..5).map(|i| route(Uuid::new_v4(), i, true)).collect();
        let page = paginate(routes, 2, 2);
        assert_eq!(page.len(), 2);
    }

    #[test]
    fn parse_select_fields_splits_on_commas() {
        let fields = parse_select_fields(Some("id, upstream_id")).unwrap();
        assert_eq!(fields, vec!["id".to_owned(), "upstream_id".to_owned()]);
    }
}
