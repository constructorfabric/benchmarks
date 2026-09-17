//! Tests for the [`ControlPlane`] management service.
use uuid::Uuid;

use super::{BUILT_IN_PLUGINS, ControlPlane, ListLimits, built_in_plugin};
use crate::domain::gts;
use crate::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, PathSuffixMode, PluginBinding, PluginKind, PluginsConfig,
    Protocol, RouteMatcher, ServerConfig, SharingMode,
};
use crate::domain::query::parse_filter;
use crate::domain::services::{PluginSpec, RouteSpec, UpstreamSpec};
use crate::infra::memory::MemoryStore;

const API_KEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
const REQUIRED_HEADERS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
const REQUEST_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
const BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";

fn service() -> ControlPlane {
    ControlPlane::new(
        std::sync::Arc::new(MemoryStore::new()),
        ListLimits::default(),
    )
}

fn https(host: &str, port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Https, host, Some(port)).expect("endpoint")
}

fn spec(alias: Option<&str>, hosts: &[(&str, u16)]) -> UpstreamSpec {
    UpstreamSpec {
        alias: alias.map(str::to_owned),
        enabled: Some(true),
        tags: Some(vec!["  Edge ".to_owned(), "".to_owned(), "edge".to_owned()]),
        server: Some(ServerConfig {
            endpoints: hosts
                .iter()
                .map(|(host, port)| {
                    Endpoint::new(EndpointScheme::Https, host, Some(*port)).expect("endpoint")
                })
                .collect(),
        }),
        protocol: Some(Protocol::Http),
        ..UpstreamSpec::default()
    }
}

fn http_matcher(path: &str) -> RouteMatcher {
    RouteMatcher::Http(HttpMatch {
        methods: vec!["GET".to_owned()],
        path: path.to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    })
}

fn route_spec(upstream_id: Uuid, path: &str) -> RouteSpec {
    RouteSpec {
        upstream_id: Some(upstream_id),
        name: Some("edge".to_owned()),
        tags: None,
        matcher: Some(http_matcher(path)),
        priority: Some(10),
        enabled: Some(true),
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn plugin_spec(name: &str, kind: PluginKind) -> PluginSpec {
    PluginSpec {
        name: Some(name.to_owned()),
        kind: Some(kind),
        description: Some("test plugin".to_owned()),
        config: Some(serde_json::json!({ "headers": ["x-trace-id"] })),
        source: Some("def plugin(ctx): pass".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Built-in catalog
// ---------------------------------------------------------------------------

#[test]
fn the_catalog_lists_the_documented_plugins() {
    assert_eq!(BUILT_IN_PLUGINS.len(), 12);
    for entry in BUILT_IN_PLUGINS {
        assert!(entry.gts_id.starts_with("gts.cf.core.oagw."));
        assert_eq!(built_in_plugin(entry.gts_id), Some(entry));
    }
}

#[test]
fn catalog_only_plugins_are_not_bindable() {
    for name in ["basic", "bearer", "timeout", "cors", "logging", "metrics"] {
        let entry = BUILT_IN_PLUGINS
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} must be catalogued"));
        assert!(!entry.bindable, "{name} must not be bindable");
    }
}

// ---------------------------------------------------------------------------
// Upstream lifecycle
// ---------------------------------------------------------------------------

#[test]
fn create_derives_the_alias_from_a_single_hostname() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.OpenAI.com", 443)]))
        .expect("service op");
    assert_eq!(upstream.alias, "api.openai.com");
    assert!(upstream.tags.iter().all(|tag| tag == "edge"));
    assert!(upstream.enabled);
    assert_eq!(upstream.protocol, Protocol::Http);
    assert_ne!(upstream.created_at, 0);
    // Server-generated ids are bare UUIDs; the GTS form is derived.
    assert_eq!(
        upstream.gts_id(),
        format!("gts.{}~{}", gts::UPSTREAM_TYPE, upstream.id)
    );
}

#[test]
fn create_requires_an_explicit_alias_for_ip_pools() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let error = svc
        .create_upstream(tenant, spec(None, &[("10.0.0.7", 443)]))
        .expect_err("IP pools need an explicit alias");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::FieldViolation { .. }
    ));
}

