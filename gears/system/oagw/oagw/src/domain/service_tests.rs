// Created: 2026-09-02 by Constructor Tech
//! Tests for the control-plane CRUD semantics.

use uuid::Uuid;

use super::Caller;
use crate::domain::model::{
    self, CorsConfig, Endpoint, GrpcMatch, HttpMatch, HttpMethod, MatchConfig, Protocol,
    RateLimitConfig, RateWindow, Route, Scheme, ServerConfig, SustainedRate, Upstream,
};
use crate::domain::query::ListQuery;
use crate::domain::store::Store;
use crate::error::GatewayError;

fn caller(tenant: u128) -> Caller {
    Caller {
        tenant_id: Uuid::from_u128(tenant),
        ancestors: Vec::new(),
        subject_tenant_id: Uuid::from_u128(tenant),
        subject_id: "user-1".to_owned(),
    }
}

fn endpoint(host: &str) -> Endpoint {
    Endpoint { scheme: Scheme::Https, host: host.to_owned(), port: None }
}

fn upstream(hosts: &[&str], alias: &str) -> Upstream {
    Upstream {
        id: None,
        enabled: true,
        alias: alias.to_owned(),
        tags: vec!["llm".to_owned()],
        server: ServerConfig {
            endpoints: hosts.iter().map(|h| endpoint(h)).collect(),
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn route(upstream_id: &str, path: &str, methods: &[HttpMethod]) -> Route {
    Route {
        id: None,
        tags: Vec::new(),
        upstream_id: upstream_id.to_owned(),
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: model::SuffixMode::Append,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
    }
}

fn service() -> (super::ControlPlane, Caller) {
    let caller = caller(0x1000);
    (
        super::ControlPlane::new(std::sync::Arc::new(Store::new()), 50, 100),
        caller,
    )
}

#[test]
fn create_derives_the_alias_from_a_hostname() {
    let (cp, caller) = service();
    let spec = upstream(&["api.openai.com"], "");
    let created = cp.create_upstream(&caller, spec).unwrap();
    assert_eq!(created.alias, "api.openai.com");
    assert!(created.id.as_deref().unwrap().starts_with("gts.cf.core.oagw.upstream.v1~"));
    assert!(created.enabled);
}

#[test]
fn create_tolerates_the_idempotent_derived_alias() {
    let (cp, caller) = service();
    let created = cp.create_upstream(&caller, upstream(&["api.openai.com"], "api.openai.com")).unwrap();
    assert_eq!(created.alias, "api.openai.com");
}

#[test]
fn create_rejects_a_diverging_alias_for_derivable_endpoints() {
    let (cp, caller) = service();
    let err = cp.create_upstream(&caller, upstream(&["api.openai.com"], "my-label")).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("auto-derived")), "{err:?}");
}

#[test]
fn create_requires_an_explicit_alias_for_ip_endpoints() {
    let (cp, caller) = service();
    let err = cp.create_upstream(&caller, upstream(&["10.0.1.1"], "")).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("alias")), "{err:?}");

    let created = cp.create_upstream(&caller, upstream(&["10.0.1.1"], "my-service")).unwrap();
    assert_eq!(created.alias, "my-service");
}

#[test]
fn create_rejects_a_duplicate_alias_within_the_tenant() {
    let (cp, caller) = service();
    cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let err = cp.create_upstream(&caller, upstream(&["API.OpenAI.COM."], "")).unwrap_err();
    assert!(matches!(err, GatewayError::Conflict(_)), "{err:?}");
}

#[test]
fn the_same_alias_is_legal_in_another_tenant() {
    let (cp, first) = service();
    cp.create_upstream(&first, upstream(&["api.openai.com"], "")).unwrap();
    let second = caller(0x2000);
    let created = cp.create_upstream(&second, upstream(&["api.openai.com"], "")).unwrap();
    assert_eq!(created.alias, "api.openai.com");
}

