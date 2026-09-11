//! Tests for the plugin system (ADR-0002): the registries and the resolution of
//! a chain, the execution order, and each built-in plugin — including the
//! credential seam and the `OAuth2` token cache, exercised against a loopback
//! token endpoint and an in-process credential-store double.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use crate::domain::model::BoundPluginBinding;
use crate::domain::resolution::DialPolicy;

use axum::http::{HeaderMap, Method, StatusCode, header};
use credstore_sdk::test_util::MockCredStoreClient;
use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use httpmock::MockServer;
use parking_lot::Mutex as BlockingMutex;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::TokenCacheConfig;
use crate::domain::model::PluginChain as PluginChainDocument;
use crate::domain::model::{PluginBinding, PluginKind, PluginPhase, PluginSpec};
use crate::error::{
    AUTHENTICATION_FAILED_TYPE, INTERNAL_ERROR_TYPE, OagwError, PLUGIN_NOT_FOUND_TYPE,
    PROTOCOL_ERROR_TYPE, VALIDATION_ERROR_TYPE,
};

use super::{
    API_KEY_AUTH_PLUGIN_ID, ApiKeyAuthPlugin, AuthPlugin, AuthPluginRegistry,
    CATALOG_AUTH_PLUGIN_IDS, CATALOG_GUARD_PLUGIN_IDS, CATALOG_TRANSFORM_PLUGIN_IDS,
    CredStoreCredentialSource, CredentialSource, ErrorContext, GuardDecision, GuardPlugin,
    GuardPluginRegistry, NoopAuthPlugin, OAuth2ClientCredAuthPlugin, PluginPipeline,
    PluginRegistries, REQUEST_ID_TRANSFORM_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID,
    RequestContext, RequestIdTransformPlugin, RequiredHeadersGuardPlugin, ResponseContext,
    TransformPlugin, TransformPluginRegistry,
};

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn tenant() -> Uuid {
    Uuid::from_u128(0xA11A)
}

fn security() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xFEED))
        .subject_tenant_id(tenant())
        .build()
        .expect("test security context")
}

fn request(headers: &[(&str, &str)], config: Option<Value>) -> RequestContext {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            header::HeaderName::from_lowercase(name.as_bytes()).expect("lowercase header name"),
            header::HeaderValue::from_str(value).expect("header value"),
        );
    }
    RequestContext {
        headers: map,
        method: Method::GET,
        tenant_id: tenant(),
        subject_id: Some(Uuid::from_u128(0xFEED)),
        upstream_id: Uuid::from_u128(0x5F1),
        route_id: Uuid::from_u128(0x7E57),
        security: Some(security()),
        config,
    }
}

fn response(headers: &[(&str, &str)], config: Option<Value>) -> ResponseContext {
    ResponseContext {
        status: StatusCode::OK,
        headers: headers
            .iter()
            .map(|(name, value)| (name.parse().unwrap(), value.parse().unwrap()))
            .collect(),
        request_id: None,
        config,
    }
}

fn chain(bindings: Vec<PluginBinding>) -> PluginChainDocument {
    PluginChainDocument {
        items: bindings,
        ..PluginChainDocument::default()
    }
}

fn reference(kind: PluginKind, name: &str) -> String {
    format!("{}cf.core.oagw.{name}.v1", kind.gts_base_type())
}

fn bound(kind: PluginKind, name: &str, config: Value) -> PluginBinding {
    PluginBinding::Bound(BoundPluginBinding {
        plugin_ref: reference(kind, name),
        config: Some(config),
    })
}

fn source() -> Arc<dyn CredentialSource> {
    Arc::new(CredStoreCredentialSource::new(Arc::new(
        MockCredStoreClient::with_secrets(vec![
            ("api-key".to_owned(), "sk-live-123".to_owned()),
            ("client-id".to_owned(), "client-42".to_owned()),
            ("client-secret".to_owned(), "shh".to_owned()),
        ]),
    )))
}

fn cache() -> TokenCacheConfig {
    TokenCacheConfig {
        ttl_secs: 300,
        capacity: 64,
    }
}

/// The dial policy the token-endpoint tests run under: plaintext allowed so the
/// loopback token endpoint is dialable, SSRF off for the same reason.
fn policy() -> DialPolicy {
    DialPolicy {
        allow_http: true,
        ssrf_enabled: false,
    }
}

/// A guard plugin that records the phases it ran in.
#[derive(Default)]
struct RecorderGuard(BlockingMutex<Vec<&'static str>>);

#[async_trait::async_trait]
impl GuardPlugin for RecorderGuard {
    fn id(&self) -> &'static str {
        "recorder"
    }

    fn plugin_type(&self) -> &'static str {
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.recorder.v1"
    }

    async fn guard_request(&self, _ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        self.0.lock().push("guard_request");
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, _ctx: &mut ResponseContext) -> Result<GuardDecision, OagwError> {
        self.0.lock().push("guard_response");
        Ok(GuardDecision::Allow)
    }
}

