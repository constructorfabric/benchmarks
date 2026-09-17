//! Tests of the hierarchical configuration resolution
//! (`docs/PRD.md` §5.5) in `crate::domain`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use uuid::uuid;

use super::*;
use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::plugin::{
    AUTH_APIKEY, AUTH_NOOP, GUARD_REQUIRED_HEADERS, PluginKind, TRANSFORM_REQUEST_ID,
};

const TENANT: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000101");
const UPSTREAM_ID: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000102");
const ROUTE_ID: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000103");

fn gear() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy { enabled: false },
        max_body_bytes: 104_857_600,
    }
}

fn upstream(plugins: Option<PluginChain>) -> Upstream {
    let server = ServerConfig::new(vec![
        Endpoint::new(EndpointScheme::Https, "api.example.com", None).unwrap(),
    ])
    .unwrap();
    let spec = UpstreamSpec {
        tenant_id: TENANT,
        alias: None,
        protocol: Protocol::Http,
        enabled: true,
        server,
        auth: Some(AuthConfig {
            sharing: SharingMode::Inherit,
            plugin: Some(PluginRef::parse(PluginKind::Auth, AUTH_APIKEY).unwrap()),
            config: serde_json::json!({ "header": "X-Api-Key" }),
        }),
        headers: Some(HeadersConfig::default()),
        plugins,
        rate_limit: Some(RateLimitConfig {
            sharing: SharingMode::Inherit,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: std::num::NonZeroU32::new(10).unwrap(),
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: std::num::NonZeroU32::new(1).unwrap(),
        }),
        cors: Some(CorsConfig {
            sharing: SharingMode::Inherit,
            enabled: true,
            allowed_origins: vec![AllowedOrigin::Any],
            allowed_methods: vec![HttpMethod::Get],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }),
        tags: vec![String::from("upstream-tag")],
    };
    Upstream::new(UPSTREAM_ID, &spec).unwrap()
}

fn http_route() -> Route {
    let route_spec = RouteSpec {
        tenant_id: TENANT,
        upstream_id: UPSTREAM_ID,
        r#match: RouteMatch::Http(
            HttpMatch::new(
                vec![HttpMethod::Post],
                String::from("/v1/chat"),
                Vec::new(),
                PathSuffixMode::Append,
            )
            .unwrap(),
        ),
        plugins: Some(PluginChain {
            sharing: SharingMode::Private,
            items: vec![PluginRef::parse(PluginKind::Transform, TRANSFORM_REQUEST_ID).unwrap()],
        }),
        rate_limit: None,
        cors: None,
        enabled: true,
        tags: vec![String::from("route-tag")],
    };
    Route::new(ROUTE_ID, &route_spec).unwrap()
}

#[test]
fn the_gear_layer_supplies_the_gateway_switches() {
    let policy = resolve_policy(&gear(), &upstream(None), None);
    assert!(policy.allow_http_upstream);
    assert_eq!(policy.proxy_timeout_secs, 2);
    assert_eq!(policy.proxy_timeout(), std::time::Duration::from_secs(2));
    assert_eq!(policy.headers, Some(HeadersConfig::default()));
    assert!(policy.auth.is_some());
}

#[test]
fn the_egress_gate_governs_plaintext_only() {
    let mut config = gear();
    let upstream = upstream(None);
    // With the gate closed, `http` endpoints may not be dialed, but every
    // TLS scheme stays dialable.
    config.allow_http_upstream = false;
    let policy = resolve_policy(&config, &upstream, None);
    assert!(!policy.permits_plaintext_egress(EndpointScheme::Http));
    assert!(policy.permits_plaintext_egress(EndpointScheme::Https));
    assert!(policy.permits_plaintext_egress(EndpointScheme::Grpc));
    // Opening the gate only affects `http`.
    config.allow_http_upstream = true;
    let policy = resolve_policy(&config, &upstream, None);
    assert!(policy.permits_plaintext_egress(EndpointScheme::Http));
    assert!(policy.permits_plaintext_egress(EndpointScheme::Https));
    // Scheme validation is independent of the gate: `http` is always legal.
    assert!(Endpoint::parse_url("http://api.example.com").is_ok());
    assert!(EndpointScheme::parse("http").is_ok());
}

#[test]
fn upstream_plugins_run_before_route_plugins() {
    let upstream_chain = PluginChain {
        sharing: SharingMode::Inherit,
        items: vec![
            PluginRef::parse(PluginKind::Auth, AUTH_APIKEY).unwrap(),
            PluginRef::parse(PluginKind::Guard, GUARD_REQUIRED_HEADERS).unwrap(),
        ],
    };
    let policy = resolve_policy(
        &gear(),
        &upstream(Some(upstream_chain)),
        Some(&http_route()),
    );
    let ranks: Vec<u8> = policy
        .plugins
        .iter()
        .map(|plugin| plugin.kind().execution_rank())
        .collect();
    let mut sorted = ranks.clone();
    sorted.sort_unstable();
    assert_eq!(ranks, sorted, "auth → guard → transform");
    assert!(policy.plugins.len() >= 2);
    assert_eq!(
        policy.plugins[0],
        PluginRef::parse(PluginKind::Auth, AUTH_APIKEY).unwrap()
    );
    assert_eq!(
        policy.plugins[2],
        PluginRef::parse(PluginKind::Transform, TRANSFORM_REQUEST_ID).unwrap()
    );
}

