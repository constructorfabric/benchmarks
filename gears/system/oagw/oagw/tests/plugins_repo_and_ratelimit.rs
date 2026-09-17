//! Built-in plugins, in-memory repository semantics and token-bucket limits.
//!
//! Covers behaviour that has no unit test inside the crate: the plugin
//! registries and their catalogued-only ids (ADR-0002), the configuration
//! parsing and rejection paths of the built-in plugins, tenant scoping and
//! conflict detection in [`MemoryStore`], and the refill/retry boundaries of
//! the rate limiter (ADR-0003).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use http::{HeaderMap, HeaderValue};
use oagw::config::TokenCacheConfig;
use oagw::domain::error::OagwError;
use oagw::domain::gts_helpers;
use oagw::domain::model::{
    BurstCapacity, Endpoint, EndpointScheme, Plugin, PluginType, RateLimitConfig, RateScope,
    RateWindow, Sharing, SustainedRate, Upstream, UpstreamServer,
};
use oagw::domain::plugin::{
    AuthPlugin as _, ErrorContext, GuardDecision, GuardPlugin as _, RequestContext,
    TransformPlugin as _,
};
use oagw::domain::repo::{
    nil_id, ConfigGenerationCounter, PluginRepository, RouteRepository as _, UpstreamRepository as _,
};
use oagw::infra::plugin::apikey_auth::ApiKeyAuthPlugin;
use oagw::infra::plugin::noop_auth::NoopAuthPlugin;
use oagw::infra::plugin::oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, Variant};
use oagw::infra::plugin::request_id_transform::RequestIdTransformPlugin;
use oagw::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;
use oagw::infra::plugin::{
    builtin_plugin_ids, AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use oagw::infra::ratelimit::{effective_rate_limit, rejection_error, scope_key, RateLimiter};
use oagw::infra::storage::memory::MemoryStore;
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn request_context(config: serde_json::Value) -> RequestContext {
    RequestContext {
        security_context: SecurityContext::anonymous(),
        tenant_id: Uuid::new_v4(),
        upstream_id: Uuid::new_v4(),
        route_id: None,
        method: http::Method::GET,
        path: "/v1/resource".to_owned(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: bytes::Bytes::new(),
        config,
    }
}

fn rate_config(rate: u64, capacity: u64, sharing: Sharing) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        sustained: SustainedRate {
            rate,
            window: RateWindow::Second,
        },
        burst: Some(BurstCapacity { capacity }),
        ..RateLimitConfig::default()
    }
}

