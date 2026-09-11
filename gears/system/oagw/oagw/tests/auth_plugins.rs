#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for the auth plugins, the rate limiting and the CORS
//! enforcement of the request pipeline (entry 2.5).
//!
//! In-crate integration tests only (DECOMPOSITION assumption 5): no e2e suite
//! is added under `testing/e2e/gears/oagw/`. The tests mount the router the way
//! `register_rest` does — a real `ChainExecutor` over a stub credstore — and
//! drive the proxy endpoint against a loopback stub upstream and a mock
//! identity provider the harness starts, asserting the `400`, `401`, `403`,
//! `429`, `500` and `503` outcomes, the injected credentials, the token-cache
//! reuse, the rate-limit and CORS headers and the absence of credential
//! material from every problem body.

// @cpt-begin:cpt-cf-oagw-dod-auth-test-coverage:p2:inst-full
use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use async_trait::async_trait;
use credstore_sdk::test_util::MockCredStoreClient;
use credstore_sdk::{
    CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef, SecretType, SharingMode,
    TenantId,
};
use http_body_util::BodyExt;
use httpmock::MockServer;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};
use oagw::api::rest::routes::{MOUNT_ROOT, register_routes_with_plugins};
use oagw::config::OagwConfig;
use oagw::domain::model::PROTOCOL_HTTP;
use oagw::domain::sharing::FlatHierarchy;
use oagw::infra::proxy::chain::ChainExecutor;
use oagw::infra::proxy::credentials::CredentialSource;
use oagw::infra::proxy::hooks::PluginChains;
use oagw::infra::storage::OagwStore;

const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000050");
const OTHER_TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000051");

/// The alias the loopback stub upstream is stored under.
const ALIAS: &str = "stub.internal";

/// The path the test routes match.
const ROUTE_PATH: &str = "/v1";

/// An address nothing in the test environment listens on.
const DEAD_PORT: u16 = 1;

/// Every scope a management caller may need.
const ALL: &[&str] = &["*"];

/// The API key secret the stub credstore resolves.
const API_KEY: &str = "sk-stub-3f91a";

/// The client credentials the stub credstore resolves.
const CLIENT_ID: &str = "svc-stub-client";
const CLIENT_SECRET: &str = "svc-stub-secret-77cd4";

/// The access token the mock identity provider mints.
const ACCESS_TOKEN: &str = "idp-minted-token-4b2e9";

/// The full identifier of the builtin auth plugins.
const APIKEY_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
const OAUTH2_FORM_PLUGIN: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
const OAUTH2_BASIC_PLUGIN: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// The full identifier of the builtin guard and transform plugins.
const REQUIRED_HEADERS_PLUGIN: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
const REQUEST_ID_PLUGIN: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// The path the mock identity provider serves its tokens on.
const TOKEN_PATH: &str = "/oauth/token";

/// Host OpenAPI registry double that records nothing.
#[derive(Default)]
struct NoopRegistry;

impl toolkit::api::OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ---------- harness ----------

/// The plaintext-opting configuration the stub upstream and the mock IdP need.
fn proxy_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// The mounted router over a fresh store, a real chain executor and a stub
/// credstore holding `secrets`.
///
/// This is the mount the gear registration performs, so the tests exercise the
/// very chain the running gear builds.
fn mounted(secrets: &[(&str, &str)]) -> Router {
    mounted_with_source(
        proxy_config(),
        CredentialSource::new(Arc::new(MockCredStoreClient::with_secrets(
            secrets
                .iter()
                .map(|(reference, value)| ((*reference).to_owned(), (*value).to_owned()))
                .collect(),
        ))),
    )
}

/// The mounted router over the given configuration and credential source.
fn mounted_with_source(config: OagwConfig, credentials: CredentialSource) -> Router {
    let chains: Arc<dyn PluginChains> =
        Arc::new(ChainExecutor::new(&config, credentials).expect("the chain builds"));
    register_routes_with_plugins(
        Router::new(),
        &NoopRegistry,
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
        &config,
        Some(chains),
    )
    .expect("the router mounts")
}

/// The subject identifier the token-cache tests share, so two requests of one
/// caller resolve the same cache entry.
const SUBJECT: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000000a1");

/// A security context the host api-gateway would inject for `tenant`.
fn context(tenant: Uuid) -> SecurityContext {
    context_for(tenant, Uuid::new_v4())
}

/// A security context for `tenant` with a fixed `subject`.
fn context_for(tenant: Uuid, subject: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_type("user")
        .subject_tenant_id(tenant)
        .token_scopes(ALL.iter().map(|scope| (*scope).to_owned()).collect())
        .build()
        .expect("context builds")
}