#[test]
fn create_accepts_an_explicit_alias_for_ip_pools() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(
            tenant,
            spec(Some("billing.internal"), &[("10.0.0.7", 8443)]),
        )
        .expect("service op");
    assert_eq!(upstream.alias, "billing.internal");
    assert_eq!(upstream.server.endpoints[0].port, 8443);
}

#[test]
fn alias_conflicts_are_rejected_per_tenant() {
    let svc = service();
    let tenant = Uuid::new_v4();
    svc.create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let error = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect_err("duplicate alias");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::AliasConflict { .. }
    ));
    // The same alias in another tenant is fine.
    svc.create_upstream(Uuid::new_v4(), spec(None, &[("api.example.com", 443)]))
        .expect("service op");
}

#[test]
fn multi_endpoint_pools_derive_the_common_registrable_suffix() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(
            tenant,
            spec(
                None,
                &[("api.eu.example.com", 443), ("api.us.example.com", 443)],
            ),
        )
        .expect("service op");
    assert_eq!(upstream.alias, "example.com");
    assert_eq!(upstream.server.endpoints.len(), 2);
}

#[test]
fn endpoint_pools_must_be_homogeneous() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let mut bad = spec(None, &[("api.example.com", 443), ("api.example.com", 8443)]);
    bad.server = Some(ServerConfig {
        endpoints: vec![
            https("api.example.com", 443),
            https("api.example.com", 8443),
        ],
    });
    let error = svc.create_upstream(tenant, bad).expect_err("mixed ports");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::FieldViolation { .. }
    ));
}

#[test]
fn http_is_a_legal_scheme_value() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let mut spec = spec(None, &[("api.example.com", 8080)]);
    spec.server = Some(ServerConfig {
        endpoints: vec![
            Endpoint::new(EndpointScheme::Http, "api.example.com", Some(8080)).expect("e"),
        ],
    });
    let upstream = svc.create_upstream(tenant, spec).expect("service op");
    assert_eq!(upstream.server.endpoints[0].scheme, EndpointScheme::Http);
    assert!(upstream.server.endpoints[0].scheme.is_plaintext());
}

#[test]
fn replace_requires_the_pool_and_the_protocol() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");

    let error = svc
        .replace_upstream(tenant, upstream.id, UpstreamSpec::default())
        .expect_err("missing server");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::FieldViolation { .. }
    ));
}

#[test]
fn replace_keeps_the_alias_and_the_identity() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let mut replacement = spec(Some("someone.else.example"), &[("api.example.com", 443)]);
    replacement.enabled = Some(false);
    let replaced = svc
        .replace_upstream(tenant, created.id, replacement)
        .expect("service op");
    assert_eq!(replaced.id, created.id);
    assert_eq!(replaced.alias, "api.example.com");
    assert!(!replaced.enabled);
    assert_eq!(replaced.created_at, created.created_at);
}

#[test]
fn replace_is_scoped_to_the_calling_tenant() {
    let svc = service();
    let created = svc
        .create_upstream(Uuid::new_v4(), spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let error = svc
        .replace_upstream(
            Uuid::new_v4(),
            created.id,
            spec(None, &[("api.example.com", 443)]),
        )
        .expect_err("foreign tenant");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::NotFound { .. }
    ));
}

#[test]
fn tags_are_normalized_and_deduplicated() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    assert_eq!(upstream.tags, vec!["edge".to_owned()]);
}

#[test]
fn delete_cascades_the_routes() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    svc.create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");
    svc.delete_upstream(tenant, upstream.id)
        .expect("service op");
    assert!(
        svc.list_routes(tenant, &Default::default())
            .expect("list")
            .is_empty()
    );
}