/// A capacity of `capacity` that refills one token per `window` seconds, so a
/// drained bucket stays drained for the lifetime of a test.
fn frozen_config(capacity: u64, sharing: Sharing) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        sustained: SustainedRate {
            rate: 1,
            window: RateWindow::Day,
        },
        burst: Some(BurstCapacity { capacity }),
        ..RateLimitConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Plugin registries (ADR-0002)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_registry_serves_the_builtins_and_names_the_catalogued_gaps() {
    let registry = AuthPluginRegistry::with_builtins(None, TokenCacheConfig::default());
    let ids = registry.ids();
    assert_eq!(
        ids,
        vec![
            gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned(),
            gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned(),
            gts_helpers::OAUTH2_CLIENT_CRED_PLUGIN_ID.to_owned(),
            gts_helpers::OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID.to_owned(),
        ]
    );

    assert_eq!(
        registry.get(gts_helpers::NOOP_AUTH_PLUGIN_ID).unwrap().id(),
        gts_helpers::NOOP_AUTH_PLUGIN_ID
    );

    // Catalogued but unimplemented: a distinct, actionable failure.
    let Err(err) = registry.get(gts_helpers::CATALOG_BEARER_AUTH_PLUGIN_ID) else {
        panic!("a catalogued id without an implementation must not resolve");
    };
    match &err {
        OagwError::PluginNotFound(detail) => {
            assert!(detail.contains("catalogued"), "{detail}");
            assert!(detail.contains("no runtime implementation"), "{detail}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(err.status(), http::StatusCode::SERVICE_UNAVAILABLE);

    // An id that was never catalogued at all.
    let Err(err) = registry.get("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.unheard_of.v1") else {
        panic!("an unknown id must not resolve");
    };
    assert!(matches!(err, OagwError::PluginNotFound(detail) if detail.contains("not registered")));
}

#[tokio::test]
async fn guard_and_transform_registries_serve_one_builtin_each() {
    let guards = GuardPluginRegistry::with_builtins();
    assert_eq!(
        guards.ids(),
        vec![gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned()]
    );
    let Err(err) = guards.get(gts_helpers::CATALOG_CORS_GUARD_PLUGIN_ID) else {
        panic!("a catalogued guard id without an implementation must not resolve");
    };
    assert_eq!(err.status(), http::StatusCode::SERVICE_UNAVAILABLE);

    let transforms = TransformPluginRegistry::with_builtins();
    assert_eq!(
        transforms.ids(),
        vec![gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned()]
    );
}

#[test]
fn builtin_plugin_ids_are_the_union_of_catalogue_and_implementations() {
    let ids = builtin_plugin_ids();
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        ids.len(),
        "no duplicates"
    );
    let sorted = {
        let mut sorted = ids.clone();
        sorted.sort();
        sorted
    };
    assert_eq!(ids, sorted);
    for catalogued in gts_helpers::CATALOG_ONLY_PLUGIN_IDS {
        assert!(ids.contains(&(*catalogued).to_owned()));
    }
    for implemented in [
        gts_helpers::NOOP_AUTH_PLUGIN_ID,
        gts_helpers::APIKEY_AUTH_PLUGIN_ID,
        gts_helpers::OAUTH2_CLIENT_CRED_PLUGIN_ID,
        gts_helpers::OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID,
        gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
    ] {
        assert!(ids.contains(&implemented.to_owned()));
    }
    assert_eq!(ids.len(), gts_helpers::CATALOG_ONLY_PLUGIN_IDS.len() + 6);
}

// ---------------------------------------------------------------------------
// ApiKeyAuthPlugin configuration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn apikey_defaults_to_the_x_api_key_header() {
    let mut ctx = request_context(serde_json::json!({"key": "sk-literal"}));
    ApiKeyAuthPlugin::new(None).authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-literal");
}

#[tokio::test]
async fn apikey_honours_the_configured_location_case_insensitively() {
    let mut ctx = request_context(serde_json::json!({
        "key": "sk-literal",
        "in": "QUERY",
        "query_param": "token"
    }));
    ctx.query.push(("token".to_owned(), "stale".to_owned()));
    ctx.query.push(("keep".to_owned(), "me".to_owned()));
    ApiKeyAuthPlugin::new(None).authenticate(&mut ctx).await.unwrap();
    assert_eq!(
        ctx.query,
        vec![
            ("keep".to_owned(), "me".to_owned()),
            ("token".to_owned(), "sk-literal".to_owned()),
        ],
        "a stale value for the same parameter is replaced, not duplicated"
    );
}

#[tokio::test]
async fn apikey_unknown_location_still_injects_a_header() {
    let mut ctx = request_context(serde_json::json!({"key": "sk-literal", "in": "cookie"}));
    ApiKeyAuthPlugin::new(None).authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-literal");
    assert!(ctx.query.is_empty());
}

#[tokio::test]
async fn apikey_prefers_the_literal_key_over_the_credential_reference() {
    let mut ctx = request_context(serde_json::json!({
        "key": "sk-literal",
        "key_ref": "cred://would-need-a-credstore"
    }));
    ApiKeyAuthPlugin::new(None).authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-literal");
}

#[tokio::test]
async fn apikey_cannot_resolve_a_reference_without_a_credential_store() {
    let mut ctx = request_context(serde_json::json!({"key_ref": "cred://tenant-a/apikey"}));
    let err = ApiKeyAuthPlugin::new(None)
        .authenticate(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, OagwError::SecretNotFound(detail) if detail.contains("cred://tenant-a/apikey")));
    assert!(ctx.headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn apikey_rejects_an_unusable_header_name() {
    let mut ctx = request_context(serde_json::json!({
        "key": "sk-literal",
        "header": "not a header name"
    }));
    let err = ApiKeyAuthPlugin::new(None)
        .authenticate(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, OagwError::Validation(detail) if detail.contains("not a header name")));
}

#[tokio::test]
async fn oauth2_rejects_malformed_configuration_before_touching_the_idp() {
    let plugin = OAuth2ClientCredAuthPlugin::new(None, Variant::Form, Duration::from_secs(60), 8);
    let cases = [
        serde_json::json!("not an object"),
        serde_json::json!({}),
        serde_json::json!({"token_endpoint": "https://idp/token"}),
        serde_json::json!({"token_endpoint": "https://idp/token", "issuer_url": "https://idp"}),
        serde_json::json!({
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
        }),
    ];
    for config in cases {
        let mut ctx = request_context(config.clone());
        let err = plugin
            .authenticate(&mut ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, OagwError::Validation(_)),
            "expected a validation error for {config}, got {err}"
        );
        assert!(ctx.headers.get(http::header::AUTHORIZATION).is_none());
    }

    // A well-formed configuration moves on to credential resolution, which
    // fails for want of a credstore rather than for want of an IdP.
    let mut ctx = request_context(serde_json::json!({
        "token_endpoint": "https://idp.example/token",
        "client_id_ref": "cred://id",
        "client_secret_ref": "cred://secret"
    }));
    let err = plugin
        .authenticate(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, OagwError::SecretNotFound(detail) if detail.contains("cred://id")));
    assert!(ctx.headers.get(http::header::AUTHORIZATION).is_none());
}

