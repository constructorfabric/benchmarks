//! Tests for [`crate::infra::plugin::PluginRegistry`].

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    DisabledPlugins, PluginRegistry, SecretResolverTrait, StaticSecretResolver,
    UnavailableSecretResolver,
};
use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::model::{
    AuthConfig, Endpoint, Plugin, PluginBinding, PluginConfig, Protocol, Route, RouteMatch, Scheme,
    ServerConfig, SharingMode, Upstream, format_plugin_id,
};
use crate::domain::plugin::{
    AuthPlugin, GUARD_PLUGIN_TYPE_ID, GuardPlugin, RequestContext, builtin,
};
use crate::infra::plugin::oauth2::TokenCacheConfig;
use crate::infra::plugin::request_id::REQUEST_ID_HEADER;

const TENANT: Uuid = Uuid::from_u128(0x11);

fn resolver() -> Arc<dyn SecretResolverTrait> {
    Arc::new(StaticSecretResolver::new(HashMap::from([
        ("client-id".to_owned(), "client-id-value".to_owned()),
        ("client-secret".to_owned(), "client-secret-value".to_owned()),
        ("payments-key".to_owned(), "resolved-key".to_owned()),
    ])))
}

fn registry() -> PluginRegistry {
    PluginRegistry::with_builtins(resolver(), TokenCacheConfig::from(&OagwConfig::default()))
}

fn empty_registry() -> PluginRegistry {
    PluginRegistry::empty()
}

/// The disabled set of a chain build that has nothing to skip.
fn nothing_disabled() -> DisabledPlugins {
    DisabledPlugins::of(Vec::<Arc<Plugin>>::new())
}

/// A plugin resource with `enabled: false` for `id`.
fn disabled_plugin(id: Uuid) -> Plugin {
    Plugin {
        id,
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1".to_owned(),
        enabled: false,
        ..Plugin::default()
    }
}

/// The disabled set holding exactly `plugins` (already `enabled: false`).
fn disabled(plugins: Vec<Plugin>) -> DisabledPlugins {
    DisabledPlugins::of(plugins.into_iter().map(Arc::new))
}

fn upstream_with_auth(auth_type: &str, config: serde_json::Value) -> Upstream {
    Upstream {
        id: Uuid::from_u128(0x55),
        enabled: true,
        alias: "payments".to_owned(),
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: "payments.internal".to_owned(),
                port: 443,
            }],
        },
        protocol: Protocol::Http,
        auth: Some(AuthConfig {
            auth_type: auth_type.to_owned(),
            sharing: SharingMode::Private,
            config,
        }),
        headers: Default::default(),
        plugins: PluginConfig::default(),
        rate_limit: None,
        cors: None,
        tenant_id: TENANT,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        updated_at: std::time::SystemTime::UNIX_EPOCH,
    }
}

fn route_with_plugins(bindings: Vec<PluginBinding>) -> Route {
    Route {
        id: Uuid::from_u128(0x66),
        upstream_id: Uuid::from_u128(0x55),
        r#match: RouteMatch::default(),
        headers: Default::default(),
        plugins: PluginConfig {
            sharing: SharingMode::Private,
            items: bindings,
        },
        rate_limit: None,
        cors: None,
        enabled: true,
        priority: 0,
        tags: Vec::new(),
        tenant_id: TENANT,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        updated_at: std::time::SystemTime::UNIX_EPOCH,
    }
}

fn security() -> Arc<SecurityContext> {
    Arc::new(
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(0x33))
            .subject_tenant_id(TENANT)
            .build()
            .expect("security context"),
    )
}

fn request() -> RequestContext {
    RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(TENANT)
        .security(security())
        .build()
}

/// Unwraps an expected failure without requiring the success type to be
/// `Debug`: the ADR-0002 plugin traits are object-safe ports, not `Debug`.
fn expect_error<T>(result: Result<T, OagwError>, message: &str) -> OagwError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(error) => error,
    }
}

