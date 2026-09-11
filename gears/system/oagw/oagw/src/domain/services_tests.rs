//! Control Plane behaviour: validation, tenant scoping, and the alias +
//! route resolution the Data Plane depends on.

use std::sync::Arc;

use super::*;
use crate::domain::model::{PluginKind, PluginPhase};
use crate::domain::tenant::FlatTenantDirectory;
use crate::infra::storage::InMemoryStore;
use crate::test_utils::{StaticTenantDirectory, security_context};

const HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn service_with(directory: Arc<dyn crate::domain::tenant::TenantDirectory>) -> (ControlPlaneService, Arc<InMemoryStore>) {
    let store = InMemoryStore::shared();
    let service = ControlPlaneService::new(
        Arc::clone(&store) as Arc<dyn crate::domain::repo::UpstreamRepository>,
        Arc::clone(&store) as Arc<dyn crate::domain::repo::RouteRepository>,
        Arc::clone(&store) as Arc<dyn crate::domain::repo::PluginRepository>,
        directory,
    );
    (service, store)
}

fn service() -> (ControlPlaneService, Arc<InMemoryStore>) {
    service_with(Arc::new(FlatTenantDirectory))
}

fn upstream_input(json: serde_json::Value) -> UpstreamInput {
    serde_json::from_value(json).expect("fixture must deserialize")
}

fn route_input(json: serde_json::Value) -> RouteInput {
    serde_json::from_value(json).expect("fixture must deserialize")
}

fn basic_upstream(alias: Option<&str>, host: &str) -> UpstreamInput {
    let mut value = serde_json::json!({
        "server": {"endpoints": [{"scheme": "http", "host": host, "port": 80}]},
        "protocol": HTTP,
    });
    if let Some(alias) = alias {
        value["alias"] = alias.into();
    }
    upstream_input(value)
}

// -- Upstream CRUD ----------------------------------------------------------

#[test]
fn creating_an_upstream_derives_the_alias_and_assigns_an_id() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    assert_eq!(upstream.alias, "api.example.com");
    assert_eq!(upstream.tenant_id, tenant);
    assert!(upstream.enabled);
    assert!(upstream.gts_id().starts_with(gts::UPSTREAM_BASE));
}

#[test]
fn a_duplicate_alias_for_the_same_tenant_conflicts() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    let err = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap_err();
    assert_eq!(err.status(), 409);
}

#[test]
fn the_same_alias_is_free_for_a_different_tenant() {
    let (service, _) = service();
    service
        .create_upstream(Uuid::new_v4(), basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_upstream(Uuid::new_v4(), basic_upstream(None, "api.example.com"))
        .unwrap();
}

#[test]
fn an_empty_endpoint_pool_is_rejected() {
    let (service, _) = service();
    let input = upstream_input(serde_json::json!({
        "server": {"endpoints": []},
        "protocol": HTTP,
    }));
    let err = service.create_upstream(Uuid::new_v4(), input).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("at least one"), "{}", err.detail);
}

#[test]
fn a_pool_must_be_homogeneous_in_scheme_and_port() {
    let (service, _) = service();
    let mixed_scheme = upstream_input(serde_json::json!({
        "alias": "pool",
        "server": {"endpoints": [
            {"scheme": "https", "host": "10.0.0.1", "port": 443},
            {"scheme": "http", "host": "10.0.0.2", "port": 443}
        ]},
        "protocol": HTTP,
    }));
    assert!(
        service
            .create_upstream(Uuid::new_v4(), mixed_scheme)
            .unwrap_err()
            .detail
            .contains("scheme")
    );

    let mixed_port = upstream_input(serde_json::json!({
        "alias": "pool",
        "server": {"endpoints": [
            {"scheme": "https", "host": "10.0.0.1", "port": 443},
            {"scheme": "https", "host": "10.0.0.2", "port": 8443}
        ]},
        "protocol": HTTP,
    }));
    assert!(
        service
            .create_upstream(Uuid::new_v4(), mixed_port)
            .unwrap_err()
            .detail
            .contains("port")
    );
}

