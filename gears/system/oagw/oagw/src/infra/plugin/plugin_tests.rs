//! Unit tests for the built-in plugins.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use httpmock::prelude::{MockServer, POST};
use serde_json::json;
use uuid::Uuid;

use super::apikey::{ApiKeyAuthPlugin, DEFAULT_API_KEY_HEADER};
use super::guards::{RequestIdTransformPlugin, REQUEST_ID_HEADER, RequiredHeadersGuardPlugin};
use super::oauth2::{OAuth2ClientCredAuthPlugin, OAUTH2_BASIC_PLUGIN_ID, OAUTH2_FORM_PLUGIN_ID};
use super::secret::{LiteralSecretResolver, SecretResolver};
use super::{NoopAuthPlugin, PluginBundle, registry};
use crate::domain::plugin::builtins::{
    AUTH_API_KEY, AUTH_BASIC, AUTH_BEARER, AUTH_NOOP, AUTH_OAUTH2_CLIENT_CRED,
    AUTH_OAUTH2_CLIENT_CRED_BASIC, GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID,
};
use crate::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginRegistry, PluginPhase,
    RequestContext, ResponseContext, TransformPlugin,
};

const LONG_TTL: std::time::Duration = std::time::Duration::from_hours(1);

fn bundle(transport: Arc<crate::infra::transport::Transport>) -> PluginBundle {
    PluginBundle {
        secrets: Arc::new(LiteralSecretResolver),
        transport,
        token_cache_ttl: LONG_TTL,
        token_cache_capacity: 128,
    }
}

fn ctx(config: serde_json::Value) -> RequestContext {
    RequestContext {
        tenant_id: Uuid::new_v4(),
        subject_id: Uuid::new_v4(),
        path: "/v1/x".to_owned(),
        method: "GET".to_owned(),
        config,
        ..RequestContext::default()
    }
}

fn transport() -> Arc<crate::infra::transport::Transport> {
    Arc::new(
        crate::infra::transport::Transport::new(std::time::Duration::from_secs(5))
            .expect("transport must build"),
    )
}

