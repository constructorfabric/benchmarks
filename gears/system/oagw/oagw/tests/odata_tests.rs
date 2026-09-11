//! OData list parameter tests.
//!
//! Covers `cpt-cf-oagw-algo-odata-list`: the defaults, the hard `$top`
//! ceiling, the rejected paging values, the closed parameter surface, a
//! `$filter` over every filterable field of both resource kinds, the
//! unexposed-field and malformed-grammar refusals, `$orderby` in both
//! directions, `$select` projecting and rejecting, `$skip` offsetting, and the
//! tenant equality holding under every combination.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use oagw::control_plane::odata::{self, DEFAULT_TOP, MAX_TOP};
use oagw::control_plane::validation::ResourceKind;
use oagw::store::{OagwStore, RouteRow, UpstreamRow};
use oagw::gts;
use oagw::{Endpoint, EndpointHost, HttpMatch, MatchConfig, Route, Scheme, ServerConfig, Upstream};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: EndpointHost::parse(host).expect("valid endpoint host"),
        port: Some(port),
    }
}

/// An upstream row with the alias and tags the case needs.
fn upstream(alias: Option<&str>, tags: &[&str]) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: alias.map(str::to_owned),
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        server: ServerConfig {
            endpoints: vec![endpoint("api.openai.com", 443)],
        },
        protocol: String::from(HTTP_PROTOCOL),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

/// A route row with the path, priority, and enabled value the case needs.
fn route(upstream_id: Uuid, path: &str, priority: i64, enabled: Option<bool>) -> Route {
    Route {
        id: Uuid::new_v4(),
        upstream_id,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![String::from("GET")],
                path: String::from(path),
                query_allowlist: vec![],
                path_suffix_mode: None,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        tags: vec![String::from("edge")],
        cors: None,
        priority: Some(priority),
        enabled,
    }
}

/// A store holding three upstreams and three routes of one tenant, plus one
/// upstream and one route of another.
fn seeded() -> OagwStore {
    let store = OagwStore::new();
    let owner = tenant(1);

    let first = store
        .insert_upstream(owner, &upstream(Some("api.openai.com"), &["llm"]))
        .expect("first upstream");
    let second = store
        .insert_upstream(owner, &upstream(Some("eu.openai.com"), &["edge"]))
        .expect("second upstream");
    store
        .insert_upstream(owner, &upstream(None, &["edge", "llm"]))
        .expect("third upstream");
    store
        .insert_upstream(tenant(2), &upstream(Some("foreign.openai.com"), &["llm"]))
        .expect("foreign upstream");

    let upstream_id = first.upstream.id;
    store
        .insert_route(owner, &route(upstream_id, "/v1/chat", 10, Some(true)))
        .expect("first route");
    store
        .insert_route(owner, &route(upstream_id, "/v1/embed", 20, Some(true)))
        .expect("second route");
    store
        .insert_route(owner, &route(second.upstream.id, "/v1/moderate", 30, Some(false)))
        .expect("third route");

    let foreign = store
        .list_upstreams(tenant(2))
        .pop()
        .expect("the foreign upstream");
    store
        .insert_route(tenant(2), &route(foreign.upstream.id, "/v1/foreign", 40, Some(true)))
        .expect("foreign route");
    let _ = &first;
    let _ = &second;
    store
}

/// Parses one query string for one resource kind.
fn query(kind: ResourceKind, text: &str) -> oagw::control_plane::odata::ListQuery {
    oagw::control_plane::odata::parse(kind.into(), text).expect("the parameters are admitted")
}

/// Asserts a query string is refused, naming the offending parameter.
fn refused(kind: ResourceKind, text: &str, needle: &str) {
    let error = oagw::control_plane::odata::parse(kind.into(), text).expect_err("refused");
    assert_eq!(error.http_status(), 400, "{error}");
    assert!(
        error.detail.contains(needle),
        "expected '{needle}' in '{error}'"
    );
}

/// The upstream aliases of one page.
fn aliases(page: &odata::Page<UpstreamRow>) -> Vec<String> {
    page.items
        .iter()
        .map(|row| row.upstream.alias.clone().unwrap_or_default())
        .collect()
}

/// The route paths of one page.
fn paths(page: &odata::Page<RouteRow>) -> Vec<String> {
    page.items
        .iter()
        .map(|row| row.route.match_config.http.clone().expect("http").path)
        .collect()
}