// ---------------------------------------------------------------------------
// Registry construction
// ---------------------------------------------------------------------------

#[test]
fn with_builtins_registers_exactly_the_six_builtins() {
    let registry = registry();
    assert_eq!(registry.len(), 6);
    assert!(!registry.is_empty());

    for id in [
        builtin::NOOP_AUTH,
        builtin::APIKEY_AUTH,
        builtin::OAUTH2_CLIENT_CRED,
        builtin::OAUTH2_CLIENT_CRED_BASIC,
        builtin::REQUIRED_HEADERS_GUARD,
        builtin::REQUEST_ID_TRANSFORM,
    ] {
        assert!(registry.contains(id), "'{id}' must be registered");
    }
}

#[test]
fn catalog_only_ids_are_not_registered() {
    let registry = registry();
    for id in [
        builtin::BASIC_AUTH,
        builtin::BEARER_AUTH,
        builtin::TIMEOUT_GUARD,
        builtin::CORS_GUARD,
        builtin::LOGGING_TRANSFORM,
        builtin::METRICS_TRANSFORM,
    ] {
        assert!(!registry.contains(id), "'{id}' must not be registered");
    }
}

#[test]
fn an_empty_registry_has_no_plugins() {
    let registry = empty_registry();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(!registry.contains(builtin::NOOP_AUTH));
}

#[test]
fn lookups_accept_the_bare_instance_fragment() {
    let registry = registry();
    assert!(registry.contains("cf.core.oagw.noop.v1"));
    assert!(registry.contains("cf.core.oagw.apikey.v1"));
    assert!(registry.contains("cf.core.oagw.required_headers.v1"));
    assert!(registry.contains("cf.core.oagw.request_id.v1"));
}

// ---------------------------------------------------------------------------
// Building a single plugin
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_plugin_id_is_a_503() {
    let registry = registry();
    let error = expect_error(
        registry.build_auth(builtin::BASIC_AUTH, &serde_json::json!({})),
        "not registered",
    );
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        error.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert_eq!(
        error.problem_body().context.plugin_id.as_deref(),
        Some(builtin::BASIC_AUTH)
    );
}