#[test]
fn an_unknown_auth_plugin_is_rejected_at_create_time() {
    let (service, _) = service();
    let input = upstream_input(serde_json::json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
        "protocol": HTTP,
        "auth": {"type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nonesuch.v1"},
    }));
    let err = service.create_upstream(Uuid::new_v4(), input).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("unknown auth plugin"), "{}", err.detail);
}

#[test]
fn catalog_only_auth_identifiers_are_accepted_at_create_time() {
    // `basic` and `bearer` are reserved identifiers; they only fail when a
    // proxy request actually tries to resolve an implementation.
    let (service, _) = service();
    for id in [gts::BASIC_AUTH_PLUGIN_ID, gts::BEARER_AUTH_PLUGIN_ID] {
        let input = upstream_input(serde_json::json!({
            "alias": "catalog-only",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9", "port": 443}]},
            "protocol": HTTP,
            "auth": {"type": id},
        }));
        service.create_upstream(Uuid::new_v4(), input).unwrap();
    }
}

#[test]
fn only_bindable_named_plugins_may_be_bound() {
    let (service, _) = service();
    let ok = upstream_input(serde_json::json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
        "protocol": HTTP,
        "plugins": {"items": [gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID]},
    }));
    service.create_upstream(Uuid::new_v4(), ok).unwrap();

    for rejected in [
        gts::TIMEOUT_GUARD_PLUGIN_ID,
        gts::CORS_GUARD_PLUGIN_ID,
        gts::LOGGING_TRANSFORM_PLUGIN_ID,
        gts::METRICS_TRANSFORM_PLUGIN_ID,
    ] {
        let input = upstream_input(serde_json::json!({
            "alias": "bad",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.7", "port": 443}]},
            "protocol": HTTP,
            "plugins": {"items": [rejected]},
        }));
        let err = service.create_upstream(Uuid::new_v4(), input).unwrap_err();
        assert_eq!(err.status(), 400, "{rejected}");
        assert!(err.detail.contains("cannot be bound"), "{}", err.detail);
    }
}

#[test]
fn cors_credentials_with_a_wildcard_origin_are_rejected() {
    let (service, _) = service();
    let input = upstream_input(serde_json::json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
        "protocol": HTTP,
        "cors": {"enabled": true, "allowed_origins": ["*"], "allow_credentials": true},
    }));
    let err = service.create_upstream(Uuid::new_v4(), input).unwrap_err();
    assert_eq!(err.status(), 400);
}

#[test]
fn tags_must_match_the_published_pattern() {
    let (service, _) = service();
    let input = upstream_input(serde_json::json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
        "protocol": HTTP,
        "tags": ["Not Valid"],
    }));
    assert_eq!(
        service.create_upstream(Uuid::new_v4(), input).unwrap_err().status(),
        400
    );
}

#[test]
fn replacing_an_upstream_clears_omitted_optional_fields() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let created = service
        .create_upstream(
            tenant,
            upstream_input(serde_json::json!({
                "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
                "protocol": HTTP,
                "tags": ["llm"],
            })),
        )
        .unwrap();
    assert_eq!(created.tags, ["llm"]);

    let replaced = service
        .replace_upstream(tenant, created.id, basic_upstream(None, "api.example.com"))
        .unwrap();
    assert!(replaced.tags.is_empty());
    assert_eq!(replaced.id, created.id);
}

#[test]
fn another_tenants_upstream_is_invisible_to_every_management_verb() {
    let (service, _) = service();
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let created = service
        .create_upstream(owner, basic_upstream(None, "api.example.com"))
        .unwrap();

    assert!(service.get_upstream(other, created.id).is_none());
    assert_eq!(
        service
            .replace_upstream(other, created.id, basic_upstream(None, "api.example.com"))
            .unwrap_err()
            .status(),
        404
    );
    assert_eq!(
        service.delete_upstream(other, created.id).unwrap_err().status(),
        404
    );
    assert!(service.list_upstreams(other).is_empty());
}