/// A transform plugin that records the phases it ran in.
#[derive(Default)]
struct RecorderTransform(BlockingMutex<Vec<&'static str>>);

#[async_trait::async_trait]
impl super::TransformPlugin for RecorderTransform {
    fn id(&self) -> &'static str {
        "recorder"
    }

    fn plugin_type(&self) -> &'static str {
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.recorder.v1"
    }

    async fn transform_request(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        self.0.lock().push("transform_request");
        Ok(())
    }

    async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), OagwError> {
        self.0.lock().push("transform_response");
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
        self.0.lock().push("transform_error");
        Ok(())
    }
}

// ── Registries ───────────────────────────────────────────────────────────────

#[test]
fn the_builtin_registries_resolve_every_documented_plugin() {
    let registries = PluginRegistries::with_builtins();
    for name in [
        "noop",
        "apikey",
        "oauth2_client_cred",
        "oauth2_client_cred_basic",
    ] {
        assert!(
            registries
                .auth
                .resolve(&reference(PluginKind::Auth, name))
                .is_some(),
            "{name} is a built-in auth plugin"
        );
    }
    assert!(
        registries
            .guard
            .resolve(&reference(PluginKind::Guard, "required_headers"))
            .is_some()
    );
    assert!(
        registries
            .transform
            .resolve(&reference(PluginKind::Transform, "request_id"))
            .is_some()
    );
}