#[tokio::test]
async fn the_two_oauth2_variants_report_their_own_plugin_id() {
    let form = OAuth2ClientCredAuthPlugin::new(None, Variant::Form, Duration::from_secs(60), 8);
    let basic = OAuth2ClientCredAuthPlugin::new(None, Variant::Basic, Duration::from_secs(60), 8);
    assert_eq!(form.id(), gts_helpers::OAUTH2_CLIENT_CRED_PLUGIN_ID);
    assert_eq!(basic.id(), gts_helpers::OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID);
}

// ---------------------------------------------------------------------------
// Guard and transform plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn required_headers_guard_reports_only_the_first_missing_header() {
    let mut ctx = request_context(serde_json::json!({
        "required_request_headers": "x-first, x-second, x-third"
    }));
    ctx.headers
        .insert("x-first", HeaderValue::from_static("present"));
    let decision = RequiredHeadersGuardPlugin.guard_request(&ctx).await.unwrap();
    match decision {
        GuardDecision::Reject(err) => {
            assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
            assert!(err.detail().contains("x-second"));
            assert!(!err.detail().contains("x-third"));
        }
        GuardDecision::Allow => panic!("expected a rejection"),
    }
}

#[tokio::test]
async fn required_headers_guard_ignores_non_string_configuration() {
    let ctx = request_context(serde_json::json!({"required_request_headers": 42}));
    let decision = RequiredHeadersGuardPlugin.guard_request(&ctx).await.unwrap();
    assert!(decision.is_allowed());
}

#[tokio::test]
async fn request_id_transform_leaves_responses_without_a_stored_id_alone() {
    let plugin = RequestIdTransformPlugin;
    let mut response = oagw::domain::plugin::ResponseContext {
        status: http::StatusCode::OK,
        headers: HeaderMap::new(),
        body: bytes::Bytes::new(),
        is_error: false,
        config: serde_json::Value::Null,
    };
    plugin.transform_response(&mut response).await.unwrap();
    assert!(response.headers.get("x-request-id").is_none());
}

#[tokio::test]
async fn request_id_transform_keeps_the_correlation_id_on_the_error_path() {
    let mut ctx = request_context(serde_json::Value::Null);
    RequestIdTransformPlugin
        .transform_request(&mut ctx)
        .await
        .unwrap();
    let minted = ctx.header("x-request-id").unwrap().to_owned();

    let mut error = ErrorContext {
        error: OagwError::RouteNotFound("no route".to_owned()),
        headers: ctx.headers.clone(),
    };
    RequestIdTransformPlugin.transform_error(&mut error).await.unwrap();
    assert_eq!(error.headers.get("x-request-id").unwrap(), minted.as_str());
}

#[tokio::test]
async fn noop_auth_forwards_the_request_untouched() {
    let mut ctx = request_context(serde_json::json!({"ignored": true}));
    ctx.headers
        .insert("authorization", HeaderValue::from_static("Bearer caller"));
    ctx.query.push(("page".to_owned(), "1".to_owned()));
    NoopAuthPlugin.authenticate(&mut ctx).await.unwrap();
    assert_eq!(ctx.headers.get("authorization").unwrap(), "Bearer caller");
    assert_eq!(ctx.query, vec![("page".to_owned(), "1".to_owned())]);
}