#[test]
fn create_validates_the_endpoint_pool() {
    let (cp, caller) = service();
    let err = cp.create_upstream(&caller, upstream(&[], "")).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(_)));

    let mut spec = upstream(&["api.openai.com", "eu.vendor.com"], "");
    spec.server.endpoints[1].port = Some(8443);
    let err = cp.create_upstream(&caller, spec).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("port")), "{err:?}");

    let mut spec = upstream(&["api.openai.com"], "");
    spec.server.endpoints[0].scheme = Scheme::Http;
    spec.server.endpoints[0].port = Some(8080);
    spec.server.endpoints.push(Endpoint { scheme: Scheme::Https, host: "a.example.com".into(), port: Some(8080) });
    let err = cp.create_upstream(&caller, spec).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("scheme")), "{err:?}");
}

#[test]
fn ancestor_upstreams_are_invisible_to_the_management_api() {
    // A parent creates an upstream; its child (which lists the parent as an
    // ancestor) must not be able to read, replace or delete it.
    let (cp, parent) = service();
    let created = cp.create_upstream(&parent, upstream(&["api.openai.com"], "")).unwrap();
    let id = created.id.clone().unwrap();

    let child = Caller {
        tenant_id: Uuid::from_u128(0x2000),
        ancestors: vec![parent.tenant_id],
        subject_tenant_id: Uuid::from_u128(0x2000),
        subject_id: "user-2".to_owned(),
    };
    assert!(matches!(cp.get_upstream(&child, &id), Err(GatewayError::NotFound(_))));
    assert_eq!(cp.list_upstreams(&child, &ListQuery::default()).unwrap().len(), 0);
    assert!(matches!(
        cp.replace_upstream(&child, &id, upstream(&["api.openai.com"], "")),
        Err(GatewayError::NotFound(_))
    ));
}

#[test]
fn put_rejects_an_endpoint_change_that_would_alter_the_alias() {
    let (cp, caller) = service();
    let created = cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let id = created.id.clone().unwrap();

    let mut spec = upstream(&["eu.vendor.com"], "");
    let err = cp.replace_upstream(&caller, &id, spec.clone()).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("immutable")), "{err:?}");

    // An explicit alias does not pin the routing key either: the endpoints
    // would re-derive a different alias, so the PUT is still rejected.
    spec.alias = created.alias.clone();
    let err = cp.replace_upstream(&caller, &id, spec).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("immutable")), "{err:?}");
}

#[test]
fn put_keeps_the_alias_of_an_ip_upstream() {
    let (cp, caller) = service();
    let created = cp.create_upstream(&caller, upstream(&["10.0.1.1"], "my-service")).unwrap();
    let id = created.id.clone().unwrap();

    // IP → IP keeps the existing alias.
    let replaced = cp
        .replace_upstream(&caller, &id, upstream(&["10.0.1.2"], "my-service"))
        .unwrap();
    assert_eq!(replaced.alias, "my-service");

    // A differing alias is rejected.
    let err = cp
        .replace_upstream(&caller, &id, upstream(&["10.0.1.3"], "renamed"))
        .unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("immutable")), "{err:?}");

    // IP → hostname that derives a different alias is rejected.
    let err = cp.replace_upstream(&caller, &id, upstream(&["api.openai.com"], "")).unwrap_err();
    assert!(matches!(err, GatewayError::Validation(ref m) if m.contains("immutable")), "{err:?}");
}

#[test]
fn put_rejects_an_invisible_resource() {
    let (cp, first) = service();
    let created = cp.create_upstream(&first, upstream(&["api.openai.com"], "")).unwrap();
    let id = created.id.clone().unwrap();
    let other = caller(0x9000);
    assert!(matches!(
        cp.replace_upstream(&other, &id, upstream(&["api.openai.com"], "")),
        Err(GatewayError::NotFound(_))
    ));
    assert!(matches!(cp.get_upstream(&other, &id), Err(GatewayError::NotFound(_))));
    assert!(matches!(cp.delete_upstream(&other, &id), Err(GatewayError::NotFound(_))));
}

#[test]
fn enable_and_disable_round_trip() {
    let (cp, caller) = service();
    let created = cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let id = created.id.clone().unwrap();
    let disabled = cp.set_upstream_enabled(&caller, &id, false).unwrap();
    assert!(!disabled.enabled);
    let enabled = cp.set_upstream_enabled(&caller, &id, true).unwrap();
    assert!(enabled.enabled);
}