#[test]
fn the_builtin_plugin_identifiers_are_the_documented_ones() {
    let registries = PluginRegistries::with_builtins();
    assert_eq!(
        registries
            .auth
            .resolve(&reference(PluginKind::Auth, "noop"))
            .expect("noop")
            .plugin_type(),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
    );
    assert_eq!(
        API_KEY_AUTH_PLUGIN_ID,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(
        REQUEST_ID_TRANSFORM_PLUGIN_ID,
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );
    assert_eq!(
        REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
}

#[test]
fn a_catalog_only_identifier_resolves_to_nothing() {
    let registries = PluginRegistries::with_builtins();
    for identifier in CATALOG_AUTH_PLUGIN_IDS {
        assert!(
            registries.auth.resolve(identifier).is_none(),
            "{identifier} is catalogued, not implemented"
        );
    }
    for identifier in CATALOG_GUARD_PLUGIN_IDS {
        assert!(
            registries.guard.resolve(identifier).is_none(),
            "{identifier}"
        );
    }
    for identifier in CATALOG_TRANSFORM_PLUGIN_IDS {
        assert!(
            registries.transform.resolve(identifier).is_none(),
            "{identifier}"
        );
    }
}

#[test]
fn an_externally_implemented_plugin_registers_through_injection() {
    let registry = AuthPluginRegistry::with_plugins(vec![Arc::new(NoopAuthPlugin)]);
    let built_in = reference(PluginKind::Auth, "noop");
    assert!(
        registry.resolve(&built_in).is_some(),
        "the injected plugin resolves"
    );
    assert!(
        registry
            .resolve(&reference(PluginKind::Auth, "apikey"))
            .is_none(),
        "an injected registry replaces the built-ins"
    );
}

// ── Chain resolution ─────────────────────────────────────────────────────────

#[test]
fn an_unresolvable_reference_is_refused_with_503() {
    let registries = PluginRegistries::with_builtins();
    let upstream = chain(vec![PluginBinding::Ref(
        CATALOG_AUTH_PLUGIN_IDS[0].to_owned(),
    )]);
    let error = PluginPipeline::resolve(&registries, Some(&upstream), None, None)
        .expect_err("basic auth is catalog-only");
    assert_eq!(error.status_code(), 503);
    assert_eq!(error.gts_type(), PLUGIN_NOT_FOUND_TYPE);
}

#[test]
fn a_reference_that_names_no_kind_is_refused_with_503() {
    let registries = PluginRegistries::with_builtins();
    let upstream = chain(vec![PluginBinding::Ref(
        "not-a-gts-plugin-reference".to_owned(),
    )]);
    let error = PluginPipeline::resolve(&registries, Some(&upstream), None, None)
        .expect_err("no plugin kind to dispatch on");
    assert_eq!(error.gts_type(), PLUGIN_NOT_FOUND_TYPE);
}

#[test]
fn a_chain_without_plugins_is_empty() {
    let pipeline = PluginPipeline::resolve(&PluginRegistries::with_builtins(), None, None, None)
        .expect("no chain, no plugins");
    assert!(pipeline.is_empty());
}

#[tokio::test]
async fn the_upstream_chain_runs_before_the_route_chain() {
    // The same transform bound to both chains records twice, in chain order.
    let recorder = Arc::new(RecorderTransform::default());
    let plugin = Arc::clone(&recorder) as Arc<dyn TransformPlugin>;
    let registries = PluginRegistries::new(
        AuthPluginRegistry::with_plugins(vec![]),
        GuardPluginRegistry::with_plugins(vec![]),
        TransformPluginRegistry::with_plugins(vec![plugin]),
    );
    let upstream = chain(vec![bound(
        PluginKind::Transform,
        "recorder",
        json!({ "position": "upstream" }),
    )]);
    let route = chain(vec![bound(
        PluginKind::Transform,
        "recorder",
        json!({ "position": "route" }),
    )]);
    let pipeline = PluginPipeline::resolve(&registries, Some(&upstream), Some(&route), None)
        .expect("both references resolve");
    let mut ctx = request(&[], None);
    pipeline
        .transform_request(&mut ctx)
        .await
        .expect("transforms run");
    assert_eq!(
        recorder.0.lock().as_slice(),
        ["transform_request", "transform_request"],
        "the upstream binding runs, then the route binding"
    );
}

#[tokio::test]
async fn the_pipeline_runs_the_documented_order_of_phases() {
    let guard = Arc::new(RecorderGuard::default());
    let recorder = Arc::new(RecorderTransform::default());
    let guard_plugin = Arc::clone(&guard) as Arc<dyn GuardPlugin>;
    let transform_plugin = Arc::clone(&recorder) as Arc<dyn TransformPlugin>;
    let registries = PluginRegistries::new(
        AuthPluginRegistry::with_plugins(vec![Arc::new(NoopAuthPlugin)]),
        GuardPluginRegistry::with_plugins(vec![guard_plugin]),
        TransformPluginRegistry::with_plugins(vec![transform_plugin]),
    );
    let upstream = chain(vec![
        PluginBinding::Ref(reference(PluginKind::Auth, "noop")),
        PluginBinding::Ref(reference(PluginKind::Guard, "recorder")),
        PluginBinding::Ref(reference(PluginKind::Transform, "recorder")),
    ]);
    let pipeline = PluginPipeline::resolve(&registries, Some(&upstream), None, None)
        .expect("the chain resolves");

    let mut ctx = request(&[], None);
    pipeline.authenticate(&mut ctx).await.expect("auth runs");
    assert!(
        guard.0.lock().is_empty(),
        "no guard ran during the auth phase"
    );
    assert!(
        recorder.0.lock().is_empty(),
        "no transform ran during the auth phase"
    );

    pipeline.guard_request(&mut ctx).await.expect("guards run");
    assert_eq!(guard.0.lock().as_slice(), ["guard_request"]);

    pipeline
        .transform_request(&mut ctx)
        .await
        .expect("transforms run");
    assert_eq!(recorder.0.lock().as_slice(), ["transform_request"]);

    let mut upstream_response = response(&[], None);
    pipeline
        .guard_response(&mut upstream_response)
        .await
        .expect("guards run");
    pipeline
        .transform_response(&mut upstream_response)
        .await
        .expect("transforms run");
    assert_eq!(
        guard.0.lock().as_slice(),
        ["guard_request", "guard_response"]
    );
    assert_eq!(
        recorder.0.lock().as_slice(),
        ["transform_request", "transform_response"]
    );

    let mut error = ErrorContext {
        error: OagwError::downstream_error("upstream gone"),
        config: None,
    };
    pipeline
        .transform_error(&mut error)
        .await
        .expect("transforms run");
    assert_eq!(
        recorder.0.lock().as_slice(),
        ["transform_request", "transform_response", "transform_error"]
    );
}

#[tokio::test]
async fn each_plugin_sees_only_the_config_of_its_own_binding() {
    struct Spy;
    #[async_trait::async_trait]
    impl GuardPlugin for Spy {
        fn id(&self) -> &'static str {
            "spy"
        }
        fn plugin_type(&self) -> &'static str {
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.spy.v1"
        }
        async fn guard_request(
            &self,
            ctx: &mut RequestContext,
        ) -> Result<GuardDecision, OagwError> {
            assert_eq!(
                ctx.config
                    .as_ref()
                    .and_then(|document| document.get("which"))
                    .and_then(Value::as_str),
                Some("mine"),
                "a plugin is handed the config of its own binding"
            );
            Ok(GuardDecision::Allow)
        }
        async fn guard_response(
            &self,
            _ctx: &mut ResponseContext,
        ) -> Result<GuardDecision, OagwError> {
            Ok(GuardDecision::Allow)
        }
    }
    let registries = PluginRegistries::new(
        AuthPluginRegistry::with_plugins(vec![]),
        GuardPluginRegistry::with_plugins(vec![Arc::new(Spy)]),
        TransformPluginRegistry::with_plugins(vec![]),
    );
    let upstream = chain(vec![bound(
        PluginKind::Guard,
        "spy",
        json!({ "which": "mine" }),
    )]);
    let pipeline = PluginPipeline::resolve(&registries, Some(&upstream), None, None)
        .expect("the chain resolves");
    let mut ctx = request(&[], None);
    pipeline
        .guard_request(&mut ctx)
        .await
        .expect("the guard runs");
    assert!(
        ctx.config.is_none(),
        "the context is left as the pipeline found it"
    );
}

