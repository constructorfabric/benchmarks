# ToolKit Auth

Authentication infrastructure for Gears / ToolKit.

## Overview

The `cf-gears-toolkit-auth` crate provides:

- **JWT / JWKS** — `KeyProvider` trait, `JwksKeyProvider` with background key refresh, `ValidationConfig`, standard claim constants
- **Token validation** — `TokenValidator` trait, `ClaimsError` / `AuthError` error types
- **Auth configuration** — `AuthConfig` (issuers, audiences, leeway, JWKS endpoint)
- **Outbound OAuth2 client credentials** — `Token` handle with proactive background refresh and on-demand invalidation, `OAuthClientConfig`, `BearerAuthLayer` (tower), `BearerAuthAutoRefreshLayer` (reactive refresh on 401, port of go-appkit's `AuthBearerRoundTripper`), `HttpClientBuilderExt` for `toolkit-http` integration
- **Auth metrics** — `AuthMetrics` trait with `LoggingMetrics` and `NoOpMetrics` implementations

## Outbound OAuth2 quick start

```rust
use toolkit_auth::{HttpClientBuilderExt, OAuthClientConfig, SecretString, Token};
use toolkit_http::HttpClientBuilder;

let token = Token::new(OAuthClientConfig {
    token_endpoint: Some("https://idp.example.com/oauth/token".parse()?),
    client_id: "my-service".into(),
    client_secret: SecretString::new("my-secret"),
    scopes: vec!["api.read".into()],
    ..Default::default()
})
.await?;

let client = HttpClientBuilder::new()
    .with_bearer_auth(token)
    .build()?;

// Every request gets Authorization: Bearer <token> automatically
let resp = client.get("https://api.example.com/resource").send().await?;
```

## Reactive refresh on 401

`with_bearer_auth` covers the proactive case: the background `TokenWatcher`
refreshes the credential before TTL expires. It does **not** detect tokens
revoked out-of-band by the issuer — those surface only as a 401 from the
upstream.

`with_bearer_auth_auto_refresh` adds the reactive half: on a 401 response it
calls `Token::invalidate()`, re-reads the cached token, and replays the
original request once with the refreshed credential. Behavior is a port of
go-appkit's `AuthBearerRoundTripper`:

- requests that already carry the configured auth header pass through
  untouched (no refresh, no retry);
- `Token::invalidate()` is throttled per layer instance — default 15 minutes
  via `toolkit_auth::oauth2::DEFAULT_MIN_INVALIDATION_INTERVAL` — so a burst
  of 401s will not hammer the token endpoint;
- if the refreshed token equals the previous value, or if the invalidate
  fetch fails, the original 401 is surfaced as-is (no retry loop);
- exactly one retry per call, no backoff. For multi-step retry strategies,
  compose with `toolkit_http::RetryLayer`.

```rust
use toolkit_auth::{
    BearerAuthAutoRefreshOpts, HttpClientBuilderExt, OAuthClientConfig, SecretString, Token,
};
use toolkit_http::HttpClientBuilder;
use std::sync::Arc;
use std::time::Duration;

let token = Token::new(OAuthClientConfig { /* … */ ..Default::default() }).await?;

// Defaults: Authorization header, retry on 401 only, 15-min throttle.
let client = HttpClientBuilder::new()
    .with_bearer_auth_auto_refresh(token.clone())
    .build()?;

// Custom predicate / header / throttle:
let opts = BearerAuthAutoRefreshOpts {
    min_invalidation_interval: Duration::from_secs(60),
    should_refresh: Arc::new(|s| s.as_u16() == 401 || s.as_u16() == 419),
    header_name: http::header::AUTHORIZATION,
};
let custom = HttpClientBuilder::new()
    .with_bearer_auth_auto_refresh_opts(token, opts)
    .build()?;
```

Cost: auto-refresh keeps a clone of the request body so it can be replayed
on retry. With `toolkit-http`'s default body (`Full<Bytes>`) this is a
reference-counted bump; with custom `B`, the type must be `Clone`. Pick the
plain `with_bearer_auth` when the upstream is known not to revoke tokens
out-of-band.

See `examples/` for more patterns (OIDC discovery, token invalidation, shared token, form auth).

## License

Licensed under Apache-2.0.
