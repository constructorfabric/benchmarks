//! Integration tests for the OAuth2 client-credentials auth plugin (ADR 0008).
//!
//! The plugin's own unit tests cover the cache key, the TTL rule and one error
//! precedence case. These tests drive [`OAuth2ClientCredAuthPlugin`] against a
//! real `httpmock` token endpoint instead, so the *wire* behaviour is what is
//! under test:
//!
//! * the client-credentials grant travels in the request body (`Form`) or in
//!   the `Authorization` header (`Basic`),
//! * the fetched bearer token is injected into the outbound request context,
//! * a token is cached for `min(config_ttl, expires_in − 30s)` and served from
//!   the cache on the next call,
//! * failed fetches and tokens inside the safety margin are never cached,
//! * configuration and credential failures raise credential-free errors.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use common::{FailSecrets, FixedSecrets, OTHER_TENANT, TENANT};
use httpmock::{Method, Mock, MockServer};
use oagw::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use oagw::domain::plugin::{
    AuthPlugin, PluginError, RequestContext, ResolvedSecret, SecretResolver,
};
use oagw::infra::plugin::oauth2_client_cred_auth::{
    EXPIRY_SAFETY_MARGIN_SECS, OAuth2ClientCredAuthPlugin,
};
use toolkit_security::SecurityContext;

/// Path served by the mock token endpoint.
const TOKEN_PATH: &str = "/oauth2/token";
/// Value resolved for `client_id_ref`.
const CLIENT_ID: &str = "oagw-client";
/// Value resolved for `client_secret_ref`. Every failure test asserts that this
/// value never reaches an error message or a debug dump.
const CLIENT_SECRET: &str = "oagw-client-secret-9f2a";
/// `Basic base64(CLIENT_ID:CLIENT_SECRET)` — the credential the Basic variant
/// must present to the IdP.
const BASIC_CREDENTIALS: &str = "Basic b2Fndy1jbGllbnQ6b2Fndy1jbGllbnQtc2VjcmV0LTlmMmE=";
/// `cred://` reference holding [`CLIENT_ID`].
const CLIENT_ID_REF: &str = "cred://oagw-client-id";
/// `cred://` reference holding [`CLIENT_SECRET`].
const CLIENT_SECRET_REF: &str = "cred://oagw-client-secret";
/// A subject that owns a cache entry of its own.
const SUBJECT: uuid::Uuid = uuid::Uuid::from_u128(0xbeef);
/// A second subject, used to assert subject isolation.
const OTHER_SUBJECT: uuid::Uuid = uuid::Uuid::from_u128(0xbeee);
/// A space-separated scope list, as it would be configured on a route.
const SCOPES: &str = "read write";

/// A security context for `tenant` with an explicit subject id.
fn subject_context(tenant: uuid::Uuid, subject: uuid::Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_type("service")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

/// The token document the mock IdP answers with (RFC 6749 §5.1).
fn token_json(token: &str, expires_in: u64) -> String {
    format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
}

/// Token endpoint of a mock IdP.
fn token_url(server: &MockServer) -> String {
    format!("http://127.0.0.1:{}{TOKEN_PATH}", server.port())
}

/// A token endpoint that hands out `token`.
fn token_endpoint<'a>(server: &'a MockServer, token: &str, expires_in: u64) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(Method::POST).path(TOKEN_PATH);
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json(token, expires_in));
    })
}

/// A token endpoint that rejects the client credentials.
fn rejected_token_endpoint<'a>(server: &'a MockServer) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(Method::POST).path(TOKEN_PATH);
        then.status(400)
            .header("content-type", "application/json")
            .body(
                r#"{"error":"invalid_client","error_description":"client authentication failed"}"#,
            );
    })
}

/// Plugin configuration pointing at the mock IdP.
fn plugin_config(token_endpoint: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("token_endpoint".to_owned(), token_endpoint.to_owned()),
        ("client_id_ref".to_owned(), CLIENT_ID_REF.to_owned()),
        ("client_secret_ref".to_owned(), CLIENT_SECRET_REF.to_owned()),
    ])
}