#[test]
fn enable_and_disable_toggle_the_flag() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    assert!(
        !svc.set_upstream_enabled(tenant, upstream.id, false)
            .expect("disable")
            .enabled
    );
    assert!(
        svc.set_upstream_enabled(tenant, upstream.id, true)
            .expect("enable")
            .enabled
    );
}

// ---------------------------------------------------------------------------
// Endpoint pool
// ---------------------------------------------------------------------------

#[test]
fn the_endpoint_pool_can_be_grown_and_shrunk() {
    let svc = service();
    let tenant = Uuid::new_v4();
    // The pool derives `example.com`, so every mutation below keeps the
    // derived alias — the alias is the routing key and cannot move.
    let created = svc
        .create_upstream(
            tenant,
            spec(
                None,
                &[("api.eu.example.com", 443), ("api.us.example.com", 443)],
            ),
        )
        .expect("service op");
    assert_eq!(created.alias, "example.com");

    let grown = svc
        .add_upstream_endpoint(tenant, created.id, https("api.ap.example.com", 443))
        .expect("append");
    assert_eq!(grown.server.endpoints.len(), 3);

    // Appending the same endpoint is idempotent.
    let again = svc
        .add_upstream_endpoint(tenant, created.id, https("api.ap.example.com", 443))
        .expect("append again");
    assert_eq!(again.server.endpoints.len(), 3);

    // An append that would move the derived alias is refused.
    let error = svc
        .add_upstream_endpoint(tenant, created.id, https("edge.other.net", 443))
        .expect_err("alias would move");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::AliasRule { .. }
    ));

    svc.delete_upstream_endpoint(tenant, created.id, 2)
        .expect("delete endpoint");
    let pool = svc
        .upstream_endpoints(tenant, created.id)
        .expect("service op");
    assert_eq!(pool.endpoints.len(), 2);
    assert_eq!(pool.endpoints[0].host, "api.eu.example.com");
}

#[test]
fn the_pool_cannot_become_empty() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let error = svc
        .delete_upstream_endpoint(tenant, created.id, 0)
        .expect_err("empty pool");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::FieldViolation { .. }
    ));
}

#[test]
fn a_missing_endpoint_position_is_a_404() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let error = svc
        .delete_upstream_endpoint(tenant, created.id, 9)
        .expect_err("out of range");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::NotFound { .. }
    ));
}

// ---------------------------------------------------------------------------
// Plugin chains and the auth slot
// ---------------------------------------------------------------------------

#[test]
fn built_in_references_are_canonicalized() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");

    let bound = svc
        .set_upstream_plugins(
            tenant,
            created.id,
            PluginsConfig {
                sharing: SharingMode::Inherit,
                items: vec![
                    PluginBinding::bare(REQUIRED_HEADERS),
                    PluginBinding::bare(REQUEST_ID),
                ],
            },
        )
        .expect("bind");
    let items = bound.plugins.expect("chain").items;
    assert_eq!(items[0].plugin_ref, REQUIRED_HEADERS);
    assert_eq!(items[0].plugin_uuid, None);
    assert_eq!(items[1].plugin_ref, REQUEST_ID);
}

#[test]
fn catalog_only_plugins_cannot_be_bound() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let error = svc
        .set_upstream_plugins(
            tenant,
            created.id,
            PluginsConfig {
                sharing: SharingMode::Inherit,
                items: vec![PluginBinding::bare(BASIC)],
            },
        )
        .expect_err("basic is catalogued only");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::UnknownPluginRef { .. }
    ));
}

#[test]
fn unknown_references_are_rejected() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let error = svc
        .set_upstream_plugins(
            tenant,
            created.id,
            PluginsConfig {
                sharing: SharingMode::Inherit,
                items: vec![PluginBinding::bare(
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nope.v1",
                )],
            },
        )
        .expect_err("unknown plugin");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::UnknownPluginRef { .. }
    ));
}