/// Send a request with the given headers and body.
async fn call(
    router: Router,
    method: &str,
    uri: &str,
    caller: Option<SecurityContext>,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> axum::response::Response {
    let method = axum::http::Method::from_bytes(method.as_bytes()).expect("method");
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(caller) = caller {
        builder = builder.extension(caller);
    }
    router
        .oneshot(
            builder
                .body(Body::from(body.unwrap_or("").to_owned()))
                .expect("request builds"),
        )
        .await
        .expect("request serves")
}

/// Send an authenticated proxy request without extra headers.
async fn proxy(router: Router, uri: &str) -> axum::response::Response {
    call(router, "GET", uri, Some(context(TENANT)), &[], None).await
}

/// Send an authenticated proxy request for `subject`.
async fn proxy_as(
    router: Router,
    uri: &str,
    tenant: Uuid,
    subject: Uuid,
) -> axum::response::Response {
    call(router, "GET", uri, Some(context_for(tenant, subject)), &[], None).await
}

/// The whole body of a response as a string.
async fn text(response: axum::response::Response) -> String {
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("body is utf-8")
}

/// The whole body of a response as a JSON document.
async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_str(&text(response).await).expect("body is a JSON document")
}

/// The value of a response header.
fn header_of(response: &axum::response::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The value of a header map member.
fn headers_of(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Assert the canonical problem contract of a gateway failure.
///
/// The response is consumed, so a test reads its headers before it calls this.
async fn assert_problem(
    response: axum::response::Response,
    status: u16,
    type_suffix: &str,
    instance: &str,
) -> Value {
    let (parts, body) = response.into_parts();
    let bytes = Body::new(body)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let document: Value = serde_json::from_slice(&bytes).expect("body is a JSON document");
    assert_eq!(
        parts.status,
        StatusCode::from_u16(status).expect("status"),
        "{document}"
    );
    assert_eq!(
        headers_of(&parts.headers, header::CONTENT_TYPE.as_str()).as_deref(),
        Some("application/problem+json"),
        "{status} carries the problem content type"
    );
    assert_eq!(
        headers_of(&parts.headers, ERROR_SOURCE_HEADER).as_deref(),
        Some(ERROR_SOURCE_GATEWAY),
        "{status} is classified as gateway-originated"
    );
    assert_eq!(
        document["type"],
        json!(format!("gts://gts.cf.core.errors.err.v1~cf.oagw.{type_suffix}")),
        "{document}"
    );
    assert_eq!(document["status"], json!(status), "{document}");
    assert_eq!(document["instance"], json!(instance), "{document}");
    document
}

/// The body of a plaintext stub upstream stored as `alias`.
fn stub_upstream(alias: Option<&str>, port: u16) -> Value {
    // `passthrough: all` is declared, because the schema default of
    // `headers.request.passthrough` is `none`: a gateway that declares no
    // header disposition forwards no client header at all.
    let mut body = json!({
        "server": {
            "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ]
        },
        "protocol": PROTOCOL_HTTP,
        "headers": { "request": { "passthrough": "all" } }
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    body
}

/// Create an upstream through the management API and return its record.
async fn create_upstream_for(router: &Router, tenant: Uuid, body: Value) -> Value {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(tenant)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        text(response).await
    );
    json_body(response).await
}

/// Create a route through the management API.
async fn create_route_for(router: &Router, tenant: Uuid, body: Value) {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/routes"),
        Some(context(tenant)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        text(response).await
    );
}

/// A route body matching every forwarded method on `path`.
fn route_body(upstream: Uuid, path: &str) -> Value {
    json!({
        "upstream_id": upstream,
        "match": { "http": {
            "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
            "path": path
        } }
    })
}

/// Store the upstream and a route matching every method on `path`, and return
/// the proxy path the route serves.
async fn seed(router: &Router, tenant: Uuid, upstream: Value, path: &str) -> String {
    let stored = create_upstream_for(router, tenant, upstream).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route_for(router, tenant, route_body(upstream, path)).await;
    let alias = stored["alias"].as_str().expect("alias").to_owned();
    format!("{MOUNT_ROOT}/proxy/{alias}{path}")
}

/// The proxy path of the stub upstream and its route.
async fn stubbed(router: &Router, server: &MockServer) -> String {
    seed(
        router,
        TENANT,
        stub_upstream(Some(ALIAS), server.port()),
        ROUTE_PATH,
    )
    .await
}

/// The upstream record `auth` describes, pointing at the loopback stub.
fn auth_upstream(port: u16, auth: Value) -> Value {
    let mut body = stub_upstream(Some(ALIAS), port);
    body["auth"] = auth;
    body
}

/// The `auth` declaration of the API key plugin.
fn apikey_auth() -> Value {
    json!({
        "type": APIKEY_PLUGIN,
        "config": { "secret_ref": "cred://partner-api-key" }
    })
}

/// The `auth` declaration of the Form client-credentials variant.
fn oauth2_form_auth(port: u16) -> Value {
    json!({
        "type": OAUTH2_FORM_PLUGIN,
        "config": {
            "client_id_ref": "cred://partner-client-id",
            "client_secret_ref": "cred://partner-client-secret",
            "token_endpoint": format!("http://127.0.0.1:{port}{TOKEN_PATH}"),
            "scopes": "read write"
        }
    })
}

/// The client credentials the client-credentials plugins resolve.
fn oauth2_secrets() -> Vec<(&'static str, &'static str)> {
    vec![
        ("partner-client-id", CLIENT_ID),
        ("partner-client-secret", CLIENT_SECRET),
    ]
}

/// The plain stub upstream every request reaches, whatever it carries.
async fn open_stub<'a>(server: &'a MockServer) -> httpmock::Mock<'a> {
    server
        .mock_async(|when, then| {
            when.method("GET").path(ROUTE_PATH);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await
}

/// The stub upstream that only answers a request carrying `header: value`, so
/// the mock's hit count is the proof that the credential was injected.
async fn credential_stub<'a>(
    server: &'a MockServer,
    header: &'a str,
    value: &'a str,
) -> httpmock::Mock<'a> {
    server
        .mock_async(move |when, then| {
            when.method("GET").path(ROUTE_PATH).header(header, value);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await
}

/// The stub upstream that only answers a request whose query carries
/// `name: value`.
async fn query_stub<'a>(
    server: &'a MockServer,
    name: &'a str,
    value: &'a str,
) -> httpmock::Mock<'a> {
    server
        .mock_async(move |when, then| {
            when.method("GET")
                .path(ROUTE_PATH)
                .query_param(name, value);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await
}

/// The stub upstream whose response carries `x-contract` when asked to.
async fn contract_stub<'a>(server: &'a MockServer, response_header: bool) -> httpmock::Mock<'a> {
    server
        .mock_async(move |when, mut then| {
            when.method("GET").path(ROUTE_PATH);
            if response_header {
                then = then.header("x-contract", "present");
            }
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await
}

/// The mock identity provider: a token endpoint minting one access token.
async fn token_stub<'a>(server: &'a MockServer, expires_in: i64) -> httpmock::Mock<'a> {
    server
        .mock_async(move |when, then| {
            when.method("POST").path(TOKEN_PATH);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "access_token": ACCESS_TOKEN,
                    "token_type": "Bearer",
                    "expires_in": expires_in
                }));
        })
        .await
}

