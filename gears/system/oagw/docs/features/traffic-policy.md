# Feature: Traffic Policy Enforcement

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Non-Applicability and Deferrals](#15-non-applicability-and-deferrals)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy Request Under Traffic Policy](#proxy-request-under-traffic-policy)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Chain Ordering](#chain-ordering)
  - [Rate Limit Token Bucket Evaluation](#rate-limit-token-bucket-evaluation)
  - [Hierarchical Rate Limit Merge](#hierarchical-rate-limit-merge)
  - [CORS Preflight Detection and Response](#cors-preflight-detection-and-response)
  - [CORS Actual Request Validation](#cors-actual-request-validation)
  - [Required Headers Guard Evaluation](#required-headers-guard-evaluation)
  - [Auth Credential Injection](#auth-credential-injection)
  - [Policy on Streaming and Upgrade Exchanges](#policy-on-streaming-and-upgrade-exchanges)
- [4. States (CDSL)](#4-states-cdsl)
  - [Rate Limit Bucket Lifecycle](#rate-limit-bucket-lifecycle)
  - [Token Cache Entry Lifecycle](#token-cache-entry-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Request/Response Plugin Chain Ordering](#requestresponse-plugin-chain-ordering)
  - [Token-Bucket Rate Limiting](#token-bucket-rate-limiting)
  - [Rate Limit Response Headers and Rejection](#rate-limit-response-headers-and-rejection)
  - [Hierarchical Rate Limit Merge](#hierarchical-rate-limit-merge-1)
  - [Per-Instance Rate Limit Counters](#per-instance-rate-limit-counters)
  - [CORS Preflight Fast Path](#cors-preflight-fast-path)
  - [CORS Actual Request Enforcement](#cors-actual-request-enforcement)
  - [Required Headers Guard Enforcement](#required-headers-guard-enforcement)
  - [Auth Credential Injection and Isolation](#auth-credential-injection-and-isolation)
  - [Token Cache Isolation and Bounds](#token-cache-isolation-and-bounds)
  - [Policy Evaluation on Streaming and Upgrade Exchanges](#policy-evaluation-on-streaming-and-upgrade-exchanges)
  - [Unimplemented Plugin No-Op](#unimplemented-plugin-no-op)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-tp-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-traffic-policy`
## 1. Feature Context

### 1.1 Overview

This feature wraps the proxy request path with the gateway's cross-cutting traffic policy: token-bucket rate limiting, CORS preflight and actual-request handling, the required-headers guard, and auth-plugin credential injection, invoked in a fixed order around the request path that HTTP Request Proxying resolves and forwards.

### 1.2 Purpose

The gateway must protect upstreams from overload, let browser-based clients call the proxy safely, let operators enforce header contracts without a bespoke plugin, and inject credentials into outbound requests without ever exposing them. This feature owns invoking that policy layer on every proxy request and CORS preflight; it consumes the plugin catalog and binding validation that `cpt-cf-oagw-feature-plugin-management` owns and the resolved request path that `cpt-cf-oagw-feature-proxy-http` owns, without redefining either.

**Requirements**: `cpt-cf-oagw-fr-rate-limiting`, `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-nfr-input-validation`, `cpt-cf-oagw-fr-plugin-system` (`p2`, matching the PRD's own tag and unchecked state for that ID)

**Principles**: `cpt-cf-oagw-principle-cred-isolation`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxy request (and, for browser clients, the CORS preflight) that this feature's policy layer evaluates before and after the upstream call. |
| `cpt-cf-oagw-actor-tenant-admin` | Configures tenant-scoped rate limit, CORS, guard, and auth bindings within the sharing modes permitted by ancestor tenants. |
| `cpt-cf-oagw-actor-platform-operator` | Configures system-wide or `enforce`-shared rate limit and CORS policy that descendant tenants cannot loosen. |
| `cpt-cf-oagw-actor-cred-store` | Resolves the credential references the bound auth plugin supplies, by UUID reference, at request time. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-proxy-http` (owns the request path — alias resolution, route matching, endpoint selection, header transformation, forwarding — that this feature wraps), `cpt-cf-oagw-feature-plugin-management` (owns the plugin catalog, immutable custom plugin definitions, and binding validation this feature invokes but does not redefine), `cpt-cf-oagw-feature-proxy-streaming` (defers rate limiting, CORS, and auth-injection policy on its SSE and WebSocket upgrade exchanges to this feature, per section 3's streaming/upgrade policy subsection)

### 1.5 Non-Applicability and Deferrals

- **No user interface**: this feature enforces policy on a data-plane HTTP path with no rendered surface, so UX and accessibility requirements do not apply.
- **No regulated or personal data of its own**: this feature evaluates policy against request/response metadata (headers, origin, method) and injects credentials it resolves by reference; it does not itself store or process regulated or personal data beyond what an upstream integration's own traffic carries.
- **`queue` and `degrade` rate-limit strategies deferred**: as detailed in section 3's Rate Limit Token Bucket Evaluation, `reject` is the only strategy exercised in this configuration; `queue` and `degrade` are accepted as configuration values but resolve to `reject` semantics because neither a bounded wait duration nor a definition of reduced functionality is specified anywhere upstream of this feature.
- **Tenant-hierarchy deferral (`cpt-cf-oagw-algo-tp-rate-limit-hierarchical-merge`)**: this gear has no access to a tenant-hierarchy source in this configuration, so the hierarchical rate-limit merge described in section 3 — walking from the requesting tenant toward the root, taking the minimum of the descendant's own limit and every `inherit`- or `enforce`-shared ancestor limit — is not served. Rate limiting in this configuration therefore operates within the requesting tenant's own configured limit only; there is no ancestor level to walk, and a tenant's `rate_limit.sharing` value is accepted and stored (per `cpt-cf-oagw-feature-upstream-management`) without being acted upon at request time. Enable/disable and alias resolution are likewise scoped to the calling tenant only, for the same reason — see `cpt-cf-oagw-feature-upstream-management`'s §1.5.
- **Auth credential injection and token cache deferred (`cpt-cf-oagw-algo-tp-auth-credential-injection`)**: neither credential-store integration, token exchange, nor a token cache is implemented in this configuration. There is no code path that resolves a `cred_store` reference, exchanges credentials for authorization material, or stores/evicts a cache entry; every bound auth plugin is therefore invoked as the documented no-op described in `cpt-cf-oagw-dod-tp-plugin-noop`, and no request in this configuration ever has credential material injected into it. `token_cache_ttl_secs` and `token_cache_capacity` are accepted, typed configuration values (`cpt-cf-oagw-dod-gf-config`) that nothing currently consumes. `cpt-cf-oagw-nfr-credential-isolation`'s guarantee — that two tenants or two subjects never share credential material — holds trivially in this configuration precisely because no credential material is ever resolved, cached, or handled at all.
- **Required-headers guard configuration surface deviation (`cpt-cf-oagw-algo-tp-required-headers-guard`)**: `cpt-cf-oagw-adr-required-headers-guard-plugin` (ADR-0009) places `required_request_headers` and `required_response_headers` under a per-binding `config` object on each `plugins.items[]` entry. The frozen `upstream.v1.schema.json` and `route.v1.schema.json` define `plugins.items` as a flat array of plain identifier strings (a GTS identifier or a UUID) with no per-entry `config` object, and the upstream object's `additionalProperties: false` rules out adding a new top-level field to carry one either. In this configuration the guard therefore reads its two comma-separated values from the upstream's `auth.config` object instead of a per-binding `config` object — the only free-form object the frozen schema admits. This is a deliberate, reviewed deviation from ADR-0009's original placement, forced by the frozen schema; the guard's 400-request / 502-response status split is unchanged.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-proxy-request`, `cpt-cf-oagw-usecase-rate-limit-exceeded`

### Proxy Request Under Traffic Policy

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-tp-proxy-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request within the effective rate limit, from an allowed CORS origin and method when CORS is enabled, carrying every required request header, and completing credential injection is forwarded to the upstream and returned with rate-limit and (when CORS is enabled) CORS response headers attached.
- A CORS preflight request is answered directly by the policy layer without reaching upstream resolution or the plugin chain.

**Error Scenarios**:
- The request exceeds the effective rate limit under the `reject` strategy.
- The request's origin or method is not allowed by the resolved upstream/route's CORS configuration.
- The request is missing a header named in the guard's `required_request_headers` configuration.
- The upstream's response is missing a header named in the guard's `required_response_headers` configuration.

**Steps**:
1. [ ] - `p1` - Application Developer sends `{METHOD} /oagw/v1/proxy/{alias}/{path}`, or `OPTIONS` for a browser preflight - `inst-tp-flow-send`
2. [ ] - `p1` - **IF** the request is a CORS preflight (`OPTIONS` carrying both `Origin` and `Access-Control-Request-Method`) - `inst-tp-flow-if-preflight`
   1. [ ] - `p1` - **RETURN** the 204 response produced by CORS Preflight Detection and Response, without resolving an upstream or evaluating the plugin chain - `inst-tp-flow-return-preflight`
3. [ ] - `p1` - **ELSE** - `inst-tp-flow-else`
   1. [ ] - `p1` - Resolve the upstream and route per HTTP Request Proxying (`cpt-cf-oagw-feature-proxy-http`) - `inst-tp-flow-resolve`
   2. [ ] - `p1` - Evaluate the request against Rate Limit Token Bucket Evaluation for the effective, hierarchically merged rate-limit configuration - `inst-tp-flow-eval-rate-limit`
   3. [ ] - `p1` - **IF** the bucket cannot satisfy the request's cost and the effective strategy is `reject` - `inst-tp-flow-if-rate-rejected`
      1. [ ] - `p1` - **RETURN** 429 with `Retry-After` and the `X-RateLimit-*` headers - `inst-tp-flow-return-429`
   4. [ ] - `p1` - **IF** CORS is enabled for the resolved upstream/route - `inst-tp-flow-if-cors-enabled`
      1. [ ] - `p1` - Evaluate CORS Actual Request Validation against the request's `Origin` and method - `inst-tp-flow-eval-cors`
      2. [ ] - `p1` - **IF** the origin or method is not allowed - `inst-tp-flow-if-cors-disallowed`
         1. [ ] - `p1` - **RETURN** 403 - `inst-tp-flow-return-403`
   5. [ ] - `p1` - Execute Chain Ordering's request phase: Auth, then Guards, then Transforms - `inst-tp-flow-exec-chain-request`
   6. [ ] - `p1` - **IF** a request-phase Guard rejects the request (for example, a missing required request header) - `inst-tp-flow-if-guard-reject-request`
      1. [ ] - `p1` - **RETURN** the guard's rejection status (400 for a missing required request header) - `inst-tp-flow-return-guard-request`
   7. [ ] - `p1` - Forward the transformed request to the upstream per HTTP Request Proxying - `inst-tp-flow-forward`
   8. [ ] - `p1` - Execute Chain Ordering's response phase: Transforms, then Guards - `inst-tp-flow-exec-chain-response`
   9. [ ] - `p1` - **IF** a response-phase Guard rejects the response (for example, a missing required response header) - `inst-tp-flow-if-guard-reject-response`
      1. [ ] - `p1` - **RETURN** 502 - `inst-tp-flow-return-guard-response`
   10. [ ] - `p1` - **RETURN** the upstream's response with `X-RateLimit-*` and, when CORS is enabled, `Access-Control-*` and `Vary: Origin` response headers attached - `inst-tp-flow-return-success`

## 3. Processes / Business Logic (CDSL)

Internal middleware invoked around every proxy request and preflight. Presented as ordered algorithms because their execution order and header contracts are relied on by other features and by clients.

### Chain Ordering

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-chain-ordering`

**Input**: The effective bound plugin chain (one Auth plugin, zero or more Guards, zero or more Transforms) resolved by `cpt-cf-oagw-feature-plugin-management` for the merged upstream/route/tenant configuration, and the request/response being processed.

**Output**: The request forwarded to the upstream (or an early rejection from the request phase), and the final response returned to the caller.

**Steps**:
1. [ ] - `p1` - Parse the effective plugin chain resolved for the merged configuration - `inst-tp-chain-parse`
2. [ ] - `p1` - On the request path, invoke Auth Credential Injection first, ahead of every Guard and Transform - `inst-tp-chain-auth-first`
3. [ ] - `p1` - **FOR EACH** bound Guard, in declared order - `inst-tp-chain-foreach-guard-request`
   1. [ ] - `p1` - Invoke the guard's request-phase check; a rejection short-circuits the chain immediately, skipping remaining guards, all transforms, and the upstream call, and returns the guard's status directly - `inst-tp-chain-guard-request-check`
4. [ ] - `p1` - **FOR EACH** bound Transform, in declared order - `inst-tp-chain-foreach-transform-request`
   1. [ ] - `p1` - Apply the transform's request-phase mutation - `inst-tp-chain-transform-request-apply`
5. [ ] - `p1` - Forward the mutated request to the upstream (owned by `cpt-cf-oagw-feature-proxy-http`) - `inst-tp-chain-forward`
6. [ ] - `p1` - On the response path, reached only when the upstream call completes, **FOR EACH** bound Transform, in declared order - `inst-tp-chain-foreach-transform-response`
   1. [ ] - `p1` - Apply the transform's response-phase mutation - `inst-tp-chain-transform-response-apply`
7. [ ] - `p1` - **FOR EACH** bound Guard, in declared order - `inst-tp-chain-foreach-guard-response`
   1. [ ] - `p1` - Invoke the guard's response-phase check; a rejection short-circuits and returns the guard's status directly - `inst-tp-chain-guard-response-check`
8. [ ] - `p1` - **TRY** - `inst-tp-chain-try`
   1. [ ] - `p1` - Invoke the plugin bound at the current chain position normally - `inst-tp-chain-invoke-normal`
9. [ ] - `p1` - **CATCH** the bound plugin is a custom plugin definition with no served implementation in this configuration - `inst-tp-chain-catch-no-impl`
   1. [ ] - `p1` - Treat the invocation as a documented no-op and continue the chain at the next position, rather than raising an error - `inst-tp-chain-noop-continue`
10. [ ] - `p1` - **RETURN** the forwarded request (request phase) or the final response (response phase) - `inst-tp-chain-return`

Response-phase order is the mirror image of request-phase order: Guards run before Transforms on the request, Transforms run before Guards on the response, keeping the chain symmetric around the upstream call.

This chain order realizes `cpt-cf-oagw-fr-plugin-system`'s execution-order requirement (Auth -> Guards -> Transform(request) -> Upstream call -> Transform(response/error)); the response-phase Guard pass added at step 7 extends that PRD text, which does not itself mention a response-phase guard, and is a documented extension needed to support the required-headers guard's response phase per `cpt-cf-oagw-adr-required-headers-guard-plugin`.

### Rate Limit Token Bucket Evaluation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-rate-limit-evaluation`

**Input**: The effective, hierarchically merged rate-limit configuration (sustained rate, window, burst capacity, cost, scope, strategy) for the resolved upstream/route/tenant, and the request being evaluated.

**Output**: Allow, with remaining/reset accounting, or a strategy-specific outcome when the bucket cannot satisfy the request's cost.

**Steps**:
1. [ ] - `p1` - Resolve the effective scope key from the configured scope: `global` uses one counter for the whole gear instance, `tenant` keys by tenant id, `user` keys by authenticated subject id, `ip` keys by client IP, `route` keys by the matched route id - `inst-tp-rl-resolve-scope`
2. [ ] - `p1` - Look up the token bucket counter for that scope key, creating and seeding it to full burst capacity on first use - `inst-tp-rl-lookup-bucket`
3. [ ] - `p1` - Refill the bucket by elapsed time since its last update, at the sustained rate, capped at the burst capacity — which defaults to the sustained rate when not explicitly configured - `inst-tp-rl-refill`
4. [ ] - `p1` - Resolve the request's cost, defaulting to 1 when not explicitly configured - `inst-tp-rl-resolve-cost`
5. [ ] - `p1` - **IF** the bucket holds at least `cost` tokens - `inst-tp-rl-if-sufficient`
   1. [ ] - `p1` - Deduct `cost` tokens and **RETURN** Allow, with `X-RateLimit-Limit` set to the sustained rate, `X-RateLimit-Remaining` set to the bucket's remaining tokens, and `X-RateLimit-Reset` set to the time the bucket next reaches full capacity - `inst-tp-rl-allow`
6. [ ] - `p1` - **ELSE** the bucket cannot satisfy the cost - `inst-tp-rl-else-insufficient`
   1. [ ] - `p1` - **IF** the effective strategy is `reject`, or is `queue` or `degrade` (both of which resolve to `reject` semantics in this configuration) - `inst-tp-rl-if-reject`
      1. [ ] - `p1` - **RETURN** 429, with `Retry-After` set to the time until the bucket holds `cost` tokens, plus the same `X-RateLimit-*` headers - `inst-tp-rl-return-429`

`queue` and `degrade` are accepted as configuration values on the rate-limit block — neither is rejected at write time — but both resolve to `reject` semantics when the bucket cannot satisfy the request's cost, because neither a bounded wait duration for `queue` nor a definition of reduced functionality for `degrade` is specified anywhere upstream of this feature. `reject` is the only strategy actually exercised in this configuration.

### Hierarchical Rate Limit Merge

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-rate-limit-hierarchical-merge`

**Input**: The rate-limit configuration at every tenant-hierarchy level from the requesting tenant to the root, each carrying its own sharing mode (`private`, `inherit`, `enforce`).

**Output**: The single effective rate-limit configuration (sustained rate and burst capacity) enforced for the request.

**Steps**:
1. [ ] - `p1` - **IF** the requesting (descendant) tenant has a configured rate-limit block - `inst-tp-merge-if-tenant-has-limit`
   1. [ ] - `p1` - Start with the requesting tenant's own configured limit as the candidate effective limit - `inst-tp-merge-start`
2. [ ] - `p1` - **ELSE** the requesting tenant has no configured rate-limit block - `inst-tp-merge-else-no-tenant-limit`
   1. [ ] - `p1` - Start with the candidate unset - `inst-tp-merge-start-unset`
3. [ ] - `p1` - **FOR EACH** ancestor level, walking from the requesting tenant toward the root - `inst-tp-merge-foreach-ancestor`
   1. [ ] - `p1` - **IF** the ancestor's sharing mode is `inherit` or `enforce` - `inst-tp-merge-if-inherit-enforce`
      1. [ ] - `p1` - **IF** the candidate is unset - `inst-tp-merge-if-candidate-unset`
         1. [ ] - `p1` - Set the candidate to this ancestor's own limit outright, since there is no defined value yet to combine it with by minimum - `inst-tp-merge-set-outright`
      2. [ ] - `p1` - **ELSE** the candidate is already set - `inst-tp-merge-else-candidate-set`
         1. [ ] - `p1` - Set the candidate to the minimum of the candidate and the ancestor's own limit - `inst-tp-merge-take-min`
   2. [ ] - `p1` - **IF** the ancestor's sharing mode is `private` - `inst-tp-merge-if-private`
      1. [ ] - `p1` - Leave the candidate unchanged; the ancestor's limit does not participate - `inst-tp-merge-unchanged`
4. [ ] - `p1` - **IF** the candidate is still unset after every ancestor has been walked (neither the tenant nor any ancestor configures a limit) - `inst-tp-merge-if-still-unset`
   1. [ ] - `p1` - **RETURN** no effective limit; no rate limiting applies to the request - `inst-tp-merge-return-none`
5. [ ] - `p1` - **RETURN** the candidate as the effective sustained rate and burst capacity enforced for the request - `inst-tp-merge-return`

Rate-limit counters backing this evaluation are held in per-gear-instance, in-memory state for this configuration, consistent with the gear's no-database posture. The cross-instance (e.g. Redis-backed) counter synchronization protocol described as a future direction for distributed accuracy is deliberately deferred: this design scopes rate-limit state to in-memory per-instance counters, and the graded deployment runs with no shared database or cache store for counters to synchronize through, so a single instance's local counters are the only available state.

### CORS Preflight Detection and Response

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-cors-preflight`

**Input**: An inbound request to the proxy path.

**Output**: Either a terminal 204 preflight response, or a determination that the request is not a preflight and continues to normal processing.

**Steps**:
1. [ ] - `p1` - Parse the request's method and headers - `inst-tp-cors-pf-parse`
2. [ ] - `p1` - **IF** the method is `OPTIONS` **AND** both an `Origin` header and an `Access-Control-Request-Method` header are present - `inst-tp-cors-pf-if-preflight`
   1. [ ] - `p1` - Treat the request as a CORS preflight - `inst-tp-cors-pf-treat`
   2. [ ] - `p1` - Skip upstream resolution, tenant-context resolution, authentication, and the plugin chain entirely - `inst-tp-cors-pf-skip`
   3. [ ] - `p1` - Build a 204 No Content response echoing the request's `Origin` value in `Access-Control-Allow-Origin`, the requested method (from `Access-Control-Request-Method`) in `Access-Control-Allow-Methods`, and the requested headers (from `Access-Control-Request-Headers`) in `Access-Control-Allow-Headers` - `inst-tp-cors-pf-build-204`
   4. [ ] - `p1` - Set `Access-Control-Max-Age: 86400` - `inst-tp-cors-pf-max-age`
   5. [ ] - `p1` - Set `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` - `inst-tp-cors-pf-vary`
   6. [ ] - `p1` - **RETURN** the 204 response - `inst-tp-cors-pf-return`
3. [ ] - `p1` - **ELSE** - `inst-tp-cors-pf-else`
   1. [ ] - `p1` - **RETURN** not-a-preflight; continue with normal request processing - `inst-tp-cors-pf-continue`

### CORS Actual Request Validation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-cors-actual-request`

**Input**: The resolved upstream/route's effective CORS configuration (`enabled`, `allowed_origins`, `allowed_methods`, `expose_headers`, `allow_credentials`) and the actual (non-preflight) request's `Origin` header and method.

**Output**: Allow, with response headers to attach after the upstream call, or a 403 rejection.

**Steps**:
1. [ ] - `p1` - **REQUIRE** CORS is enabled for the resolved upstream/route; otherwise skip this evaluation entirely - `inst-tp-cors-ar-require-enabled`
2. [ ] - `p1` - Compare the request's `Origin` value against each configured allowed origin using exact string comparison of scheme, host, and port — no pattern, prefix, or suffix matching - `inst-tp-cors-ar-compare-origin`
3. [ ] - `p1` - **IF** the origin does not exactly match any allowed origin - `inst-tp-cors-ar-if-origin-mismatch`
   1. [ ] - `p1` - **RETURN** 403 with the `origin_not_allowed` problem type defined in `cpt-cf-oagw-adr-cors` - `inst-tp-cors-ar-403-origin`
4. [ ] - `p1` - **IF** the request method is not in `allowed_methods` - `inst-tp-cors-ar-if-method-mismatch`
   1. [ ] - `p1` - **RETURN** 403 with the distinct `method_not_allowed` problem type defined in `cpt-cf-oagw-adr-cors` - `inst-tp-cors-ar-403-method`
5. [ ] - `p1` - Allow the request to proceed to the plugin chain and upstream forwarding - `inst-tp-cors-ar-allow`
6. [ ] - `p1` - After the upstream responds, set `Access-Control-Allow-Origin` to the matched origin, `Access-Control-Expose-Headers` to the configured `expose_headers`, `Access-Control-Allow-Credentials: true` when `allow_credentials` is configured, and add `Vary: Origin` - `inst-tp-cors-ar-set-headers`
7. [ ] - `p1` - **RETURN** the response with these headers attached - `inst-tp-cors-ar-return`

A wildcard `allowed_origins` entry combined with `allow_credentials: true` is rejected when the CORS configuration is written, not evaluated here. CORS is configured on the upstream only in this configuration — the frozen Route wire contract defines a `cors` object in its schema but never references it from Route's own properties, so there is no route-level CORS surface to validate against; this evaluation can assume the upstream configuration it reads is already internally consistent.

The origin-not-allowed and method-not-allowed rejections above carry the two distinct problem-detail types `cpt-cf-oagw-adr-cors` defines for CORS actual-request failures; each response's `detail` names which check failed.

### Required Headers Guard Evaluation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-required-headers-guard`

**Input**: The comma-separated `required_request_headers` and `required_response_headers` configuration values bound to the guard, and the header set of the phase currently being checked (request or response).

**Output**: Allow, or a rejection naming the first missing header.

**Steps**:
1. [ ] - `p1` - Select the configuration value for the current phase: `required_request_headers` for the request phase, `required_response_headers` for the response phase - `inst-tp-rhg-select`
2. [ ] - `p1` - **IF** the selected value is absent, or entirely blank after trimming - `inst-tp-rhg-if-blank`
   1. [ ] - `p1` - **RETURN** Allow; the phase is a no-op - `inst-tp-rhg-return-noop`
3. [ ] - `p1` - **ELSE** split the value on commas, trim each entry, lowercase each entry, and drop empty entries - `inst-tp-rhg-parse`
4. [ ] - `p1` - **FOR EACH** remaining header name, in the order declared in the configuration - `inst-tp-rhg-foreach`
   1. [ ] - `p1` - Check the phase's header set for that name, matching case-insensitively - `inst-tp-rhg-check`
   2. [ ] - `p1` - **IF** the name is not present - `inst-tp-rhg-if-missing`
      1. [ ] - `p1` - **RETURN** a rejection naming that header, with status 400 for the request phase and 502 for the response phase, both under the same error code, and stop scanning without checking any further names - `inst-tp-rhg-return-reject`
5. [ ] - `p1` - **RETURN** Allow; every configured name was found - `inst-tp-rhg-return-allow`

Only header presence is checked; header values are never inspected. `required_request_headers` and `required_response_headers` are configured and evaluated independently, so an upstream can enforce one phase without the other.

**Deviation from `cpt-cf-oagw-adr-required-headers-guard-plugin`**: ADR-0009 places these two keys under a per-binding `config` object on the guard's `plugins.items[]` entry. The frozen `upstream.v1.schema.json` and `route.v1.schema.json` define `plugins.items` as a flat array of plain identifier strings, with no per-entry `config` object, and the upstream object's `additionalProperties: false` admits no new top-level field to carry one either. In this configuration the guard reads `required_request_headers`/`required_response_headers` from the upstream's `auth.config` object instead — the only free-form object the frozen schema admits — rather than from a per-binding plugin `config`. This is a deliberate, reviewed deviation forced by the frozen schema, not an oversight; the 400-request / 502-response split above is unaffected by it.

### Auth Credential Injection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-auth-credential-injection`

**Input**: The bound auth plugin's identity and configuration (including `cred_store` reference fields) for the resolved upstream/route/tenant, and the outbound request being prepared.

**Output**: The request with authorization material injected, or a rejection when credential resolution fails.

**Steps**:
1. [ ] - `p1` - Resolve the bound auth plugin from the effective configuration (identifier resolution and binding validation owned by `cpt-cf-oagw-feature-plugin-management`) - `inst-tp-auth-resolve-plugin`
2. [ ] - `p1` - **IF** the bound auth plugin is a catalog identifier with no served implementation in this configuration - `inst-tp-auth-if-no-impl`
   1. [ ] - `p1` - Treat the invocation as a documented no-op and continue the chain without injecting anything - `inst-tp-auth-noop`
3. [ ] - `p1` - **ELSE** the bound plugin has a served implementation - `inst-tp-auth-else-has-impl`
   1. [ ] - `p1` - Derive a cache key from the combination of tenant identity, subject identity, and the plugin's own configuration, so that two tenants or two subjects never resolve to the same cache entry - `inst-tp-auth-derive-key`
   2. [ ] - `p1` - Look up the derived key in the bounded token cache (capacity `token_cache_capacity`) - `inst-tp-auth-lookup-cache`
   3. [ ] - `p1` - **IF** a cache entry exists and its stored key matches the lookup key - `inst-tp-auth-if-hit`
      1. [ ] - `p1` - Inject the cached authorization material into the outbound request and **RETURN** success - `inst-tp-auth-inject-cached`
   4. [ ] - `p1` - **ELSE** (cache miss, including a stored-key mismatch treated as a miss) - `inst-tp-auth-else-miss`
      1. [ ] - `p1` - Resolve each configured credential reference from the credential store (`cpt-cf-oagw-actor-cred-store`) by its reference - `inst-tp-auth-resolve-creds`
      2. [ ] - `p1` - **TRY** - `inst-tp-auth-try`
         1. [ ] - `p1` - Exchange or otherwise derive the authorization material (for example, a bearer token) using the resolved credentials - `inst-tp-auth-exchange`
      3. [ ] - `p1` - **CATCH** the exchange fails - `inst-tp-auth-catch-fail`
         1. [ ] - `p1` - **RETURN** a failure without caching anything, so the next request for the same key retries - `inst-tp-auth-return-fail`
      4. [ ] - `p1` - Compute the derived lifetime as the smaller of `token_cache_ttl_secs` and the derived material's own reported lifetime minus a 30-second safety margin, per `min(config_ttl, expires_in - 30s)` as specified in `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` - `inst-tp-auth-compute-ttl`
      5. [ ] - `p1` - **IF** the derived lifetime is zero or negative (the material's reported lifetime is at or under the 30-second margin) - `inst-tp-auth-if-lifetime-nonpositive`
         1. [ ] - `p1` - Use the material for the current request without storing it in the token cache - `inst-tp-auth-use-uncached`
      6. [ ] - `p1` - **ELSE** store the result in the token cache under the derived key with the computed expiry, evicting per the cache's bounded-capacity policy when at `token_cache_capacity` - `inst-tp-auth-store-cache`
      7. [ ] - `p1` - Inject the authorization material into the outbound request and **RETURN** success - `inst-tp-auth-inject-fresh`
4. [ ] - `p1` - **NEVER** persist, log, or include resolved secret values or derived authorization material anywhere other than the forwarded request's authorization material — not in gateway logs, not in responses, and not in error bodies - `inst-tp-auth-never-persist`

### Policy on Streaming and Upgrade Exchanges

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-tp-streaming-upgrade-policy`

**Input**: An SSE or WebSocket-upgrade request proxied per `cpt-cf-oagw-feature-proxy-streaming`, and the traffic policy (rate limiting, CORS, required-headers guard, auth injection) bound to the resolved upstream/route.

**Output**: The same policy enforcement this feature already applies to a bounded request/response, adapted to a request that establishes a long-lived stream or upgraded connection rather than completing in one exchange.

`cpt-cf-oagw-feature-proxy-streaming` defers rate limiting, CORS, and auth-plugin credential injection on its SSE and WebSocket exchanges to this feature; this feature is otherwise written entirely for a bounded request/response, so this subsection states how each policy applies to the establishing request instead.

**Steps**:
1. [ ] - `p1` - Evaluate rate limiting, CORS (when enabled), the request-phase required-headers guard, and auth credential injection exactly once, at request time, before the stream or upgrade is established — never per relayed event or per frame - `inst-tp-stream-policy-once`
2. [ ] - `p1` - Charge the token bucket once, for the establishing request's cost, rather than once per SSE event or once per WebSocket frame - `inst-tp-stream-policy-charge-once`
3. [ ] - `p1` - **IF** the establishing request is allowed and the upstream answers `101 Switching Protocols` - `inst-tp-stream-policy-if-101`
   1. [ ] - `p1` - Attach no `X-RateLimit-*` headers to the `101 Switching Protocols` response; those headers attach only to a rejected response or to an ordinary (non-upgraded) response - `inst-tp-stream-policy-no-headers-101`
4. [ ] - `p1` - **ELSE** (a rejected response, or an ordinary SSE response) - `inst-tp-stream-policy-else-ordinary`
   1. [ ] - `p1` - Attach `X-RateLimit-*` and, when CORS is enabled, `Access-Control-*` and `Vary: Origin` headers as this feature already defines for a bounded response - `inst-tp-stream-policy-headers-ordinary`
5. [ ] - `p1` - Apply CORS actual-request validation to the establishing request's `Origin` and method only; there is no per-event or per-frame CORS check - `inst-tp-stream-policy-cors-establishing`
6. [ ] - `p1` - Evaluate the required-headers guard's response phase against the upstream's response headers (for an SSE stream) or handshake response headers (for a WebSocket upgrade); once relaying or frame relay begins, there is no further response phase to evaluate - `inst-tp-stream-policy-guard-response-phase`
7. [ ] - `p1` - **RETURN** the policy-evaluated outcome (forwarded and established, or rejected before establishment) - `inst-tp-stream-policy-return`

## 4. States (CDSL)

### Rate Limit Bucket Lifecycle

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-tp-bucket-lifecycle`

**States**: Unseeded, Full, Partial, Empty

**Initial State**: Unseeded

**Transitions**:
1. [ ] - `p1` - **FROM** Unseeded **TO** Full **WHEN** a scope key is looked up for the first time and its bucket is created and seeded to full burst capacity - `inst-tp-bucket-state-01`
2. [ ] - `p1` - **FROM** Full **TO** Partial **WHEN** a request deducts tokens, leaving at least one but fewer than burst capacity - `inst-tp-bucket-state-02`
3. [ ] - `p1` - **FROM** Partial **TO** Empty **WHEN** a request deducts the bucket's remaining tokens, or a request's cost cannot be satisfied - `inst-tp-bucket-state-03`
4. [ ] - `p1` - **FROM** Empty **TO** Partial **WHEN** elapsed-time refill restores at least one token but fewer than burst capacity - `inst-tp-bucket-state-04`
5. [ ] - `p1` - **FROM** Partial **TO** Full **WHEN** elapsed-time refill restores the bucket to burst capacity - `inst-tp-bucket-state-05`

This bucket state lives for the lifetime of the gear instance process, held per-instance and in-memory per `cpt-cf-oagw-dod-tp-rate-limit-per-instance`; it is not persisted and does not survive a restart.

### Token Cache Entry Lifecycle

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-tp-token-cache-lifecycle`

**States**: Absent, Cached, Expired, Evicted

**Initial State**: Absent

**Transitions**:
1. [ ] - `p1` - **FROM** Absent **TO** Cached **WHEN** a credential exchange succeeds and the derived lifetime minus the 30-second safety margin is positive, so the result is stored under the derived key - `inst-tp-tokencache-state-01`
2. [ ] - `p1` - **FROM** Cached **TO** Expired **WHEN** the computed expiry (`min(config_ttl, expires_in - 30s)`) elapses without the entry being evicted first - `inst-tp-tokencache-state-02`
3. [ ] - `p1` - **FROM** Cached **TO** Evicted **WHEN** the cache is at `token_cache_capacity` and storing a new entry evicts this one under the cache's bounded-capacity policy - `inst-tp-tokencache-state-03`
4. [ ] - `p1` - **FROM** Expired **TO** Absent **WHEN** the next lookup for that key treats the expired entry as a miss - `inst-tp-tokencache-state-04`
5. [ ] - `p1` - **FROM** Evicted **TO** Absent **WHEN** the next lookup for that key treats the evicted entry as a miss - `inst-tp-tokencache-state-05`

A credential exchange failure never transitions Absent to Cached, per `cpt-cf-oagw-dod-tp-token-cache`'s prohibition on caching a failed fetch; a derived lifetime at or under the 30-second margin also leaves that request's material at Absent rather than transitioning to Cached.

## 5. Definitions of Done

### Request/Response Plugin Chain Ordering

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-chain-ordering`

The system **MUST** invoke the bound plugin chain in the order Auth, then Guards, then Transforms on the request, and Transforms, then Guards, on the response, short-circuiting on the first Guard rejection in either phase and skipping the remainder of that phase, all subsequent phases, and the upstream call when the rejection occurs on the request phase.

**Implements**:
- `cpt-cf-oagw-flow-tp-proxy-request`
- `cpt-cf-oagw-algo-tp-chain-ordering`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `PluginsConfig`

### Token-Bucket Rate Limiting

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tp-rate-limit-token-bucket`

The system **MUST** evaluate every proxy request against a token-bucket rate limit using the effective sustained rate and window, a burst capacity defaulting to the sustained rate, and a per-request cost defaulting to 1, keyed by the configured scope (`global`, `tenant`, `user`, `ip`, or `route`). The system **MUST** accept `reject`, `queue`, and `degrade` as configured strategy values, but in this configuration **MUST** resolve an empty bucket under any of the three to `reject` semantics (429 with `Retry-After`): `queue` and `degrade` are deliberately deferred because neither a bounded wait duration nor a definition of reduced functionality is specified anywhere upstream of this feature.

**Implements**:
- `cpt-cf-oagw-flow-tp-proxy-request`
- `cpt-cf-oagw-algo-tp-rate-limit-evaluation`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `RateLimitConfig`

### Rate Limit Response Headers and Rejection

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-rate-limit-headers`

The system **MUST** attach `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` to every rate-limit-evaluated response, and **MUST** additionally attach `Retry-After` and answer 429 when the `reject` strategy applies to a request the bucket cannot satisfy.

**Implements**:
- `cpt-cf-oagw-flow-tp-proxy-request`
- `cpt-cf-oagw-algo-tp-rate-limit-evaluation`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `RateLimitConfig`

### Hierarchical Rate Limit Merge

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-rate-limit-hierarchical-merge`

The system **MUST** merge rate limits across the tenant hierarchy by taking the minimum of the descendant's own limit and every ancestor limit whose sharing mode is `inherit` or `enforce`, and **MUST** leave an ancestor limit whose sharing mode is `private` out of the merge so the descendant's own value applies unchanged. When the requesting tenant has no configured rate-limit block, the system **MUST** start the candidate unset and adopt the first `inherit`- or `enforce`-shared ancestor limit reached outright, rather than combining it by minimum with an undefined value; when neither the tenant nor any ancestor configures a limit, the system **MUST** apply no rate limiting to the request. This gear has no access to a tenant-hierarchy source in this configuration, so the ancestor walk this merge depends on is not served: the effective limit in this configuration is always the requesting tenant's own configured limit (or no limit, when the tenant configures none), and `rate_limit.sharing` is accepted and stored without being acted upon.

**Implements**:
- `cpt-cf-oagw-algo-tp-rate-limit-hierarchical-merge`

**Constraints**: None

**Touches**:
- Entities: `RateLimitConfig`

### Per-Instance Rate Limit Counters

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-rate-limit-per-instance`

The system **MUST** hold rate-limit token bucket counters as per-gear-instance, in-memory state, with no cross-instance counter synchronization, for this configuration.

**Implements**:
- `cpt-cf-oagw-algo-tp-rate-limit-evaluation`

**Constraints**: None

**Touches**:
- Entities: `RateLimitConfig`

### CORS Preflight Fast Path

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tp-cors-preflight`

The system **MUST** detect a CORS preflight (`OPTIONS` carrying both `Origin` and `Access-Control-Request-Method`) and answer it with 204 before upstream resolution and without authentication or plugin-chain evaluation, echoing the requested origin, method, and headers, and **MUST** set `Access-Control-Max-Age: 86400` and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` on that response.

**Implements**:
- `cpt-cf-oagw-flow-tp-proxy-request`
- `cpt-cf-oagw-algo-tp-cors-preflight`

**Constraints**: None

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}/{path}`
- Entities: `CorsConfig`

### CORS Actual Request Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tp-cors-actual-request`

The system **MUST** reject an actual request whose origin does not exactly match a configured allowed origin (scheme, host, and port all significant, no pattern matching) with 403 carrying the `origin_not_allowed` problem type, **MUST** reject an actual request whose method is not in `allowed_methods` with 403 carrying the distinct `method_not_allowed` problem type (both types defined in `cpt-cf-oagw-adr-cors`, with the `detail` naming which check failed), and otherwise **MUST** attach `Access-Control-Allow-Origin`, `Access-Control-Expose-Headers`, `Access-Control-Allow-Credentials` (when configured), and `Vary: Origin` to the forwarded response.

**Implements**:
- `cpt-cf-oagw-flow-tp-proxy-request`
- `cpt-cf-oagw-algo-tp-cors-actual-request`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `CorsConfig`

### Required Headers Guard Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tp-required-headers-guard`

The system **MUST** enforce `required_request_headers` and `required_response_headers` independently, treating an absent or all-blank configuration value as a no-op for that phase, **MUST** check header names case-insensitively in the order declared after trimming, lowercasing, and dropping empty entries, and **MUST** reject on the first missing name only, with 400 for the request phase and 502 for the response phase, both under the same error code. In this configuration these two values are read from the upstream's `auth.config` object rather than from a per-binding plugin `config` object, a deliberate, reviewed deviation from `cpt-cf-oagw-adr-required-headers-guard-plugin` forced by the frozen schema's flat `plugins.items` array and the upstream object's `additionalProperties: false`; the 400/502 split itself is unaffected.

**Implements**:
- `cpt-cf-oagw-flow-tp-proxy-request`
- `cpt-cf-oagw-algo-tp-required-headers-guard`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `PluginsConfig`

### Auth Credential Injection and Isolation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-auth-credential-injection`

The system **MUST** invoke the bound auth plugin to resolve its configured credential references and inject the resulting authorization material into the forwarded request, and **MUST NOT** ever persist, log, or return resolved secret values or derived authorization material in responses, logs, or error bodies. Neither credential-store integration nor token exchange is implemented in this configuration: there is no code path that resolves a `cred_store` reference or exchanges credentials for authorization material, so every bound auth plugin is invoked as the documented no-op of `cpt-cf-oagw-dod-tp-plugin-noop` instead, and no request has credential material injected. The never-persist/never-log guarantee holds trivially in this configuration precisely because no credential material is ever resolved or handled.

**Implements**:
- `cpt-cf-oagw-flow-tp-proxy-request`
- `cpt-cf-oagw-algo-tp-auth-credential-injection`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `PluginsConfig`

### Token Cache Isolation and Bounds

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-token-cache`

The system **MUST** key the auth token cache so that two tenants or two subjects never share an entry, **MUST** bound the cache to `token_cache_capacity` entries, **MUST** expire each entry at `min(config_ttl, expires_in - 30s)` — the smaller of `token_cache_ttl_secs` and the token's own reported lifetime minus a 30-second safety margin, per `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` — and **MUST NOT** cache a failed token fetch. When the derived lifetime minus the margin is zero or negative, the system **MUST** use the resolved material for the current request without storing it in the cache. No token cache is implemented in this configuration: with no credential-store integration or token exchange (see `cpt-cf-oagw-dod-tp-auth-credential-injection`), there is nothing to cache, so `token_cache_ttl_secs` and `token_cache_capacity` are accepted, typed configuration values (`cpt-cf-oagw-dod-gf-config`) that nothing currently consumes. The isolation guarantee holds trivially in this configuration precisely because no credential material is ever cached at all.

**Implements**:
- `cpt-cf-oagw-algo-tp-auth-credential-injection`

**Constraints**: None

**Touches**:
- Entities: `PluginsConfig`

### Policy Evaluation on Streaming and Upgrade Exchanges

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-streaming-upgrade-policy`

The system **MUST** evaluate rate limiting, CORS, the request-phase required-headers guard, and auth credential injection exactly once per SSE or WebSocket-upgrade request, before the stream or upgrade is established, and **MUST** charge the token bucket once for that establishing request rather than per relayed event or per frame. The system **MUST NOT** attach `X-RateLimit-*` headers to a `101 Switching Protocols` response, **MUST** apply CORS actual-request validation to the establishing request only, and **MUST** evaluate the required-headers guard's response phase against the upstream's response or handshake headers, with no further response phase once relaying or frame relay begins.

**Implements**:
- `cpt-cf-oagw-algo-tp-streaming-upgrade-policy`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `RateLimitConfig`, `CorsConfig`, `PluginsConfig`

### Unimplemented Plugin No-Op

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-tp-plugin-noop`

The system **MUST** treat invocation of a bound plugin that has no served implementation in this configuration as a documented no-op at whichever chain position it is bound, rather than raising an error or failing the request.

**Implements**:
- `cpt-cf-oagw-algo-tp-chain-ordering`
- `cpt-cf-oagw-algo-tp-auth-credential-injection`

**Constraints**: None

**Touches**:
- Entities: `PluginsConfig`

## 6. Acceptance Criteria

- [ ] A proxy request that exceeds the effective sustained rate under the `reject` strategy answers 429 and carries a `Retry-After` header.
- [ ] An allowed proxy request evaluated against a configured rate limit carries `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` response headers.
- [ ] An `OPTIONS` request carrying `Origin` and `Access-Control-Request-Method` against a CORS-enabled upstream answers 204 with the request's origin echoed in `Access-Control-Allow-Origin`, `Access-Control-Max-Age: 86400`, and the three-header `Vary` line, without any upstream resolution occurring.
- [ ] An actual request from an origin not present in `allowed_origins` answers 403 with the `origin_not_allowed` problem type.
- [ ] An actual request using a method not present in `allowed_methods` answers 403 with the distinct `method_not_allowed` problem type.
- [ ] A proxy request missing a header named in `required_request_headers` answers 400.
- [ ] An upstream response missing a header named in `required_response_headers` answers 502.
- [ ] A resource whose ancestor tenant shares a rate limit with `sharing: enforce` and whose descendant tenant configures a stricter own limit enforces the descendant's stricter value (the minimum of the two), while a `private` ancestor limit does not affect the descendant's own value.
- [ ] A requesting tenant with no configured rate-limit block, whose nearest ancestor shares a limit with `sharing: enforce`, enforces that ancestor's limit outright; a request chain with no configured limit at the tenant or any ancestor level is not rate limited.
- [ ] A second request within the same (tenant, subject, auth-configuration) key inside the token cache TTL is served from the cache and does not trigger a second credential-store resolution or token exchange.
- [ ] A failed token exchange is not cached, so credential resolution is retried on the next request for the same cache key.
- [ ] A forced credential-resolution or token-exchange failure produces an error response and a log record containing neither the resolved credential value nor any derived authorization material.
- [ ] A rate-limit configuration with `strategy: queue` or `strategy: degrade` is accepted at write time, and a request that exhausts the bucket under either configured strategy answers 429, the same as `reject`.
- [ ] A cached OAuth2 token's expiry never exceeds `expires_in - 30s`; a token whose reported lifetime is at or under 30 seconds is used for the current request but is not found in the cache on a subsequent lookup for the same key.
- [ ] A successful WebSocket upgrade's `101 Switching Protocols` response carries no `X-RateLimit-*` headers, while a rejected upgrade request carries them.