#[tokio::test]
async fn a_guard_rejection_travels_through_the_pipeline() {
    let registries = PluginRegistries::with_builtins();
    let upstream = chain(vec![bound(
        PluginKind::Guard,
        "required_headers",
        json!({ "required_request_headers": "X-Required" }),
    )]);
    let pipeline = PluginPipeline::resolve(&registries, Some(&upstream), None, None)
        .expect("the chain resolves");
    let mut ctx = request(&[], None);
    let error = pipeline
        .guard_request(&mut ctx)
        .await
        .expect_err("the required header is missing");
    assert_eq!(error.status_code(), 400);
    assert_eq!(error.gts_type(), VALIDATION_ERROR_TYPE);
}

// ── Credential seam ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_cred_store_source_resolves_a_reference_to_its_value() {
    let resolved = source()
        .resolve(&security(), "cred://api-key")
        .await
        .expect("the reference resolves")
        .expect("the secret exists");
    assert_eq!(resolved.expose(), "sk-live-123");
}

#[tokio::test]
async fn a_reference_no_tenant_can_see_resolves_to_none() {
    let resolved = source()
        .resolve(&security(), "cred://absent")
        .await
        .expect("the lookup succeeded");
    assert!(resolved.is_none(), "a missing secret is not an error");
}

#[tokio::test]
async fn a_malformed_reference_is_a_401() {
    let error = source()
        .resolve(&security(), "cred://../escape")
        .await
        .expect_err("the reference is not a secret key");
    assert_eq!(error.status_code(), 401);
    assert_eq!(error.gts_type(), AUTHENTICATION_FAILED_TYPE);
}

#[tokio::test]
async fn an_unreachable_cred_store_is_reported_as_an_authentication_failure() {
    struct Unavailable;
    #[async_trait::async_trait]
    impl CredStoreClientV1 for Unavailable {
        async fn get(
            &self,
            _ctx: &SecurityContext,
            _key: &SecretRef,
        ) -> Result<Option<credstore_sdk::GetSecretResponse>, CredStoreError> {
            Err(CredStoreError::AccessDenied)
        }
    }
    let adapter = CredStoreCredentialSource::new(Arc::new(Unavailable));
    let error = adapter
        .resolve(&security(), "cred://api-key")
        .await
        .expect_err("the store refused");
    assert_eq!(error.status_code(), 401);
    assert_eq!(error.gts_type(), AUTHENTICATION_FAILED_TYPE);
}

// ── noop auth ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_noop_auth_plugin_leaves_the_request_alone() {
    let plugin = NoopAuthPlugin;
    let mut ctx = request(&[("authorization", "opaque")], None);
    plugin.authenticate(&mut ctx).await.expect("nothing to do");
    assert_eq!(ctx.headers.get("authorization").unwrap(), "opaque");
}

// ── apikey auth ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_apikey_plugin_injects_the_resolved_secret_into_the_named_header() {
    let plugin = ApiKeyAuthPlugin::new(Some(source()));
    let mut ctx = request(
        &[],
        Some(json!({ "secret_ref": "cred://api-key", "header_name": "X-Api-Key" })),
    );
    plugin
        .authenticate(&mut ctx)
        .await
        .expect("the key resolves");
    assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-live-123");
}

#[tokio::test]
async fn the_apikey_plugin_defaults_to_the_x_api_key_header() {
    let plugin = ApiKeyAuthPlugin::new(Some(source()));
    let mut ctx = request(&[], Some(json!({ "secret_ref": "cred://api-key" })));
    plugin
        .authenticate(&mut ctx)
        .await
        .expect("the key resolves");
    assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-live-123");
}