/// The mock identity provider that refuses the grant.
async fn refusing_token_stub<'a>(server: &'a MockServer) -> httpmock::Mock<'a> {
    server
        .mock_async(|when, then| {
            when.method("POST").path(TOKEN_PATH);
            then.status(401)
                .header("content-type", "application/json")
                .json_body(json!({"error": "invalid_client"}));
        })
        .await
}

// ---------- plugin resolution ----------

/// A binding naming a catalog-only auth plugin is refused at the write path:
/// the management validator admits no identifier the runtime would refuse, so
/// the `503` the chain maps is the defence a stored configuration can never
/// reach through this router (the chain unit test covers it directly).
#[tokio::test]
async fn a_catalog_only_plugin_binding_is_refused_at_the_write_path() {
    let server = MockServer::start();
    let router = mounted(&[]);

    let response = call(
        router,
        "POST",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(TENANT)),
        &[("content-type", "application/json")],
        Some(
            &json!({
                "alias": ALIAS,
                "server": {
                    "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server.port() } ]
                },
                "protocol": PROTOCOL_HTTP,
                "plugins": {
                    "items": [ {
                        "plugin_ref": "gts.cf.core.oagw.plugin.v1~cf.core.oagw.basic.v1"
                    } ]
                }
            })
            .to_string(),
        ),
    )
    .await;
    let document = assert_problem(
        response,
        400,
        "validation.error.v1",
        &format!("{MOUNT_ROOT}/upstreams"),
    )
    .await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("basic"),
        "the refusal names the identifier"
    );
}

/// A UUID-backed custom Starlark definition has no runtime instance either,
/// because no interpreter exists in this build: the same `503`.
#[tokio::test]
async fn a_custom_plugin_binding_is_a_plugin_not_found() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ { "plugin_ref": Uuid::new_v4().to_string() } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = proxy(router, &path).await;
    assert_problem(response, 503, "plugin.not_found.v1", &path).await;
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// An upstream that binds no plugin at all runs an empty chain: the request is
/// forwarded with no injected credential.
#[tokio::test]
async fn an_upstream_without_plugins_is_forwarded_unchanged() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = stubbed(&router, &server).await;
    let upstream = open_stub(&server).await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(upstream.calls(), 1, "the request was forwarded");
}

// ---------- auth plugin credential injection ----------

/// The API key plugin resolves its reference and injects the secret into the
/// configured header, `X-API-Key` when the config names none.
#[tokio::test]
async fn an_api_key_is_injected_into_the_default_request_header() {
    let server = MockServer::start();
    let router = mounted(&[("partner-api-key", API_KEY)]);
    let path = seed(
        &router,
        TENANT,
        auth_upstream(server.port(), apikey_auth()),
        ROUTE_PATH,
    )
    .await;
    let upstream = credential_stub(&server, "x-api-key", API_KEY).await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(upstream.calls(), 1, "the credential reached the upstream");
}