#[test]
fn deleting_an_upstream_cascades_to_its_routes() {
    let (service, store) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    service.delete_upstream(tenant, upstream.id).unwrap();
    assert!(
        crate::domain::repo::RouteRepository::list_by_upstream(&*store, upstream.id).is_empty()
    );
}

// -- Route CRUD -------------------------------------------------------------

#[test]
fn a_route_must_target_the_callers_own_upstream() {
    let (service, _) = service();
    let owner = Uuid::new_v4();
    let upstream = service
        .create_upstream(owner, basic_upstream(None, "api.example.com"))
        .unwrap();

    let err = service
        .create_route(
            Uuid::new_v4(),
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("does not exist"), "{}", err.detail);
}

#[test]
fn a_route_needs_exactly_one_match_family() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();

    let neither = route_input(serde_json::json!({"upstream_id": upstream.id, "match": {}}));
    assert_eq!(service.create_route(tenant, neither).unwrap_err().status(), 400);

    let both = route_input(serde_json::json!({
        "upstream_id": upstream.id,
        "match": {
            "http": {"methods": ["GET"], "path": "/"},
            "grpc": {"service": "foo.v1.Svc", "method": "Get"}
        },
    }));
    assert_eq!(service.create_route(tenant, both).unwrap_err().status(), 400);
}

#[test]
fn a_grpc_match_needs_a_grpc_upstream() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    let input = route_input(serde_json::json!({
        "upstream_id": upstream.id,
        "match": {"grpc": {"service": "foo.v1.Svc", "method": "Get"}},
    }));
    assert_eq!(service.create_route(tenant, input).unwrap_err().status(), 400);
}

#[test]
fn unsupported_methods_are_rejected() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    let input = route_input(serde_json::json!({
        "upstream_id": upstream.id,
        "match": {"http": {"methods": ["TRACE"], "path": "/"}},
    }));
    assert_eq!(service.create_route(tenant, input).unwrap_err().status(), 400);
}

#[test]
fn a_duplicate_match_rule_conflicts() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    let make = || {
        route_input(serde_json::json!({
            "upstream_id": upstream.id,
            "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        }))
    };
    service.create_route(tenant, make()).unwrap();
    assert_eq!(service.create_route(tenant, make()).unwrap_err().status(), 409);
}

#[test]
fn a_different_priority_makes_the_same_path_unique() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}},
            })),
        )
        .unwrap();
    service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "priority": 10,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}},
            })),
        )
        .unwrap();
}

#[test]
fn a_route_cannot_be_moved_to_another_upstream() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let first = service
        .create_upstream(tenant, basic_upstream(None, "a.example.com"))
        .unwrap();
    let second = service
        .create_upstream(tenant, basic_upstream(None, "b.example.com"))
        .unwrap();
    let route = service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": first.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    let err = service
        .replace_route(
            tenant,
            route.id,
            route_input(serde_json::json!({
                "upstream_id": second.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("immutable"), "{}", err.detail);
}

#[test]
fn a_route_path_is_normalized() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    let route = service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["get"], "path": "v1/chat/"}},
            })),
        )
        .unwrap();
    let http = route.http().unwrap();
    assert_eq!(http.path, "/v1/chat");
    assert_eq!(http.methods, ["GET"]);
}

// -- Plugins ----------------------------------------------------------------

#[test]
fn a_plugin_needs_a_name_and_a_source() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let blank_name: PluginInput = serde_json::from_value(serde_json::json!({
        "name": "  ",
        "plugin_type": "guard",
        "source_code": "def on_request(ctx): pass",
    }))
    .unwrap();
    assert_eq!(service.create_plugin(tenant, blank_name).unwrap_err().status(), 400);

    let blank_source: PluginInput = serde_json::from_value(serde_json::json!({
        "name": "guard",
        "plugin_type": "guard",
        "source_code": "",
    }))
    .unwrap();
    assert_eq!(service.create_plugin(tenant, blank_source).unwrap_err().status(), 400);
}

