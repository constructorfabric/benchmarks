//! Crate-level tests for the OAGW gear: lifecycle, config, control-plane
//! semantics and readiness wiring.

use std::time::Duration;

use crate::api::rest::ProxyPort;
use crate::config::{OagwConfig, RouteSeed};
use crate::domain::error::DomainError;
use crate::domain::models::{Plugin, PluginKind, Route, Upstream, UpstreamScheme};
use crate::domain::repository::ControlPlaneService;

/// The seed repo must be constructible from a default config and expose the
/// expected control-plane defaults (persistence-free, ADR 0010).
#[test]
fn control_plane_from_default_config_is_empty_and_usable() {
    let cfg = OagwConfig::default();
    let repo = ControlPlaneService::from_config(&cfg).expect("default config seeds cleanly");
    assert!(repo.list_upstreams().is_empty());
    assert!(repo.list_routes().is_empty());
    assert!(repo.list_plugins().is_empty());
    // Serving-tenant gate admits the serving tenant.
    repo.assert_tenant("root", "upstreams")
        .expect("root admitted");
    // And rejects foreign tenants immediately.
    let err = repo.assert_tenant("foreign", "upstreams").unwrap_err();
    assert!(matches!(err, DomainError::TenantScope(_)));
}

/// CRUD on the control plane mirrors `oagw_upstream` / `oagw_route` /
/// `oagw_plugin` semantics with binding and cache-invalidation hooks.
#[test]
fn control_plane_crud_and_bindings() {
    let repo = ControlPlaneService::new(0, Duration::from_secs(2), false);

    let upstream = Upstream {
        alias: "httpbin".to_owned(),
        name: "httpbin".to_owned(),
        host: "httpbin.org".to_owned(),
        port: 443,
        scheme: UpstreamScheme::Https,
        path_prefix: String::new(),
        enabled: true,
        timeout_secs: 0,
    };
    repo.upsert_upstream(upstream.clone())
        .expect("seed upstream");
    assert_eq!(repo.list_upstreams().len(), 1);

    let route = Route {
        alias: "bin".to_owned(),
        upstream_alias: Some("httpbin".to_owned()),
        methods: None,
        http_matches: Vec::new(),
        rate_limit: None,
        cors: Default::default(),
        enabled: true,
        priority: 0,
    };
    repo.upsert_route(route.clone()).expect("seed route");
    assert_eq!(repo.get_route("bin").expect("route present").alias, "bin");

    let plugin = Plugin {
        alias: "noop-1".to_owned(),
        kind: PluginKind::Noop,
        enabled: true,
        config: serde_json::json!({}),
    };
    repo.upsert_plugin(plugin).expect("seed plugin");
    repo.bind_plugin_to_route("bin", "noop-1")
        .expect("bind to route");
    assert!(
        repo.list_route_plugins("bin")
            .contains(&"noop-1".to_owned())
    );
    repo.unbind_plugin_from_route("bin", "noop-1")
        .expect("unbind from route");
    assert!(repo.list_route_plugins("bin").is_empty());

    // Deleting an upstream that a route still references is rejected.
    let err = repo.delete_upstream("httpbin").unwrap_err();
    assert!(matches!(
        err,
        DomainError::RouteDisabledUpstream(_, _) | DomainError::Validation { .. }
    ));
}

/// A route cannot reference an upstream that does not exist.
#[test]
fn route_rejects_unknown_upstream() {
    let repo = ControlPlaneService::new(0, Duration::from_secs(2), false);
    let route = Route {
        alias: "orphan".to_owned(),
        upstream_alias: Some("nope".to_owned()),
        methods: None,
        http_matches: Vec::new(),
        rate_limit: None,
        cors: Default::default(),
        enabled: true,
        priority: 0,
    };
    let err = repo
        .upsert_route(route)
        .expect_err("unknown upstream rejected");
    assert!(err.to_string().contains("nope"), "err: {err}");
}

/// `from_config` seeds config-declared upstreams/routes/plugins and wiring
/// errors surface loudly at boot rather than at request time.
#[test]
fn from_config_seeds_declared_resources() {
    let mut cfg = OagwConfig::default();
    cfg.upstreams.push(crate::config::UpstreamSeed {
        alias: "a".to_owned(),
        name: String::new(),
        host: "example.com".to_owned(),
        port: None,
        scheme: "https".to_owned(),
        path_prefix: String::new(),
        enabled: true,
    });
    cfg.routes.push(RouteSeed {
        alias: "r".to_owned(),
        upstream_alias: "a".to_owned(),
        methods: None,
        rate_limit: None,
        cors: None,
        enabled: true,
    });
    let repo = ControlPlaneService::from_config(&cfg).expect("seeds");
    assert_eq!(repo.list_upstreams().len(), 1);
    assert_eq!(repo.list_routes().len(), 1);

    // A route referencing a missing config upstream fails loudly at boot.
    cfg.routes[0].upstream_alias = "missing".to_owned();
    let err = ControlPlaneService::from_config(&cfg).expect_err("invalid seed rejected");
    assert!(err.to_string().contains("missing"), "err: {err}");

    // Plaintext upstreams are blocked unless allow_http_upstream is set — and
    // the validation rejects them before any service is wired.
    let mut cfg2 = OagwConfig {
        allow_http_upstream: false,
        ..Default::default()
    };
    cfg2.upstreams.push(crate::config::UpstreamSeed {
        alias: "plain".to_owned(),
        name: String::new(),
        host: "example.com".to_owned(),
        port: Some(80),
        scheme: "http".to_owned(),
        path_prefix: String::new(),
        enabled: true,
    });
    let err = OagwConfig::validate(&cfg2).unwrap_err();
    assert!(err.to_string().contains("http"), "err: {err}");
}

/// The proxy port cell stays `0` until bound, then reports the bound value —
/// the ready/not-ready signal the healthcheck and relay rely on.
#[test]
fn proxy_port_cell_tracks_binding() {
    let port = ProxyPort::new();
    assert_eq!(port.port(), 0);
    port.bind(41234);
    assert_eq!(port.port(), 41234);
}