/// The API key plugin injects into the `api_key` query parameter when the
/// config selects query injection.
#[tokio::test]
async fn an_api_key_is_injected_into_the_query_parameter() {
    let server = MockServer::start();
    let router = mounted(&[("partner-api-key", API_KEY)]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            // The auth configuration carries the injection target beside the
            // reference: `auth.config` holds every pair the plugin reads.
            let mut auth = apikey_auth();
            auth["config"]["target"] = json!("query");
            body["auth"] = auth;
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = query_stub(&server, "api_key", API_KEY).await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(upstream.calls(), 1, "the credential reached the query string");
}

/// The client-credentials Form variant injects `Authorization: Bearer <token>`
/// on the proxied request.
#[tokio::test]
async fn a_client_credentials_token_is_injected_as_a_bearer_token() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    let token = token_stub(&idp, 300).await;
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let bearer = format!("Bearer {ACCESS_TOKEN}");
    let upstream = credential_stub(&upstream_server, "authorization", &bearer).await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(upstream.calls(), 1, "the bearer token reached the upstream");
    assert_eq!(token.calls(), 1, "the IdP minted one token");
}

/// The Basic variant authenticates to the token endpoint with
/// `Authorization: Basic` carrying the base64 encoding of
/// `client_id:client_secret`.
#[tokio::test]
async fn the_basic_variant_authenticates_with_a_basic_header() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    // `svc-stub-client:svc-stub-secret-77cd4` in base64.
    let token = idp
        .mock_async(|when, then| {
            when.method("POST")
                .path(TOKEN_PATH)
                .header(
                    "authorization",
                    "Basic c3ZjLXN0dWItY2xpZW50OnN2Yy1zdHViLXNlY3JldC03N2NkNA==",
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "access_token": ACCESS_TOKEN,
                    "token_type": "Bearer",
                    "expires_in": 300
                }));
        })
        .await;
    let router = mounted(&oauth2_secrets());
    let mut auth = oauth2_form_auth(idp.port());
    auth["type"] = json!(OAUTH2_BASIC_PLUGIN);
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), auth),
        ROUTE_PATH,
    )
    .await;
    let bearer = format!("Bearer {ACCESS_TOKEN}");
    let upstream = credential_stub(&upstream_server, "authorization", &bearer).await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(token.calls(), 1, "the IdP accepted the basic credentials");
    assert_eq!(upstream.calls(), 1, "the bearer token reached the upstream");
}

// ---------- credential resolution failures ----------

