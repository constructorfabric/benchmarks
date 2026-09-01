//! Request extractors / list-query parsing for the OAGW REST surface.
//!
//! List endpoints support the OData-style query parameters from DESIGN §5:
//! `$filter`, `$select`, `$orderby`, `$top`, `$skip`. `$skip` is not
//! expressible through the platform's typed query-toolbox, so a small custom
//! parser lives here.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::Serialize;

use crate::domain::dto::{Plugin, Route, Upstream};
use crate::domain::services::management::normalize_path;

/// Parsed list-query parameters.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// `$filter` expression (`OData` light: `field eq 'value'`).
    pub filter: Option<String>,
    /// `$select` — comma-separated field names to project.
    pub select: Option<Vec<String>>,
    /// `$orderby` — e.g. `alias asc`.
    pub orderby: Option<String>,
    /// `$top` — max results.
    pub top: Option<usize>,
    /// `$skip` — offset.
    pub skip: Option<usize>,
}

impl ListQuery {
    /// Build from a raw query-parameter map.
    #[must_use]
    pub fn from_params(params: &HashMap<String, String>) -> Self {
        let parse_usize = |k: &str| params.get(k).and_then(|v| v.trim().parse::<usize>().ok());
        Self {
            filter: params.get("$filter").cloned().filter(|s| !s.is_empty()),
            select: params
                .get("$select")
                .filter(|s| !s.is_empty())
                .map(|s| s.split(',').map(|p| p.trim().to_owned()).collect()),
            orderby: params.get("$orderby").cloned().filter(|s| !s.is_empty()),
            top: parse_usize("$top"),
            skip: parse_usize("$skip"),
        }
    }

    /// Apply the query to a list of items. `keep` is the `$filter` predicate
    /// (returns `true` to retain), `cmp` the `$orderby` comparator.
    ///
    /// Returns projected `serde_json::Value` items so `$select` can prune
    /// fields without coupling the transport to per-resource serde types.
    #[must_use]
    pub fn apply<T>(
        &self,
        items: Vec<T>,
        keep: impl Fn(&T) -> bool,
        cmp: impl Fn(&T, &T) -> Ordering,
    ) -> Vec<serde_json::Value>
    where
        T: Serialize + Clone,
    {
        let mut items: Vec<T> = items.into_iter().filter(|i| keep(i)).collect();
        if self.orderby.is_some() {
            items.sort_by(|a, b| cmp(a, b));
        }
        let skip = self.skip.unwrap_or(0);
        let mut out: Vec<serde_json::Value> = items
            .into_iter()
            .skip(skip)
            .map(|i| serde_json::to_value(&i).unwrap_or(serde_json::Value::Null))
            .collect();
        if let Some(top) = self.top {
            out.truncate(top);
        }
        if let Some(select) = &self.select {
            out = out.into_iter().map(|v| project(&v, select)).collect();
        }
        out
    }

    /// `$filter` predicate for upstreams (`alias eq '…'`, `protocol eq '…'`).
    #[must_use = "the upstream filter predicate must be consumed to filter"]
    pub fn upstream_keep(&self) -> impl Fn(&Upstream) -> bool {
        let filter = parse_eq(self.filter.as_deref());
        move |u: &Upstream| match filter.as_ref() {
            Some((field, value)) if field == "alias" => u.alias.as_deref() == Some(value.as_str()),
            Some((field, value)) if field == "protocol" => u.protocol == *value,
            Some((field, value)) if field == "name" => u.alias.as_deref() == Some(value.as_str()),
            _ => true,
        }
    }

    /// `$orderby` comparator for upstreams (`alias asc|desc`).
    #[must_use = "the upstream comparator must be consumed to sort"]
    pub fn upstream_cmp(&self) -> impl Fn(&Upstream, &Upstream) -> Ordering {
        let descending = is_descending(self.orderby.as_deref());
        move |a: &Upstream, b: &Upstream| {
            let ord = a
                .alias
                .as_deref()
                .unwrap_or_default()
                .cmp(b.alias.as_deref().unwrap_or_default());
            if descending { ord.reverse() } else { ord }
        }
    }

    /// `$filter` predicate for routes (`upstream_id eq '…'`).
    #[must_use = "the route filter predicate must be consumed to filter"]
    pub fn route_keep(&self) -> impl Fn(&Route) -> bool {
        let filter = parse_eq(self.filter.as_deref());
        move |r: &Route| match filter.as_ref() {
            Some((field, value)) if field == "upstream_id" => {
                uuid::Uuid::parse_str(value).is_ok_and(|id| r.upstream_id == id)
            }
            Some((field, value)) if field == "id" => {
                uuid::Uuid::parse_str(value).is_ok_and(|id| r.id == Some(id))
            }
            _ => true,
        }
    }