#[test]
fn a_route_without_plugins_keeps_the_upstream_chain() {
    let chain = PluginChain {
        sharing: SharingMode::Inherit,
        items: vec![PluginRef::parse(PluginKind::Auth, AUTH_NOOP).unwrap()],
    };
    let policy = resolve_policy(&gear(), &upstream(Some(chain.clone())), None);
    assert_eq!(policy.plugins, chain.items);
}

#[test]
fn an_upstream_without_plugins_yields_an_empty_chain() {
    let policy = resolve_policy(&gear(), &upstream(None), Some(&http_route()));
    assert_eq!(
        policy.plugins,
        vec![PluginRef::parse(PluginKind::Transform, TRANSFORM_REQUEST_ID).unwrap()]
    );
}

#[test]
fn the_route_rate_limit_overrides_the_upstream_one() {
    let upstream_value = upstream(None);
    let route = http_route();
    let without_route = resolve_policy(&gear(), &upstream_value, None)
        .rate_limit
        .expect("the upstream limit applies when no route matched");
    assert_eq!(without_route.sustained.rate.get(), 10);

    let mut tightening = route.clone();
    tightening.rate_limit = Some(RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::SlidingWindow,
        sustained: SustainedRate {
            rate: std::num::NonZeroU32::new(3).unwrap(),
            window: RateLimitWindow::Second,
        },
        burst: None,
        scope: RateLimitScope::User,
        strategy: RateLimitStrategy::Reject,
        cost: std::num::NonZeroU32::new(1).unwrap(),
    });
    let effective = resolve_policy(&gear(), &upstream_value, Some(&tightening))
        .rate_limit
        .expect("the route limit applies");
    assert_eq!(effective.sustained.rate.get(), 3);
    assert_eq!(effective.scope, RateLimitScope::User);
}

#[test]
fn route_cors_overrides_upstream_cors() {
    let upstream_value = upstream(None);
    let mut route = http_route();
    route.cors = Some(CorsConfig {
        sharing: SharingMode::Private,
        enabled: false,
        allowed_origins: Vec::new(),
        allowed_methods: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    });
    let policy = resolve_policy(&gear(), &upstream_value, Some(&route));
    assert_eq!(policy.cors.map(|cors| cors.enabled), Some(false));
    let policy = resolve_policy(&gear(), &upstream_value, None);
    assert_eq!(policy.cors.map(|cors| cors.enabled), Some(true));
}

#[test]
fn tags_are_merged_add_only_across_the_hierarchy() {
    let upstream_tag = vec![String::from("shared"), String::from("upstream")];
    let route_tag = vec![String::from("shared"), String::from("route")];
    assert_eq!(
        merge_tags(&[upstream_tag.as_slice(), route_tag.as_slice()]),
        vec![
            String::from("shared"),
            String::from("upstream"),
            String::from("route")
        ]
    );
    assert!(merge_tags(&[]).is_empty());
}

#[test]
fn resolve_slot_honours_the_sharing_mode() {
    let ancestor = String::from("ancestor");
    let descendant = String::from("descendant");
    assert_eq!(
        resolve_slot(Some(&ancestor), Some(&descendant), SharingMode::Private),
        Some(String::from("descendant"))
    );
    assert_eq!(
        resolve_slot(Some(&ancestor), None, SharingMode::Private),
        None
    );
    assert_eq!(
        resolve_slot(Some(&ancestor), None, SharingMode::Inherit),
        Some(String::from("ancestor"))
    );
    assert_eq!(
        resolve_slot(Some(&ancestor), Some(&descendant), SharingMode::Inherit),
        Some(String::from("descendant"))
    );
    assert_eq!(
        resolve_slot(Some(&ancestor), None, SharingMode::Enforce),
        Some(String::from("ancestor"))
    );
}

#[test]
fn an_unresolved_upstream_is_reported_as_route_not_found() {
    // The 404 path of the proxy flow (`docs/PRD.md` §5.6): the error type
    // carries the code and status the handler must emit.
    let err = crate::error::OagwError::UpstreamNotFound {
        alias: String::from("api.example.com"),
    };
    assert_eq!(err.code(), crate::error::ErrorCode::RouteNotFound);
    assert_eq!(err.http_status(), 404);
}