/// An outbound request context owned by `tenant`/`subject`.
fn request_context(
    tenant: uuid::Uuid,
    subject: uuid::Uuid,
    config: BTreeMap<String, String>,
) -> RequestContext {
    RequestContext {
        security_context: Some(subject_context(tenant, subject)),
        tenant_id: Some(tenant),
        config,
        ..RequestContext::default()
    }
}

/// Credentials backing the `cred://` references of [`plugin_config`].
fn credentials() -> Arc<dyn SecretResolver> {
    Arc::new(FixedSecrets(vec![
        ("oagw-client-id".to_owned(), CLIENT_ID.to_owned()),
        ("oagw-client-secret".to_owned(), CLIENT_SECRET.to_owned()),
    ]))
}

/// A credential resolver that fails like an unreachable CredStore.
struct BrokenSecrets;

#[async_trait]
impl SecretResolver for BrokenSecrets {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        reference: &str,
    ) -> Result<Option<ResolvedSecret>, PluginError> {
        Err(PluginError::new(
            "CREDSTORE_UNAVAILABLE",
            format!("credstore unreachable for {reference}"),
        ))
    }
}

/// Plugin, credential resolver and a context factory bound to one mock IdP.
struct Fixture {
    plugin: OAuth2ClientCredAuthPlugin,
    secrets: Arc<dyn SecretResolver>,
    token_url: String,
}

impl Fixture {
    /// Form-credential variant (credentials in the request body).
    fn form(server: &MockServer) -> Self {
        let secrets = credentials();
        Self {
            plugin: OAuth2ClientCredAuthPlugin::form(Arc::clone(&secrets)),
            secrets,
            token_url: token_url(server),
        }
    }

    /// Basic-credential variant (credentials in the `Authorization` header).
    fn basic(server: &MockServer) -> Self {
        let secrets = credentials();
        Self {
            plugin: OAuth2ClientCredAuthPlugin::basic(Arc::clone(&secrets)),
            secrets,
            token_url: token_url(server),
        }
    }

    /// A fresh outbound request context for `tenant`/`subject`.
    fn context(&self, tenant: uuid::Uuid, subject: uuid::Uuid) -> RequestContext {
        request_context(tenant, subject, plugin_config(&self.token_url))
    }

    /// Runs `authenticate` on an explicitly owned context.
    ///
    /// # Errors
    ///
    /// Forwards the plugin failure to the caller.
    async fn call(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        self.plugin.authenticate(ctx, self.secrets.as_ref()).await
    }

    /// Runs one `authenticate` call on a fresh context and hands it back.
    ///
    /// # Errors
    ///
    /// Forwards the plugin failure to the caller.
    async fn authenticate(
        &self,
        tenant: uuid::Uuid,
        subject: uuid::Uuid,
    ) -> Result<RequestContext, PluginError> {
        let mut ctx = self.context(tenant, subject);
        self.call(&mut ctx).await?;
        Ok(ctx)
    }
}

#[tokio::test]
async fn form_variant_sends_the_client_credentials_grant_in_the_request_body() {
    let server = MockServer::start();

    // Only matches when the credentials travel in the header: the form variant
    // must never satisfy it.
    let basic_only = server.mock(|when, then| {
        when.method(Method::POST)
            .path(TOKEN_PATH)
            .header_exists("authorization");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json("tok-header-only", 3600));
    });
    // The grant itself, with every credential in the form body.
    let grant = server.mock(|when, then| {
        when.method(Method::POST)
            .path(TOKEN_PATH)
            .form_urlencoded_tuple("grant_type", "client_credentials")
            .form_urlencoded_tuple("client_id", CLIENT_ID)
            .form_urlencoded_tuple("client_secret", CLIENT_SECRET);
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json("tok-form", 3600));
    });

    let fixture = Fixture::form(&server);
    let ctx = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("authenticate");

    assert_eq!(ctx.header("Authorization"), Some("Bearer tok-form"));
    assert_eq!(
        ctx.headers.len(),
        1,
        "only the Authorization header is injected"
    );
    assert_eq!(fixture.plugin.id(), OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID);
    grant.assert_calls(1);
    assert_eq!(
        basic_only.calls(),
        0,
        "the form variant must not use the Authorization header"
    );
}