/// An unknown `cred://` reference is a `500` SecretNotFound that names the
/// configuration key only, with no upstream call.
#[tokio::test]
async fn an_unresolvable_reference_is_a_secret_not_found() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        auth_upstream(server.port(), apikey_auth()),
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = proxy(router, &path).await;
    let document = assert_problem(response, 500, "secret.not_found.v1", &path).await;
    assert_eq!(
        document["detail"], json!("the credential reference of `secret_ref` cannot be resolved"),
        "the detail names the configuration key only"
    );
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// A reference the calling tenant does not own is indistinguishable from an
/// unknown one: the same `500` with the same detail.
#[tokio::test]
async fn a_cross_tenant_reference_is_a_secret_not_found() {
    let server = MockServer::start();
    // The store holds the reference for `TENANT` only, and the caller is
    // `OTHER_TENANT`, which holds the same upstream shape under its own alias.
    let credentials = CredentialSource::new(Arc::new(TenantScopedCredStore::with_reference(
        TENANT,
        "partner-api-key",
        API_KEY,
    )));
    let router = mounted_with_source(proxy_config(), credentials);
    let path = seed(
        &router,
        OTHER_TENANT,
        auth_upstream(server.port(), apikey_auth()),
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = call(router, "GET", &path, Some(context(OTHER_TENANT)), &[], None).await;
    let document = assert_problem(response, 500, "secret.not_found.v1", &path).await;
    assert_eq!(
        document["detail"], json!("the credential reference of `secret_ref` cannot be resolved"),
        "the resolution is scoped to the calling tenant"
    );
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// A store that cannot serve the reference at all is the same outcome as an
/// unknown one: `500` SecretNotFound naming the configuration key.
#[tokio::test]
async fn a_failing_cred_store_is_a_secret_not_found() {
    let server = MockServer::start();
    let router = mounted_with_source(
        proxy_config(),
        CredentialSource::new(Arc::new(MockCredStoreClient::always_failing())),
    );
    let path = seed(
        &router,
        TENANT,
        auth_upstream(server.port(), apikey_auth()),
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = proxy(router, &path).await;
    let document = assert_problem(response, 500, "secret.not_found.v1", &path).await;
    assert_eq!(
        document["detail"], json!("the credential reference of `secret_ref` cannot be resolved")
    );
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// A credstore double that resolves only the references the calling tenant
/// owns, so a shared reference is admitted and a foreign one is not.
#[derive(Default)]
struct TenantScopedCredStore {
    /// `(owner tenant, reference, value)` triples the store holds.
    store: Vec<(Uuid, String, String)>,
}

impl TenantScopedCredStore {
    /// A store holding `reference` for `owner` only.
    fn with_reference(owner: Uuid, reference: &str, value: &str) -> Self {
        Self {
            store: vec![(owner, reference.to_owned(), value.to_owned())],
        }
    }
}

#[async_trait]
impl CredStoreClientV1 for TenantScopedCredStore {
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        let tenant = ctx.subject_tenant_id();
        let found = self
            .store
            .iter()
            .find(|(owner, stored, _)| owner == &tenant && stored == key.as_ref());
        Ok(found.map(|(_, _, value)| GetSecretResponse {
                value: credstore_sdk::SecretValue::new(value.clone().into_bytes()),
                id: uuid::Uuid::nil(),
                owner_tenant_id: TenantId(tenant),
                sharing: SharingMode::default(),
                is_inherited: false,
                version: 1,
                secret_type: SecretType::generic().gts_id().to_owned(),
                expires_at: None,
            }))
    }
}

/// A reference the caller's tenant owns resolves, and one another tenant owns
/// does not: the isolation the credential store contract records.
#[tokio::test]
async fn a_tenant_scoped_store_resolves_only_its_own_reference() {
    let server = MockServer::start();
    let credentials = CredentialSource::new(Arc::new(TenantScopedCredStore::with_reference(
        TENANT,
        "partner-api-key",
        API_KEY,
    )));
    let router = mounted_with_source(proxy_config(), credentials);
    let path = seed(
        &router,
        TENANT,
        auth_upstream(server.port(), apikey_auth()),
        ROUTE_PATH,
    )
    .await;
    // The same alias, held by the other tenant, so the second caller resolves a
    // record of its own chain.
    let other = seed(
        &router,
        OTHER_TENANT,
        auth_upstream(server.port(), apikey_auth()),
        ROUTE_PATH,
    )
    .await;
    let upstream = credential_stub(&server, "x-api-key", API_KEY).await;

    let own = proxy(router.clone(), &path).await;
    assert_eq!(own.status(), StatusCode::OK, "{}", text(own).await);
    let foreign = call(
        router,
        "GET",
        &other,
        Some(context(OTHER_TENANT)),
        &[],
        None,
    )
    .await;
    assert_eq!(
        foreign.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a foreign reference resolves to nothing"
    );
    assert_eq!(upstream.calls(), 1, "only the owning tenant forwarded a request");
}

// ---------- identity provider failures ----------

/// An identity provider that refuses the grant is a `401`, not a `500`.
#[tokio::test]
async fn a_refusing_identity_provider_is_an_authentication_failed() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    let token = refusing_token_stub(&idp).await;
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&upstream_server).await;

    let response = proxy(router, &path).await;
    let document = assert_problem(response, 401, "auth.failed.v1", &path).await;
    assert!(
        !document["detail"].as_str().expect("detail").contains(CLIENT_SECRET),
        "the refusal names no credential material"
    );
    assert_eq!(token.calls(), 1, "the IdP was asked once");
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// An unreachable identity provider is the same `401` row as a refusal.
#[tokio::test]
async fn an_unreachable_identity_provider_is_an_authentication_failed() {
    let upstream_server = MockServer::start();
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(DEAD_PORT)),
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&upstream_server).await;

    let response = proxy(router, &path).await;
    assert_problem(response, 401, "auth.failed.v1", &path).await;
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

// ---------- token cache ----------

/// A second request with the same tenant, subject and configuration is served
/// from the token cache: no second IdP call, the same token forwarded.
#[tokio::test]
async fn a_second_request_is_served_from_the_token_cache() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    let token = token_stub(&idp, 300).await;
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let bearer = format!("Bearer {ACCESS_TOKEN}");
    let upstream = credential_stub(&upstream_server, "authorization", &bearer).await;

    let first = proxy_as(router.clone(), &path, TENANT, SUBJECT).await;
    let second = proxy_as(router, &path, TENANT, SUBJECT).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(upstream.calls(), 2, "both requests were forwarded");
    assert_eq!(token.calls(), 1, "the second request reused the cached token");
}