#[test]
fn an_unknown_kind_is_reported_the_same_way() {
    let registry = registry();
    for build in [
        |registry: &PluginRegistry| {
            registry
                .build_auth("totally-unknown.v1", &serde_json::json!({}))
                .err()
        },
        |registry: &PluginRegistry| {
            registry
                .build_guard("totally-unknown.v1", &serde_json::json!({}))
                .err()
        },
        |registry: &PluginRegistry| {
            registry
                .build_transform("totally-unknown.v1", &serde_json::json!({}))
                .err()
        },
    ] {
        let error = build(&registry).expect("must fail");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

#[test]
fn a_known_id_of_the_wrong_kind_is_a_503() {
    let registry = registry();
    let error = expect_error(
        registry.build_guard(builtin::NOOP_AUTH, &serde_json::json!({})),
        "auth is not a guard",
    );
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn the_noop_plugin_builds_without_configuration() {
    let registry = registry();
    let plugin = registry
        .build_auth(builtin::NOOP_AUTH, &serde_json::Value::Null)
        .expect("plugin");
    assert_eq!(plugin.id(), builtin::NOOP_AUTH);
}

#[test]
fn the_required_headers_plugin_builds_from_a_configuration() {
    let registry = registry();
    let plugin = registry
        .build_guard(
            builtin::REQUIRED_HEADERS_GUARD,
            &serde_json::json!({
                "required_request_headers": "x-trace-id"
            }),
        )
        .expect("plugin");
    assert_eq!(plugin.id(), builtin::REQUIRED_HEADERS_GUARD);
}

#[test]
fn the_request_id_plugin_builds_from_any_configuration() {
    let registry = registry();
    let plugin = registry
        .build_transform(builtin::REQUEST_ID_TRANSFORM, &serde_json::Value::Null)
        .expect("plugin");
    assert_eq!(plugin.id(), builtin::REQUEST_ID_TRANSFORM);
}

#[test]
fn the_apikey_plugin_validates_its_configuration() {
    let registry = registry();
    let error = expect_error(
        registry.build_auth(builtin::APIKEY_AUTH, &serde_json::json!({})),
        "no credential source",
    );
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn the_oauth2_plugins_validate_their_configuration() {
    let registry = registry();
    for id in [
        builtin::OAUTH2_CLIENT_CRED,
        builtin::OAUTH2_CLIENT_CRED_BASIC,
    ] {
        let error = expect_error(
            registry.build_auth(id, &serde_json::json!({ "client_id_ref": "cred://x" })),
            "incomplete configuration",
        );
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }
}

#[test]
fn the_oauth2_plugins_accept_the_documented_configuration() {
    let registry = registry();
    let config = serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    });
    assert!(
        registry
            .build_auth(builtin::OAUTH2_CLIENT_CRED, &config)
            .is_ok()
    );
    assert!(
        registry
            .build_auth(builtin::OAUTH2_CLIENT_CRED_BASIC, &config)
            .is_ok()
    );
}

#[test]
fn an_empty_registry_returns_a_503_for_every_id() {
    let registry = empty_registry();
    let error = expect_error(
        registry.build_auth(builtin::NOOP_AUTH, &serde_json::json!({})),
        "empty registry",
    );
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn a_registry_can_be_extended_with_a_custom_plugin() {
    let mut registry = registry();
    registry.register_auth(
        "cf.core.oagw.custom.v1",
        Arc::new(|_config| Ok(Arc::new(CustomAuth) as Arc<dyn AuthPlugin>)),
    );
    assert_eq!(registry.len(), 7);
    assert!(
        registry
            .build_auth("cf.core.oagw.custom.v1", &serde_json::json!({}))
            .is_ok()
    );
}

struct CustomAuth;

#[async_trait::async_trait]
impl AuthPlugin for CustomAuth {
    fn id(&self) -> &str {
        "cf.core.oagw.custom.v1"
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::AUTH_PLUGIN_TYPE_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        ctx.request_id = Some("custom".to_owned());
        Ok(())
    }
}

struct NoopGuard;

#[async_trait::async_trait]
impl GuardPlugin for NoopGuard {
    fn id(&self) -> &str {
        "cf.core.oagw.custom-guard.v1"
    }

    fn plugin_type(&self) -> &str {
        GUARD_PLUGIN_TYPE_ID
    }

    async fn guard_request(
        &self,
        _ctx: &RequestContext,
    ) -> Result<crate::domain::plugin::GuardDecision, OagwError> {
        Ok(crate::domain::plugin::GuardDecision::allow())
    }

    async fn guard_response(
        &self,
        _ctx: &crate::domain::plugin::ResponseContext,
    ) -> Result<crate::domain::plugin::GuardDecision, OagwError> {
        Ok(crate::domain::plugin::GuardDecision::allow())
    }
}

#[test]
fn a_custom_guard_registers_under_its_own_kind() {
    let mut registry = empty_registry();
    registry.register_guard(
        "cf.core.oagw.custom-guard.v1",
        Arc::new(|_config| Ok(Arc::new(NoopGuard) as Arc<dyn GuardPlugin>)),
    );
    assert_eq!(registry.len(), 1);
    assert!(
        registry
            .build_guard("cf.core.oagw.custom-guard.v1", &serde_json::json!({}))
            .is_ok()
    );
    let error = expect_error(
        registry.build_auth("cf.core.oagw.custom-guard.v1", &serde_json::json!({})),
        "guard is not an auth plugin",
    );
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// Building a chain
// ---------------------------------------------------------------------------

#[test]
fn the_chain_starts_with_the_upstream_auth_binding() {
    let registry = registry();
    let upstream = upstream_with_auth(
        builtin::APIKEY_AUTH,
        serde_json::json!({ "key": "raw-key" }),
    );
    let chain = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("chain");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain.plugin_refs(), vec![builtin::APIKEY_AUTH]);
    assert_eq!(chain.auth_plugins().len(), 1);
}

#[test]
fn an_upstream_without_auth_yields_only_the_plugin_chain() {
    let registry = registry();
    let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    upstream.auth = None;
    let chain = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("chain");
    assert!(chain.is_empty());
}

#[test]
fn upstream_plugins_run_before_route_plugins() {
    let registry = registry();
    let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    upstream.auth = None;
    upstream.plugins = PluginConfig {
        sharing: SharingMode::Private,
        items: vec![
            PluginBinding::new(builtin::REQUIRED_HEADERS_GUARD, serde_json::json!({})),
            PluginBinding::new(builtin::REQUEST_ID_TRANSFORM, serde_json::json!({})),
        ],
    };
    let route = route_with_plugins(vec![
        PluginBinding::new(builtin::REQUEST_ID_TRANSFORM, serde_json::json!({})),
        PluginBinding::new(builtin::REQUIRED_HEADERS_GUARD, serde_json::json!({})),
    ]);
    let chain = registry
        .build_chain(&upstream, Some(&route), &nothing_disabled())
        .expect("chain");
    assert_eq!(
        chain.plugin_refs(),
        vec![
            builtin::REQUIRED_HEADERS_GUARD,
            builtin::REQUEST_ID_TRANSFORM,
            builtin::REQUEST_ID_TRANSFORM,
            builtin::REQUIRED_HEADERS_GUARD,
        ]
    );
}

#[test]
fn declaration_order_is_kept_within_a_chain() {
    let registry = registry();
    let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    upstream.auth = None;
    upstream.plugins = PluginConfig {
        sharing: SharingMode::Private,
        items: vec![
            PluginBinding::new(builtin::REQUEST_ID_TRANSFORM, serde_json::json!({})),
            PluginBinding::new(builtin::REQUIRED_HEADERS_GUARD, serde_json::json!({})),
        ],
    };
    let chain = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("chain");
    assert_eq!(
        chain.plugin_refs(),
        vec![
            builtin::REQUEST_ID_TRANSFORM,
            builtin::REQUIRED_HEADERS_GUARD
        ]
    );
}

#[test]
fn an_unknown_binding_fails_the_whole_chain() {
    let registry = registry();
    let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    upstream.auth = None;
    upstream.plugins = PluginConfig {
        sharing: SharingMode::Private,
        items: vec![
            PluginBinding::new(builtin::REQUEST_ID_TRANSFORM, serde_json::json!({})),
            PluginBinding::new("cf.core.oagw.logging.v1", serde_json::json!({})),
        ],
    };
    let error = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect_err("503");
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        error.problem_body().context.plugin_id.as_deref(),
        Some("cf.core.oagw.logging.v1")
    );
}

#[test]
fn an_invalid_auth_configuration_fails_the_chain() {
    let registry = registry();
    let upstream = upstream_with_auth(builtin::APIKEY_AUTH, serde_json::json!({}));
    let error = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect_err("400");
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn a_bare_instance_fragment_is_accepted_in_a_binding() {
    let registry = registry();
    let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    upstream.auth = None;
    upstream.plugins = PluginConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding::new(
            "cf.core.oagw.request_id.v1",
            serde_json::json!({}),
        )],
    };
    let chain = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("chain");
    assert_eq!(chain.plugin_refs(), vec!["cf.core.oagw.request_id.v1"]);
}

// ---------------------------------------------------------------------------
// Skipping a disabled plugin resource
// ---------------------------------------------------------------------------

#[test]
fn the_disabled_set_matches_both_spellings_of_a_reference() {
    let id = Uuid::from_u128(0x77);
    let disabled = disabled(vec![disabled_plugin(id)]);
    assert!(!disabled.is_empty());
    assert!(disabled.is_disabled(&id.to_string()), "bare instance UUID");
    assert!(disabled.is_disabled(&format_plugin_id(id)), "GTS-form id");
    assert!(
        !disabled.is_disabled(builtin::REQUIRED_HEADERS_GUARD),
        "a builtin id is never a custom plugin reference"
    );
    assert!(
        !disabled.is_disabled(&Uuid::from_u128(0x78).to_string()),
        "another plugin resource is not disabled"
    );
}

#[test]
fn the_disabled_set_only_holds_disabled_resources() {
    let id = Uuid::from_u128(0x77);
    let enabled = Plugin {
        id,
        ..Plugin::default()
    };
    let disabled = DisabledPlugins::of(vec![Arc::new(enabled)]);
    assert!(disabled.is_empty());
    assert!(!disabled.is_disabled(&id.to_string()));
}

#[test]
fn a_disabled_plugin_resource_is_skipped_in_both_spellings() {
    let registry = registry();
    let id = Uuid::from_u128(0x77);
    let disabled = disabled(vec![disabled_plugin(id)]);
    for reference in [id.to_string(), format_plugin_id(id)] {
        let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
        upstream.auth = None;
        upstream.plugins = PluginConfig {
            sharing: SharingMode::Private,
            items: vec![
                PluginBinding::new(reference.clone(), serde_json::json!({})),
                PluginBinding::new(builtin::REQUEST_ID_TRANSFORM, serde_json::json!({})),
            ],
        };
        let chain = registry
            .build_chain(&upstream, None, &disabled)
            .expect("the disabled binding must not fail the build");
        assert_eq!(
            chain.plugin_refs(),
            vec![builtin::REQUEST_ID_TRANSFORM],
            "'{reference}' is disabled and must be skipped"
        );
    }
}

#[test]
fn a_disabled_plugin_resource_is_skipped_on_the_route_tier_too() {
    let registry = registry();
    let id = Uuid::from_u128(0x77);
    let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    upstream.auth = None;
    let route = route_with_plugins(vec![
        PluginBinding::new(format_plugin_id(id), serde_json::json!({})),
        PluginBinding::new(builtin::REQUIRED_HEADERS_GUARD, serde_json::json!({})),
    ]);
    let chain = registry
        .build_chain(
            &upstream,
            Some(&route),
            &disabled(vec![disabled_plugin(id)]),
        )
        .expect("chain");
    assert_eq!(
        chain.plugin_refs(),
        vec![builtin::REQUIRED_HEADERS_GUARD],
        "the remaining route binding is still built"
    );
}

#[test]
fn an_enabled_plugin_resource_still_fails_the_chain_with_a_503() {
    let registry = registry();
    // The reference names a plugin resource the tenant keeps enabled, so it is
    // not in the disabled set: an unresolvable reference stays a loud 503
    // instead of being dropped from the chain.
    let enabled = Uuid::from_u128(0x77);
    let mut upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    upstream.auth = None;
    upstream.plugins = PluginConfig {
        sharing: SharingMode::Private,
        items: vec![
            PluginBinding::new(enabled.to_string(), serde_json::json!({})),
            PluginBinding::new(builtin::REQUEST_ID_TRANSFORM, serde_json::json!({})),
        ],
    };
    let error = registry
        .build_chain(
            &upstream,
            None,
            &disabled(vec![disabled_plugin(Uuid::from_u128(0x78))]),
        )
        .expect_err("503");
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        error.problem_body().context.plugin_id.as_deref(),
        Some(enabled.to_string().as_str())
    );
}