#[tokio::test]
async fn an_apikey_binding_without_a_credential_source_is_a_401() {
    let plugin = ApiKeyAuthPlugin::new(None);
    let mut ctx = request(&[], Some(json!({ "secret_ref": "cred://api-key" })));
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("no credential source is wired");
    assert_eq!(error.status_code(), 401);
    assert_eq!(error.gts_type(), AUTHENTICATION_FAILED_TYPE);
    assert!(
        error.detail().contains("no credential source"),
        "the rejection names the missing seam: {}",
        error.detail()
    );
    assert!(
        ctx.headers.get("x-api-key").is_none(),
        "nothing was injected"
    );
}

#[tokio::test]
async fn an_apikey_binding_with_an_unresolvable_reference_is_a_401() {
    let plugin = ApiKeyAuthPlugin::new(Some(source()));
    let mut ctx = request(&[], Some(json!({ "secret_ref": "cred://absent" })));
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("the reference names nothing");
    assert_eq!(error.status_code(), 401);
    assert!(
        error.detail().contains("does not resolve"),
        "{}",
        error.detail()
    );
}

#[tokio::test]
async fn an_apikey_binding_without_a_secret_ref_is_a_500() {
    let plugin = ApiKeyAuthPlugin::new(Some(source()));
    let mut ctx = request(&[], Some(json!({ "header_name": "x-api-key" })));
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("misconfigured");
    assert_eq!(error.status_code(), 500);
    assert_eq!(error.gts_type(), INTERNAL_ERROR_TYPE);
}

#[tokio::test]
async fn an_apikey_request_without_a_security_context_is_a_401() {
    let plugin = ApiKeyAuthPlugin::new(Some(source()));
    let mut ctx = request(&[], Some(json!({ "secret_ref": "cred://api-key" })));
    ctx.security = None;
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("no caller identity");
    assert_eq!(error.status_code(), 401);
}

// ── oauth2 client credentials ────────────────────────────────────────────────

/// A token endpoint answering `token` with `expires_in`, matching only a form
/// exchange from `client_id`. Returns the mock (for its call count) and the URL
/// to bind.
async fn form_token_endpoint<'a>(
    server: &'a MockServer,
    client_id: &str,
    token: &str,
    expires_in: u64,
) -> (httpmock::Mock<'a>, String) {
    let client_id = client_id.to_owned();
    let mock = server
        .mock_async(move |when, then| {
            when.method("POST")
                .path("/oauth/token")
                .header_includes("content-type", "application/x-www-form-urlencoded")
                .body_includes("grant_type=client_credentials")
                .body_includes(format!("client_id={client_id}"));
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"{{"access_token":"{token}","token_type":"Bearer","expires_in":{expires_in}}}"#
                ));
        })
        .await;
    let url = format!("http://{}/oauth/token", server.address());
    (mock, url)
}

/// A token endpoint that authenticates the client with HTTP Basic.
async fn basic_token_endpoint<'a>(
    server: &'a MockServer,
    token: &str,
) -> (httpmock::Mock<'a>, String) {
    let mock = server
        .mock_async(move |when, then| {
            when.method("POST")
                .path("/oauth/token")
                .header_includes("authorization", "Basic ")
                .body_excludes("client_id=client-42")
                .body_includes("grant_type=client_credentials");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"{{"access_token":"{token}","token_type":"Bearer","expires_in":3600}}"#
                ));
        })
        .await;
    let url = format!("http://{}/oauth/token", server.address());
    (mock, url)
}

fn oauth_binding(endpoint: &str, client_ref: &str) -> Value {
    json!({
        "token_endpoint": endpoint,
        "client_id_ref": format!("cred://{client_ref}"),
        "client_secret_ref": "cred://client-secret"
    })
}

#[tokio::test]
async fn the_oauth2_plugin_exchanges_credentials_for_a_bearer_token() {
    let server = MockServer::start();
    let (mock, endpoint) = form_token_endpoint(&server, "client-42", "token-1", 3600).await;
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let mut ctx = request(
        &[],
        Some(json!({
            "token_endpoint": endpoint,
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
            "scopes": "read write"
        })),
    );
    plugin
        .authenticate(&mut ctx)
        .await
        .expect("the exchange succeeds");
    assert_eq!(
        ctx.headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer token-1",
        "the access token is injected as a bearer credential"
    );
    mock.assert_calls(1);
}

#[tokio::test]
async fn the_oauth2_plugin_sends_client_credentials_as_basic_auth_when_configured() {
    let server = MockServer::start();
    let (mock, endpoint) = basic_token_endpoint(&server, "token-basic").await;
    let plugin = OAuth2ClientCredAuthPlugin::basic(Some(source()), cache(), policy());
    let mut ctx = request(&[], Some(oauth_binding(&endpoint, "client-id")));
    plugin
        .authenticate(&mut ctx)
        .await
        .expect("the exchange succeeds");
    assert_eq!(
        ctx.headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer token-basic"
    );
    mock.assert_calls(1);
}