fn lookup(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// Token endpoint stub counting the requests it received.
struct TokenServer {
    server: MockServer,
    calls: Arc<AtomicUsize>,
}

impl TokenServer {
    fn start(status: u16, body: &'static str) -> Self {
        let server = MockServer::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        server.mock(move |when, then| {
            when.method(POST).path("/token");
            then.respond_with(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                httpmock::HttpMockResponse::builder()
                    .status(status)
                    .body(body)
                    .build()
            });
        });
        Self { server, calls }
    }

    fn endpoint(&self) -> String {
        self.server.url("/token")
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn the_noop_plugin_injects_nothing() {
    let mut context = ctx(json!({}));
    NoopAuthPlugin.authenticate(&mut context).await.unwrap();
    assert!(context.outbound_headers.is_empty());
}

#[tokio::test]
async fn every_built_in_resolves_by_name_and_by_gts_id() {
    let plugins = registry(&bundle(transport()));
    for (name, gts) in [
        ("noop", AUTH_NOOP),
        ("apikey", AUTH_API_KEY),
        (OAUTH2_FORM_PLUGIN_ID, AUTH_OAUTH2_CLIENT_CRED),
        (OAUTH2_BASIC_PLUGIN_ID, AUTH_OAUTH2_CLIENT_CRED_BASIC),
        ("required_headers", GUARD_REQUIRED_HEADERS),
        ("request_id", TRANSFORM_REQUEST_ID),
    ] {
        assert!(plugins.resolves(name), "{name} must resolve");
        assert!(plugins.resolves(gts), "{gts} must resolve");
    }
    assert_eq!(
        plugins.auth("apikey").unwrap().plugin_type(),
        AUTH_API_KEY
    );
}

#[tokio::test]
async fn catalogue_only_auth_plugins_stay_unresolvable() {
    let plugins = registry(&bundle(transport()));
    assert!(plugins.auth("basic").is_none());
    assert!(plugins.auth("bearer").is_none());
    assert!(plugins.auth(AUTH_BASIC).is_none());
    assert!(plugins.auth(AUTH_BEARER).is_none());
}

#[tokio::test]
async fn an_api_key_is_injected_into_the_outbound_headers() {
    let mut context = ctx(json!({ "value": "sekrit" }));
    ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
        .authenticate(&mut context)
        .await
        .unwrap();
    assert_eq!(
        lookup(&context.outbound_headers, DEFAULT_API_KEY_HEADER).as_deref(),
        Some("sekrit")
    );
    assert!(lookup(&context.headers, DEFAULT_API_KEY_HEADER).is_none());
}

#[tokio::test]
async fn an_api_key_honours_a_custom_header_name() {
    let mut context = ctx(json!({ "name": "X-Vendor-Key", "key": "abc" }));
    ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
        .authenticate(&mut context)
        .await
        .unwrap();
    assert_eq!(
        lookup(&context.outbound_headers, "x-vendor-key"),
        Some("abc".to_owned())
    );
}

#[tokio::test]
async fn an_api_key_can_be_sent_as_a_query_parameter() {
    let mut context = ctx(json!({ "query": true, "param_name": "apikey", "value": "sekrit" }));
    ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
        .authenticate(&mut context)
        .await
        .unwrap();
    assert_eq!(
        lookup(&context.outbound_headers, crate::infra::transport::OUTBOUND_QUERY_HEADER),
        Some("apikey=sekrit".to_owned())
    );
}

#[tokio::test]
async fn a_credential_reference_fails_closed() {
    let mut context = ctx(json!({ "value": "cred://partner-openai-key" }));
    let error = ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
        .authenticate(&mut context)
        .await
        .unwrap_err();
    assert_eq!(error.status(), 500);
    assert!(context.outbound_headers.is_empty());
}

#[tokio::test]
async fn a_blank_credential_reference_is_rejected() {
    let mut context = ctx(json!({ "value": "   " }));
    let error = ApiKeyAuthPlugin::new(Arc::new(LiteralSecretResolver))
        .authenticate(&mut context)
        .await
        .unwrap_err();
    assert_eq!(error.status(), 500);
}

#[tokio::test]
async fn required_request_headers_reject_with_400() {
    let guard = RequiredHeadersGuardPlugin;
    let mut context = ctx(json!({ "required_request_headers": "X-Correlation-Id, accept" }));
    context.headers = vec![("accept".to_owned(), "text/plain".to_owned())];
    let decision = guard.guard_request(&context).await.unwrap();
    match decision {
        GuardDecision::Reject(error) => {
            assert_eq!(error.status(), 400);
            assert!(error.detail().contains("x-correlation-id"));
        }
        GuardDecision::Allow => panic!("a missing header must be reported"),
    }

    context
        .headers
        .push(("X-Correlation-Id".to_owned(), "1".to_owned()));
    assert!(guard.guard_request(&context).await.unwrap().allowed());
}

#[tokio::test]
async fn required_response_headers_reject_with_502() {
    let guard = RequiredHeadersGuardPlugin;
    let mut response = ResponseContext {
        status: 200,
        headers: vec![("x-other".to_owned(), "1".to_owned())],
        ..ResponseContext::default()
    };
    response.config = json!({ "required_response_headers": "content-type" });
    let decision = guard.guard_response(&response).await.unwrap();
    match decision {
        GuardDecision::Reject(error) => assert_eq!(error.status(), 502),
        GuardDecision::Allow => panic!("a missing header must be reported"),
    }

    response
        .headers
        .push(("Content-Type".to_owned(), "application/json".to_owned()));
    assert!(guard.guard_response(&response).await.unwrap().allowed());
}

#[tokio::test]
async fn an_unconfigured_required_headers_guard_fails_open() {
    let guard = RequiredHeadersGuardPlugin;
    assert!(guard.guard_request(&ctx(json!({}))).await.unwrap().allowed());
    assert!(guard
        .guard_response(&ResponseContext {
            status: 200,
            headers: Vec::new(),
            ..ResponseContext::default()
        })
        .await
        .unwrap()
        .allowed());
}

#[tokio::test]
async fn a_blank_required_headers_entry_is_a_no_op() {
    let guard = RequiredHeadersGuardPlugin;
    assert!(guard
        .guard_request(&ctx(json!({ "required_request_headers": " , ,, " })))
        .await
        .unwrap()
        .allowed());
}

#[tokio::test]
async fn the_request_id_transform_propagates_an_inbound_identifier() {
    let transform = RequestIdTransformPlugin;
    let mut context = ctx(json!({}));
    context.headers = vec![("X-Request-Id".to_owned(), "abc-123".to_owned())];
    transform.transform_request(&mut context).await.unwrap();
    assert_eq!(
        lookup(&context.outbound_headers, REQUEST_ID_HEADER),
        Some("abc-123".to_owned())
    );
}

#[tokio::test]
async fn the_request_id_transform_generates_one_when_absent() {
    let transform = RequestIdTransformPlugin;
    let mut context = ctx(json!({}));
    transform.transform_request(&mut context).await.unwrap();
    let generated = lookup(&context.outbound_headers, REQUEST_ID_HEADER).expect("generated id");
    assert!(!generated.is_empty());
}

#[tokio::test]
async fn the_request_id_transform_declares_both_phases() {
    assert_eq!(
        RequestIdTransformPlugin.phases(),
        vec![PluginPhase::OnRequest, PluginPhase::OnResponse]
    );
    let mut error = ErrorContext::default();
    RequestIdTransformPlugin
        .transform_error(&mut error)
        .await
        .unwrap();
    assert!(error.error.is_none());
}

#[tokio::test]
async fn an_oauth2_plugin_caches_the_token() {
    let server = TokenServer::start(
        200,
        r#"{"access_token":"tok","expires_in":3600,"token_type":"Bearer"}"#,
    );
    let plugin = OAuth2ClientCredAuthPlugin::form(
        Arc::new(LiteralSecretResolver),
        transport(),
        LONG_TTL,
        128,
    );
    let config = json!({
        "token_endpoint": server.endpoint(),
        "client_id_ref": "client-a",
        "client_secret_ref": "secret-a"
    });

    let mut first = ctx(config.clone());
    let mut second = first.clone();
    plugin.authenticate(&mut first).await.unwrap();
    assert_eq!(
        lookup(&first.outbound_headers, "authorization"),
        Some("Bearer tok".to_owned())
    );
    assert_eq!(server.calls(), 1, "the token must be fetched once");

    plugin.authenticate(&mut second).await.unwrap();
    assert_eq!(
        lookup(&second.outbound_headers, "authorization"),
        Some("Bearer tok".to_owned())
    );
    assert_eq!(server.calls(), 1, "the second request must be served from cache");
}

#[tokio::test]
async fn an_oauth2_plugin_isolates_tenants_in_the_cache() {
    let server = TokenServer::start(200, r#"{"access_token":"tok","expires_in":3600}"#);
    let plugin = OAuth2ClientCredAuthPlugin::form(
        Arc::new(LiteralSecretResolver),
        transport(),
        LONG_TTL,
        128,
    );
    let config = json!({
        "token_endpoint": server.endpoint(),
        "client_id_ref": "client-a",
        "client_secret_ref": "secret-a"
    });
    let mut first = ctx(config.clone());
    let mut second = ctx(config);
    second.tenant_id = Uuid::new_v4();
    plugin.authenticate(&mut first).await.unwrap();
    plugin.authenticate(&mut second).await.unwrap();
    // A different tenant must never be served the first tenant's token.
    assert_eq!(server.calls(), 2);
}

#[tokio::test]
async fn an_oauth2_plugin_does_not_cache_a_failed_fetch() {
    let server = TokenServer::start(500, "boom");
    let plugin = OAuth2ClientCredAuthPlugin::basic(
        Arc::new(LiteralSecretResolver),
        transport(),
        LONG_TTL,
        128,
    );
    let config = json!({
        "token_endpoint": server.endpoint(),
        "client_id_ref": "client-a",
        "client_secret_ref": "secret-a"
    });
    let mut context = ctx(config.clone());
    let error = plugin.authenticate(&mut context).await.unwrap_err();
    assert_eq!(error.status(), 502);
    assert!(lookup(&context.outbound_headers, "authorization").is_none());

    let mut again = ctx(config);
    let second = plugin.authenticate(&mut again).await.unwrap_err();
    assert_eq!(second.status(), 502);
    assert_eq!(server.calls(), 2, "a failed fetch must not be cached");
}

#[tokio::test]
async fn an_oauth2_plugin_requires_a_token_endpoint() {
    let plugin = OAuth2ClientCredAuthPlugin::form(
        Arc::new(LiteralSecretResolver),
        transport(),
        LONG_TTL,
        128,
    );
    let error = plugin
        .authenticate(&mut ctx(json!({ "client_id_ref": "a", "client_secret_ref": "b" })))
        .await
        .unwrap_err();
    assert_eq!(error.status(), 400);
}

#[tokio::test]
async fn an_oauth2_issuer_url_resolves_the_discovery_document() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST)
            .path("/.well-known/openid-configuration");
        then.status(200).body(r#"{"access_token":"tok"}"#);
    });
    let plugin = OAuth2ClientCredAuthPlugin::form(
        Arc::new(LiteralSecretResolver),
        transport(),
        LONG_TTL,
        128,
    );
    let mut context = ctx(json!({
        "issuer_url": format!("{}/", server.base_url()),
        "client_id_ref": "client-a",
        "client_secret_ref": "secret-a"
    }));
    plugin.authenticate(&mut context).await.unwrap();
    assert_eq!(
        lookup(&context.outbound_headers, "authorization"),
        Some("Bearer tok".to_owned())
    );
}

#[test]
fn the_config_hash_is_order_independent() {
    let first = json!({ "scopes": "a b", "token_endpoint": "https://idp/token" });
    let second = json!({ "token_endpoint": "https://idp/token", "scopes": "a b" });
    assert_eq!(
        super::oauth2::stable_config_hash(&first),
        super::oauth2::stable_config_hash(&second)
    );
    let third = json!({ "scopes": "b a", "token_endpoint": "https://idp/token" });
    assert_ne!(
        super::oauth2::stable_config_hash(&first),
        super::oauth2::stable_config_hash(&third)
    );
}

#[tokio::test]
async fn an_empty_registry_has_no_built_ins() {
    assert!(PluginRegistry::new().auth("noop").is_none());
}

#[tokio::test]
async fn the_secret_resolver_fails_closed_on_credential_references() {
    let resolver = LiteralSecretResolver;
    let literal = resolver
        .resolve(&RequestContext::default(), "plain-key")
        .await
        .unwrap();
    assert_eq!(literal, "plain-key");

    let error = resolver
        .resolve(&RequestContext::default(), "cred://partner-openai-key")
        .await
        .unwrap_err();
    assert_eq!(error.status(), 500);
}