#[tokio::test]
async fn basic_variant_authenticates_the_client_with_the_authorization_header() {
    let server = MockServer::start();

    // The mock only answers a request that presents the Basic credential and
    // carries no credential in the body.
    let grant = server.mock(|when, then| {
        when.method(Method::POST)
            .path(TOKEN_PATH)
            .header("authorization", BASIC_CREDENTIALS)
            .form_urlencoded_tuple("grant_type", "client_credentials")
            .form_urlencoded_tuple_missing("client_id")
            .form_urlencoded_tuple_missing("client_secret")
            .body_excludes(CLIENT_SECRET);
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json("tok-basic", 3600));
    });

    let fixture = Fixture::basic(&server);
    let ctx = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("authenticate");

    assert_eq!(ctx.header("Authorization"), Some("Bearer tok-basic"));
    assert_eq!(fixture.plugin.id(), OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID);
    grant.assert_calls(1);
}

#[tokio::test]
async fn a_second_call_is_served_from_the_token_cache() {
    let server = MockServer::start();
    let mut first_fetch = token_endpoint(&server, "tok-cached", 3600);

    let fixture = Fixture::form(&server);
    let first = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("first call");
    assert_eq!(first.header("Authorization"), Some("Bearer tok-cached"));
    assert_eq!(first_fetch.calls(), 1, "the first call fetches the token");

    // Retire the mock: a second IdP round trip would now fail outright.
    first_fetch.delete();
    let refetch = token_endpoint(&server, "tok-refetched", 3600);

    let second = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("second call is served from the cache");
    assert_eq!(second.header("Authorization"), Some("Bearer tok-cached"));
    assert_eq!(
        refetch.calls(),
        0,
        "a cached token must not trigger another fetch"
    );
}

#[tokio::test]
async fn a_rejected_fetch_is_not_cached_and_the_next_call_refetches() {
    let server = MockServer::start();
    let mut rejected = rejected_token_endpoint(&server);
    let fixture = Fixture::form(&server);

    let mut ctx = fixture.context(TENANT, SUBJECT);
    let err = fixture
        .call(&mut ctx)
        .await
        .expect_err("the IdP rejects the client");
    assert_eq!(err.code, "OAUTH2_TOKEN_FETCH_FAILED");
    assert!(
        err.message.contains("400"),
        "the rejection status is reported: {err}"
    );
    assert_eq!(
        ctx.header("Authorization"),
        None,
        "nothing is injected on failure"
    );
    assert_eq!(rejected.calls(), 1);

    // The IdP document and the client secret stay out of the failure.
    assert!(
        !err.message.contains("invalid_client"),
        "the IdP body must not reach the error message: {err}"
    );
    assert!(!err.message.contains(CLIENT_SECRET));
    assert!(!format!("{err:?}").contains(CLIENT_SECRET));

    // The failure was not cached, so the very same identity retries the IdP.
    rejected.delete();
    let recovered = token_endpoint(&server, "tok-recovered", 3600);
    let ctx = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("the retry refetches");
    assert_eq!(ctx.header("Authorization"), Some("Bearer tok-recovered"));
    assert_eq!(recovered.calls(), 1, "a failed fetch must not be cached");
}

#[tokio::test]
async fn a_token_expiring_within_the_safety_margin_is_not_cached() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-fleeting", EXPIRY_SAFETY_MARGIN_SECS);
    let fixture = Fixture::form(&server);

    let first = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("first call");
    let second = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("second call");

    assert_eq!(first.header("Authorization"), Some("Bearer tok-fleeting"));
    assert_eq!(second.header("Authorization"), Some("Bearer tok-fleeting"));
    endpoint.assert_calls(2);
}

#[tokio::test]
async fn a_token_expiring_just_outside_the_safety_margin_is_cached() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-durable", EXPIRY_SAFETY_MARGIN_SECS + 1);
    let fixture = Fixture::form(&server);

    let first = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("first call");
    let second = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("second call");

    assert_eq!(first.header("Authorization"), Some("Bearer tok-durable"));
    assert_eq!(second.header("Authorization"), Some("Bearer tok-durable"));
    assert_eq!(
        endpoint.calls(),
        1,
        "one second of usable lifetime is enough to cache the token"
    );
}