#[test]
fn an_absent_parameter_set_takes_the_declared_defaults() {
    let parsed = query(ResourceKind::Upstream, "");
    assert_eq!(parsed.top, DEFAULT_TOP);
    assert_eq!(parsed.skip, 0);
    assert!(parsed.filter.is_none());
    assert!(parsed.orderby.is_none());
    assert!(parsed.select.is_empty());
}

#[test]
fn an_oversized_top_is_bounded_to_the_ceiling() {
    let parsed = query(ResourceKind::Upstream, "$top=100000");
    assert_eq!(parsed.top, MAX_TOP);
    let parsed = query(ResourceKind::Upstream, "$top=100");
    assert_eq!(parsed.top, MAX_TOP);
    let parsed = query(ResourceKind::Upstream, "$top=101");
    assert_eq!(parsed.top, MAX_TOP, "the ceiling is a hard bound");
}

#[test]
fn a_malformed_paging_value_is_refused() {
    refused(ResourceKind::Upstream, "$top=-1", "$top");
    refused(ResourceKind::Upstream, "$top=many", "$top");
    refused(ResourceKind::Route, "$skip=-5", "$skip");
    refused(ResourceKind::Route, "$skip=later", "$skip");
    refused(ResourceKind::Route, "$skip=1.5", "$skip");
}

#[test]
fn an_unknown_parameter_is_refused_by_name() {
    refused(ResourceKind::Upstream, "$count=true", "$count");
    refused(ResourceKind::Upstream, "filter=alias", "filter");
    refused(ResourceKind::Route, "$filterx=id", "$filterx");
}

#[test]
fn a_filter_admits_every_filterable_upstream_field() {
    let parsed = query(ResourceKind::Upstream, "$filter=alias eq 'api.openai.com'");
    assert_eq!(parsed.filter.expect("filter").terms.len(), 1);

    let parsed = query(
        ResourceKind::Upstream,
        &format!("$filter=id eq '{}'", gts::gts_instance(gts::UPSTREAM_TYPE, Uuid::nil())),
    );
    assert_eq!(parsed.filter.expect("filter").terms.len(), 1);

    let parsed = query(ResourceKind::Upstream, "$filter=enabled eq 'true'");
    assert_eq!(parsed.filter.expect("filter").terms.len(), 1);

    let parsed = query(ResourceKind::Upstream, "$filter=tag eq 'llm'");
    assert_eq!(parsed.filter.expect("filter").terms.len(), 1);
}

#[test]
fn a_filter_admits_every_filterable_route_field() {
    for field in ["id", "upstream_id", "path", "method", "priority", "enabled", "tag"] {
        let parsed = query(ResourceKind::Route, &format!("$filter={field} eq 'x'"));
        assert_eq!(parsed.filter.expect("filter").terms.len(), 1, "{field}");
    }
}

#[test]
fn a_filter_naming_an_unexposed_field_is_refused() {
    refused(
        ResourceKind::Upstream,
        "$filter=path eq '/v1'",
        "$filter names a field the resource kind does not expose",
    );
    refused(
        ResourceKind::Route,
        "$filter=alias eq 'api.openai.com'",
        "$filter names a field the resource kind does not expose",
    );
    refused(
        ResourceKind::Upstream,
        "$filter=created_at eq 'yesterday'",
        "$filter names a field the resource kind does not expose",
    );
}

#[test]
fn a_malformed_filter_grammar_is_refused() {
    for expression in [
        "alias",
        "alias eq",
        "alias eq '",
        "alias eq 'unterminated",
        "alias like 'x'",
        "alias eq 'a' or alias eq 'b'",
        "eq 'a'",
    ] {
        refused(
            ResourceKind::Upstream,
            &format!("$filter={expression}"),
            "$filter is not a well-formed filter expression",
        );
    }
}

#[test]
fn a_conjunction_of_comparisons_is_admitted() {
    let parsed = query(
        ResourceKind::Upstream,
        "$filter=enabled eq 'true' and tag eq 'llm'",
    );
    let filter = parsed.filter.expect("filter");
    assert_eq!(filter.terms.len(), 2);
    assert_eq!(filter.terms[0].value, "true");
    assert_eq!(filter.terms[1].value, "llm");
}

#[test]
fn an_ordering_is_parsed_in_both_directions() {
    let parsed = query(ResourceKind::Upstream, "$orderby=alias");
    let orderby = parsed.orderby.expect("orderby");
    assert!(!orderby.descending);

    let parsed = query(ResourceKind::Route, "$orderby=priority desc");
    let orderby = parsed.orderby.expect("orderby");
    assert!(orderby.descending);

    let parsed = query(ResourceKind::Route, "$orderby=priority asc");
    assert!(!parsed.orderby.expect("orderby").descending);
}

