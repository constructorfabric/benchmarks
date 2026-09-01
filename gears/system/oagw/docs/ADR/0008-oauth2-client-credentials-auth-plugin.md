---
status: accepted
date: 2026-02-24
decision-makers: Constructor Fabric Steering Committee
---

# OAuth2 Client Credentials Auth Plugin — Internal Token Cache with `fetch_token`


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
  - [Why `fetch_token()` over `Token`](#why-fetch_token-over-token)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Plugin Variants](#plugin-variants)
  - [Plugin Config (ctx.config keys)](#plugin-config-ctxconfig-keys)
  - [Gear-Level Configuration (OagwConfig)](#gear-level-configuration-oagwconfig)
  - [Cache Key Design](#cache-key-design)
  - [Hash-Collision Safety via CachedToken Wrapper](#hash-collision-safety-via-cachedtoken-wrapper)
  - [Authentication Flow](#authentication-flow)
  - [Plugin Implementation](#plugin-implementation)
  - [Registry Integration](#registry-integration)
  - [Upstream Configuration Example](#upstream-configuration-example)
  - [Retry on Upstream 401 (Deferred)](#retry-on-upstream-401-deferred)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Internal `pingora-memory-cache` token cache](#internal-pingora-memory-cache-token-cache)
  - [Generic caching decorator over `AuthPlugin`](#generic-caching-decorator-over-authplugin)
  - [No caching (per-request IdP call)](#no-caching-per-request-idp-call)
- [Out of Scope](#out-of-scope)
- [Future Considerations](#future-considerations)
- [Related ADRs](#related-adrs)
- [References](#references)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`

## Context and Problem Statement

OAGW needs to authenticate outbound proxy requests to upstream services that require OAuth2 Client Credentials flow (RFC 6749 §4.4). The caller exchanges `client_id` + `client_secret` for a short-lived access token at a token endpoint, then injects it as `Authorization: Bearer <token>` on each proxied request.

Current built-in auth plugins (`ApiKeyAuthPlugin`, `NoopAuthPlugin`) handle static secrets only. Without a dedicated OAuth2 plugin, operators must pre-compute tokens externally and supply them as static bearer credentials — breaking automatic token rotation.

**Existing infrastructure**:

- `AuthPlugin` trait in `oagw/src/domain/plugin/mod.rs` — `authenticate(&self, ctx, security_context, config, parts)` interface
- `CredStoreClientV1` in `credstore-sdk` — resolves `cred://` references to secret values
- `toolkit_auth::oauth2` in `libs/toolkit-auth` — one-shot token exchange returning bearer + `expires_in`, OIDC Discovery, `Basic`/`Form` client auth (no background watcher)
- GTS identifiers already reserved in `oagw/src/domain/gts.rs`:
  - `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` (Form)
  - `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` (Basic)

### Why `fetch_token()` over `Token`

`toolkit-auth` offers two token acquisition APIs. `Token` is a long-lived handle that spawns a `token_watcher::TokenWatcher` background task for automatic refresh — designed for service-level singletons where one identity authenticates to one upstream for the process lifetime. `fetch_token()` performs a single HTTP exchange and returns the bearer value alongside `expires_in`, spawning nothing.

The OAGW plugin is multi-tenant: each cache miss resolves a different (tenant, subject, config) tuple. Using `Token` here would spawn a background watcher per miss — potentially thousands of orphaned tokio tasks sleeping until their next refresh cycle, each holding `client_id` and `client_secret` in the captured `source_factory` closure. `fetch_token()` avoids both problems: credentials are transient (dropped after the fetch), and no background tasks are created. The plugin manages its own cache with `pingora-memory-cache`, making `Token`'s built-in refresh redundant.

## Decision Drivers

* Credential safety: secrets sourced via CredStore, wrapped in `SecretString`, never logged
* Per-request IdP calls incur 100–500ms latency and risk IdP rate limits — caching required
* Dual auth methods: `Form` and `Basic` as separate registered plugin IDs
* Cross-tenant and cross-subject credential isolation must be guaranteed by cache key design
* Strategic alignment: `pingora-memory-cache` aligns with planned Pingora adoption

## Considered Options

* Plugin with internal `pingora-memory-cache` token cache
* Generic caching decorator wrapping any `AuthPlugin`
* No caching (per-request IdP call)

## Decision Outcome

Chosen option: "Plugin with internal `pingora-memory-cache` token cache". The plugin fetches a token from the IdP on the first request for a given (tenant, subject, config) tuple, caches it with a configurable TTL (default 5 minutes), and serves subsequent requests from cache until expiry.

Caching is an internal concern of the OAuth2 CC plugin — not a generic decorator. `ApiKeyAuthPlugin` and `NoopAuthPlugin` have no expensive fetch and carry no caching infrastructure. The `AuthPlugin` trait remains unchanged.

### Plugin Variants

| GTS Plugin ID | Client Auth Method |
|---|---|
| `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` | `Form` (credentials in request body) |
| `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` | `Basic` (credentials in `Authorization` header) |

Both registered in `BuiltinPlugins::with_builtins`. Only `auth_method` differs; both share the same cache configuration.

### Plugin Config (ctx.config keys)

| Key | Required | Description |
|---|---|---|
| `token_endpoint` | Mutually exclusive with `issuer_url` | Direct token endpoint URL |
| `issuer_url` | Mutually exclusive with `token_endpoint` | OIDC issuer URL (Discovery) |
| `client_id_ref` | Yes | `cred://` reference for `client_id` |
| `client_secret_ref` | Yes | `cred://` reference for `client_secret` |
| `scopes` | No | Space-separated OAuth2 scopes |

### Gear-Level Configuration (OagwConfig)

| Key | Default | Description |
|---|---|---|
| `token_cache_ttl_secs` | 300 (5 min) | Ceiling for cached access token TTL. The actual TTL is `min(config_ttl, expires_in − 30s safety margin)`, where `expires_in` is reported by the IdP. Kept short because there is no cache-invalidation mechanism yet — a revoked or rotated token remains cached until expiry. |
| `token_cache_capacity` | 10,000 | Maximum entries in the token cache. |

These keys exist in `OagwConfig` (`oagw/src/config.rs`, defaults from `TOKEN_CACHE_TTL_SECS` / `TOKEN_CACHE_CAPACITY`) and are reachable through `OagwConfig::token_cache_ttl_secs()` / `token_cache_capacity()`. **The plugin layer does not consume them yet**: `OAuth2ClientCredAuthPlugin` reads the build-time constants `DEFAULT_TOKEN_CACHE_TTL_SECS` / `DEFAULT_TOKEN_CACHE_CAPACITY` instead, because `BuiltinPlugins::with_builtins(credstore)` takes no configuration argument and the `OagwConfig` → `DataPlaneServiceImpl` plumbing is not wired. Recorded in `DEVIATIONS.md`; the values are identical, so behaviour matches the table above.

### Cache Key Design

`TinyUfo` (used internally by `pingora-memory-cache`) hashes keys to `u64` and does **not** use `Eq` for collision resolution. To avoid silent collisions, the cache uses a `String` key encoding all identity components:

```rust
// oagw/src/infra/plugin/oauth2_cc_auth.rs

fn cache_key(&self, security_context: &SecurityContext, config: &serde_json::Value) -> String {
    format!(
        "{}:{}:{}:{:016x}",
        security_context.subject_tenant_id(),
        security_context.subject_id(),
        self.auth_method.tag(),
        config_hash(config),
    )
}
```

The key includes:
- `subject_tenant_id` — cross-tenant isolation
- `subject_id` — cross-subject isolation for CredStore `private` sharing mode
- `auth_method.tag()` — `"form"` / `"basic"`, prevents collisions if both variants share identical config
- `config_hash` — FNV-1a over `config.to_string()`; `serde_json::Value` maps are `BTreeMap`-backed, so the rendering is key-sorted and stable, and different upstream configs (e.g. different scopes) get different entries

### Hash-Collision Safety via CachedToken Wrapper

To eliminate the (2^-45 probability) risk of `TinyUfo` silently returning another tenant's token on a `u64` hash collision, the cache stores a `CachedToken` wrapper that includes the original key for verification on hit:

```rust
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}
```

On cache hit, `entry.key == lookup_key` is verified before using the token. A mismatch is treated as a cache miss. This provides defense-in-depth for the multi-tenant security boundary.

### Authentication Flow

```text
authenticate(security_context, config, parts) called
  ├─ key = cache_key(security_context, config)
  ├─ cache.get(&key) → CachedToken?
  │   ├─ Hit + entry.key == key → inject token, return Ok(())
  │   └─ Miss (or key mismatch) → continue
  ├─ resolve_secret(client_id_ref)     → CredStore lookup
  ├─ resolve_secret(client_secret_ref) → CredStore lookup
  ├─ fetch_token(OAuthClientConfig)    → FetchedToken { bearer, expires_in }
  ├─ ttl = min(configured_ttl, expires_in − 30s safety margin), floored at 1s
  ├─ ttl is Some → cache.put(&key, CachedToken { key, token }, ttl)
  ├─ Inject Authorization: Bearer <token> into parts.headers
  └─ Return Ok(())
```

Failed token fetches are **not** cached — the next request for the same key retries the IdP. A token whose `expires_in` leaves nothing after the 30 s margin is **not cached at all**: `pingora-memory-cache` treats a `None` expiry as *never*, so `cache_ttl_for` returns `None` rather than storing it forever.

### Plugin Implementation

```rust
// oagw/src/infra/plugin/oauth2_cc_auth.rs

use pingora_memory_cache::MemoryCache;

/// Default cache ceiling (ADR-0008, `token_cache_ttl_secs`).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default cache capacity (ADR-0008, `token_cache_capacity`).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

pub struct OAuth2ClientCredAuthPlugin {
    secrets: Arc<SecretResolver>,
    auth_method: ClientAuth,
    cache: Arc<MemoryCache<String, CachedToken>>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    pub fn new(secrets: Arc<SecretResolver>, auth_method: ClientAuth) -> Self {
        Self {
            secrets,
            auth_method,
            cache: Arc::new(MemoryCache::new(DEFAULT_TOKEN_CACHE_CAPACITY)),
            cache_ttl: Duration::from_secs(DEFAULT_TOKEN_CACHE_TTL_SECS),
        }
    }

    pub fn form(secrets: Arc<SecretResolver>) -> Self {
        Self::new(secrets, ClientAuth::Form)
    }

    pub fn basic(secrets: Arc<SecretResolver>) -> Self {
        Self::new(secrets, ClientAuth::Basic)
    }

    /// Overrides the build-time cache defaults (test seam).
    pub fn with_cache(self, ttl: Duration, capacity: usize) -> Self { /* ... */ }
}
```

The credential store is reached through [`SecretResolver`] (`oagw/src/infra/plugin/mod.rs`), not held directly: the resolver is shared with `ApiKeyAuthPlugin` and fails closed (500 `SecretNotFound`) when the `credstore` gear is not linked into the deployment.

### Registry Integration

```rust
// oagw/src/infra/plugin/mod.rs

impl BuiltinPlugins {
    pub fn with_builtins(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self::with_builtins_optional(Some(credstore))
    }

    pub fn with_builtins_optional(
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    ) -> Self {
        let resolver = Arc::new(SecretResolver::new(credstore));
        Self {
            // ... noop and apikey plugins unchanged ...
            auth: Arc::new(AuthPluginRegistry::new(vec![
                Arc::new(OAuth2ClientCredAuthPlugin::form(resolver.clone())),
                Arc::new(OAuth2ClientCredAuthPlugin::basic(resolver)),
                // ...
            ])),
            // ...
        }
    }
}
```

### Upstream Configuration Example

```json
{
  "server": {
    "endpoints": [
      { "scheme": "https", "host": "graph.microsoft.com", "port": 443 }
    ]
  },
  "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
  "auth": {
    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
    "config": {
      "token_endpoint": "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token",
      "client_id_ref": "cred://ms-graph-client-id",
      "client_secret_ref": "cred://ms-graph-client-secret",
      "scopes": "https://graph.microsoft.com/.default"
    }
  }
}
```

OIDC Discovery variant (endpoint resolved automatically):

```json
{
  "auth": {
    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
    "config": {
      "issuer_url": "https://accounts.google.com",
      "client_id_ref": "cred://google-client-id",
      "client_secret_ref": "cred://google-client-secret",
      "scopes": "https://www.googleapis.com/auth/cloud-platform"
    }
  }
}
```

### Retry on Upstream 401 (Deferred)

The current design does **not** implement retry logic when the upstream rejects credentials with a 401. In order to support retries, the `AuthPlugin` trait needs to return a meaningful response that consumers (the Data Plane) can use to decide whether a retry with fresh credentials is warranted. Today's trait returns `Result<(), PluginError>`, which provides no such signal.

Key considerations for future retry design:

- Not all plugins benefit from retry. A static API key plugin would produce the same credential on retry, making a second attempt pointless. An OAuth2 plugin with cached tokens could produce a fresh token, making retry meaningful.
- The Data Plane is the only layer that sees both the plugin's output and the upstream's response, so retry orchestration belongs there.
- The plugin must communicate enough metadata for the Data Plane to make the retry decision without the Data Plane needing to understand plugin internals.

This is an area of active design. The trait shape, the metadata returned, and the retry policy are all open questions that will be addressed in a dedicated iteration.

### Consequences

#### Positive

- Good, because it eliminates per-request IdP calls — cached tokens served in microseconds (vs 100–500ms)
- Good, because it reduces IdP rate-limit pressure — one call per unique (tenant, subject, config) per TTL window
- Good, because cross-tenant and cross-subject isolation is guaranteed by cache key design + `CachedToken` key verification
- Good, because `SecretString` (`ZeroizeOnDrop`) securely zeroes token buffers on cache eviction
- Good, because failed token fetches are not cached — transient IdP errors self-heal on the next request
- Good, because no changes to the `AuthPlugin` trait — existing plugins (`ApiKeyAuthPlugin`, `NoopAuthPlugin`) unchanged
- Good, because `pingora-memory-cache` aligns with planned Pingora adoption (S3-FIFO + TinyLFU eviction, stampede protection)
- Good, because `expires_in`-aware cache TTL via `min(config_ttl, expires_in − 30s)` prevents serving tokens near expiry when the IdP issues short-lived tokens
- Good, because no orphaned background tasks — `fetch_token()` performs a single HTTP exchange and returns; no `TokenWatcher` is spawned

#### Negative

- Bad, because the plugin is no longer stateless — it carries an in-memory cache with security-sensitive material
- Bad, because CredStore lookups still happen on every cache miss (not cached separately)
- Bad, because there is no automatic recovery from upstream 401 — the Data Plane has no metadata to decide whether retrying with fresh credentials is meaningful (see [Retry on Upstream 401 (Deferred)](#retry-on-upstream-401-deferred))
- Neutral, because `TinyUfo`'s lazy expiry means tokens may linger in memory slightly past TTL until the next `get()` check

#### Risks

- **CredStore unreachable**: returns `PluginError::Internal` on cache miss; cached tokens continue to be served until TTL expiry
- **IdP unavailable**: `fetch_token()` fails → `PluginError::Internal`; not cached; next request retries
- **Hash collision in TinyUfo**: mitigated by `CachedToken` key verification — a collision returns a cache miss, never another tenant's token

#### Known Residual Plaintext

- The `format!("Bearer {}", token.expose())` string in `ctx.headers` is a plain `String` — not zeroed, but short-lived (scoped to the request).
- Inside `OAuthTokenSource::request_token()`, the access token is a plain `String` — not zeroed. However, the source is created, used once, and dropped immediately by `fetch_token()`, so the plaintext lifetime is limited to the fetch call.
- The long-lived cache entry itself IS zeroed on eviction via `SecretString`'s `ZeroizeOnDrop`. `pingora-memory-cache` uses flurry/seize for deferred reclamation, so there is a small delay between eviction and actual `Drop`. For bearer tokens with ~1 hour TTL, this delay (milliseconds to seconds) is negligible.

### Confirmation

Code review confirms: `OAuth2ClientCredAuthPlugin` implemented in `oagw/src/infra/plugin/oauth2_cc_auth.rs` using `pingora_memory_cache::MemoryCache<String, CachedToken>`, `toolkit_auth::oauth2` token fetching, and the `min(configured_ttl, expires_in − 30s)` TTL rule (`cache_ttl_for`). Both `Form` and `Basic` variants are registered in `BuiltinPlugins::with_builtins_optional` (`oagw/src/infra/plugin/mod.rs`) under `AUTH_PLUGIN_OAUTH2_CC_INSTANCE` = `cf.core.oagw.oauth2_client_cred.v1` and `AUTH_PLUGIN_OAUTH2_CC_BASIC_INSTANCE` = `cf.core.oagw.oauth2_client_cred_basic.v1`. The cache defaults are the build-time constants `DEFAULT_TOKEN_CACHE_TTL_SECS` (300) and `DEFAULT_TOKEN_CACHE_CAPACITY` (10 000) in the same file; `OagwConfig` exposes the same values as `token_cache_ttl_secs` / `token_cache_capacity` but the plumbing into the plugin is not wired yet (see `DEVIATIONS.md`).

## Pros and Cons of the Options

### Internal `pingora-memory-cache` token cache

The plugin owns a `MemoryCache<String, CachedToken>` and manages token lifetime itself.

* Good, because caching lives only where an expensive fetch exists (no cost for `ApiKey`/`Noop`)
* Good, because the `AuthPlugin` trait stays unchanged
* Good, because `pingora-memory-cache` aligns with the planned Pingora data plane
* Bad, because the plugin becomes stateful and holds security-sensitive material in memory

### Generic caching decorator over `AuthPlugin`

A wrapper type caches the output of any `AuthPlugin`.

* Good, because caching logic is written once and reused
* Bad, because most plugins (`ApiKey`, `Noop`) have nothing expensive to cache — the abstraction is dead weight
* Bad, because credential-injection outputs are not uniformly cacheable (static vs rotating), so the decorator needs per-plugin policy anyway
* Bad, because it complicates the trait/registry wiring for no MVP benefit

### No caching (per-request IdP call)

Fetch a fresh token from the IdP on every proxied request.

* Good, because it is the simplest possible implementation and always current
* Bad, because it adds 100–500ms latency to every request
* Bad, because it risks tripping IdP rate limits under load

## Out of Scope

- **Authorization Code Grant** (RFC 6749 §4.1): Requires user consent, redirect callbacks (OAuth Router), and refresh token storage across data centers. Separate ADR needed.

## Future Considerations

- **Retry on upstream 401**: The `AuthPlugin` trait needs to return a richer response so the Data Plane can determine whether retrying with fresh credentials is meaningful. This requires revisiting the trait signature (e.g. returning metadata alongside injected headers). See [Retry on Upstream 401 (Deferred)](#retry-on-upstream-401-deferred).
- **Event-driven cache invalidation**: Once OAGW has access to the Event Broker, consuming `oauth_client.updated` and `oauth_client.deleted` events to trigger immediate cache eviction would reduce the staleness window from TTL to event propagation latency (~seconds).
- ~~**Per-IdP TTL from token response**~~: Implemented — `fetch_token()` returns `expires_in` from the IdP response, and the plugin caches with `min(config_ttl, expires_in − 30s safety margin)`. Tokens with `expires_in ≤ 30s` are not cached.

## Related ADRs

- [ADR: Plugin System](./0002-plugin-system.md) — `AuthPlugin` trait and execution model
- [ADR: Control Plane Caching](./0005-data-plane-caching.md) — Cache invalidation pattern
- [ADR: State Management](./0006-state-management.md) — Cache eviction and state coordination

## References

- [RFC 6749: OAuth 2.0 Authorization Framework](https://datatracker.ietf.org/doc/html/rfc6749) — §4.4 Client Credentials Grant
- [OpenID Connect Discovery 1.0](https://openid.net/specs/openid-connect-discovery-1_0.html) — Token endpoint discovery
- [pingora-memory-cache](https://github.com/cloudflare/pingora/tree/main/pingora-memory-cache) — S3-FIFO + TinyLFU eviction, cache stampede protection
- `libs/toolkit-auth` (`oauth2/config.rs`, `oauth2/source.rs`, `oauth2/token.rs`, `oauth2/fetch.rs`) — Token management library
- `oagw/src/infra/plugin/apikey_auth.rs` — Reference `AuthPlugin` implementation
- `oagw/src/infra/plugin/mod.rs` — `SecretResolver` and `BuiltinPlugins::with_builtins{,_optional}`
- `oagw/src/domain/gts.rs` — `AUTH_PLUGIN_OAUTH2_CC{,_BASIC}_INSTANCE` identifiers
- `oagw/src/config.rs` — `OagwConfig::token_cache_ttl_secs` / `token_cache_capacity` (ADR-0008 defaults)

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Related ADR**: [ADR: Plugin System](./0002-plugin-system.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-auth-injection` — Auth plugins handle credential injection
* `cpt-cf-oagw-fr-plugin-system` — OAuth2 CC plugin registered as a built-in auth plugin
* `cpt-cf-oagw-fr-builtin-plugins` — Built-in plugin implementation with internal token caching
