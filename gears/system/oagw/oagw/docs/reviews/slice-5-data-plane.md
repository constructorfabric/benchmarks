# Slice 5 review evidence — data-plane core

Semantic review of `src/domain/services/proxy.rs` (resolution, route matching,
rate-limit charging) and its wire edges in `src/api/rest/proxy.rs`,
`src/infra/proxy.rs`, `src/infra/ratelimit.rs`, `src/infra/cors.rs`.
Status: **PASS** (0 critical, 0 major open).

## Findings and dispositions

| # | Severity | Finding (abridged) | Disposition |
|---|---|---|---|
| 1 | critical | The rate-limit identity for `scope: ip` comes from the client-supplied `X-OAGW-Client-Ip` header. | **Fixed.** The identity is the socket peer (`PeerAddr`), never a forwarded header; the counter table is bounded. Shared with slice 7 finding 1. |
| 2 | major | `split_suffix` accepts a non-segment-boundary prefix, so `/api/v1/admin` would swallow `/api/v1/administrators`. | **Fixed.** A match key covers whole path segments only; the remainder must begin with `/`. `prefix_matching_respects_segment_boundaries` pins it. |
| 3 | major | Routing matches on `path?query`, so the query participates in prefix matching and reaches the upstream glued to the path. | **Fixed.** Path and query are separated; the query is filtered against the route's `query_allowlist` and re-joined only when dialling. |
| 4 | major | `HttpMatch.path_suffix_mode` and `HttpMatch.query_allowlist` are never read on the data plane. | **Fixed.** `enforce_match_rules()` evaluates both per DESIGN §4.4: a suffix is rejected with `RouteRejected` when `path_suffix_mode: disabled`, and a query parameter outside the allowlist is rejected (an empty allowlist admits none, per `route.v1.schema.json`). Both are unit-tested and exercised by the deployment smoke suite. |
| 5 | major | `apply_cors` only decorates the response; the 403 path is unimplemented, so `DomainError::CorsOriginNotAllowed` is unreachable. | **Fixed.** `cors::enforce()` rejects a disallowed origin (403 `cf.oagw.cors.origin_not_allowed.v1`) or method (`cf.oagw.cors.method_not_allowed.v1`) on the actual request after resolution and before the dial; preflights stay permissive per ADR-0004. Live-verified. |
| 6 | major | Preflight hardcodes `&Method::GET`, so `Access-Control-Request-Method` is ignored, and a preflight is answered from `upstream.cors` only, ignoring the route-level CORS `effective()` merged. | **Fixed.** The preflight is permissive and resolution-free (ADR-0004 "Preflight Request Handling"); it echoes the requested method and headers. Enforcement on the actual request uses `resolved.cors`, the merged upstream+route policy. |
| 7 | major | A bucket is created from whatever config is charged first and is never reconciled; `RateLimiter::clear()` has no production call site, so the table grows without bound. | **Fixed.** Buckets carry a `BucketShape` (capacity, refill) and restart full when the charged config changes, so an admin edit to a limit takes effect immediately; the table is capped at `MAX_BUCKETS`. |
| 8 | major | `scope: route` with no matched route charges `Route("")` and `scope: user` charges `User("")`, so distinct tenants collapse into one shared counter. | **Fixed.** `CounterKey::User`/`Route` carry the tenant, so counters are scoped per tenant and never collapse; `user_and_route_counters_are_scoped_to_the_tenant` pins it. |
| 9 | major | The upstream response is projected without `X-OAGW-Error-Source: upstream`. | **Fixed.** `mark_upstream_errors()` + `GATEWAY_CONTROL_HEADERS` (see slice 6, finding 2). |
| 10 | major | `effective()` clones the upstream policy directly and never merges the route's own `rate_limit`/`cors`/`plugins`/`tags`. | **Fixed.** `effective()` merges route-level rate limit, CORS, plugin chains and tags over the upstream policy with the `enforce`-beats-descendant semantics; `merge_*` helpers are unit-tested. |
| 11 | major | Endpoint selection never round-robins and requires the header for every multi-host upstream. | **Fixed.** A multi-endpoint upstream picks a pool member per request (deterministic rotation) when no `X-OAGW-Target-Host` pins one; an explicit header must still name a configured endpoint. `record_endpoint_selection` reports the decision. |
| 12 | minor | The target-host character class admits `:`, so `us.vendor.com:8443` reports `UnknownTargetHost` instead of `InvalidTargetHost`. | **Accepted deviation.** Both are 400 catalog rows from the same source; see slice 6, finding 15. |
| 13 | minor | A missing `SecurityContext` degrades to the nil tenant instead of 401. | **Fixed.** `identity()` returns `Unauthorized` (401). |
| 14 | minor | The `RateVerdict` is computed and dropped; `X-RateLimit-*` reaches the client only on a 429. | **Fixed.** A successful hop stamps `X-RateLimit-Limit/Remaining/Reset` when the charged config opts into response headers (`RateLimitConfig.response_headers`), in addition to the 429 path. |
| 15 | minor | `algorithm: sliding_window` and `strategy: queue|degrade` are accepted and silently executed as token bucket / reject. | **Accepted deviation.** ADR-0003 specifies the token bucket as the implemented algorithm and reject as the strategy; the schema admits the other spellings so configuration written against the full vocabulary validates, and the behaviour is documented at the limiter. |
| 16 | minor | `SimpleCors::apply` `insert`s a fixed `Vary`, clobbering an upstream `Vary: Accept-Encoding`. | **Fixed.** The CORS `Vary` is appended to any value the upstream already sent rather than replacing it. |
| 17 | minor | The route tie-break `id.as_bytes()` makes cross-level duplicates of the same `(path, priority)` order-dependent. | **Accepted deviation.** Descendant routes are collected before ancestor ones, so a descendant shadowing an ancestor at equal `(path, priority)` is already deterministic; the id tie-break only orders two routes within one level, which the management plane's 409 on duplicate match keys prevents. |
| 18 | minor | `resolve()` walks `hierarchy.chain()` twice, so the upstream and the route pool can come from two different snapshots. | **Accepted deviation.** The chain is a resolved vector (not a live query) and the store is in-memory; the two walks read the same state. A transactional snapshot is a persistence-slice concern, listed with the storage deviation in `SLICE-PLAN.md`. |
| 19 | minor | `pinned()`/`bindings()` are dead code and `bindings()` silently converts malformed plugin JSON to an empty vector. | **Fixed.** `bindings()` is gone; plugin JSON that fails to parse is reported as a validation error instead of an empty chain. |
| 20 | minor | `BoundedCache`/`SharedConfigCache` has no consumer and `dp_cache_capacity`/`cp_cache_capacity` are no-op config. | **Accepted deviation.** ADR-0005 records the data-plane cache as not implemented in this phase; the type is kept behind the boundary so a cache slice can adopt it, and the config knobs are documented as reserved. |
| 21 | minor | The OPTIONS branch never calls `charge_rate`, so preflights bypass the edge limiter. | **Accepted deviation.** ADR-0004 forbids upstream resolution on a preflight, so there is no resolved policy to charge against; browsers also do not send credentials on a preflight, which is what a tenant-scoped counter would need. |
| 22 | minor | `SsrfPolicy::default()` leaves `enabled: false`, so the private/loopback blocks are inert unless the deployment opts in. | **Accepted deviation.** The e2e deployment deliberately proxies to loopback mock upstreams, so the policy defaults off and `config/e2e-local.yaml` states it; a production profile enables it (`check_host` runs on the write path and again immediately before the dial). |

## Clean checks

- **Tenant isolation**: the repositories are tenant-scoped on every read and write;
  a foreign upstream id is a 404 on the route path.
- **SSRF within `target_endpoint`**: a pinned host can only select among configured
  endpoints, and the dial path re-checks the resolved host.
- **Header injection**: inbound values are copied as already-parsed `HeaderValue`s,
  `insert_header` drops values the wire cannot carry, and the outbound `host` is set
  from the configured endpoint rather than from the caller.
- **Concurrency**: no await-while-locked or deadlock hazards; the bucket table lock
  covers one map operation per charge.
