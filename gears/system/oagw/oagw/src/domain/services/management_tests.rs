use std::sync::Arc;
use uuid::Uuid;

use super::*;
use crate::domain::error::ErrorKind;
use crate::domain::model::PluginBinding;
use crate::domain::model::{Endpoint, HttpMatch, RouteMatch, Scheme, ServerConfig, UpstreamSpec};
use crate::domain::repo::RouteRepository;
use crate::infra::storage::memory::MemoryStore;

fn service() -> ControlPlaneService {
    let store = MemoryStore::new();
    ControlPlaneService::new(
        store.clone(),
        Arc::clone(&store) as Arc<dyn RouteRepository>,
        store as Arc<dyn PluginRepository>,
    )
}

fn spec(host: &str, port: u16) -> UpstreamSpec {
    UpstreamSpec {
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: host.to_owned(),
                port,
            }],
        },
        ..UpstreamSpec::default()
    }
}

fn http_route(path: &str) -> RouteSpec {
    RouteSpec {
        r#match: RouteMatch::Http(HttpMatch {
            methods: vec!["GET".to_owned()],
            path: path.to_owned(),
            ..HttpMatch::default()
        }),
        ..RouteSpec::default()
    }
}

#[test]
fn create_upstream_derives_the_alias_and_generates_ids() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let record = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("created");
    assert_eq!(record.alias, "api.openai.com");
    assert!(!record.alias_explicit);
    assert!(record.created_at > 0);
    assert_eq!(record.created_at, record.updated_at);
}

#[test]
fn create_upstream_accepts_an_exact_derived_alias_and_rejects_an_override() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let exact = svc
        .create_upstream(tenant, spec("api.openai.com", 443), Some("API.OpenAI.COM."))
        .expect("tolerated");
    assert_eq!(exact.alias, "api.openai.com");

    let override_attempt = svc.create_upstream(tenant, spec("api.openai.com", 443), Some("nope"));
    assert_eq!(override_attempt.unwrap_err().kind, ErrorKind::Validation);
}

#[test]
fn ip_upstreams_require_an_explicit_alias() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let error = svc
        .create_upstream(tenant, spec("127.0.0.1", 8080), None)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Validation);

    let record = svc
        .create_upstream(tenant, spec("127.0.0.1", 8080), Some("My-Service"))
        .expect("created");
    assert_eq!(record.alias, "my-service");
    assert!(record.alias_explicit);
}

#[test]
fn alias_is_unique_per_tenant() {
    let svc = service();
    let tenant = Uuid::new_v4();
    svc.create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("first");
    let conflict = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .unwrap_err();
    assert_eq!(conflict.kind, ErrorKind::ResourceConflict);
    assert!(
        svc.create_upstream(Uuid::new_v4(), spec("api.openai.com", 443), None)
            .is_ok()
    );
}

#[test]
fn empty_endpoints_are_rejected() {
    let svc = service();
    let error = svc
        .create_upstream(Uuid::new_v4(), UpstreamSpec::default(), None)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Validation);
}

#[test]
fn replace_upstream_keeps_the_alias_and_rejects_endpoint_drift() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("created");

    let replaced = svc
        .replace_upstream(tenant, created.id, spec("api.openai.com", 443), None)
        .expect("replaced");
    assert_eq!(replaced.alias, "api.openai.com");
    assert!(replaced.updated_at >= replaced.created_at);

    let drift = svc.replace_upstream(tenant, created.id, spec("other.openai.com", 443), None);
    assert_eq!(drift.unwrap_err().kind, ErrorKind::Validation);

    assert_eq!(
        svc.replace_upstream(tenant, Uuid::new_v4(), spec("api.openai.com", 443), None)
            .unwrap_err()
            .kind,
        ErrorKind::ResourceNotFound
    );
}

#[test]
fn deleting_an_upstream_cascades_its_routes() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("created");
    let route = svc
        .create_route(tenant, created.id, http_route("/v1/chat"))
        .expect("route");
    assert!(svc.delete_upstream(tenant, created.id).expect("deleted"));
    assert!(svc.get_route(tenant, route.id).expect("get").is_none());
    assert!(svc.list_routes(tenant, None).expect("list").is_empty());
}

#[test]
fn routes_require_a_tenant_local_upstream() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let foreign = svc
        .create_upstream(Uuid::new_v4(), spec("api.openai.com", 443), None)
        .expect("created");
    assert_eq!(
        svc.create_route(tenant, foreign.id, http_route("/v1"))
            .unwrap_err()
            .kind,
        ErrorKind::ResourceNotFound
    );

    let local = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("created");
    assert!(
        svc.create_route(tenant, local.id, http_route("/v1"))
            .is_ok()
    );
}