    /// `$orderby` comparator for routes (`path asc|desc`).
    #[must_use = "the route comparator must be consumed to sort"]
    pub fn route_cmp(&self) -> impl Fn(&Route, &Route) -> Ordering {
        let descending = is_descending(self.orderby.as_deref());
        move |a: &Route, b: &Route| {
            let a_path = a
                .r#match
                .as_http()
                .map(|m| normalize_path(&m.path))
                .unwrap_or_default();
            let b_path = b
                .r#match
                .as_http()
                .map(|m| normalize_path(&m.path))
                .unwrap_or_default();
            let ord = a_path.cmp(&b_path);
            if descending { ord.reverse() } else { ord }
        }
    }

    /// `$filter` predicate for plugins (`type eq '…'`, `name eq '…'`).
    #[must_use = "the plugin filter predicate must be consumed to filter"]
    pub fn plugin_keep(&self) -> impl Fn(&Plugin) -> bool {
        let filter = parse_eq(self.filter.as_deref());
        move |p: &Plugin| match filter.as_ref() {
            Some((field, value)) if field == "type" => &p.plugin_type == value,
            Some((field, value)) if field == "name" => &p.name == value,
            _ => true,
        }
    }

    /// `$orderby` comparator for plugins (`name asc|desc`).
    #[must_use = "the plugin comparator must be consumed to sort"]
    pub fn plugin_cmp(&self) -> impl Fn(&Plugin, &Plugin) -> Ordering {
        let descending = is_descending(self.orderby.as_deref());
        move |a: &Plugin, b: &Plugin| {
            let ord = a.name.cmp(&b.name);
            if descending { ord.reverse() } else { ord }
        }
    }
}

/// Parse a single `field eq 'value'` predicate (OData-light).
fn parse_eq(filter: Option<&str>) -> Option<(String, String)> {
    let filter = filter?.trim();
    let (field, rest) = filter.split_once("eq")?;
    let field = field.trim().trim_start_matches('$').to_owned();
    let value = rest.trim().trim_matches('\'');
    let value = value.trim_matches('"');
    if value.is_empty() {
        return None;
    }
    Some((field, value.to_owned()))
}

/// Whether `$orderby` requests descending order.
fn is_descending(orderby: Option<&str>) -> bool {
    orderby
        .and_then(|o| o.trim().split_once(' '))
        .is_some_and(|(_, dir)| dir.trim().eq_ignore_ascii_case("desc"))
}

/// Project a serialized item to the `$select` field list.
fn project(value: &serde_json::Value, select: &[String]) -> serde_json::Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut out = serde_json::Map::new();
    for field in select {
        if let Some(v) = object.get(field) {
            out.insert(field.clone(), v.clone());
        }
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::dto::Upstream;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn upstream(alias: &str) -> Upstream {
        Upstream {
            alias: Some(alias.to_owned()),
            ..Upstream::default()
        }
    }

    #[test]
    fn top_skip_and_select_are_applied() {
        let query = ListQuery::from_params(&params(&[
            ("$top", "1"),
            ("$skip", "1"),
            ("$select", "alias"),
        ]));
        let items = vec![upstream("a.com"), upstream("b.com"), upstream("c.com")];
        let out = query.apply(items, |_| true, |a, b| a.alias.cmp(&b.alias));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["alias"], "b.com");
        assert_eq!(out[0].as_object().unwrap().len(), 1);
    }

    #[test]
    fn filter_alias_eq() {
        let query = ListQuery::from_params(&params(&[("$filter", "alias eq 'b.com'")]));
        let items = vec![upstream("a.com"), upstream("b.com")];
        let out = query.apply(items, query.upstream_keep(), query.upstream_cmp());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["alias"], "b.com");
    }

    #[test]
    fn orderby_descending() {
        let query = ListQuery::from_params(&params(&[("$orderby", "alias desc")]));
        let items = vec![upstream("a.com"), upstream("b.com")];
        let out = query.apply(items, query.upstream_keep(), query.upstream_cmp());
        assert_eq!(out[0]["alias"], "b.com");
        assert_eq!(out[1]["alias"], "a.com");
    }
}
