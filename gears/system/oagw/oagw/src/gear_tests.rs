#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use super::OagwGear;
use crate::config::OagwConfig;
use crate::domain::dto::{PluginCommand, RequestContext, UpstreamCommand};
use crate::domain::model::{Endpoint, EndpointScheme, GUARD_PLUGIN_TYPE, Protocol, ServerConfig};
use crate::domain::repo::{
    AllowAllAuthorizer, PluginRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::services::ControlPlaneService;
use crate::infra::storage::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository,
};

fn tenant() -> uuid::Uuid {
    uuid::Uuid::from_u128(0xA001)
}

/// The same wiring [`OagwGear::init`] performs, expressed over the same
/// configuration so the assembly is exercised without a platform `GearCtx`.
fn service_from(config: &OagwConfig) -> Arc<ControlPlaneService> {
    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    let plugins = Arc::new(MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::clone(&upstreams) as Arc<dyn UpstreamRepository>,
        Arc::clone(&routes) as Arc<dyn RouteRepository>,
    ));
    Arc::new(ControlPlaneService::new(
        upstreams as Arc<dyn UpstreamRepository>,
        routes as Arc<dyn RouteRepository>,
        plugins as Arc<dyn PluginRepository>,
        Arc::new(AllowAllAuthorizer),
        config.list_default_page_size,
        config.list_max_page_size,
    ))
}

#[test]
fn the_module_name_is_the_registered_gear_name() {
    assert_eq!(OagwGear::MODULE_NAME, "oagw");
}

#[test]
fn the_gear_is_a_fresh_singleton_before_init() {
    let gear = OagwGear::default();
    assert!(gear.service().is_none(), "no service before `init`");
}

#[test]
fn the_default_config_is_https_only_and_defaults_the_page_window() {
    let cfg = OagwConfig::default();
    assert!(!cfg.allow_http_upstream);
    assert!(cfg.ssrf_policy.enabled);
    assert_eq!(cfg.list_default_page_size, crate::config::DEFAULT_PAGE_SIZE);
    assert_eq!(cfg.list_max_page_size, crate::config::MAX_PAGE_SIZE);
    assert!(cfg.validate().is_ok());
}

#[test]
fn the_init_wiring_yields_the_configured_page_window() {
    let cfg = OagwConfig {
        list_default_page_size: 5,
        list_max_page_size: 9,
        ..OagwConfig::default()
    };
    let service = service_from(&cfg);
    assert_eq!(service.default_page_size(), 5);
    assert_eq!(service.max_page_size(), 9);
}

#[tokio::test]
async fn the_init_wiring_serves_the_control_plane() {
    let service = service_from(&OagwConfig::default());
    let created = service
        .create_upstream(
            &RequestContext {
                tenant: tenant(),
                subject: "svc.oagw.test".to_owned(),
            },
            upstream_command(None, "api.openai.com"),
        )
        .await
        .unwrap();
    assert!(created.alias.starts_with("api.openai.com"));
    assert!(created.created_at.ends_with('Z'));
}

fn upstream_command(alias: Option<&str>, host: &str) -> UpstreamCommand {
    UpstreamCommand {
        alias: alias.map(str::to_owned),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: host.to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
    }
}

#[tokio::test]
async fn the_init_wiring_authorizes_and_creates_plugins() {
    let service = service_from(&OagwConfig::default());
    let ctx = RequestContext {
        tenant: tenant(),
        subject: "svc.oagw.test".to_owned(),
    };
    let plugin = service
        .create_plugin(
            &ctx,
            PluginCommand {
                plugin_type: crate::domain::model::PluginType::Guard,
                name: "require-tenant".to_owned(),
                config_schema: None,
                source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
                phases: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(plugin.id.to_string().len(), 36);
    let _ = GUARD_PLUGIN_TYPE;
}

// ── rest registration (guarded by the platform boundary) ───────────────────

#[test]
fn the_rest_capability_is_declared_through_the_gear_macro() {
    // `capabilities = [rest]` makes the generated impl satisfy the platform
    // contract; assert the trait is implemented so a capability removal fails
    // here rather than at app bootstrap.
    fn assert_rest<G: toolkit::contracts::RestApiCapability>() {}
    assert_rest::<OagwGear>();
    let _ = OagwGear::MODULE_NAME;
}