#[test]
fn an_ordering_naming_an_unorderable_field_is_refused() {
    refused(
        ResourceKind::Upstream,
        "$orderby=created_at desc",
        "$orderby names a field the resource kind does not order by",
    );
    refused(
        ResourceKind::Route,
        "$orderby=path",
        "$orderby names a field the resource kind does not order by",
    );
}

#[test]
fn a_malformed_ordering_is_refused() {
    refused(
        ResourceKind::Upstream,
        "$orderby=alias sideways",
        "$orderby is not a well-formed ordering expression",
    );
}

#[test]
fn a_projection_is_parsed_and_deduplicated() {
    let parsed = query(ResourceKind::Upstream, "$select=id,alias,alias");
    assert_eq!(parsed.select, vec![String::from("id"), String::from("alias")]);
    let parsed = query(ResourceKind::Route, "$select=match,priority");
    assert_eq!(parsed.select, vec![String::from("match"), String::from("priority")]);
}

#[test]
fn a_projection_naming_an_unknown_property_is_refused() {
    refused(
        ResourceKind::Upstream,
        "$select=id,upstream_id",
        "$select names a property the resource kind does not expose",
    );
    refused(
        ResourceKind::Route,
        "$select=alias",
        "$select names a property the resource kind does not expose",
    );
}

#[test]
fn every_defect_of_one_query_string_is_reported_in_one_error() {
    refused(
        ResourceKind::Upstream,
        "$top=late&$count=true&$select=zzz",
        "$top",
    );
    let error = oagw::control_plane::odata::parse(ResourceKind::Upstream.into(), "$top=late&$count=true")
        .expect_err("refused");
    assert!(error.detail.contains("$top"), "{error}");
    assert!(error.detail.contains("$count"), "{error}");
}

#[test]
fn a_filter_selects_the_rows_that_compare_equal() {
    let store = seeded();
    let scan = store.list_upstreams(tenant(1));
    let parsed = query(ResourceKind::Upstream, "$filter=alias eq 'api.openai.com'");
    let page = odata::apply_upstream(&parsed, scan);
    assert_eq!(aliases(&page), vec![String::from("api.openai.com")]);
}

#[test]
fn an_alias_comparison_is_case_insensitive() {
    let store = seeded();
    let scan = store.list_upstreams(tenant(1));
    let parsed = query(ResourceKind::Upstream, "$filter=alias eq 'API.OPENAI.COM'");
    let page = odata::apply_upstream(&parsed, scan);
    assert_eq!(aliases(&page).len(), 1);
}

#[test]
fn a_tag_comparison_selects_every_parent_holding_the_tag() {
    let store = seeded();
    let scan = store.list_upstreams(tenant(1));
    let parsed = query(ResourceKind::Upstream, "$filter=tag eq 'llm'");
    let page = odata::apply_upstream(&parsed, scan);
    assert_eq!(aliases(&page).len(), 2, "two upstreams hold the tag");
}

#[test]
fn an_enabled_comparison_selects_the_enabled_rows() {
    let store = seeded();
    let scan = store.list_routes(tenant(1));
    let parsed = query(ResourceKind::Route, "$filter=enabled eq 'true'");
    let page = odata::apply_route(&parsed, scan);
    assert_eq!(paths(&page).len(), 2, "one of the three routes is disabled");
}

#[test]
fn a_path_and_a_method_comparison_read_the_match_rows() {
    let store = seeded();
    let scan = store.list_routes(tenant(1));
    let parsed = query(ResourceKind::Route, "$filter=path eq '/v1/embed'");
    let page = odata::apply_route(&parsed, scan);
    assert_eq!(paths(&page), vec![String::from("/v1/embed")]);

    let parsed = query(ResourceKind::Route, "$filter=method eq 'get'");
    let page = odata::apply_route(&parsed, store.list_routes(tenant(1)));
    assert_eq!(paths(&page).len(), 3);
}

#[test]
fn a_gts_instance_identifier_compares_after_parsing() {
    let store = seeded();
    let scan = store.list_routes(tenant(1));
    let owner = store.list_upstreams(tenant(1));
    let upstream_id = owner
        .iter()
        .find(|row| row.upstream.alias.as_deref() == Some("api.openai.com"))
        .expect("the referenced upstream")
        .upstream
        .id;
    let parsed = query(
        ResourceKind::Route,
        &format!(
            "$filter=upstream_id eq '{}'",
            gts::gts_instance(gts::UPSTREAM_TYPE, upstream_id)
        ),
    );
    let page = odata::apply_route(&parsed, scan);
    assert_eq!(paths(&page).len(), 2, "two routes share the upstream");
}