#[test]
fn a_transform_plugin_defaults_to_the_request_and_response_phases() {
    let (service, _) = service();
    let plugin: PluginInput = serde_json::from_value(serde_json::json!({
        "name": "redact",
        "type": "transform",
        "source_code": "def on_response(ctx): pass",
    }))
    .unwrap();
    let created = service.create_plugin(Uuid::new_v4(), plugin).unwrap();
    assert_eq!(created.plugin_type, PluginKind::Transform);
    assert_eq!(
        created.phases,
        [PluginPhase::OnRequest, PluginPhase::OnResponse]
    );
    assert!(created.gts_id().starts_with(gts::TRANSFORM_PLUGIN_BASE));
}

#[test]
fn a_referenced_plugin_cannot_be_deleted() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let plugin: PluginInput = serde_json::from_value(serde_json::json!({
        "name": "guard",
        "plugin_type": "guard",
        "source_code": "def on_request(ctx): pass",
    }))
    .unwrap();
    let created = service.create_plugin(tenant, plugin).unwrap();

    let err = service
        .delete_plugin(tenant, created.id, (vec![Uuid::new_v4()], Vec::new()))
        .unwrap_err();
    assert_eq!(err.status(), 409);
    assert_eq!(err.kind, ErrorKind::PluginInUse);

    service
        .delete_plugin(tenant, created.id, (Vec::new(), Vec::new()))
        .unwrap();
}

#[test]
fn a_uuid_backed_binding_must_name_an_existing_plugin() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let missing = Uuid::new_v4();
    let input = upstream_input(serde_json::json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
        "protocol": HTTP,
        "plugins": {"items": [format!("{}{missing}", gts::GUARD_PLUGIN_BASE)]},
    }));
    let err = service.create_upstream(tenant, input).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("unknown plugin"), "{}", err.detail);
}

// -- Route matching ---------------------------------------------------------

#[test]
fn path_prefix_matching_respects_segment_boundaries() {
    assert!(path_prefix_matches("/v1", "/v1"));
    assert!(path_prefix_matches("/v1", "/v1/chat"));
    assert!(!path_prefix_matches("/v1", "/v11"));
    assert!(path_prefix_matches("/", "/anything/at/all"));
}

#[test]
fn normalize_path_adds_a_root_and_trims_trailing_slashes() {
    assert_eq!(normalize_path("v1/chat/"), "/v1/chat");
    assert_eq!(normalize_path(""), "/");
    assert_eq!(normalize_path("/"), "/");
}

#[tokio::test]
async fn the_longest_matching_prefix_wins() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let ctx = security_context(tenant);
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    for path in ["/", "/v1", "/v1/chat"] {
        service
            .create_route(
                tenant,
                route_input(serde_json::json!({
                    "upstream_id": upstream.id,
                    "match": {"http": {"methods": ["GET"], "path": path}},
                })),
            )
            .unwrap();
    }

    let target = service
        .resolve_proxy_target(&ctx, "api.example.com", "GET", "/v1/chat/completions")
        .await
        .unwrap();
    assert_eq!(target.route.http().unwrap().path, "/v1/chat");
}

#[tokio::test]
async fn a_disabled_route_is_excluded_from_matching() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let ctx = security_context(tenant);
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "enabled": false,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    let err = service
        .resolve_proxy_target(&ctx, "api.example.com", "GET", "/x")
        .await
        .unwrap_err();
    assert_eq!(err.status(), 404);
}

#[tokio::test]
async fn a_method_outside_the_allowlist_does_not_match() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let ctx = security_context(tenant);
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    assert_eq!(
        service
            .resolve_proxy_target(&ctx, "api.example.com", "DELETE", "/x")
            .await
            .unwrap_err()
            .status(),
        404
    );
}

#[tokio::test]
async fn alias_resolution_is_case_insensitive() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let ctx = security_context(tenant);
    let upstream = service
        .create_upstream(tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    let target = service
        .resolve_proxy_target(&ctx, "API.Example.COM.", "GET", "/x")
        .await
        .unwrap();
    assert_eq!(target.upstream.id, upstream.id);
}