#[test]
fn the_auth_binding_is_never_skipped_by_the_disabled_set() {
    let registry = registry();
    let id = Uuid::from_u128(0x77);
    let upstream = upstream_with_auth(&id.to_string(), serde_json::json!({}));
    let error = registry
        .build_chain(&upstream, None, &disabled(vec![disabled_plugin(id)]))
        .expect_err("503");
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        error.problem_body().context.plugin_id.as_deref(),
        Some(id.to_string().as_str())
    );
}

// ---------------------------------------------------------------------------
// End-to-end chain behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_built_chain_injects_the_api_key() {
    let registry = registry();
    let upstream = upstream_with_auth(
        builtin::APIKEY_AUTH,
        serde_json::json!({ "secret_ref": "cred://payments-key" }),
    );
    let chain = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("chain");
    let mut ctx = request();
    chain.authenticate(&mut ctx).await.expect("authenticated");
    assert_eq!(
        ctx.header("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("resolved-key")
    );
}

#[tokio::test]
async fn the_built_chain_stamps_a_request_id_and_enforces_the_guard() {
    let registry = registry();
    let upstream = upstream_with_auth(builtin::NOOP_AUTH, serde_json::json!({}));
    let mut upstream = upstream;
    upstream.auth = Some(AuthConfig {
        auth_type: builtin::NOOP_AUTH.to_owned(),
        sharing: SharingMode::Private,
        config: serde_json::json!({}),
    });
    upstream.plugins = PluginConfig {
        sharing: SharingMode::Private,
        items: vec![
            PluginBinding::new(
                builtin::REQUIRED_HEADERS_GUARD,
                serde_json::json!({
                    "required_request_headers": "x-trace-id"
                }),
            ),
            PluginBinding::new(builtin::REQUEST_ID_TRANSFORM, serde_json::json!({})),
        ],
    };
    let chain = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("chain");

    let mut rejected = request();
    chain.authenticate(&mut rejected).await.expect("auth");
    let error = chain.guard_request(&rejected).await.expect_err("400");
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);

    let mut accepted = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(TENANT)
        .headers(HeaderMap::from_iter([(
            HeaderName::from_static("x-trace-id"),
            HeaderValue::from_static("present"),
        )]))
        .security(security())
        .build();
    chain.authenticate(&mut accepted).await.expect("auth");
    chain.guard_request(&accepted).await.expect("guard");
    chain
        .transform_request(&mut accepted)
        .await
        .expect("transform");
    assert!(accepted.request_id.is_some());
    assert!(accepted.has_header(REQUEST_ID_HEADER));
}

