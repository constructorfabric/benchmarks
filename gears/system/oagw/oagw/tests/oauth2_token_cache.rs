//! The `OAuth2` client-credentials plugin against a stand-in identity provider.
//!
//! ADR-0008: one token exchange per `(tenant, subject, method, config)` tuple,
//! cached for `min(configured ceiling, expires_in − 30 s)`. The stand-in `IdP`
//! names every token it grants after its own issuance counter and the client
//! that asked, so a cached token reads back as the number its owner was first
//! given.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::StatusCode;
use common::{
    Harness, JsonConfig, OAUTH2_PLUGIN, TOKEN_TTL_SECONDS, TOKEN_TTL_SECONDS_SHORT, record,
    request, token_issuances,
};

/// Secrets the credential store answers to, in the form the plugin references.
fn credentials() -> Vec<(String, String)> {
    vec![
        ("idp-client-id".to_owned(), "the-client".to_owned()),
        ("idp-client-secret".to_owned(), "the-secret".to_owned()),
    ]
}

/// A gateway whose stand-in upstream is also its identity provider.
///
/// `endpoint` is the path on it that grants tokens.
async fn gateway(endpoint: &str) -> Harness {
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), credentials()).await;
    let auth = serde_json::json!({
        "auth": {
            "type": OAUTH2_PLUGIN,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/{}", harness.upstream_port(), endpoint),
                "client_id_ref": "cred://idp-client-id",
                "client_secret_ref": "cred://idp-client-secret",
            },
        }
    });
    harness.upstream_with_route("idp", Some(auth), None).await;
    harness
}

/// Proxies a request and returns the `authorization` header the upstream saw.
async fn forwarded_authorization(harness: &Harness) -> Option<String> {
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/idp/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    response.body["headers"]["authorization"]
        .as_str()
        .map(str::to_owned)
}

/// The issuance ordinal a granted token carries, `issued-N-for-the-client`.
///
/// The counter is shared by every test in the process, so an ordinal is only
/// meaningful relative to another read of the same header.
fn ordinal(token: Option<&str>) -> usize {
    let raw = token.expect("a token was forwarded").trim_start_matches("Bearer issued-");
    let body = raw.strip_suffix("-for-the-client").expect("the client is named");
    body.parse().expect("the ordinal is a number")
}

#[tokio::test]
async fn a_second_request_reuses_the_cached_token() {
    let harness = gateway("oauth/token").await;

    let first = forwarded_authorization(&harness).await;
    assert!(
        first.as_deref().is_some_and(|token| token.starts_with("Bearer issued-")),
        "the exchange names the client whose credentials were resolved: {first:?}"
    );

    let second = forwarded_authorization(&harness).await;
    assert_eq!(
        second, first,
        "the second request for the same identity reads the cache"
    );
    assert_eq!(
        ordinal(first.as_deref()),
        ordinal(second.as_deref()),
        "one exchange, not two"
    );
}

#[tokio::test]
async fn a_token_already_inside_the_safety_margin_is_not_cached() {
    let harness = gateway("oauth/short-token").await;

    let first = forwarded_authorization(&harness).await;
    assert!(
        first.as_deref().is_some_and(|token| token.starts_with("Bearer issued-")),
        "the exchange still succeeds and the caller still gets its token: {first:?}"
    );

    let second = forwarded_authorization(&harness).await;
    assert_ne!(
        second, first,
        "a token whose useful life the margin has eaten cannot be reused"
    );
    assert_ne!(
        ordinal(first.as_deref()),
        ordinal(second.as_deref()),
        "each request exchanges again, since nothing was stored"
    );
}