/// A different tenant never reuses another tenant's token: the IdP is asked
/// again for the second caller.
#[tokio::test]
async fn another_tenant_does_not_reuse_a_cached_token() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    let token = token_stub(&idp, 300).await;
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let other = seed(
        &router,
        OTHER_TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let bearer = format!("Bearer {ACCESS_TOKEN}");
    let upstream = credential_stub(&upstream_server, "authorization", &bearer).await;

    let first = proxy(router.clone(), &path).await;
    let second = call(
        router,
        "GET",
        &other,
        Some(context(OTHER_TENANT)),
        &[],
        None,
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(token.calls(), 2, "each tenant resolved its own token");
    assert_eq!(upstream.calls(), 2, "both requests were forwarded");
}

/// A token whose lifetime leaves nothing of it is injected for its own request
/// and not cached: the next request asks the IdP again.
#[tokio::test]
async fn a_short_lived_token_is_injected_but_never_cached() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    let token = token_stub(&idp, 20).await;
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let bearer = format!("Bearer {ACCESS_TOKEN}");
    let upstream = credential_stub(&upstream_server, "authorization", &bearer).await;

    let first = proxy(router.clone(), &path).await;
    let second = proxy(router, &path).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(upstream.calls(), 2, "both requests were forwarded");
    assert_eq!(token.calls(), 2, "no short-lived token was cached");
}

/// A failed token fetch is not cached: the next request retries the IdP.
#[tokio::test]
async fn a_failed_token_fetch_is_not_cached() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    let token = refusing_token_stub(&idp).await;
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let _upstream = open_stub(&upstream_server).await;

    let first = proxy(router.clone(), &path).await;
    let second = proxy(router, &path).await;
    assert_eq!(first.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(token.calls(), 2, "each request asked the IdP again");
}

// ---------- required headers and request identifier ----------

/// A missing required request header is a `400` carrying the canonical error
/// code, naming the first missing header only, with no upstream call.
#[tokio::test]
async fn a_missing_required_request_header_is_a_400_with_the_error_code() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ {
                    "plugin_ref": REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_request_headers": "x-contract, x-signature" }
                } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = proxy(router, &path).await;
    let document = assert_problem(response, 400, "validation.error.v1", &path).await;
    assert_eq!(
        document["error_code"], json!("REQUIRED_HEADER_MISSING"),
        "the problem body carries the guard's error code"
    );
    let detail = document["detail"].as_str().expect("detail");
    assert!(detail.contains("x-contract"), "the first missing header is named");
    assert!(
        !detail.contains("x-signature"),
        "only the first missing header is named"
    );
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// A request carrying every required header is forwarded.
#[tokio::test]
async fn a_request_carrying_the_required_headers_is_forwarded() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ {
                    "plugin_ref": REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_request_headers": "x-contract" }
                } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT)),
        &[("x-contract", "present")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(upstream.calls(), 1, "the request was forwarded");
}

/// A missing required response header is a `502` carrying the same error code,
/// with the upstream body discarded.
#[tokio::test]
async fn a_missing_required_response_header_is_a_502() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ {
                    "plugin_ref": REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_response_headers": "x-contract" }
                } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let _upstream = contract_stub(&server, false).await;

    let response = proxy(router, &path).await;
    assert_eq!(
        header_of(&response, "x-contract"),
        None,
        "the upstream body was discarded with the response"
    );
    let document = assert_problem(response, 502, "downstream.error.v1", &path).await;
    assert_eq!(
        document["error_code"], json!("REQUIRED_HEADER_MISSING"),
        "the response-phase guard carries the same error code"
    );
}

/// A response carrying the required header is passed through unchanged.
#[tokio::test]
async fn a_response_carrying_the_required_header_is_passed_through() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ {
                    "plugin_ref": REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_response_headers": "x-contract" }
                } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let _upstream = contract_stub(&server, true).await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, "x-contract").as_deref(), Some("present"));
}

/// An absent or blank RequiredHeaders configuration no-ops for its phase.
#[tokio::test]
async fn a_blank_required_headers_configuration_is_a_no_op() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ {
                    "plugin_ref": REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_request_headers": "  " }
                } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(upstream.calls(), 1, "the request was forwarded");
}

/// An inbound `X-Request-ID` is forwarded unchanged and echoed on the response.
#[tokio::test]
async fn an_inbound_request_id_is_propagated_and_echoed() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ { "plugin_ref": REQUEST_ID_PLUGIN } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = credential_stub(&server, "x-request-id", "req-from-caller").await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT)),
        &[("x-request-id", "req-from-caller")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(
        header_of(&response, "x-request-id").as_deref(),
        Some("req-from-caller"),
        "the response carries the identifier the request carried"
    );
    assert_eq!(upstream.calls(), 1, "the identifier was forwarded");
}

