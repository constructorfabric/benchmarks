---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# OAuth2 Client Credentials Auth Plugin — Internal Token Cache with `fetch_token`

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
  - [Why `fetch_token()` over `Token`](#why-fetch_token-over-token)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Plugin Variants and GTS Identifiers](#plugin-variants-and-gts-identifiers)
  - [Plugin Configuration](#plugin-configuration)
  - [Cache Design](#cache-design)
  - [Authentication Flow](#authentication-flow)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Internal `pingora-memory-cache` token cache](#internal-pingora-memory-cache-token-cache)
  - [Generic caching decorator over `AuthPlugin`](#generic-caching-decorator-over-authplugin)
  - [No caching (per-request IdP call)](#no-caching-per-request-idp-call)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`

**Priority**: p8

## Context and Problem Statement

OAGW needs to authenticate outbound proxy requests to upstream services that require the OAuth2 Client Credentials flow (RFC 6749 §4.4). The caller exchanges `client_id` + `client_secret` for a short-lived access token at a token endpoint, then injects it as `Authorization: Bearer <token>` on each proxied request.

The current built-in auth plugins (`ApiKeyAuthPlugin`, `NoopAuthPlugin`) handle static secrets only. Without a dedicated OAuth2 plugin, operators must pre-compute tokens externally and supply them as static bearer credentials — breaking automatic token rotation. The existing `AuthPlugin` trait, the CredStore SDK (resolves `cred://` references to secret values), and `toolkit_auth::oauth2::fetch_token` (a one-shot token exchange returning the bearer plus `expires_in`, with OIDC Discovery and both `Basic`/`Form` client auth) provide the foundation. Two GTS identifiers are already reserved for this plugin.

### Why `fetch_token()` over `Token`

`toolkit-auth` offers two token acquisition APIs. `Token` is a long-lived handle that spawns a background token-watcher task for automatic refresh — designed for service-level singletons where one identity authenticates to one upstream for the process lifetime. `fetch_token()` performs a single HTTP exchange and returns the bearer alongside `expires_in`, spawning nothing.

The OAGW plugin is multi-tenant: each cache miss resolves a different (tenant, subject, config) tuple. Using `Token` here would spawn a background watcher per miss — potentially thousands of orphaned background tasks, each holding `client_id` and `client_secret` in a captured closure. `fetch_token()` avoids both problems: credentials are transient (dropped after the fetch) and no background tasks are created. The plugin manages its own cache, making `Token`'s built-in refresh redundant.

## Decision Drivers

* Credential safety: secrets sourced via CredStore, wrapped in a zeroizing secret type, never logged
* Per-request IdP calls incur 100–500ms latency and risk IdP rate limits — caching is required
* Dual auth methods: `Form` and `Basic` as separate registered plugin IDs
* Cross-tenant and cross-subject credential isolation must be guaranteed by the cache key design
* Strategic alignment: `pingora-memory-cache` aligns with planned Pingora adoption

## Considered Options

* Plugin with internal `pingora-memory-cache` token cache
* Generic caching decorator wrapping any `AuthPlugin`
* No caching (per-request IdP call)

## Decision Outcome

Chosen option: "Plugin with internal `pingora-memory-cache` token cache". The plugin fetches a token from the IdP on the first request for a given (tenant, subject, config) tuple, caches it with a configurable TTL (default 300 seconds / 5 minutes), and serves subsequent requests from cache until expiry.

Caching is an internal concern of the OAuth2 client-credentials plugin — not a generic decorator. `ApiKeyAuthPlugin` and `NoopAuthPlugin` have no expensive fetch and carry no caching infrastructure. The `AuthPlugin` trait remains unchanged.

### Plugin Variants and GTS Identifiers

| GTS Plugin ID | Client Auth Method |
|---|---|
| `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` | `Form` (credentials in request body) |
| `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` | `Basic` (credentials in `Authorization` header) |

Both variants are registered in the auth plugin registry's built-ins. Only the client-auth method differs; both share the same cache configuration.

### Plugin Configuration

**Per-upstream config keys** (`ctx.config`):

| Key | Required | Description |
|---|---|---|
| `token_endpoint` | Mutually exclusive with `issuer_url` | Direct token endpoint URL |
| `issuer_url` | Mutually exclusive with `token_endpoint` | OIDC issuer URL (Discovery) |
| `client_id_ref` | Yes | `cred://` reference for `client_id` |
| `client_secret_ref` | Yes | `cred://` reference for `client_secret` |
| `scopes` | No | Space-separated OAuth2 scopes |

**Gear-level configuration**:

| Key | Default | Description |
|---|---|---|
| `token_cache_ttl_secs` | 300 (5 min) | Ceiling for the cached access-token TTL |
| `token_cache_capacity` | 10,000 | Maximum entries in the token cache |

### Cache Design

The plugin holds an in-memory cache (`pingora-memory-cache` `MemoryCache` keyed by `String`, storing a wrapper holding the cache key and the token as a zeroizing secret value). The actual cache TTL is `min(config_ttl, expires_in − 30s safety margin)`, using the `expires_in` reported by the IdP; tokens with `expires_in ≤ 30s` are not cached. The TTL is kept short because there is no cache-invalidation mechanism yet — a revoked or rotated token remains cached until expiry.

**Cache key**: `subject_tenant_id:subject_id:auth_method:config_hash`, providing cross-tenant isolation, cross-subject isolation (for CredStore `private` sharing), separation of the `Form` and `Basic` variants that might share identical config, and separation of distinct upstream configs (e.g., different scopes).

**Hash-collision safety**: because the underlying hash is lossy, the stored wrapper retains the original key and re-verifies it on every cache hit; a mismatch is treated as a cache miss. This is defense-in-depth for the multi-tenant security boundary — a collision can never leak another tenant's token.

### Authentication Flow

On `authenticate`: parse the plugin config, build the cache key, and check the cache — on a hit with a matching key, inject the cached token and return. On a miss (or key mismatch), resolve the `client_id`/`client_secret` references via CredStore, call `fetch_token`, compute `ttl = min(config_ttl, expires_in − 30s)`, store the token in the cache with that TTL, inject `Authorization: Bearer <token>`, and return. Failed token fetches are **not** cached — the next request for the same key retries the IdP.

### Consequences

#### Positive

- Good, because it eliminates per-request IdP calls — cached tokens are served in microseconds (vs 100–500ms)
- Good, because it reduces IdP rate-limit pressure — one call per unique (tenant, subject, config) per TTL window
- Good, because cross-tenant and cross-subject isolation is guaranteed by the cache key design plus key verification on hit
- Good, because the zeroizing secret type securely zeroes token buffers on cache eviction
- Good, because failed token fetches are not cached — transient IdP errors self-heal on the next request
- Good, because no changes to the `AuthPlugin` trait — existing plugins are unchanged
- Good, because `pingora-memory-cache` aligns with planned Pingora adoption (S3-FIFO + TinyLFU eviction, stampede protection)
- Good, because the `expires_in`-aware TTL prevents serving tokens near expiry when the IdP issues short-lived tokens
- Good, because no orphaned background tasks — `fetch_token()` performs a single HTTP exchange and returns

#### Negative

- Bad, because the plugin is no longer stateless — it carries an in-memory cache with security-sensitive material
- Bad, because CredStore lookups still happen on every cache miss (not cached separately)
- Bad, because there is no automatic recovery from an upstream 401 — the Data Plane has no metadata to decide whether retrying with fresh credentials is meaningful (deferred to future design)
- Neutral, because lazy expiry means tokens may linger in memory slightly past TTL until the next lookup

#### Risks

- **CredStore unreachable**: returns an internal error on cache miss; cached tokens continue to be served until TTL expiry
- **IdP unavailable**: the token fetch fails and is not cached; the next request retries
- **Hash collision in the cache key hash**: mitigated by the key-verification wrapper — a collision returns a cache miss, never another tenant's token

### Confirmation

Code review confirms: the plugin is implemented using `pingora_memory_cache::MemoryCache` with the key-verification wrapper, `toolkit_auth::oauth2::fetch_token` for the exchange, and the `min(config_ttl, expires_in − 30s)` TTL rule. Both `Form` and `Basic` variants are registered in the auth plugin registry's built-ins under the two documented GTS identifiers.

## Pros and Cons of the Options

### Internal `pingora-memory-cache` token cache

The plugin owns its cache and manages token lifetime itself.

* Good, because caching lives only where an expensive fetch exists (no cost for API key / no-op plugins)
* Good, because the `AuthPlugin` trait stays unchanged
* Good, because `pingora-memory-cache` aligns with the planned Pingora data plane
* Bad, because the plugin becomes stateful and holds security-sensitive material in memory

### Generic caching decorator over `AuthPlugin`

A wrapper type caches the output of any `AuthPlugin`.

* Good, because caching logic is written once and reused
* Bad, because most plugins (API key, no-op) have nothing expensive to cache — the abstraction is dead weight
* Bad, because credential-injection outputs are not uniformly cacheable (static vs rotating), so the decorator needs per-plugin policy anyway
* Bad, because it complicates the trait and registry wiring for no MVP benefit

### No caching (per-request IdP call)

Fetch a fresh token from the IdP on every proxied request.

* Good, because it is the simplest possible implementation and always current
* Bad, because it adds 100–500ms latency to every request
* Bad, because it risks tripping IdP rate limits under load

## More Information

Out of scope: the **Authorization Code Grant** (RFC 6749 §4.1) requires user consent, redirect callbacks, and refresh-token storage across data centers — a separate design is needed.

Future considerations:
- **Retry on upstream 401**: requires a richer response from the `AuthPlugin` trait so the Data Plane can decide whether retrying with fresh credentials is meaningful
- **Event-driven cache invalidation**: once OAGW has access to the event broker, consuming `oauth_client.updated` / `oauth_client.deleted` events could shrink the staleness window from TTL to propagation latency

- [RFC 6749: OAuth 2.0 Authorization Framework](https://datatracker.ietf.org/doc/html/rfc6749) — §4.4 Client Credentials Grant
- [OpenID Connect Discovery 1.0](https://openid.net/specs/openid-connect-discovery-1_0.html)
- [pingora-memory-cache](https://github.com/cloudflare/pingora/tree/main/pingora-memory-cache)
- Related: [ADR: Plugin System](./0002-plugin-system.md), [ADR: State Management](./0006-state-management.md)

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-auth-injection` — Auth plugins handle credential injection
* `cpt-cf-oagw-fr-plugin-system` — OAuth2 client-credentials plugin registered as a built-in auth plugin
* `cpt-cf-oagw-fr-builtin-plugins` — Built-in plugin implementation with internal token caching
