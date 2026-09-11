//! Unit tests for the `OAuth2` client-credentials auth plugins (ADR 0008).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{ClientAuthMethod, FetchedToken, OAuth2ClientCredAuthPlugin, TokenFetcher, ttl_for};
use crate::domain::error::OagwError;
use crate::domain::model::AuthConfig;
use crate::domain::plugin::{
    AuthDecision, AuthPlugin,
    test_support::{StaticResolver, context},
};
use crate::gts_helpers;
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn config(plugin_type: &str, entries: &[(&str, &str)]) -> AuthConfig {
    AuthConfig {
        plugin_type: plugin_type.to_owned(),
        sharing: crate::domain::model::SharingMode::default(),
        config: entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), serde_json::Value::from(*value)))
            .collect(),
    }
}

fn token_config() -> AuthConfig {
    config(
        gts_helpers::AUTH_OAUTH2_CLIENT_CRED,
        &[
            ("token_endpoint", "https://idp.example.com/token"),
            ("client_id_ref", "cred://client-id"),
            ("client_secret_ref", "cred://client-secret"),
            ("scopes", "read write"),
        ],
    )
}

/// A token endpoint double counting the exchanges it serves.
struct StubFetcher {
    calls: AtomicUsize,
    bearer: String,
    expires_in: u64,
}

impl StubFetcher {
    fn new(bearer: &str, expires_in: u64) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            bearer: bearer.to_owned(),
            expires_in,
        }
    }

    fn exchanges(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TokenFetcher for StubFetcher {
    async fn fetch(
        &self,
        _endpoint: &str,
        _method: ClientAuthMethod,
        _client_id: &crate::domain::plugin::Credential,
        _client_secret: &crate::domain::plugin::Credential,
        _scopes: Option<&str>,
    ) -> Result<FetchedToken, OagwError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(FetchedToken {
            bearer: self.bearer.clone(),
            expires_in: self.expires_in,
        })
    }
}

/// A token endpoint that always refuses the credentials.
struct RefusingFetcher;

#[async_trait]
impl TokenFetcher for RefusingFetcher {
    async fn fetch(
        &self,
        _endpoint: &str,
        _method: ClientAuthMethod,
        _client_id: &crate::domain::plugin::Credential,
        _client_secret: &crate::domain::plugin::Credential,
        _scopes: Option<&str>,
    ) -> Result<FetchedToken, OagwError> {
        Err(OagwError::AuthenticationFailed(
            "the identity provider rejected the client".to_owned(),
        ))
    }
}

fn credentials() -> StaticResolver {
    StaticResolver::new(&[("client-id", "cid-123"), ("client-secret", "sekret")])
}

#[tokio::test]
async fn injects_a_bearer_token_from_the_token_endpoint() {
    let fetcher = Arc::new(StubFetcher::new("tok-1", 3_600));
    let plugin = OAuth2ClientCredAuthPlugin::new(
        Arc::clone(&fetcher) as Arc<dyn TokenFetcher>,
        ClientAuthMethod::Form,
        Duration::from_mins(5),
        16,
    );
    let mut request = context("GET", "/v1/things");

    let decision = plugin
        .authenticate(&mut request, &token_config(), &credentials())
        .await
        .unwrap();

    assert_eq!(decision, AuthDecision::Injected);
    assert_eq!(
        request.headers.get(http::header::AUTHORIZATION).unwrap(),
        "Bearer tok-1"
    );
    assert_eq!(fetcher.exchanges(), 1);
}

#[tokio::test]
async fn a_second_request_is_served_from_the_cache() {
    let fetcher = Arc::new(StubFetcher::new("tok-1", 3_600));
    let plugin = OAuth2ClientCredAuthPlugin::new(
        Arc::clone(&fetcher) as Arc<dyn TokenFetcher>,
        ClientAuthMethod::Basic,
        Duration::from_mins(5),
        16,
    );
    let configuration = token_config();

    for _ in 0..3 {
        let mut request = context("GET", "/v1/things");
        plugin
            .authenticate(&mut request, &configuration, &credentials())
            .await
            .unwrap();
        assert_eq!(
            request.headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer tok-1"
        );
    }

    assert_eq!(fetcher.exchanges(), 1, "only the first request fetches");
}