#[test]
fn custom_plugins_bind_by_uuid_and_by_gts_id() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let plugin = svc
        .create_plugin(tenant, plugin_spec("signer", PluginKind::Guard))
        .expect("service op");

    let by_uuid = svc
        .set_upstream_plugins(
            tenant,
            created.id,
            PluginsConfig {
                sharing: SharingMode::Inherit,
                items: vec![PluginBinding::bare(plugin.id.to_string())],
            },
        )
        .expect("bind by uuid");
    let binding = by_uuid.plugins.as_ref().expect("chain").items[0].clone();
    assert_eq!(binding.plugin_uuid, Some(plugin.id));
    assert_eq!(binding.plugin_ref, plugin.gts_id());

    let by_gts = svc
        .set_upstream_plugins(
            tenant,
            created.id,
            PluginsConfig {
                sharing: SharingMode::Inherit,
                items: vec![PluginBinding::bare(plugin.gts_id())],
            },
        )
        .expect("bind by gts id");
    assert_eq!(
        by_gts.plugins.as_ref().expect("chain").items[0].plugin_ref,
        plugin.gts_id()
    );
}

#[test]
fn the_auth_slot_requires_an_auth_plugin() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");

    let error = svc
        .set_upstream_auth(
            tenant,
            created.id,
            Some(crate::domain::model::AuthConfig {
                plugin_type: REQUIRED_HEADERS.to_owned(),
                plugin_uuid: None,
                sharing: SharingMode::Inherit,
                config: None,
            }),
        )
        .expect_err("guard plugin in the auth slot");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::UnknownPluginRef { .. }
    ));

    let bound = svc
        .set_upstream_auth(
            tenant,
            created.id,
            Some(crate::domain::model::AuthConfig {
                plugin_type: API_KEY.to_owned(),
                plugin_uuid: None,
                sharing: SharingMode::Inherit,
                config: Some(serde_json::json!({ "key": "secret" })),
            }),
        )
        .expect("auth plugin");
    assert_eq!(bound.auth.expect("auth").plugin_type, API_KEY);
}

// ---------------------------------------------------------------------------
// Route lifecycle
// ---------------------------------------------------------------------------

#[test]
fn routes_require_an_upstream_of_the_calling_tenant() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");

    let route = svc
        .create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");
    assert_eq!(route.priority, 10);
    assert_eq!(route.matcher.match_key(), "GET|/v1");

    let error = svc
        .create_route(Uuid::new_v4(), route_spec(upstream.id, "/v1"))
        .expect_err("foreign tenant");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::NotFound { .. }
    ));
}

#[test]
fn duplicate_match_keys_are_conflicts() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    svc.create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");
    let error = svc
        .create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect_err("duplicate match key");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::RouteMatchConflict { .. }
    ));

    // A different path or priority is a different match key.
    let mut other = route_spec(upstream.id, "/v2");
    other.priority = Some(11);
    svc.create_route(tenant, other).expect("service op");
}

#[test]
fn replace_keeps_the_upstream_binding() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let route = svc
        .create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");

    // `upstream_id` is immutable: echoing a different one is a 400.
    let mut rebinding = route_spec(upstream.id, "/v9");
    rebinding.upstream_id = Some(Uuid::new_v4());
    let error = svc
        .replace_route(tenant, route.id, rebinding)
        .expect_err("the upstream binding is immutable");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::ImmutableField {
            field: "upstream_id"
        }
    ));

    let replaced = svc
        .replace_route(tenant, route.id, route_spec(upstream.id, "/v9"))
        .expect("service op");
    assert_eq!(replaced.upstream_id, upstream.id);
    assert_eq!(replaced.matcher.match_key(), "GET|/v9");
    assert_eq!(replaced.id, route.id);
}

#[test]
fn routes_can_be_disabled_and_deleted() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let route = svc
        .create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");

    assert!(
        !svc.set_route_enabled(tenant, route.id, false)
            .expect("disable")
            .enabled
    );
    svc.delete_route(tenant, route.id).expect("service op");
    let error = svc.get_route(tenant, route.id).expect_err("deleted");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::NotFound { .. }
    ));
}