#[test]
fn route_match_rules_are_unique_within_an_upstream() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("created");
    svc.create_route(tenant, created.id, http_route("/v1"))
        .expect("first");
    assert_eq!(
        svc.create_route(tenant, created.id, http_route("/v1"))
            .unwrap_err()
            .kind,
        ErrorKind::ResourceConflict
    );
}

#[test]
fn replace_route_rejects_a_changed_upstream() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let a = svc
        .create_upstream(tenant, spec("a.example.com", 443), None)
        .expect("created");
    let b = svc
        .create_upstream(tenant, spec("b.example.com", 443), None)
        .expect("created");
    let route = svc
        .create_route(tenant, a.id, http_route("/v1"))
        .expect("route");

    let moved = svc.replace_route(tenant, route.id, b.id, http_route("/v1"));
    assert_eq!(moved.unwrap_err().kind, ErrorKind::Validation);

    let kept = svc.replace_route(tenant, route.id, a.id, http_route("/v2"));
    assert!(kept.is_ok());
}

#[test]
fn routes_validate_the_match_rule() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let created = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("created");
    let bad_path = http_route("v1");
    assert_eq!(
        svc.create_route(tenant, created.id, bad_path)
            .unwrap_err()
            .kind,
        ErrorKind::Validation
    );
    let bad_method = RouteSpec {
        r#match: RouteMatch::Http(HttpMatch {
            methods: vec!["get".to_owned()],
            path: "/v1".to_owned(),
            ..HttpMatch::default()
        }),
        ..RouteSpec::default()
    };
    assert!(svc.create_route(tenant, created.id, bad_method).is_err());
}

#[test]
fn plugins_are_created_listed_and_deleted() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let plugin = svc
        .create_plugin(
            tenant,
            NewPlugin {
                name: "my-transform".to_owned(),
                kind: PluginType::Transform,
                config_schema: None,
                source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
            },
        )
        .expect("created");
    assert_eq!(svc.list_plugins(tenant).expect("list").len(), 1);
    assert!(svc.delete_plugin(tenant, plugin.id).expect("deleted"));
    assert!(!svc.delete_plugin(tenant, plugin.id).expect("deleted"));
}

#[test]
fn a_bound_plugin_cannot_be_deleted() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let plugin = svc
        .create_plugin(
            tenant,
            NewPlugin {
                name: "my-transform".to_owned(),
                kind: PluginType::Transform,
                config_schema: None,
                source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
            },
        )
        .expect("created");
    let upstream = svc
        .create_upstream(tenant, spec("api.openai.com", 443), None)
        .expect("created");
    let mut bound = spec("api.openai.com", 443);
    bound.plugins.items = vec![crate::domain::model::PluginBinding::from_spec(
        crate::domain::model::PluginBindingSpec::Reference(plugin.id.to_string()),
    )];

    let bound_upstream = svc
        .replace_upstream(tenant, upstream.id, bound, None)
        .expect("bound");
    assert_eq!(bound_upstream.spec.plugins.items.len(), 1);

    let error = svc.delete_plugin(tenant, plugin.id).unwrap_err();
    assert_eq!(error.kind, ErrorKind::PluginInUse);

    let mut unbound = spec("api.openai.com", 443);
    unbound.plugins.items.clear();
    svc.replace_upstream(tenant, upstream.id, unbound, None)
        .expect("unbound");
    assert!(svc.delete_plugin(tenant, plugin.id).expect("deleted"));
}

#[test]
fn plugin_create_validates_name_and_source() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let empty_name = svc.create_plugin(
        tenant,
        NewPlugin {
            name: "  ".to_owned(),
            kind: PluginType::Transform,
            config_schema: None,
            source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
        },
    );
    assert_eq!(empty_name.unwrap_err().kind, ErrorKind::Validation);

    let empty_source = svc.create_plugin(
        tenant,
        NewPlugin {
            name: "x".to_owned(),
            kind: PluginType::Transform,
            config_schema: None,
            source_code: "   ".to_owned(),
        },
    );
    assert_eq!(empty_source.unwrap_err().kind, ErrorKind::Validation);
}

#[test]
fn catalog_only_plugins_cannot_be_bound() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let mut spec = spec("api.openai.com", 443);
    spec.plugins.items = vec![PluginBinding {
        plugin_ref: format!(
            "{}{}",
            crate::ids::GUARD_PLUGIN_TYPE,
            "cf.core.oagw.timeout.v1"
        ),
        config: serde_json::Value::Null,
    }];
    let error = svc.create_upstream(tenant, spec, None).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Validation);
    assert_eq!(error.kind.http_status(), 400);
}