// ---------------------------------------------------------------------------
// In-memory repository semantics
// ---------------------------------------------------------------------------

fn upstream(tenant: Uuid, alias: &str, created_at: u64) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        enabled: true,
        alias: alias.to_owned(),
        alias_explicit: true,
        tags: Vec::new(),
        server: UpstreamServer {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: "api.example.com".to_owned(),
                port: 443,
            }],
        },
        protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at,
        updated_at: created_at,
    }
}

fn plugin(tenant: Uuid, name: &str) -> Plugin {
    let uuid = Uuid::new_v4();
    Plugin {
        id: format!("{}{uuid}", gts_helpers::GUARD_PLUGIN_PREFIX),
        tenant_id: tenant,
        name: name.to_owned(),
        description: String::new(),
        plugin_type: PluginType::Guard,
        config_schema: serde_json::Value::Null,
        config: serde_json::Value::Null,
        source_code: "def guard(ctx): pass".to_owned(),
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn memory_store_scopes_every_lookup_by_tenant() {
    let store = MemoryStore::new();
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let first = upstream(tenant_a, "api.example.com", 10);
    let first_id = first.id;
    let second = upstream(tenant_b, "api.example.com", 5);
    store.insert_upstream(first).unwrap();
    store.insert_upstream(second).unwrap();

    // Aliases collide across tenants but are unique within one.
    let found = store
        .find_upstream_by_alias(tenant_a, "api.example.com")
        .unwrap()
        .unwrap();
    assert_eq!(found.id, first_id);
    assert_eq!(found.alias, "api.example.com");
    assert!(
        store
            .find_upstream_by_alias(tenant_a, "API.Example.com")
            .unwrap()
            .is_none(),
        "alias lookup is exact, not case-insensitive"
    );

    let owned_a = store.upstreams_for_tenant(tenant_a).unwrap();
    assert_eq!(owned_a.len(), 1);
    assert_eq!(owned_a[0].id, first_id);
    assert_eq!(store.upstreams_for_tenant(tenant_b).unwrap().len(), 1);
    assert_eq!(store.upstreams_for_tenant(Uuid::new_v4()).unwrap().len(), 0);

    // Listing is ordered by creation time.
    store.update_upstream(upstream(tenant_a, "second.example", 1)).unwrap();
    let ordered = store.upstreams_for_tenant(tenant_a).unwrap();
    assert_eq!(ordered.len(), 2);
    assert_eq!(ordered[0].alias, "second.example", "created_at wins over id");

    store.delete_upstream(first_id).unwrap();
    assert!(store.find_upstream(first_id).unwrap().is_none());
}

#[test]
fn routes_are_counted_per_upstream_and_listed_across_tenants() {
    let store = MemoryStore::new();
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let owner = upstream(tenant_a, "api.example.com", 1);
    let upstream_id = owner.id;
    store.insert_upstream(owner).unwrap();

    for (tenant, path) in [(tenant_a, "/a"), (tenant_b, "/b")] {
        store
            .insert_route(oagw::domain::model::Route {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                tags: Vec::new(),
                upstream_id,
                r#match: oagw::domain::model::MatchRule {
                    http: Some(oagw::domain::model::HttpMatch {
                        methods: vec![oagw::domain::model::HttpMethod::Get],
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: oagw::domain::model::PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                plugins: oagw::domain::model::PluginListConfig::default(),
                rate_limit: None,
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
    }

    assert_eq!(store.upstream_route_count(upstream_id).unwrap(), 2);
    assert_eq!(store.upstream_route_count(Uuid::new_v4()).unwrap(), 0);
    // Data-plane matching ignores tenants: every route of the upstream counts.
    assert_eq!(store.routes_for_upstream(upstream_id).unwrap().len(), 2);
    assert_eq!(store.routes_for_tenant(tenant_a).unwrap().len(), 1);

    let routes = store.routes_for_tenant(tenant_b).unwrap();
    let route_id = routes[0].id;
    store.delete_route(route_id).unwrap();
    assert!(store.find_route(route_id).unwrap().is_none());
    assert_eq!(store.upstream_route_count(upstream_id).unwrap(), 1);
}

#[test]
fn plugins_are_scoped_by_tenant_and_keyed_by_their_gts_id() {
    let store = MemoryStore::new();
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let mine = plugin(tenant_a, "mine");
    let mine_id = mine.id.clone();
    store.insert_plugin(mine).unwrap();
    store.insert_plugin(plugin(tenant_b, "theirs")).unwrap();

    assert!(store.find_plugin(&mine_id).unwrap().is_some());
    assert_eq!(store.plugins_for_tenant(tenant_a).unwrap().len(), 1);
    assert_eq!(store.plugins_for_tenant(tenant_b).unwrap().len(), 1);
    assert_eq!(store.plugins_for_tenant(Uuid::new_v4()).unwrap().len(), 0);

    store.delete_plugin(&mine_id).unwrap();
    assert!(store.find_plugin(&mine_id).unwrap().is_none());
}

#[test]
fn nil_id_is_the_shared_sentinel() {
    assert_eq!(nil_id(), Uuid::nil());
}

#[test]
fn config_generation_counter_counts_mutations() {
    let counter = ConfigGenerationCounter::default();
    assert_eq!(counter.current(), 0);
    assert_eq!(counter.bump(), 1);
    assert_eq!(counter.bump(), 2);
    assert_eq!(counter.current(), 2);
}

// ---------------------------------------------------------------------------
// Rate limiting (ADR-0003)
// ---------------------------------------------------------------------------

#[test]
fn a_rejected_request_reports_retry_and_reset_guidance() {
    let limiter = RateLimiter::new();
    let config = rate_config(1, 1, Sharing::Private);
    assert!(limiter.try_acquire(&config, "scope", 1).allowed);
    let decision = limiter.try_acquire(&config, "scope", 1);
    assert!(!decision.allowed);
    assert_eq!(decision.limit, 1);
    assert_eq!(decision.remaining, 0);
    assert_eq!(decision.retry_after_secs, 1, "deficit 1 at 1 token/second");
    assert!(decision.reset_epoch_secs >= oagw::infra::ratelimit::current_epoch_secs());

    let error = rejection_error(&decision);
    assert_eq!(error.status(), http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
    assert_eq!(error.retry_after_secs(), Some(1));
}

#[test]
fn a_drained_bucket_stays_drained_for_the_length_of_its_window() {
    let limiter = RateLimiter::new();
    // One token per day: whatever the test's wall-clock jitter is, the bucket
    // cannot refill while the test runs.
    let config = frozen_config(1, Sharing::Private);
    assert!(limiter.try_acquire(&config, "frozen", 1).allowed);
    std::thread::sleep(Duration::from_millis(10));
    assert!(
        !limiter.try_acquire(&config, "frozen", 1).allowed,
        "a drained bucket must not hand out tokens"
    );
}

#[test]
fn a_bucket_without_refill_reports_a_bounded_reset_instead_of_overflowing() {
    let limiter = RateLimiter::new();
    // `rate: 0` is wire-reachable — `RateLimitConfig` deserializes unvalidated
    // from the upstream/route payloads — so a drained bucket must report a
    // bounded reset instant rather than overflowing the epoch addition.
    let config = rate_config(0, 1, Sharing::Private);
    assert!(limiter.try_acquire(&config, "no-refill", 1).allowed);
    let decision = limiter.try_acquire(&config, "no-refill", 1);
    assert!(!decision.allowed);
    assert_eq!(
        decision.reset_epoch_secs
            - oagw::infra::ratelimit::current_epoch_secs(),
        oagw::infra::ratelimit::RESET_HORIZON_SECS,
        "a bucket that never refills reports the reset horizon"
    );
}

#[test]
fn capacity_changes_apply_to_the_existing_bucket() {
    let limiter = RateLimiter::new();
    let small = frozen_config(1, Sharing::Private);
    assert!(limiter.try_acquire(&small, "resize", 1).allowed);
    assert!(!limiter.try_acquire(&small, "resize", 1).allowed);

    // A *fresh* bucket for the widened configuration would start with three
    // tokens; the retained one must stay where it was.
    let larger = frozen_config(3, Sharing::Private);
    assert!(
        !limiter.try_acquire(&larger, "resize", 1).allowed,
        "a configuration change re-caps the existing bucket instead of resetting it"
    );
}

#[test]
fn scope_keys_separate_configurations_and_scope_values() {
    let config = frozen_config(1, Sharing::Inherit);
    let first = scope_key("upstream-1", &config, &["tenant-a"]);
    let second = scope_key("upstream-2", &config, &["tenant-a"]);
    assert_ne!(first, second, "the config id keeps pools apart");
    assert!(first.starts_with("upstream-1:"));
    assert!(first.ends_with(":tenant-a"));

    let limiter = RateLimiter::new();
    assert!(limiter.try_acquire(&config, &first, 1).allowed);
    assert!(limiter.try_acquire(&config, &second, 1).allowed);
    assert!(
        !limiter.try_acquire(&config, &first, 1).allowed,
        "two scope keys are two independent buckets"
    );
}

#[test]
fn effective_rate_limit_ignores_private_ancestors_and_caps_inherited_ones() {
    // An empty chain has no limit at all.
    assert_eq!(effective_rate_limit(&[]), None);
    assert_eq!(effective_rate_limit(&[None, None]), None);

    // The last configuration in the chain is always the base, so its own
    // `private` sharing never hides it from itself.
    let private = rate_config(10, 10, Sharing::Private);
    assert_eq!(effective_rate_limit(&[Some(private.clone()), None]), Some(private));

    // A private *ancestor* is not inherited by a descendant.
    let private_ancestor = rate_config(1, 1, Sharing::Private);
    let descendant = rate_config(1_000, 500, Sharing::Inherit);
    let effective = effective_rate_limit(&[Some(private_ancestor), Some(descendant)]).unwrap();
    assert_eq!(effective.sustained.rate, 1_000);
    assert_eq!(effective.capacity(), 500);

    // An enforced ancestor caps the descendant.
    let enforced_ancestor = rate_config(100, 200, Sharing::Enforce);
    let descendant = rate_config(1_000, 500, Sharing::Inherit);
    let effective = effective_rate_limit(&[Some(enforced_ancestor), Some(descendant)]).unwrap();
    assert_eq!(effective.sustained.rate, 100);
    assert_eq!(effective.capacity(), 200);
    assert_eq!(effective.sharing, Sharing::Enforce);
}

#[test]
fn rate_limit_configs_round_trip_the_wire_shape() {
    let json = serde_json::json!({
        "sharing": "enforce",
        "algorithm": "token_bucket",
        "sustained": {"rate": 5, "window": "minute"},
        "burst": {"capacity": 50},
        "scope": "tenant",
        "strategy": "reject",
        "cost": 2,
        "response_headers": true
    });
    let config: RateLimitConfig = serde_json::from_value(json).unwrap();
    assert_eq!(config.sharing, Sharing::Enforce);
    assert_eq!(config.capacity(), 50);
    assert_eq!(config.refill_per_second(), 5.0 / 60.0);
    assert_eq!(config.scope, RateScope::Tenant);

    // An absent burst falls back to the sustained rate.
    let config: RateLimitConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(config.capacity(), config.sustained.rate);
    assert_eq!(config.sustained.window, RateWindow::Second);
}

#[test]
fn header_rule_maps_are_sorted_and_deterministic() {
    let mut set = BTreeMap::new();
    set.insert("b".to_owned(), "two".to_owned());
    set.insert("a".to_owned(), "one".to_owned());
    let rules = oagw::domain::model::RequestHeaderRules {
        set,
        add: BTreeMap::new(),
        remove: Vec::new(),
        passthrough: oagw::domain::model::HeaderPassthrough::All,
        passthrough_allowlist: Vec::new(),
    };
    let wire = serde_json::to_value(&rules).unwrap();
    let keys: Vec<&String> = wire["set"].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["a", "b"], "JSON objects keep their insertion order");

    let inbound = {
        let mut headers = HeaderMap::new();
        headers.insert("x", HeaderValue::from_static("kept"));
        headers
    };
    let outbound = oagw::infra::proxy::headers::apply_request_rules(&inbound, Some(&rules));
    assert_eq!(outbound.get("x").unwrap(), "kept");
}

#[test]
fn endpoint_schemes_declare_their_default_port_and_transport_security() {
    assert_eq!(EndpointScheme::Http.default_port(), 80);
    for scheme in [
        EndpointScheme::Https,
        EndpointScheme::Wss,
        EndpointScheme::Wt,
        EndpointScheme::Grpc,
    ] {
        assert_eq!(scheme.default_port(), 443, "{scheme:?}");
        assert!(scheme.is_tls(), "{scheme:?}");
    }
    assert!(!EndpointScheme::Http.is_tls());
}