#[test]
fn route_plugin_chains_are_positional() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let route = svc
        .create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");

    let bound = svc
        .set_route_plugins(
            tenant,
            route.id,
            PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![
                    PluginBinding::bare(REQUEST_ID),
                    PluginBinding::bare(REQUIRED_HEADERS),
                ],
            },
        )
        .expect("bind");
    assert_eq!(bound.plugins.expect("chain").items.len(), 2);

    svc.delete_route_plugin(tenant, route.id, 0)
        .expect("service op");
    let chain = svc.route_plugins(tenant, route.id).expect("service op");
    assert_eq!(chain.items.len(), 1);
    assert_eq!(chain.items[0].plugin_ref, REQUIRED_HEADERS);
}

// ---------------------------------------------------------------------------
// Plugin CRUD
// ---------------------------------------------------------------------------

#[test]
fn plugins_are_created_with_a_server_generated_id() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let plugin = svc
        .create_plugin(tenant, plugin_spec("signer", PluginKind::Guard))
        .expect("service op");
    assert_eq!(plugin.name, "signer");
    assert_eq!(plugin.kind, PluginKind::Guard);
    assert_eq!(plugin.gts_id(), plugin.kind.gts_id(&plugin.id));
    assert_eq!(
        svc.plugin_source(tenant, plugin.id).expect("source").source,
        "def plugin(ctx): pass".to_owned()
    );
}

#[test]
fn plugin_names_are_unique_per_tenant() {
    let svc = service();
    let tenant = Uuid::new_v4();
    svc.create_plugin(tenant, plugin_spec("signer", PluginKind::Guard))
        .expect("service op");
    let error = svc
        .create_plugin(tenant, plugin_spec("signer", PluginKind::Transform))
        .expect_err("duplicate name");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::Conflict { .. }
    ));
    svc.create_plugin(Uuid::new_v4(), plugin_spec("signer", PluginKind::Transform))
        .expect("service op");
}

#[test]
fn an_unreferenced_plugin_can_be_deleted() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let plugin = svc
        .create_plugin(tenant, plugin_spec("orphan", PluginKind::Auth))
        .expect("service op");
    svc.delete_plugin(tenant, plugin.id).expect("service op");
    let error = svc.get_plugin(tenant, plugin.id).expect_err("deleted");
    assert!(matches!(
        error,
        crate::domain::error::DomainError::NotFound { .. }
    ));
}

#[test]
fn a_referenced_plugin_reports_what_references_it() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let plugin = svc
        .create_plugin(tenant, plugin_spec("used", PluginKind::Guard))
        .expect("service op");
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");

    svc.set_upstream_plugins(
        tenant,
        upstream.id,
        PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![PluginBinding::bare(plugin.id.to_string())],
        },
    )
    .expect("service op");
    let route = svc
        .create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");
    svc.set_route_plugins(
        tenant,
        route.id,
        PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![PluginBinding::bare(plugin.gts_id())],
        },
    )
    .expect("service op");

    let error = svc
        .delete_plugin(tenant, plugin.id)
        .expect_err("plugin is in use");
    let crate::domain::error::DomainError::PluginInUse {
        plugin_id,
        references,
    } = error
    else {
        panic!("expected PluginInUse, got {error:?}");
    };
    assert_eq!(plugin_id, plugin.gts_id());
    assert_eq!(references.upstreams, vec![upstream.gts_id()]);
    assert_eq!(references.routes, vec![route.gts_id()]);
    assert_eq!(references.len(), 2);
}