/// An absent `X-Request-ID` is generated, and the response carries it.
#[tokio::test]
async fn an_absent_request_id_is_generated() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["plugins"] = json!({
                "items": [ { "plugin_ref": REQUEST_ID_PLUGIN } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = server
        .mock_async(|when, then| {
            when.method("GET")
                .path(ROUTE_PATH)
                .header_exists("x-request-id");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await;

    let response = proxy(router, &path).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    let identifier = header_of(&response, "x-request-id").expect("the response carries one");
    assert!(
        Uuid::parse_str(&identifier).is_ok(),
        "`{identifier}` is a generated identifier"
    );
    assert_eq!(upstream.calls(), 1, "the generated identifier was forwarded");
}

// ---------- execution order ----------

/// The auth stage and the guard stage both apply to one request: the guard
/// requires a header the caller sent, the auth plugin injects its credential,
/// and the upstream sees both. The chain records the order the stages ran in
/// (`infra::proxy::chain::tests::the_stages_run_in_order_and_are_recorded`).
#[tokio::test]
async fn the_auth_stage_and_the_guard_stage_both_apply() {
    let server = MockServer::start();
    let router = mounted(&[("partner-api-key", API_KEY)]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = auth_upstream(server.port(), apikey_auth());
            body["plugins"] = json!({
                "items": [ {
                    "plugin_ref": REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_request_headers": "x-proof" }
                } ]
            });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = credential_stub(&server, "x-api-key", API_KEY).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT)),
        &[("x-proof", "carried")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(upstream.calls(), 1, "both stages admitted the request");
}

// ---------- rate limiting ----------

/// The rate limit configuration a layer declares.
fn rate_limit(strategy: &str, capacity: u64) -> Value {
    json!({
        "sustained": { "rate": 1, "window": "second" },
        "burst": { "capacity": capacity },
        "scope": "tenant",
        "strategy": strategy
    })
}

/// An exhausted bucket is rejected with `429` and its headers, before any
/// upstream call.
#[tokio::test]
async fn an_exhausted_bucket_is_rejected_with_429_and_its_headers() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["rate_limit"] = rate_limit("reject", 1);
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let first = proxy(router.clone(), &path).await;
    assert_eq!(first.status(), StatusCode::OK, "{}", text(first).await);
    let second = proxy(router, &path).await;
    assert!(
        header_of(&second, "retry-after").is_some(),
        "the rejection carries a retry delay"
    );
    assert_eq!(header_of(&second, "x-ratelimit-limit"), Some("1".to_owned()));
    assert_eq!(
        header_of(&second, "x-ratelimit-remaining"),
        Some("0".to_owned())
    );
    assert!(
        header_of(&second, "x-ratelimit-reset").is_some(),
        "the reset carries the seconds left in the window"
    );
    assert_problem(second, 429, "rate_limit.exceeded.v1", &path).await;
    assert_eq!(upstream.calls(), 1, "the throttled request never left");
}

/// A route-level limit stricter than the upstream-level limit governs.
#[tokio::test]
async fn a_route_level_limit_stricter_than_the_upstream_limit_governs() {
    let server = MockServer::start();
    let router = mounted(&[]);
    // The upstream admits 1000 requests a second; the route admits 1, so the
    // route is the stricter bound of the two.
    let stored = create_upstream_for(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["rate_limit"] = json!({
                "sustained": { "rate": 1000, "window": "second" },
                "scope": "tenant",
                "strategy": "reject"
            });
            body
        },
    )
    .await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route_for(
        &router,
        TENANT,
        {
            let mut body = route_body(upstream, ROUTE_PATH);
            body["rate_limit"] = rate_limit("reject", 1);
            body
        },
    )
    .await;
    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}");
    let stub = open_stub(&server).await;

    let first = proxy(router.clone(), &path).await;
    assert_eq!(first.status(), StatusCode::OK, "{}", text(first).await);
    let second = proxy(router, &path).await;
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the route-level limit governed: {}",
        text(second).await
    );
    assert_eq!(stub.calls(), 1, "one request left");
}

/// Under `strategy: degrade` an exhausted bucket admits the request without
/// consuming tokens, so a burst of two is served in full.
#[tokio::test]
async fn a_degraded_request_is_admitted_without_consuming() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["rate_limit"] = rate_limit("degrade", 1);
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let first = proxy(router.clone(), &path).await;
    let second = proxy(router, &path).await;
    assert_eq!(first.status(), StatusCode::OK, "{}", text(first).await);
    assert_eq!(second.status(), StatusCode::OK, "{}", text(second).await);
    assert_eq!(upstream.calls(), 2, "both requests were forwarded");
}

/// An upstream that declares no rate limit consumes nothing: a burst of three
/// is forwarded in full.
#[tokio::test]
async fn an_upstream_without_a_rate_limit_forwards_every_request() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = stubbed(&router, &server).await;
    let upstream = open_stub(&server).await;

    for _ in 0..3 {
        let response = proxy(router.clone(), &path).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(upstream.calls(), 3, "no bucket was consumed");
}

// ---------- CORS ----------

/// The CORS configuration an upstream declares.
fn cors_upstream(port: u16, allow_credentials: bool) -> Value {
    let mut body = stub_upstream(Some(ALIAS), port);
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET", "POST"],
        "expose_headers": ["x-upstream"],
        "allow_credentials": allow_credentials
    });
    body
}