#[tokio::test]
async fn the_oauth2_plugin_caches_its_token_across_requests() {
    let server = MockServer::start();
    let (mock, endpoint) = form_token_endpoint(&server, "client-42", "token-1", 3600).await;
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let document = oauth_binding(&endpoint, "client-id");
    for _ in 0..3 {
        let mut ctx = request(&[], Some(document.clone()));
        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the exchange succeeds");
        assert_eq!(
            ctx.headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer token-1"
        );
    }
    assert_eq!(mock.calls(), 1, "one exchange served three requests");
}

#[tokio::test]
async fn a_token_the_idp_retires_soon_is_not_cached() {
    let server = MockServer::start();
    let (mock, endpoint) = form_token_endpoint(&server, "client-42", "token-1", 10).await;
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let document = oauth_binding(&endpoint, "client-id");
    for _ in 0..2 {
        let mut ctx = request(&[], Some(document.clone()));
        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the exchange succeeds");
    }
    assert_eq!(
        mock.calls(),
        2,
        "a token with 10s of life sits inside the 30s cache margin and is re-asked"
    );
}

#[tokio::test]
async fn a_different_binding_does_not_share_a_cached_token() {
    let server = MockServer::start();
    let (first_mock, first_endpoint) =
        form_token_endpoint(&server, "client-42", "token-1", 3600).await;
    let (second_mock, second_endpoint) =
        form_token_endpoint(&server, "client-43", "token-2", 3600).await;
    let credentials = Arc::new(CredStoreCredentialSource::new(Arc::new(
        MockCredStoreClient::with_secrets(vec![
            ("client-id".to_owned(), "client-42".to_owned()),
            ("other-client".to_owned(), "client-43".to_owned()),
            ("client-secret".to_owned(), "shh".to_owned()),
        ]),
    )));
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(credentials), cache(), policy());
    let mut first = request(&[], Some(oauth_binding(&first_endpoint, "client-id")));
    plugin
        .authenticate(&mut first)
        .await
        .expect("the first binding resolves");
    let mut second = request(&[], Some(oauth_binding(&second_endpoint, "other-client")));
    plugin
        .authenticate(&mut second)
        .await
        .expect("the second binding resolves");
    assert_eq!(
        first.headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer token-1"
    );
    assert_eq!(
        second.headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer token-2"
    );
    assert_eq!(first_mock.calls(), 1);
    assert_eq!(
        second_mock.calls(),
        1,
        "each binding exchanges its own token"
    );
}

#[tokio::test]
async fn an_oauth2_binding_with_no_credential_source_is_a_401() {
    let plugin = OAuth2ClientCredAuthPlugin::form(None, cache(), policy());
    let mut ctx = request(
        &[],
        Some(json!({
            "token_endpoint": "http://idp.example.com/oauth/token",
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret"
        })),
    );
    let error = plugin.authenticate(&mut ctx).await.expect_err("no source");
    assert_eq!(error.status_code(), 401);
    assert_eq!(error.gts_type(), AUTHENTICATION_FAILED_TYPE);
}

#[tokio::test]
async fn an_oauth2_binding_with_an_unresolvable_client_secret_is_a_401() {
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let mut ctx = request(
        &[],
        Some(json!({
            "token_endpoint": "http://idp.example.com/oauth/token",
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://absent"
        })),
    );
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("the secret is missing");
    assert_eq!(error.status_code(), 401);
    assert!(
        error.detail().contains("does not resolve"),
        "{}",
        error.detail()
    );
}

#[tokio::test]
async fn an_oauth2_binding_without_an_endpoint_is_a_500() {
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let mut ctx = request(
        &[],
        Some(json!({
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret"
        })),
    );
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("misconfigured");
    assert_eq!(error.status_code(), 500);
}

#[tokio::test]
async fn an_oauth2_binding_with_both_endpoints_is_a_500() {
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let mut ctx = request(
        &[],
        Some(json!({
            "token_endpoint": "http://idp.example.com/oauth/token",
            "issuer_url": "https://idp.example.com",
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret"
        })),
    );
    let error = plugin.authenticate(&mut ctx).await.expect_err("ambiguous");
    assert_eq!(error.status_code(), 500);
}