#[test]
fn deleting_the_referencing_resources_frees_the_plugin() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let plugin = svc
        .create_plugin(tenant, plugin_spec("loosened", PluginKind::Guard))
        .expect("service op");
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    svc.set_upstream_plugins(
        tenant,
        upstream.id,
        PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![PluginBinding::bare(plugin.id.to_string())],
        },
    )
    .expect("service op");
    svc.delete_upstream(tenant, upstream.id)
        .expect("service op");
    svc.delete_plugin(tenant, plugin.id).expect("service op");
    assert!(
        svc.list_plugins(tenant, &crate::domain::query::ListQuery::default())
            .expect("list")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[test]
fn lists_are_paged_filtered_and_ordered() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let first = svc
        .create_upstream(tenant, spec(None, &[("a.example.com", 443)]))
        .expect("service op");
    svc.create_upstream(tenant, spec(None, &[("b.example.com", 443)]))
        .expect("service op");
    let third = svc
        .create_upstream(tenant, spec(None, &[("c.example.com", 443)]))
        .expect("service op");

    let query = crate::domain::query::ListQuery {
        filter: Some(parse_filter("alias eq 'a.example.com'").expect("filter")),
        orderby: vec![],
        top: Some(10),
        skip: Some(0),
    };
    let filtered = svc.list_upstreams(tenant, &query).expect("service op");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, first.id);
    assert_eq!(svc.count_upstreams(tenant, &query).expect("count"), 1);

    let query = crate::domain::query::ListQuery {
        filter: None,
        orderby: crate::domain::query::parse_orderby("alias desc").expect("orderby"),
        top: Some(2),
        skip: Some(1),
    };
    let page = svc.list_upstreams(tenant, &query).expect("service op");
    assert_eq!(page.len(), 2);
    assert!(page[0].alias > page[1].alias);
    assert_eq!(svc.count_upstreams(tenant, &query).expect("count"), 3);

    let route = svc
        .create_route(tenant, route_spec(third.id, "/v1"))
        .expect("service op");
    let by_upstream = parse_filter(&format!("upstream_id eq '{}'", third.id)).expect("filter");
    let query = crate::domain::query::ListQuery {
        filter: Some(by_upstream),
        orderby: vec![],
        top: Some(50),
        skip: Some(0),
    };
    let routes = svc.list_routes(tenant, &query).expect("service op");
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].id, route.id);
}

#[test]
fn tenant_scoping_hides_every_ancestor_resource() {
    let svc = service();
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let upstream = svc
        .create_upstream(parent, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    let route = svc
        .create_route(parent, route_spec(upstream.id, "/v1"))
        .expect("service op");
    let plugin = svc
        .create_plugin(parent, plugin_spec("hidden", PluginKind::Guard))
        .expect("service op");

    for id in [upstream.id, route.id, plugin.id] {
        assert!(svc.get_upstream(child, id).is_err());
        assert!(svc.get_route(child, id).is_err());
        assert!(svc.get_plugin(child, id).is_err());
    }
    assert!(
        svc.list_upstreams(child, &crate::domain::query::ListQuery::default())
            .expect("list")
            .is_empty()
    );
    assert!(
        svc.list_routes(child, &crate::domain::query::ListQuery::default())
            .expect("list")
            .is_empty()
    );
    assert!(
        svc.list_plugins(child, &crate::domain::query::ListQuery::default())
            .expect("list")
            .is_empty()
    );
}

#[test]
fn snapshot_reports_the_whole_control_plane() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let upstream = svc
        .create_upstream(tenant, spec(None, &[("api.example.com", 443)]))
        .expect("service op");
    svc.create_route(tenant, route_spec(upstream.id, "/v1"))
        .expect("service op");
    let snapshot = svc.snapshot(tenant).expect("service op");
    assert_eq!(snapshot.upstreams.len(), 1);
    assert_eq!(snapshot.routes.len(), 1);
}

#[test]
fn list_limits_come_from_the_configuration() {
    let svc = ControlPlane::new(
        std::sync::Arc::new(MemoryStore::new()),
        ListLimits {
            default_top: 3,
            max_top: 5,
        },
    );
    assert_eq!(svc.limits().default_top, 3);
    assert_eq!(svc.limits().max_top, 5);
    assert_eq!(ListLimits::default().default_top, 50);
    assert_eq!(ListLimits::default().max_top, 100);
}
