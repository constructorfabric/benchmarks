use uuid::Uuid;

use super::*;
use crate::domain::error::ErrorKind;
use crate::domain::model::{
    Endpoint, GrpcMatch, HttpMatch, RouteMatch, RouteSpec, Scheme, ServerConfig, UpstreamSpec,
};

/// The store bound through each repository trait, so the identically named
/// methods stay unambiguous at the call sites.
struct Fixtures {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
}

fn store() -> Fixtures {
    let store = MemoryStore::new();
    Fixtures {
        upstreams: store.clone(),
        routes: store.clone(),
        plugins: store,
    }
}

fn upstream(tenant: Uuid, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        alias_explicit: false,
        spec: UpstreamSpec {
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: format!("{alias}.example.com"),
                    port: 443,
                }],
            },
            ..UpstreamSpec::default()
        },
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

fn route(tenant: Uuid, upstream: Uuid, path: &str) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id: upstream,
        spec: RouteSpec {
            r#match: RouteMatch::Http(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: path.to_owned(),
                ..HttpMatch::default()
            }),
            ..RouteSpec::default()
        },
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

fn grpc_route(tenant: Uuid, upstream: Uuid) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id: upstream,
        spec: RouteSpec {
            r#match: RouteMatch::Grpc(GrpcMatch {
                service: "foo.v1.UserService".to_owned(),
                method: "GetUser".to_owned(),
            }),
            ..RouteSpec::default()
        },
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

fn plugin(tenant: Uuid) -> Plugin {
    Plugin {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        name: "my-transform".to_owned(),
        kind: crate::domain::model::PluginType::Transform,
        config_schema: None,
        source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

#[test]
fn insert_find_and_list_are_tenant_scoped() {
    let Fixtures { upstreams, .. } = store();
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    let record = upstream(tenant, "api.openai.com");
    let id = record.id;
    upstreams.insert(record).expect("inserted");

    assert!(upstreams.find(tenant, id).expect("find").is_some());
    assert!(upstreams.find(other, id).expect("find").is_none());
    assert_eq!(upstreams.list(tenant).expect("list").len(), 1);
    assert_eq!(upstreams.list(other).expect("list").len(), 0);
}

#[test]
fn duplicate_alias_in_a_tenant_conflicts() {
    let Fixtures { upstreams, .. } = store();
    let tenant = Uuid::new_v4();
    upstreams
        .insert(upstream(tenant, "api.openai.com"))
        .expect("first insert");
    let error = upstreams
        .insert(upstream(tenant, "api.openai.com"))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::ResourceConflict);

    // A different tenant may reuse the alias.
    assert!(
        upstreams
            .insert(upstream(Uuid::new_v4(), "api.openai.com"))
            .is_ok()
    );
}

#[test]
fn update_rejects_foreign_rows_and_alias_stealing() {
    let Fixtures { upstreams, .. } = store();
    let tenant = Uuid::new_v4();
    let first = upstream(tenant, "a.example.com");
    let second = upstream(tenant, "b.example.com");
    upstreams.insert(first.clone()).expect("insert");
    upstreams.insert(second.clone()).expect("insert");

    let renamed = Upstream {
        alias: "b.example.com".to_owned(),
        updated_at: 1_700_000_100,
        ..second
    };
    upstreams.update(renamed).expect("rename of self is fine");

    let steal = Upstream {
        id: first.id,
        alias: "b.example.com".to_owned(),
        ..first.clone()
    };
    assert_eq!(
        upstreams.update(steal).unwrap_err().kind,
        ErrorKind::ResourceConflict
    );

    let foreign = Upstream {
        tenant_id: Uuid::new_v4(),
        ..first
    };
    assert_eq!(
        upstreams.update(foreign).unwrap_err().kind,
        ErrorKind::ResourceNotFound
    );
}

#[test]
fn delete_removes_only_the_owning_tenant_row() {
    let Fixtures { upstreams, .. } = store();
    let tenant = Uuid::new_v4();
    let record = upstream(tenant, "api.openai.com");
    let id = record.id;
    upstreams.insert(record).expect("insert");

    assert!(!upstreams.delete(Uuid::new_v4(), id).expect("delete"));
    assert!(upstreams.delete(tenant, id).expect("delete"));
    assert!(!upstreams.delete(tenant, id).expect("delete again"));
}

#[test]
fn route_match_rule_and_priority_are_unique_per_upstream() {
    let Fixtures {
        upstreams, routes, ..
    } = store();
    let tenant = Uuid::new_v4();
    let record = upstream(tenant, "api.openai.com");
    upstreams.insert(record.clone()).expect("insert");

    routes
        .insert(route(tenant, record.id, "/v1/chat"))
        .expect("first route");
    assert_eq!(
        routes
            .insert(route(tenant, record.id, "/v1/chat"))
            .unwrap_err()
            .kind,
        ErrorKind::ResourceConflict
    );

    // Same path on another upstream is fine, and so is a different path.
    let other = upstream(tenant, "other.example.com");
    upstreams.insert(other.clone()).expect("insert");
    assert!(routes.insert(route(tenant, other.id, "/v1/chat")).is_ok());
    assert!(routes.insert(route(tenant, record.id, "/v1/embed")).is_ok());

    // A different discriminant never collides with the HTTP rule.
    assert!(routes.insert(grpc_route(tenant, record.id)).is_ok());
}

#[test]
fn delete_by_upstream_cascades_and_respects_the_tenant() {
    let Fixtures {
        upstreams, routes, ..
    } = store();
    let tenant = Uuid::new_v4();
    let record = upstream(tenant, "api.openai.com");
    upstreams.insert(record.clone()).expect("insert");
    routes
        .insert(route(tenant, record.id, "/v1/a"))
        .expect("route");
    routes
        .insert(route(tenant, record.id, "/v1/b"))
        .expect("route");

    assert_eq!(routes.delete_by_upstream(tenant, record.id).expect("ok"), 2);
    assert_eq!(
        routes
            .delete_by_upstream(Uuid::new_v4(), record.id)
            .expect("ok"),
        0
    );
}

#[test]
fn plugin_rows_roundtrip_through_the_store() {
    let Fixtures { plugins, .. } = store();
    let tenant = Uuid::new_v4();
    let record = plugin(tenant);
    plugins.insert(record.clone()).expect("insert");
    assert_eq!(plugins.list(tenant).expect("list").len(), 1);
    assert_eq!(
        plugins.find(tenant, record.id).expect("find").unwrap(),
        record
    );
    assert!(
        plugins
            .find(Uuid::new_v4(), record.id)
            .expect("find")
            .is_none()
    );
    assert!(plugins.delete(tenant, record.id).expect("delete"));
}

#[test]
fn match_keys_are_stable_and_discriminant_aware() {
    let http = RouteMatch::Http(HttpMatch {
        methods: vec!["GET".to_owned(), "POST".to_owned()],
        path: "/v1/x".to_owned(),
        ..HttpMatch::default()
    });
    let http_flipped = RouteMatch::Http(HttpMatch {
        methods: vec!["POST".to_owned(), "GET".to_owned()],
        path: "/v1/x".to_owned(),
        ..HttpMatch::default()
    });
    let grpc = RouteMatch::Grpc(GrpcMatch {
        service: "s".to_owned(),
        method: "m".to_owned(),
    });
    assert_eq!(http.key(), http_flipped.key());
    assert_ne!(http.key(), grpc.key());
    assert!(http.key().starts_with("http|"));
    assert!(grpc.key().starts_with("grpc|"));
}