#[test]
fn delete_cascades_to_bound_routes() {
    let (cp, caller) = service();
    let up = cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let gid = up.id.clone().unwrap();
    cp.create_route(&caller, route(&gid, "/v1", &[HttpMethod::Get])).unwrap();
    cp.delete_upstream(&caller, &gid).unwrap();
    assert_eq!(cp.list_routes(&caller, &ListQuery::default()).unwrap().len(), 0);
}

#[test]
fn routes_must_reference_an_upstream_of_the_calling_tenant() {
    let (cp, first) = service();
    let up = cp.create_upstream(&first, upstream(&["api.openai.com"], "")).unwrap();
    let other = caller(0x4000);
    let err = cp
        .create_route(&other, route(up.id.as_deref().unwrap(), "/v1", &[HttpMethod::Get]))
        .unwrap_err();
    assert!(matches!(err, GatewayError::NotFound(_)), "{err:?}");
}

#[test]
fn routes_reject_a_duplicate_match_rule() {
    let (cp, caller) = service();
    let up = cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let gid = up.id.clone().unwrap();
    cp.create_route(&caller, route(&gid, "/v1", &[HttpMethod::Get, HttpMethod::Post]))
        .unwrap();
    let err = cp.create_route(&caller, route(&gid, "/v1", &[HttpMethod::Post])).unwrap_err();
    assert!(matches!(err, GatewayError::Conflict(_)), "{err:?}");
    // A different method on the same path is a different route.
    cp.create_route(&caller, route(&gid, "/v1", &[HttpMethod::Delete])).unwrap();
}

#[test]
fn routes_validate_the_match_shape() {
    let (cp, caller) = service();
    let up = cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let gid = up.id.clone().unwrap();

    let mut spec = route(&gid, "/v1", &[]);
    assert!(matches!(cp.create_route(&caller, spec.clone()), Err(GatewayError::Validation(_))));

    spec.match_config.http.as_mut().unwrap().path = "v1".to_owned();
    assert!(matches!(cp.create_route(&caller, spec.clone()), Err(GatewayError::Validation(_))));

    spec.match_config.http.as_mut().unwrap().path = "/v1".to_owned();
    spec.match_config.grpc = Some(GrpcMatch { service: "s".into(), method: "m".into() });
    assert!(matches!(cp.create_route(&caller, spec.clone()), Err(GatewayError::Validation(_))));

    spec.match_config.grpc = None;
    spec.match_config.http = None;
    assert!(matches!(cp.create_route(&caller, spec.clone()), Err(GatewayError::Validation(_))));

    spec.match_config.http = Some(HttpMatch {
        methods: vec![HttpMethod::Get],
        path: "/v1".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: model::SuffixMode::Append,
    });
    assert!(cp.create_route(&caller, spec).is_ok());
}

#[test]
fn route_upstream_id_is_immutable_on_replace() {
    let (cp, caller) = service();
    let a = cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let b = cp.create_upstream(&caller, upstream(&["api.anthropic.com"], "")).unwrap();
    let created = cp
        .create_route(&caller, route(a.id.as_deref().unwrap(), "/v1", &[HttpMethod::Get]))
        .unwrap();
    let mut spec = route(b.id.as_deref().unwrap(), "/v2", &[HttpMethod::Get]);
    let err = cp
        .replace_route(&caller, created.id.as_deref().unwrap(), spec.clone())
        .unwrap_err();
    assert!(matches!(err, GatewayError::Conflict(_)), "{err:?}");
    spec.upstream_id = String::new();
    let replaced = cp
        .replace_route(&caller, created.id.as_deref().unwrap(), spec)
        .unwrap();
    assert_eq!(replaced.upstream_id, a.id.unwrap());
}