#[tokio::test]
async fn the_unavailable_resolver_fails_the_chain_at_use_time() {
    let registry = PluginRegistry::with_builtins(
        Arc::new(UnavailableSecretResolver),
        TokenCacheConfig::from(&OagwConfig::default()),
    );
    let upstream = upstream_with_auth(
        builtin::APIKEY_AUTH,
        serde_json::json!({ "secret_ref": "cred://payments-key" }),
    );
    let chain = registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("chain");
    let mut ctx = request();
    let error = chain.authenticate(&mut ctx).await.expect_err("500");
    assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        ctx.injected_headers.is_empty(),
        "never proxy unauthenticated"
    );
}

#[test]
fn the_builtins_registry_is_cloneable() {
    let registry = registry();
    let clone = registry.clone();
    assert_eq!(clone.len(), registry.len());
}

// ---------------------------------------------------------------------------
// Instance memoisation (ADR-0008 token cache survival)
// ---------------------------------------------------------------------------

/// The OAuth2 configuration the memoisation tests bind.
fn oauth2_config() -> serde_json::Value {
    serde_json::json!({
        "token_endpoint": "https://idp.example.com/token",
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    })
}

#[test]
fn the_same_binding_returns_the_same_instance() {
    let registry = registry();
    let first = registry
        .build_auth(builtin::OAUTH2_CLIENT_CRED, &oauth2_config())
        .expect("plugin");
    let second = registry
        .build_auth(builtin::OAUTH2_CLIENT_CRED, &oauth2_config())
        .expect("plugin");
    assert!(
        Arc::ptr_eq(&first, &second),
        "the token cache of the ADR-0008 plugin must survive across requests"
    );
}