#[tokio::test]
async fn a_token_inside_the_safety_margin_is_not_cached() {
    // 20s minus the 30s margin saturates to zero, so the entry expires at once.
    let fetcher = Arc::new(StubFetcher::new("tok-short", 20));
    let plugin = OAuth2ClientCredAuthPlugin::new(
        Arc::clone(&fetcher) as Arc<dyn TokenFetcher>,
        ClientAuthMethod::Form,
        Duration::from_mins(5),
        16,
    );
    let configuration = token_config();

    for _ in 0..2 {
        let mut request = context("GET", "/v1/things");
        plugin
            .authenticate(&mut request, &configuration, &credentials())
            .await
            .unwrap();
    }

    assert_eq!(fetcher.exchanges(), 2, "the short-lived token is refetched");
}

#[tokio::test]
async fn a_failed_exchange_is_not_cached() {
    let plugin = OAuth2ClientCredAuthPlugin::new(
        Arc::new(RefusingFetcher),
        ClientAuthMethod::Form,
        Duration::from_mins(5),
        16,
    );
    let configuration = token_config();

    for _ in 0..2 {
        let mut request = context("GET", "/v1/things");
        assert_eq!(
            plugin
                .authenticate(&mut request, &configuration, &credentials())
                .await
                .unwrap_err()
                .status(),
            401
        );
        assert!(request.headers.get(http::header::AUTHORIZATION).is_none());
    }
}

#[tokio::test]
async fn missing_configuration_is_a_validation_error() {
    let plugin = OAuth2ClientCredAuthPlugin::new(
        Arc::new(StubFetcher::new("tok-1", 3_600)),
        ClientAuthMethod::Form,
        Duration::from_mins(5),
        16,
    );
    let mut request = context("GET", "/v1/things");

    let decision = plugin
        .authenticate(
            &mut request,
            &config(gts_helpers::AUTH_OAUTH2_CLIENT_CRED, &[]),
            &credentials(),
        )
        .await;

    assert_eq!(decision.unwrap_err().status(), 400);
}

#[test]
fn the_ttl_is_the_configured_ceiling_capped_by_the_idp_lifetime() {
    assert_eq!(
        ttl_for(Duration::from_mins(5), 3_600),
        Duration::from_mins(5)
    );
    assert_eq!(ttl_for(Duration::from_mins(5), 90), Duration::from_mins(1));
    assert_eq!(ttl_for(Duration::from_mins(5), 10), Duration::from_secs(0));
}

#[test]
fn the_two_variants_carry_distinct_identifiers() {
    let fetcher = Arc::new(StubFetcher::new("tok-1", 3_600));
    let form = OAuth2ClientCredAuthPlugin::new(
        Arc::clone(&fetcher) as Arc<dyn TokenFetcher>,
        ClientAuthMethod::Form,
        Duration::from_mins(5),
        16,
    );
    let basic = OAuth2ClientCredAuthPlugin::new(
        fetcher,
        ClientAuthMethod::Basic,
        Duration::from_mins(5),
        16,
    );

    assert_eq!(form.id(), gts_helpers::AUTH_OAUTH2_CLIENT_CRED);
    assert_eq!(basic.id(), gts_helpers::AUTH_OAUTH2_CLIENT_CRED_BASIC);
    assert_ne!(form.id(), basic.id());
}

#[test]
fn the_cache_key_separates_tenants_methods_and_configurations() {
    let base = token_config();
    let tenant_a = uuid::Uuid::nil();
    let tenant_b = uuid::Uuid::from_u128(7);

    let same = OAuth2ClientCredAuthPlugin::build_cache_key(tenant_a, &base, ClientAuthMethod::Form);
    assert_eq!(
        same,
        OAuth2ClientCredAuthPlugin::build_cache_key(tenant_a, &base, ClientAuthMethod::Form)
    );
    assert_ne!(
        same,
        OAuth2ClientCredAuthPlugin::build_cache_key(tenant_b, &base, ClientAuthMethod::Form)
    );
    assert_ne!(
        same,
        OAuth2ClientCredAuthPlugin::build_cache_key(tenant_a, &base, ClientAuthMethod::Basic)
    );

    let wider = config(
        gts_helpers::AUTH_OAUTH2_CLIENT_CRED,
        &[
            ("token_endpoint", "https://idp.example.com/token"),
            ("client_id_ref", "cred://client-id"),
            ("client_secret_ref", "cred://client-secret"),
            ("scopes", "read"),
        ],
    );
    assert_ne!(
        same,
        OAuth2ClientCredAuthPlugin::build_cache_key(tenant_a, &wider, ClientAuthMethod::Form)
    );
}