#[test]
fn an_unparseable_identifier_selects_nothing() {
    let store = seeded();
    let parsed = query(ResourceKind::Route, "$filter=id eq 'not-an-id'");
    let page = odata::apply_route(&parsed, store.list_routes(tenant(1)));
    assert!(page.items.is_empty());
}

#[test]
fn an_ordering_sorts_ascending_and_descending() {
    let store = seeded();
    let parsed = query(ResourceKind::Route, "$orderby=priority");
    let page = odata::apply_route(&parsed, store.list_routes(tenant(1)));
    assert_eq!(paths(&page), vec!["/v1/chat", "/v1/embed", "/v1/moderate"]);

    let parsed = query(ResourceKind::Route, "$orderby=priority desc");
    let page = odata::apply_route(&parsed, store.list_routes(tenant(1)));
    assert_eq!(paths(&page), vec!["/v1/moderate", "/v1/embed", "/v1/chat"]);
}

#[test]
fn an_ordering_by_alias_sorts_the_absent_alias_first() {
    let store = seeded();
    let parsed = query(ResourceKind::Upstream, "$orderby=alias");
    let page = odata::apply_upstream(&parsed, store.list_upstreams(tenant(1)));
    assert_eq!(
        aliases(&page),
        vec![
            String::new(),
            String::from("api.openai.com"),
            String::from("eu.openai.com"),
        ]
    );
}

#[test]
fn an_offset_and_a_bound_shape_the_page() {
    let store = seeded();
    let parsed = query(ResourceKind::Route, "$orderby=priority&$skip=1&$top=1");
    let page = odata::apply_route(&parsed, store.list_routes(tenant(1)));
    assert_eq!(paths(&page), vec![String::from("/v1/embed")]);
    assert_eq!(page.items.len(), 1);

    let parsed = query(ResourceKind::Route, "$orderby=priority&$skip=99");
    let page = odata::apply_route(&parsed, store.list_routes(tenant(1)));
    assert!(page.items.is_empty());
}

#[test]
fn the_projection_is_returned_with_the_page() {
    let store = seeded();
    let parsed = query(ResourceKind::Upstream, "$select=alias&$orderby=alias");
    let page = odata::apply_upstream(&parsed, store.list_upstreams(tenant(1)));
    assert_eq!(page.projection, vec![String::from("alias")]);
    assert_eq!(page.items.len(), 3);
}

#[test]
fn the_tenant_equality_holds_under_every_combination() {
    let store = seeded();
    let upstream_combinations = [
        "",
        "$top=100",
        "$filter=tag eq 'llm'",
        "$filter=alias eq 'foreign.openai.com'",
        "$orderby=alias",
        "$orderby=alias desc&$top=1&$skip=0",
        "$select=id,alias",
        "$filter=enabled eq 'true'&$orderby=alias desc&$skip=1&$top=2",
    ];
    for combination in upstream_combinations {
        let parsed = query(ResourceKind::Upstream, combination);
        let page = odata::apply_upstream(&parsed, store.list_upstreams(tenant(2)));
        assert!(page.items.iter().all(|row| row.tenant_id == tenant(2)), "{combination}");
        assert!(
            aliases(&page)
                .iter()
                .all(|alias| alias == "foreign.openai.com"),
            "{combination}"
        );
    }

    let route_combinations = [
        "",
        "$top=100",
        "$filter=tag eq 'edge'",
        "$filter=path eq '/v1/foreign'",
        "$orderby=priority",
        "$orderby=priority desc&$top=1&$skip=0",
        "$select=id,match",
        "$filter=enabled eq 'true'&$orderby=priority desc&$skip=1&$top=2",
    ];
    for combination in route_combinations {
        let parsed = query(ResourceKind::Route, combination);
        let page = odata::apply_route(&parsed, store.list_routes(tenant(2)));
        assert!(page.items.iter().all(|row| row.tenant_id == tenant(2)), "{combination}");
        assert!(page.items.len() <= 1, "{combination}");
    }
}

#[test]
fn a_page_of_another_tenant_is_never_visible_to_a_filter() {
    let store = seeded();
    let parsed = query(ResourceKind::Upstream, "$filter=alias eq 'api.openai.com'");
    let page = odata::apply_upstream(&parsed, store.list_upstreams(tenant(2)));
    assert!(page.items.is_empty());
}