#[test]
fn both_spellings_of_a_reference_share_one_instance() {
    let registry = registry();
    let qualified = registry
        .build_auth(builtin::OAUTH2_CLIENT_CRED, &oauth2_config())
        .expect("plugin");
    let bare = registry
        .build_auth("cf.core.oagw.oauth2_client_cred.v1", &oauth2_config())
        .expect("plugin");
    assert!(
        Arc::ptr_eq(&qualified, &bare),
        "both spellings resolve to the same registry key"
    );
}

#[test]
fn a_different_configuration_builds_a_new_instance() {
    let registry = registry();
    let first = registry
        .build_auth(builtin::OAUTH2_CLIENT_CRED, &oauth2_config())
        .expect("plugin");
    let other = registry
        .build_auth(
            builtin::OAUTH2_CLIENT_CRED,
            &serde_json::json!({
                "token_endpoint": "https://other.example.com/token",
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "cred://client-secret"
            }),
        )
        .expect("plugin");
    assert!(!Arc::ptr_eq(&first, &other));
}

#[test]
fn a_failed_construction_is_never_memoised() {
    let registry = registry();
    let config = serde_json::json!({ "client_id_ref": "cred://x" });
    let first = expect_error(
        registry.build_auth(builtin::OAUTH2_CLIENT_CRED, &config),
        "incomplete configuration",
    );
    let second = expect_error(
        registry.build_auth(builtin::OAUTH2_CLIENT_CRED, &config),
        "still incomplete",
    );
    assert_eq!(first.status(), second.status());
    assert_eq!(first.detail(), second.detail(), "the same error every time");
}