#[tokio::test]
async fn a_refused_token_exchange_is_a_401() {
    let server = MockServer::start();
    server
        .mock_async(|when, then| {
            when.method("POST").path("/oauth/token");
            then.status(400).body(r#"{"error":"invalid_client"}"#);
        })
        .await;
    let endpoint = format!("http://{}/oauth/token", server.address());
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let mut ctx = request(&[], Some(oauth_binding(&endpoint, "client-id")));
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("the IdP refuses");
    assert_eq!(error.status_code(), 401);
    assert_eq!(error.gts_type(), AUTHENTICATION_FAILED_TYPE);
}

#[tokio::test]
async fn an_issuer_url_binding_resolves_the_token_endpoint_through_discovery() {
    let server = MockServer::start();
    // The discovery document of the issuer points at the same mock's token
    // endpoint, so the exchange only succeeds when discovery ran.
    let address = server.address();
    let discovery = server
        .mock_async(move |when, then| {
            when.method("GET").path("/.well-known/openid-configuration");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"{{"token_endpoint":"http://{address}/oauth/token"}}"#
                ));
        })
        .await;
    let (token, _endpoint) = form_token_endpoint(&server, "client-42", "token-1", 3600).await;
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let mut ctx = request(
        &[],
        Some(json!({
            "issuer_url": format!("http://{address}"),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret"
        })),
    );

    plugin
        .authenticate(&mut ctx)
        .await
        .expect("discovery resolves the token endpoint");

    assert_eq!(discovery.calls(), 1, "the issuer document was fetched");
    assert_eq!(
        token.calls(),
        1,
        "the exchange ran against the resolved endpoint"
    );
    assert!(ctx.headers.contains_key(header::AUTHORIZATION));
}

#[tokio::test]
async fn a_loopback_token_endpoint_is_refused_under_an_enabled_ssrf_policy() {
    let server = MockServer::start();
    let (mock, endpoint) = form_token_endpoint(&server, "client-42", "token-1", 3600).await;
    // The SSRF policy the upstream endpoints are held to covers the credential
    // endpoint too: 127.0.0.1 is a loopback address.
    let plugin = OAuth2ClientCredAuthPlugin::form(
        Some(source()),
        cache(),
        DialPolicy {
            allow_http: true,
            ssrf_enabled: true,
        },
    );
    let mut ctx = request(&[], Some(oauth_binding(&endpoint, "client-id")));

    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("the policy refuses the loopback token endpoint");

    assert_eq!(
        error.status_code(),
        500,
        "a stored binding is a misconfiguration"
    );
    assert!(error.detail().contains("ssrf policy"), "{error}");
    assert_eq!(mock.calls(), 0, "nothing was dialed");
}

#[tokio::test]
async fn a_plaintext_token_endpoint_is_refused_while_plaintext_is_disabled() {
    let plugin = OAuth2ClientCredAuthPlugin::form(
        Some(source()),
        cache(),
        DialPolicy {
            allow_http: false,
            ssrf_enabled: false,
        },
    );
    let mut ctx = request(
        &[],
        Some(oauth_binding("http://127.0.0.1:1/oauth/token", "client-id")),
    );

    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("plaintext outbound connections are disabled");

    assert_eq!(error.status_code(), 500);
    assert!(
        error.detail().contains("allow_http_upstream"),
        "the refusal names the configuration that would allow it: {error}"
    );
}

#[tokio::test]
async fn an_unreachable_token_endpoint_is_a_401() {
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let mut ctx = request(
        &[],
        Some(oauth_binding("http://127.0.0.1:1/oauth/token", "client-id")),
    );
    let error = plugin
        .authenticate(&mut ctx)
        .await
        .expect_err("nothing is listening");
    assert_eq!(error.status_code(), 401);
}

#[tokio::test]
async fn a_cached_token_spares_the_credential_store_and_the_idp() {
    let server = MockServer::start();
    let (mock, endpoint) = form_token_endpoint(&server, "client-42", "token-1", 3600).await;
    let plugin = OAuth2ClientCredAuthPlugin::form(Some(source()), cache(), policy());
    let document = oauth_binding(&endpoint, "client-id");
    for _ in 0..2 {
        let mut ctx = request(&[], Some(document.clone()));
        plugin
            .authenticate(&mut ctx)
            .await
            .expect("the exchange succeeds");
    }
    assert_eq!(
        mock.calls(),
        1,
        "the second request was served from the cache"
    );
}

// ── required_headers guard ───────────────────────────────────────────────────

#[tokio::test]
async fn the_required_headers_guard_admits_a_request_that_carries_them() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut ctx = request(
        &[("x-tenant-id", "acme"), ("authorization", "Bearer x")],
        Some(json!({ "required_request_headers": "X-Tenant-Id, Authorization" })),
    );
    assert!(
        plugin
            .guard_request(&mut ctx)
            .await
            .expect("the guard runs")
            .is_allow()
    );
}