#[tokio::test]
async fn an_unknown_alias_is_a_route_not_found() {
    let (service, _) = service();
    let ctx = security_context(Uuid::new_v4());
    let err = service
        .resolve_proxy_target(&ctx, "nope.example.com", "GET", "/x")
        .await
        .unwrap_err();
    assert_eq!(err.status(), 404);
    assert_eq!(err.kind, ErrorKind::RouteNotFound);
}

#[tokio::test]
async fn a_disabled_upstream_is_unavailable_rather_than_missing() {
    let (service, _) = service();
    let tenant = Uuid::new_v4();
    let ctx = security_context(tenant);
    let mut input = basic_upstream(None, "api.example.com");
    input.enabled = false;
    let upstream = service.create_upstream(tenant, input).unwrap();
    service
        .create_route(
            tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    let err = service
        .resolve_proxy_target(&ctx, "api.example.com", "GET", "/x")
        .await
        .unwrap_err();
    assert_eq!(err.status(), 503);
}

// -- Hierarchy --------------------------------------------------------------

#[tokio::test]
async fn a_descendant_shadows_an_ancestor_alias() {
    let child_tenant = Uuid::new_v4();
    let parent_tenant = Uuid::new_v4();
    let (service, _) = service_with(Arc::new(StaticTenantDirectory::new(vec![(
        child_tenant,
        parent_tenant,
    )])));

    for tenant in [parent_tenant, child_tenant] {
        let upstream = service
            .create_upstream(tenant, basic_upstream(None, "api.example.com"))
            .unwrap();
        service
            .create_route(
                tenant,
                route_input(serde_json::json!({
                    "upstream_id": upstream.id,
                    "match": {"http": {"methods": ["GET"], "path": "/"}},
                })),
            )
            .unwrap();
    }

    let target = service
        .resolve_proxy_target(&security_context(child_tenant), "api.example.com", "GET", "/x")
        .await
        .unwrap();
    assert_eq!(target.upstream.tenant_id, child_tenant);
    assert_eq!(target.ancestors.len(), 1);
    assert_eq!(target.ancestors[0].tenant_id, parent_tenant);
}

#[tokio::test]
async fn a_descendant_inherits_an_ancestors_upstream_and_route() {
    let child_tenant = Uuid::new_v4();
    let parent_tenant = Uuid::new_v4();
    let (service, _) = service_with(Arc::new(StaticTenantDirectory::new(vec![(
        child_tenant,
        parent_tenant,
    )])));

    let upstream = service
        .create_upstream(parent_tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_route(
            parent_tenant,
            route_input(serde_json::json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    let target = service
        .resolve_proxy_target(&security_context(child_tenant), "api.example.com", "GET", "/x")
        .await
        .unwrap();
    assert_eq!(target.upstream.tenant_id, parent_tenant);
    // The management API still hides it from the descendant.
    assert!(service.get_upstream(child_tenant, upstream.id).is_none());
}

#[tokio::test]
async fn an_ancestor_disabling_the_alias_disables_it_for_descendants() {
    let child_tenant = Uuid::new_v4();
    let parent_tenant = Uuid::new_v4();
    let (service, _) = service_with(Arc::new(StaticTenantDirectory::new(vec![(
        child_tenant,
        parent_tenant,
    )])));

    let mut disabled = basic_upstream(None, "api.example.com");
    disabled.enabled = false;
    service.create_upstream(parent_tenant, disabled).unwrap();

    let child_upstream = service
        .create_upstream(child_tenant, basic_upstream(None, "api.example.com"))
        .unwrap();
    service
        .create_route(
            child_tenant,
            route_input(serde_json::json!({
                "upstream_id": child_upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
        )
        .unwrap();

    let err = service
        .resolve_proxy_target(&security_context(child_tenant), "api.example.com", "GET", "/x")
        .await
        .unwrap_err();
    assert_eq!(err.status(), 503);
}