#[tokio::test]
async fn a_missing_token_endpoint_is_reported_as_a_configuration_error() {
    let server = MockServer::start();
    let fixture = Fixture::form(&server);
    let mut ctx = fixture.context(TENANT, SUBJECT);
    ctx.config.remove("token_endpoint");

    let err = fixture
        .call(&mut ctx)
        .await
        .expect_err("no endpoint is configured");
    assert_eq!(err.code, "OAUTH2_CONFIG_MISSING");
    assert!(
        err.message.contains("token_endpoint") && err.message.contains("issuer_url"),
        "both alternatives are named: {err}"
    );
    assert!(!err.message.contains(CLIENT_SECRET));
}

#[tokio::test]
async fn a_blank_token_endpoint_is_treated_as_missing() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    let fixture = Fixture::form(&server);
    let mut ctx = fixture.context(TENANT, SUBJECT);
    ctx.config
        .insert("token_endpoint".to_owned(), "   ".to_owned());

    let err = fixture
        .call(&mut ctx)
        .await
        .expect_err("a blank endpoint is no endpoint");
    assert_eq!(err.code, "OAUTH2_CONFIG_MISSING");
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn a_malformed_token_endpoint_is_reported_as_an_invalid_url() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    let fixture = Fixture::form(&server);
    let mut ctx = fixture.context(TENANT, SUBJECT);
    ctx.config
        .insert("token_endpoint".to_owned(), "not-a-url".to_owned());

    let err = fixture.call(&mut ctx).await.expect_err("not a URL");
    assert_eq!(err.code, "OAUTH2_CONFIG_INVALID");
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn a_missing_client_id_reference_is_reported_as_a_configuration_error() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    let fixture = Fixture::form(&server);
    let mut ctx = fixture.context(TENANT, SUBJECT);
    ctx.config.remove("client_id_ref");

    let err = fixture
        .call(&mut ctx)
        .await
        .expect_err("no client id reference");
    assert_eq!(err.code, "OAUTH2_CONFIG_MISSING");
    assert!(
        err.message.contains("client_id_ref"),
        "the key is named: {err}"
    );
    assert!(!err.message.contains(CLIENT_SECRET));
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn a_missing_client_secret_reference_is_reported_as_a_configuration_error() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    let fixture = Fixture::form(&server);
    let mut ctx = fixture.context(TENANT, SUBJECT);
    ctx.config.remove("client_secret_ref");

    let err = fixture
        .call(&mut ctx)
        .await
        .expect_err("no client secret reference");
    assert_eq!(err.code, "OAUTH2_CONFIG_MISSING");
    assert!(
        err.message.contains("client_secret_ref"),
        "the key is named: {err}"
    );
    assert!(!err.message.contains(CLIENT_SECRET));
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn an_unresolvable_client_id_reference_is_reported_as_secret_not_found() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    // The fail-closed resolver resolves neither reference.
    let plugin = OAuth2ClientCredAuthPlugin::form(Arc::new(FailSecrets));
    let mut ctx = request_context(TENANT, SUBJECT, plugin_config(&token_url(&server)));

    let err = plugin
        .authenticate(&mut ctx, &FailSecrets)
        .await
        .expect_err("the reference resolves to nothing");
    assert_eq!(err.code, "SECRET_NOT_FOUND");
    assert!(
        !err.message.contains(CLIENT_ID),
        "no resolved value is echoed: {err}"
    );
    assert!(!err.message.contains(CLIENT_SECRET));
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn an_unresolvable_client_secret_reference_is_reported_as_secret_not_found() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    // The id resolves, the secret does not: the secret lookup is the failing one.
    let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecrets(vec![(
        "oagw-client-id".to_owned(),
        CLIENT_ID.to_owned(),
    )]));
    let plugin = OAuth2ClientCredAuthPlugin::form(Arc::clone(&secrets));
    let mut ctx = request_context(TENANT, SUBJECT, plugin_config(&token_url(&server)));

    let err = plugin
        .authenticate(&mut ctx, secrets.as_ref())
        .await
        .expect_err("the secret reference resolves to nothing");
    assert_eq!(err.code, "SECRET_NOT_FOUND");
    assert!(!err.message.contains(CLIENT_ID));
    assert!(!err.message.contains(CLIENT_SECRET));
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn a_credential_store_failure_is_forwarded_without_a_fetch() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    let plugin = OAuth2ClientCredAuthPlugin::form(Arc::new(BrokenSecrets));
    let mut ctx = request_context(TENANT, SUBJECT, plugin_config(&token_url(&server)));

    let err = plugin
        .authenticate(&mut ctx, &BrokenSecrets)
        .await
        .expect_err("the CredStore is unreachable");
    assert_eq!(err.code, "CREDSTORE_UNAVAILABLE");
    assert_eq!(
        err.message,
        "credstore unreachable for cred://oagw-client-id"
    );
    assert!(!err.message.contains(CLIENT_SECRET));
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn a_missing_security_context_is_reported_before_the_idp_is_called() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-never", 3600);
    let fixture = Fixture::form(&server);
    let mut ctx = fixture.context(TENANT, SUBJECT);
    ctx.security_context = None;

    let err = fixture
        .call(&mut ctx)
        .await
        .expect_err("there is no subject to resolve credentials for");
    assert_eq!(err.code, "OAUTH2_NO_SUBJECT");
    assert_eq!(endpoint.calls(), 0, "the IdP is never called");
}