#[tokio::test]
async fn the_required_headers_guard_rejects_a_request_missing_one_of_them() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut ctx = request(
        &[("authorization", "Bearer x")],
        Some(json!({ "required_request_headers": "X-Tenant-Id, Authorization" })),
    );
    let error = plugin
        .guard_request(&mut ctx)
        .await
        .expect("the guard runs")
        .into_result()
        .expect_err("x-tenant-id is missing");
    assert_eq!(error.status_code(), 400);
    assert_eq!(error.gts_type(), VALIDATION_ERROR_TYPE);
    assert_eq!(
        error.detail(),
        "the required request header `x-tenant-id` is missing"
    );
}

#[tokio::test]
async fn the_required_headers_guard_is_case_insensitive_about_header_names() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut ctx = request(
        &[("x-tenant-id", "acme")],
        Some(json!({ "required_request_headers": "X-TENANT-ID" })),
    );
    assert!(
        plugin
            .guard_request(&mut ctx)
            .await
            .expect("the guard runs")
            .is_allow(),
        "HTTP header names are case-insensitive"
    );
}

#[tokio::test]
async fn an_unconfigured_required_headers_guard_fails_open() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut ctx = request(&[], None);
    assert!(
        plugin
            .guard_request(&mut ctx)
            .await
            .expect("the guard runs")
            .is_allow()
    );
    let mut ctx = request(&[], Some(json!({ "required_request_headers": "" })));
    assert!(
        plugin
            .guard_request(&mut ctx)
            .await
            .expect("the guard runs")
            .is_allow(),
        "a blank list enforces nothing"
    );
}

#[tokio::test]
async fn the_required_headers_guard_rejects_a_response_missing_one_of_them() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut upstream_response = response(
        &[],
        Some(json!({ "required_response_headers": "X-Request-ID" })),
    );
    let error = plugin
        .guard_response(&mut upstream_response)
        .await
        .expect("the guard runs")
        .into_result()
        .expect_err("x-request-id is missing");
    assert_eq!(error.status_code(), 502);
    assert_eq!(error.gts_type(), PROTOCOL_ERROR_TYPE);
}

#[tokio::test]
async fn the_required_headers_guard_admits_a_response_that_carries_them() {
    let plugin = RequiredHeadersGuardPlugin;
    let mut upstream_response = response(
        &[("x-request-id", "abc")],
        Some(json!({ "required_response_headers": "X-Request-ID" })),
    );
    assert!(
        plugin
            .guard_response(&mut upstream_response)
            .await
            .expect("the guard runs")
            .is_allow()
    );
}

// ── request_id transform ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_request_id_transform_gives_an_unmarked_request_one() {
    let plugin = RequestIdTransformPlugin;
    let mut ctx = request(&[], None);
    plugin
        .transform_request(&mut ctx)
        .await
        .expect("the transform runs");
    let generated = ctx
        .headers
        .get("x-request-id")
        .expect("a request id was injected")
        .to_str()
        .unwrap()
        .to_owned();
    Uuid::parse_str(&generated).expect("the generated id is a UUID");
}

#[tokio::test]
async fn the_request_id_transform_propagates_the_callers_id() {
    let plugin = RequestIdTransformPlugin;
    let mut ctx = request(&[("x-request-id", "caller-chose-this")], None);
    plugin
        .transform_request(&mut ctx)
        .await
        .expect("the transform runs");
    assert_eq!(
        ctx.headers.get("x-request-id").unwrap(),
        "caller-chose-this"
    );
}

#[tokio::test]
async fn the_request_id_transform_echoes_the_id_on_the_response() {
    let plugin = RequestIdTransformPlugin;
    let mut upstream_response = response(&[], None);
    upstream_response.request_id = Some("caller-chose-this".to_owned());
    plugin
        .transform_response(&mut upstream_response)
        .await
        .expect("the transform runs");
    assert_eq!(
        upstream_response.headers.get("x-request-id").unwrap(),
        "caller-chose-this"
    );
}

// ── Control-plane surface of the plugin model ────────────────────────────────

#[test]
fn a_plugin_spec_with_a_source_is_valid() {
    let spec = PluginSpec {
        id: None,
        plugin_ref: None,
        name: "mask-pii".to_owned(),
        kind: PluginKind::Guard,
        phases: vec![PluginPhase::Request],
        config: None,
        source: Some("def guard(context): pass".to_owned()),
    };
    spec.validate().expect("a stored plugin");
}

#[test]
fn a_named_plugin_reference_is_recognised_as_named() {
    assert!(crate::domain::model::is_named_plugin_id(&reference(
        PluginKind::Guard,
        "required_headers"
    )));
}