#[test]
fn plugins_are_immutable_and_deletable_only_when_unreferenced() {
    let (cp, caller) = service();
    let spec = model::Plugin {
        id: None,
        plugin_type: model::PluginType::Guard,
        name: "redact".to_owned(),
        source_code: "def plugin(ctx): return ctx.next()".to_owned(),
        config: None,
        tags: Vec::new(),
    };
    let created = cp.create_plugin(&caller, spec.clone()).unwrap();
    assert!(created.id.as_deref().unwrap().starts_with("gts.cf.core.oagw.plugin.v1~"));

    // No PUT exists; creating the same name again conflicts.
    let err = cp.create_plugin(&caller, spec).unwrap_err();
    assert!(matches!(err, GatewayError::Conflict(_)), "{err:?}");

    let listed = cp.list_plugins(&caller, &ListQuery::default()).unwrap();
    assert_eq!(listed.len(), 1);

    let id = created.id.clone().unwrap();
    let source = cp.plugin_source(&caller, &id).unwrap();
    assert_eq!(source, "def plugin(ctx): return ctx.next()");

    cp.delete_plugin(&caller, &id).unwrap();
    assert!(matches!(cp.get_plugin(&caller, &id), Err(GatewayError::NotFound(_))));
}

#[test]
fn delete_plugin_reports_references() {
    let (cp, caller) = service();
    let spec = model::Plugin {
        id: None,
        plugin_type: model::PluginType::Guard,
        name: "redact".to_owned(),
        source_code: "def plugin(ctx): pass".to_owned(),
        config: None,
        tags: Vec::new(),
    };
    let created = cp.create_plugin(&caller, spec).unwrap();
    let plugin_id = created.id.clone().unwrap();
    let up = cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    let gid = up.id.clone().unwrap();
    let mut spec = route(&gid, "/v1", &[HttpMethod::Get]);
    spec.plugins = Some(model::PluginsConfig {
        sharing: model::Sharing::default(),
        items: vec![serde_json::json!(plugin_id.clone())],
    });
    cp.create_route(&caller, spec).unwrap();

    let err = cp.delete_plugin(&caller, &plugin_id).unwrap_err();
    match err {
        GatewayError::PluginInUse { routes, upstreams, .. } => {
            assert_eq!(routes.len(), 1);
            assert!(upstreams.is_empty());
        }
        other => panic!("expected PluginInUse, got {other:?}"),
    }
    // The plugin is still there: the delete did not go through.
    assert!(cp.get_plugin(&caller, &plugin_id).is_ok());
}

#[test]
fn cors_rejects_credentials_with_a_wildcard_origin() {
    let cors = CorsConfig {
        sharing: model::Sharing::default(),
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allowed_methods: model::default_cors_methods(),
        expose_headers: Vec::new(),
        allow_credentials: true,
    };
    assert!(matches!(super::validate_cors(&cors), Err(GatewayError::Validation(_))));
    let specific = CorsConfig {
        allowed_origins: vec!["https://app.example.com".to_owned()],
        allow_credentials: true,
        ..cors.clone()
    };
    assert!(super::validate_cors(&specific).is_ok());
    let wildcard = CorsConfig { allow_credentials: false, ..cors };
    assert!(super::validate_cors(&wildcard).is_ok());
}

#[test]
fn list_endpoints_filter_and_order() {
    let (cp, caller) = service();
    cp.create_upstream(&caller, upstream(&["api.openai.com"], "")).unwrap();
    cp.create_upstream(&caller, upstream(&["api.anthropic.com"], "")).unwrap();

    let query = ListQuery {
        filter: Some("alias eq 'api.anthropic.com'".to_owned()),
        select: Some(vec!["alias".to_owned()]),
        orderby: Some("alias asc".to_owned()),
        top: None,
        skip: None,
    };
    let out = cp.list_upstreams(&caller, &query).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["alias"], "api.anthropic.com");
    assert!(out[0].get("server").is_none(), "$select projects the response");
}

#[test]
fn rate_limits_are_stored_with_their_defaults() {
    let (cp, caller) = service();
    let mut spec = upstream(&["api.openai.com"], "");
    spec.rate_limit = Some(RateLimitConfig {
        sharing: model::Sharing::Enforce,
        algorithm: model::RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate { rate: 5, window: RateWindow::Minute },
        burst: None,
        scope: model::RateScope::Tenant,
        strategy: model::RateStrategy::Reject,
        cost: 1,
    });
    let created = cp.create_upstream(&caller, spec).unwrap();
    let limit = created.rate_limit.unwrap();
    assert_eq!(limit.capacity(), 5);
    assert_eq!(limit.refill_period(), std::time::Duration::from_secs(12));
}