#[tokio::test]
async fn contexts_differing_in_tenant_subject_or_auth_method_never_share_a_cached_token() {
    let server = MockServer::start();
    let endpoint = token_endpoint(&server, "tok-per-identity", 3600);
    let form = Fixture::form(&server);
    let basic = Fixture::basic(&server);

    let first = form
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("first identity");
    let other_tenant = form
        .authenticate(OTHER_TENANT, SUBJECT)
        .await
        .expect("other tenant");
    let other_subject = form
        .authenticate(TENANT, OTHER_SUBJECT)
        .await
        .expect("other subject");
    let other_method = basic
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("other auth method");

    for ctx in [&first, &other_tenant, &other_subject, &other_method] {
        assert_eq!(ctx.header("Authorization"), Some("Bearer tok-per-identity"));
    }
    assert_eq!(
        endpoint.calls(),
        4,
        "every distinct identity fetched its own token"
    );

    // The first identity is still served from its own cache entry.
    let cached = form.authenticate(TENANT, SUBJECT).await.expect("cache hit");
    assert_eq!(
        cached.header("Authorization"),
        Some("Bearer tok-per-identity")
    );
    assert_eq!(
        endpoint.calls(),
        4,
        "the cached entry survived the other identities' fetches"
    );
}

#[tokio::test]
async fn a_different_scope_configuration_does_not_reuse_a_cached_token() {
    let server = MockServer::start();

    // Registered first, so the scoped request matches it and the unscoped one
    // falls through to the plain mock.
    let scoped = server.mock(|when, then| {
        when.method(Method::POST)
            .path(TOKEN_PATH)
            .form_urlencoded_tuple("scope", SCOPES);
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json("tok-scoped", 3600));
    });
    let plain = token_endpoint(&server, "tok-unscoped", 3600);
    let fixture = Fixture::form(&server);

    let unscoped = fixture
        .authenticate(TENANT, SUBJECT)
        .await
        .expect("unscoped call");
    assert_eq!(
        unscoped.header("Authorization"),
        Some("Bearer tok-unscoped")
    );

    let mut scoped_ctx = fixture.context(TENANT, SUBJECT);
    scoped_ctx
        .config
        .insert("scopes".to_owned(), SCOPES.to_owned());
    fixture.call(&mut scoped_ctx).await.expect("scoped call");
    assert_eq!(
        scoped_ctx.header("Authorization"),
        Some("Bearer tok-scoped")
    );

    assert_eq!(scoped.calls(), 1, "the scope list reaches the IdP");
    assert_eq!(
        plain.calls(),
        1,
        "a different config must not share the cached token"
    );
}