#[test]
fn re_registering_a_reference_forgets_its_instances() {
    let registry = registry();
    let first = registry
        .build_auth(builtin::NOOP_AUTH, &serde_json::Value::Null)
        .expect("plugin");
    let mut registry = registry;
    registry.register_auth(
        builtin::NOOP_AUTH,
        Arc::new(|_config| Ok(Arc::new(NoopAuth) as Arc<dyn AuthPlugin>)),
    );
    let second = registry
        .build_auth(builtin::NOOP_AUTH, &serde_json::Value::Null)
        .expect("plugin");
    assert!(!Arc::ptr_eq(&first, &second));
}

struct NoopAuth;

#[async_trait::async_trait]
impl AuthPlugin for NoopAuth {
    fn id(&self) -> &str {
        builtin::NOOP_AUTH
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::AUTH_PLUGIN_TYPE_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        Ok(())
    }
}

#[test]
fn two_chain_builds_construct_a_binding_once() {
    let constructions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&constructions);
    let mut registry = registry();
    registry.register_auth(
        "cf.core.oagw.counting.v1",
        Arc::new(move |_config| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Arc::new(NoopAuth) as Arc<dyn AuthPlugin>)
        }),
    );
    let upstream = upstream_with_auth("cf.core.oagw.counting.v1", serde_json::json!({}));
    registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("first chain");
    registry
        .build_chain(&upstream, None, &nothing_disabled())
        .expect("second chain");
    assert_eq!(
        constructions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "every request rebuilds the chain, so the instance must be memoised"
    );
}

#[test]
fn the_memo_table_stays_bounded() {
    let mut registry = empty_registry();
    registry.register_auth(
        "cf.core.oagw.counting.v1",
        Arc::new(|_config| Ok(Arc::new(NoopAuth) as Arc<dyn AuthPlugin>)),
    );
    for index in 0..2_000u32 {
        registry
            .build_auth(
                "cf.core.oagw.counting.v1",
                &serde_json::json!({ "index": index }),
            )
            .expect("plugin");
    }
    assert!(registry.constructed.read().len() <= 1024);
}