/// An actual cross-origin request with a disallowed origin is refused with
/// `403` before any upstream call.
#[tokio::test]
async fn a_disallowed_origin_is_refused_with_403_before_the_upstream_call() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(&router, TENANT, cors_upstream(server.port(), false), ROUTE_PATH).await;
    let upstream = open_stub(&server).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT)),
        &[("origin", "https://app.example.com:8443")],
        None,
    )
    .await;
    let document = assert_problem(response, 403, "cors.origin_not_allowed.v1", &path).await;
    let detail = document["detail"].as_str().expect("detail");
    assert!(
        detail.contains("app.example.com"),
        "the refusal names the rejected origin"
    );
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// An actual cross-origin request with a disallowed method is refused with the
/// `403` of the method row, before any upstream call.
#[tokio::test]
async fn a_disallowed_cors_method_is_refused_with_403_before_the_upstream_call() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(&router, TENANT, cors_upstream(server.port(), false), ROUTE_PATH).await;
    let upstream = open_stub(&server).await;

    let response = call(
        router,
        "DELETE",
        &path,
        Some(context(TENANT)),
        &[("origin", "https://app.example.com")],
        None,
    )
    .await;
    assert_problem(response, 403, "cors.method_not_allowed.v1", &path).await;
    assert_eq!(upstream.calls(), 0, "no upstream call is made");
}

/// An allowed cross-origin request is forwarded and its response carries the
/// CORS headers the decision describes.
#[tokio::test]
async fn an_allowed_cross_origin_request_carries_the_cors_headers() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(&router, TENANT, cors_upstream(server.port(), true), ROUTE_PATH).await;
    let upstream = open_stub(&server).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT)),
        &[("origin", "https://app.example.com")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(
        header_of(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com"),
        "the origin is echoed"
    );
    assert_eq!(
        header_of(&response, "access-control-allow-credentials").as_deref(),
        Some("true"),
        "the credentials flag is carried for an exact origin"
    );
    assert!(
        header_of(&response, "access-control-expose-headers").is_some(),
        "the exposed headers are carried"
    );
    assert_eq!(upstream.calls(), 1, "the request was forwarded");
}

/// An origin header carried while CORS is disabled is forwarded with no CORS
/// check and no CORS header.
#[tokio::test]
async fn an_origin_under_a_disabled_cors_configuration_is_forwarded() {
    let server = MockServer::start();
    let router = mounted(&[]);
    let path = seed(
        &router,
        TENANT,
        {
            let mut body = stub_upstream(Some(ALIAS), server.port());
            body["cors"] = json!({ "enabled": false });
            body
        },
        ROUTE_PATH,
    )
    .await;
    let upstream = open_stub(&server).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT)),
        &[("origin", "https://app.example.com")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(response).await);
    assert_eq!(header_of(&response, "access-control-allow-origin"), None);
    assert_eq!(upstream.calls(), 1, "the request was forwarded");
}

// ---------- credential isolation ----------

/// A problem body never carries credential material: the refusal of a
/// client-credentials upstream names neither the client secret, nor the client
/// identifier, nor a minted token.
#[tokio::test]
async fn no_credential_material_reaches_a_problem_body() {
    let upstream_server = MockServer::start();
    let idp = MockServer::start();
    let _token = refusing_token_stub(&idp).await;
    let router = mounted(&oauth2_secrets());
    let path = seed(
        &router,
        TENANT,
        auth_upstream(upstream_server.port(), oauth2_form_auth(idp.port())),
        ROUTE_PATH,
    )
    .await;
    let _upstream = open_stub(&upstream_server).await;

    let response = proxy(router, &path).await;
    let body = text(response).await;
    assert!(!body.contains(CLIENT_SECRET), "no secret material in the body");
    assert!(!body.contains(CLIENT_ID), "no credential identifier in the body");
    assert!(!body.contains(ACCESS_TOKEN), "no token material in the body");
}

/// The stored configuration carries references only: a management read of the
/// upstream the auth plugin consumes shows the `cred://` reference and no
/// resolved value.
#[tokio::test]
async fn the_stored_configuration_carries_references_only() {
    let server = MockServer::start();
    let router = mounted(&[("partner-api-key", API_KEY)]);
    let path = seed(
        &router,
        TENANT,
        auth_upstream(server.port(), apikey_auth()),
        ROUTE_PATH,
    )
    .await;

    let response = call(
        router,
        "GET",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(TENANT)),
        &[],
        None,
    )
    .await;
    let body = text(response).await;
    assert!(
        body.contains("cred://partner-api-key"),
        "the reference is stored: {body}"
    );
    assert!(!body.contains(API_KEY), "no secret value is stored");
    assert!(proxy_mount(&path), "the proxy path is the route's");
}

/// Whether `path` is the proxy path of the mounted router.
fn proxy_mount(path: &str) -> bool {
    path.starts_with(&format!("{MOUNT_ROOT}/proxy/"))
}
// @cpt-end:cpt-cf-oagw-dod-auth-test-coverage:p2:inst-full
