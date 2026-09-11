# Feature: Policy and Plugins

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Credential-Injected Proxy Flow](#credential-injected-proxy-flow)
  - [Required Header Rejection Flow](#required-header-rejection-flow)
  - [Rate Limit Exceeded Flow](#rate-limit-exceeded-flow)
  - [CORS Preflight Flow](#cors-preflight-flow)
  - [CORS Actual Request Flow](#cors-actual-request-flow)
  - [Configure Policy Layer Flow](#configure-policy-layer-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Hierarchical Configuration Merge Algorithm](#hierarchical-configuration-merge-algorithm)
  - [Plugin Chain Composition Algorithm](#plugin-chain-composition-algorithm)
  - [Plugin Chain Execution Algorithm](#plugin-chain-execution-algorithm)
  - [Auth Credential Injection Algorithm](#auth-credential-injection-algorithm)
  - [OAuth2 Token Cache Algorithm](#oauth2-token-cache-algorithm)
  - [Required Headers Evaluation Algorithm](#required-headers-evaluation-algorithm)
  - [Token Bucket Admission Algorithm](#token-bucket-admission-algorithm)
  - [CORS Preflight Handling Algorithm](#cors-preflight-handling-algorithm)
  - [CORS Actual Request Validation Algorithm](#cors-actual-request-validation-algorithm)
  - [CORS Configuration Validation Algorithm](#cors-configuration-validation-algorithm)
  - [Header Rule Application Algorithm](#header-rule-application-algorithm)
  - [Observability Emission Algorithm](#observability-emission-algorithm)
- [4. States (CDSL)](#4-states-cdsl)
  - [Token Cache Entry State Machine](#token-cache-entry-state-machine)
  - [Rate Limit Bucket State Machine](#rate-limit-bucket-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Plugin Chain Composition and Execution](#plugin-chain-composition-and-execution)
  - [Auth Plugin Registry and Built-ins](#auth-plugin-registry-and-built-ins)
  - [Request-Time Credential Resolution](#request-time-credential-resolution)
  - [OAuth2 Token Cache](#oauth2-token-cache)
  - [Required Headers Guard](#required-headers-guard)
  - [Rate Limiting](#rate-limiting)
  - [CORS Preflight Fast Path](#cors-preflight-fast-path)
  - [CORS Actual Request Enforcement](#cors-actual-request-enforcement)
  - [CORS Configuration Validation](#cors-configuration-validation)
  - [Configurable Header Transformation](#configurable-header-transformation)
  - [Hierarchical Configuration Merge](#hierarchical-configuration-merge)
  - [Metrics and Audit Logging](#metrics-and-audit-logging)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)
  - [7.1 Out of Scope, With Reasons](#71-out-of-scope-with-reasons)
  - [7.2 Availability and Resilience Posture](#72-availability-and-resilience-posture)
  - [7.3 Explicit Non-Applicability](#73-explicit-non-applicability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-policy-and-plugins-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-policy-and-plugins`

## 1. Feature Context

### 1.1 Overview

This feature is the policy layer that OAGW (Outbound API Gateway) applies to a proxied request
once the plain data-plane path already resolves an upstream and forwards a call. It adds
credential injection, guards, rate limiting, CORS (Cross-Origin Resource Sharing), configurable
header rules, hierarchical configuration merging, and observability around that existing path.

### 1.2 Purpose

The bare proxy path forwards a request; it does not authenticate it to the upstream, throttle
it, validate its origin, or reshape its headers. This feature supplies those behaviours, so the
gateway matches the product described in PRD.md §1.1. It is deliberately the largest feature in
the decomposition and ships as twelve independently testable slices inside one artifact, one
slice per Definition of Done listed in §5.

**In scope**: plugin chain composition and execution; the auth plugin registry and its four
resolvable built-ins; the required-headers guard; rate limiting; built-in CORS; configurable
header transformation; hierarchical configuration; metrics and audit logging.

**Out of scope**, each with its reason, expanded in §7.1: Starlark custom-plugin execution, a
Redis-backed distributed rate-limit sync, the Redis L2 configuration cache, and the circuit
breaker.

**Requirements**: `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-fr-rate-limiting`,
`cpt-cf-oagw-fr-builtin-plugins`, `cpt-cf-oagw-fr-header-transform`,
`cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-hierarchical-config`,
`cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-nfr-observability`,
`cpt-cf-oagw-nfr-high-availability`, `cpt-cf-oagw-nfr-starlark-sandbox`

**Principles**: `cpt-cf-oagw-principle-cred-isolation`

**Constraints**: none. The decomposition entry allocates no design constraint to this feature;
body limits, the HTTPS-only posture, and multi-backend storage are owned by other features.

`cpt-cf-oagw-fr-header-transform` is split across two features. The proxy-data-plane feature
owns routing-header consumption, hop-by-hop stripping, and `Host` or `:authority` rewriting.
This feature owns only the configurable `set`, `add`, `remove`, and passthrough rules.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxied request whose credentials are injected, whose rate limit is charged, and whose origin is validated; receives the 429, 403, 400, and 502 policy rejections. |
| `cpt-cf-oagw-actor-tenant-admin` | Configures auth, CORS, and header rules on their own upstreams, and rate limits and plugin lists on their own upstreams and routes, within the sharing modes their ancestors allow. |
| `cpt-cf-oagw-actor-platform-operator` | Configures the ancestor-level policy that descendants inherit, and grants the override permissions the merge algorithm checks. |
| `cpt-cf-oagw-actor-cred-store` | Resolves every `cred://` reference to secret material at request time; denies access to secrets the calling tenant may not read. |
| `cpt-cf-oagw-actor-upstream-service` | Receives the credential-bearing outbound request and returns the response whose headers the response-phase policy inspects. |
| `cpt-cf-oagw-actor-types-registry` | Holds the GTS (Global Type System) catalog entries for identifiers such as `basic` and `bearer` that this feature deliberately refuses to resolve. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) §5.2 Proxy Execution, §5.3 Plugin System, §5.5 Configuration
  Hierarchy, §6.1 Non-Functional Requirements
- **Design**: [DESIGN.md](../DESIGN.md) §3.2 Component Model (plugin system, hierarchical
  configuration, headers transformation, secret access control), §3.3 API Contracts
  (`cpt-cf-oagw-interface-api`), §4.2 Metrics and Observability, §4.3 Audit Logging
- **ADRs**: [0002-plugin-system.md](../ADR/0002-plugin-system.md),
  `cpt-cf-oagw-adr-rate-limiting`, `cpt-cf-oagw-adr-cors`,
  `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`,
  `cpt-cf-oagw-adr-required-headers-guard-plugin`
- **Contracts**: `cpt-cf-oagw-contract-cred-store`, `cpt-cf-oagw-contract-types-registry`
- **Schemas**: [upstream.v1.schema.json](../schemas/upstream.v1.schema.json),
  [route.v1.schema.json](../schemas/route.v1.schema.json)
- **Decomposition**: `cpt-cf-oagw-feature-policy-and-plugins`
- **Dependencies**: `cpt-cf-oagw-feature-proxy-data-plane-http` (the working proxy request this
  policy layer decorates) and `cpt-cf-oagw-feature-plugin-management-api` (the plugin
  identification and storage model the chain resolves against)

This feature adds no endpoint. All behaviour below runs inside the existing proxy request cycle
at `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]`.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-rate-limit-exceeded`

Every flow except the CORS preflight runs after the proxy handler has already authenticated the
caller, resolved the alias, and matched a route. Preflight is the one path that answers before
any of that, because a browser preflight carries no credentials and therefore no tenant context.

### Credential-Injected Proxy Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-policy-credential-injected-proxy`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An `apikey` auth plugin injects the configured header on the outbound request, and the secret
  value appears in no log record and no response.
- An `oauth2_client_cred` plugin serves a cached bearer token without contacting the identity
  provider a second time within the cached time to live.
- Upstream-level plugins run before route-level ones, so a declared chain of two upstream and
  two route plugins executes in that concatenated order.

**Error Scenarios**:
- The configured auth identifier is `basic` or `bearer`, which are catalog-only, so the request
  fails with "unknown auth plugin".
- The referenced secret does not exist, giving 500 SecretNotFound.
- The referenced secret exists but is not accessible to the calling tenant, giving 401.
- A guard rejects during the request phase, so the upstream is never called.

**Steps**:
1. [ ] - `p1` - Developer sends a proxied call to an upstream that declares auth, guards, and transforms - `inst-cip-1`
2. [ ] - `p1` - API: {METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}] (client body and headers forwarded to the policy layer) - `inst-cip-2`
3. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-hierarchical-merge` to produce one effective configuration from upstream, route, and tenant layers - `inst-cip-3`
4. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-cors-actual-validation` when the request carries an `Origin` header - `inst-cip-4`
5. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-token-bucket-admission` before any credential is fetched - `inst-cip-5`
6. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-plugin-chain-compose` to resolve the auth, guard, and transform stages - `inst-cip-6`
7. [ ] - `p1` - **IF** any declared identifier fails to resolve - `inst-cip-7`
   1. [ ] - `p1` - **RETURN** 503 PluginNotFound as `application/problem+json` with `X-OAGW-Error-Source: gateway` - `inst-cip-7a`
8. [ ] - `p1` - **ELSE** - `inst-cip-8`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-auth-injection` once, before every guard - `inst-cip-8a`
   2. [ ] - `p1` - **IF** credential resolution fails - `inst-cip-8a1`
      1. [ ] - `p1` - **RETURN** 500 SecretNotFound for a missing secret, or 401 AuthenticationFailed for an inaccessible one - `inst-cip-8a2`
   3. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-plugin-chain-execute` for the guard and request-transform stages - `inst-cip-8b`
   4. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-header-rules-apply` in the request phase over the outbound header map - `inst-cip-8c`
   5. [ ] - `p1` - Forward the outbound request to `cpt-cf-oagw-actor-upstream-service` with the injected credential - `inst-cip-8d`
   6. [ ] - `p1` - Run the response half of `cpt-cf-oagw-algo-policy-plugin-chain-execute`, then the response phase of `cpt-cf-oagw-algo-policy-header-rules-apply` - `inst-cip-8e`
   7. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-observability-emit` to record metrics and one audit line - `inst-cip-8f`
   8. [ ] - `p1` - **RETURN** the upstream response with the configured response headers applied - `inst-cip-8g`

### Required Header Rejection Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-policy-required-header-rejection`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request carrying every configured header, in any letter case, reaches the upstream normally.
- An upstream whose configuration is absent or blank after trimming sees no behaviour change,
  because the guard fails open.

**Error Scenarios**:
- A configured request header is missing, so the call is rejected with 400 before the upstream
  is contacted.
- A configured response header is missing from the upstream reply, so the caller receives 502.

**Steps**:
1. [ ] - `p1` - Developer sends a proxied call to an upstream that binds the required-headers guard - `inst-rhr-1`
2. [ ] - `p1` - API: {METHOD} /oagw/v1/proxy/{alias}[/{path}] (guard reads its configuration from the bound plugin entry) - `inst-rhr-2`
3. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-required-headers-eval` in the request phase - `inst-rhr-3`
4. [ ] - `p1` - **IF** the request phase reports a missing header - `inst-rhr-4`
   1. [ ] - `p1` - **RETURN** 400 with error code `REQUIRED_HEADER_MISSING` naming only the first missing header - `inst-rhr-4a`
5. [ ] - `p1` - **ELSE** - `inst-rhr-5`
   1. [ ] - `p1` - Forward the request and await the upstream response - `inst-rhr-5a`
   2. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-required-headers-eval` in the response phase - `inst-rhr-5b`
   3. [ ] - `p1` - **IF** the response phase reports a missing header - `inst-rhr-5c`
      1. [ ] - `p1` - **RETURN** 502 with error code `REQUIRED_HEADER_MISSING` naming only the first missing header - `inst-rhr-5c1`
   4. [ ] - `p1` - **ELSE** - `inst-rhr-5d`
      1. [ ] - `p1` - **RETURN** the upstream response unchanged by this guard - `inst-rhr-5d1`

### Rate Limit Exceeded Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-policy-rate-limit-exceeded`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Calls within the sustained rate are admitted and carry the three `X-RateLimit-*` headers.
- A burst up to the configured capacity is admitted before the bucket empties.

**Error Scenarios**:
- The bucket is empty under the default `reject` strategy, so the caller receives 429 with
  `Retry-After`.
- The bucket is empty under `queue`, and the wait exceeds the configured proxy timeout, so the
  request is rejected with the same 429 shape.

**Steps**:
1. [ ] - `p1` - Developer sends proxied calls faster than the configured sustained rate - `inst-rle-1`
2. [ ] - `p1` - API: {METHOD} /oagw/v1/proxy/{alias}[/{path}] (each call charges the configured cost) - `inst-rle-2`
3. [ ] - `p1` - Run `cpt-cf-oagw-algo-policy-token-bucket-admission` against the per-instance limiter for the configured scope - `inst-rle-3`
4. [ ] - `p1` - **IF** the bucket holds at least the configured cost - `inst-rle-4`
   1. [ ] - `p1` - Deduct the cost and continue the proxy request with `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` attached to the eventual response - `inst-rle-4a`
5. [ ] - `p1` - **ELSE IF** strategy is `reject` - `inst-rle-5`
   1. [ ] - `p1` - Emit a WARN audit record naming the scope key, never the caller's credentials - `inst-rle-5a`
   2. [ ] - `p1` - **RETURN** 429 RateLimitExceeded with `Retry-After` and the three `X-RateLimit-*` headers - `inst-rle-5b`
6. [ ] - `p1` - **ELSE IF** strategy is `queue` - `inst-rle-6`
   1. [ ] - `p1` - Hold the request until enough tokens accumulate or the proxy timeout elapses - `inst-rle-6a`
   2. [ ] - `p1` - **RETURN** the same 429 shape when the timeout wins the race - `inst-rle-6b`
7. [ ] - `p1` - **ELSE** (strategy is `degrade`) - `inst-rle-7`
   1. [ ] - `p1` - Admit the request, mark the proxy context degraded, report `X-RateLimit-Remaining: 0`, and emit a WARN audit record - `inst-rle-7a`

### CORS Preflight Flow

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-policy-cors-preflight`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A browser preflight receives 204 with the echoed origin, echoed method, and
  `Access-Control-Max-Age: 86400`, without any upstream being resolved.

**Error Scenarios**:
- An `OPTIONS` call that lacks `Origin` or `Access-Control-Request-Method` is not a preflight and
  falls through to ordinary proxy handling, including its authentication requirement.

**Steps**:
1. [ ] - `p2` - Browser sends the preflight on behalf of the developer's page - `inst-cpf-1`
2. [ ] - `p2` - API: OPTIONS /oagw/v1/proxy/{alias}[/{path}] with `Origin` and `Access-Control-Request-Method` - `inst-cpf-2`
3. [ ] - `p2` - Run `cpt-cf-oagw-algo-policy-cors-preflight` at handler entry, before alias resolution - `inst-cpf-3`
4. [ ] - `p2` - **IF** the three preflight conditions all hold - `inst-cpf-4`
   1. [ ] - `p2` - **RETURN** 204 No Content with the echoed CORS headers and no body - `inst-cpf-4a`
5. [ ] - `p2` - **ELSE** - `inst-cpf-5`
   1. [ ] - `p2` - **RETURN** control to ordinary proxy handling for a normal `OPTIONS` request - `inst-cpf-5a`

### CORS Actual Request Flow

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-policy-cors-actual-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A cross-origin call from an allowed origin and method reaches the upstream, and the response
  carries `Access-Control-Allow-Origin` and `Vary: Origin`.

**Error Scenarios**:
- The origin is absent from `allowed_origins`, so the call is rejected with 403 before forwarding.
- The method is absent from `allowed_methods`, so the call is rejected with 403 before forwarding.
- The origin differs only by port or by scheme, which still fails, because matching is exact.

**Steps**:
1. [ ] - `p2` - Browser sends the actual cross-origin call with an `Origin` header - `inst-car-1`
2. [ ] - `p2` - API: {METHOD} /oagw/v1/proxy/{alias}[/{path}] with `Origin` and the caller's Bearer token - `inst-car-2`
3. [ ] - `p2` - Resolve the upstream and merge configuration, so a tenant-scoped CORS policy exists - `inst-car-3`
4. [ ] - `p2` - Run `cpt-cf-oagw-algo-policy-cors-actual-validation` before forwarding - `inst-car-4`
5. [ ] - `p2` - **IF** the origin is not allowed - `inst-car-5`
   1. [ ] - `p2` - **RETURN** 403 with type `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` - `inst-car-5a`
6. [ ] - `p2` - **ELSE IF** the method is not allowed - `inst-car-6`
   1. [ ] - `p2` - **RETURN** 403 with type `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` - `inst-car-6a`
7. [ ] - `p2` - **ELSE** - `inst-car-7`
   1. [ ] - `p2` - Forward the request, then add the configured CORS response headers plus `Vary: Origin` - `inst-car-7a`
   2. [ ] - `p2` - **RETURN** the upstream response to the browser - `inst-car-7b`

### Configure Policy Layer Flow

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-policy-configure-policy-layer`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A descendant tenant sets a stricter rate limit than its ancestor, and the stricter value wins.
- A descendant appends plugins to an inherited chain and keeps every enforced ancestor plugin.
- A descendant adds an origin under `inherit`, and the effective list is the union of both.

**Error Scenarios**:
- A configuration sets `allow_credentials` together with a wildcard origin and is rejected at
  configuration-validation time.
- A merge under `inherit` would union a wildcard origin into a credential-bearing policy, and the
  merged result is rejected by the same rule.
- A descendant without `oagw:upstream:override_auth` tries to replace an inherited credential.

**Steps**:
1. [ ] - `p2` - Admin submits an upstream document carrying `auth`, `rate_limit`, `plugins`, `cors`, `headers`, and `tags`, or a route document carrying only `rate_limit`, `plugins`, and `tags`, since the Route schema defines no `auth`, `cors`, or `headers` property - `inst-cpl-1`
2. [ ] - `p2` - API: POST /oagw/v1/upstreams or PUT /oagw/v1/upstreams/{id} (or the equivalent route endpoint for `rate_limit`, `plugins`, and `tags`; policy fields validated by this feature) - `inst-cpl-2`
3. [ ] - `p2` - Run `cpt-cf-oagw-algo-policy-cors-config-validation` over the submitted `cors` block - `inst-cpl-3`
4. [ ] - `p2` - **IF** validation reports the wildcard-with-credentials combination - `inst-cpl-4`
   1. [ ] - `p2` - **RETURN** 400 ValidationError naming the offending field pair - `inst-cpl-4a`
5. [ ] - `p2` - **ELSE** - `inst-cpl-5`
   1. [ ] - `p2` - Store the document through the management surface owned by the upstream and route features - `inst-cpl-5a`
   2. [ ] - `p2` - Run `cpt-cf-oagw-algo-policy-hierarchical-merge` on the next proxied request that selects it - `inst-cpl-5b`
   3. [ ] - `p2` - **RETURN** the effective configuration the merge produced, applied to that request - `inst-cpl-5c`

## 3. Processes / Business Logic (CDSL)

### Hierarchical Configuration Merge Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-hierarchical-merge`

**Input**: The selected upstream document, the matched route document, the calling tenant's own
upstream and route documents for that alias — found by the alias-shadowing walk from descendant
toward root that PRD.md §5.5 defines, where the closest match wins — the ancestor chain walked
from that same descendant toward the root, and the caller's granted permissions

**Output**: One effective configuration holding auth, rate limit, plugins, CORS, headers, and tags,
with auth, CORS, and headers drawn only from Upstream documents, because Route defines none of
those three properties

**Steps**:
1. [ ] - `p1` - Layer the documents by priority: the upstream document the alias-shadowing walk resolves is the base, its matched route document overrides it for the fields the Route schema defines, and the calling tenant's own upstream and route documents override both wherever that same walk finds the calling tenant closer than the ancestor who owns the base documents; an ancestor field marked `enforce` still applies regardless of which tenant the walk selects - `inst-hm-1`
2. [ ] - `p1` - State the schema reality before merging any field: `auth`, `headers`, and `cors` are Upstream-level fields only, since `route.v1.schema.json` defines no such property; only `plugins`, `rate_limit`, and `tags` exist at the Route layer and participate in the merge there - `inst-hm-2`
3. [ ] - `p1` - **FOR EACH** field carrying a sharing mode — `auth` and `cors`, read from the Upstream documents only, plus `rate_limit` and `plugins`, read from whichever of the Upstream and Route documents defines each - `inst-hm-3`
   1. [ ] - `p1` - **IF** the ancestor sharing mode is `private` - `inst-hm-3a`
      1. [ ] - `p1` - Discard the ancestor value; only the descendant's own value applies - `inst-hm-3a1`
   2. [ ] - `p1` - **ELSE IF** the mode is `inherit` - `inst-hm-3b`
      1. [ ] - `p1` - Use the descendant value when present, otherwise fall back to the ancestor value - `inst-hm-3b1`
   3. [ ] - `p1` - **ELSE** (the mode is `enforce`) - `inst-hm-3c`
      1. [ ] - `p1` - Keep the ancestor value active and forbid any descendant value that would relax it - `inst-hm-3c1`
4. [ ] - `p1` - **IF** the descendant supplies its own `auth` over an `inherit` ancestor - `inst-hm-4`
   1. [ ] - `p1` - **IF** the caller lacks `oagw:upstream:override_auth` - `inst-hm-4a`
      1. [ ] - `p1` - Keep the ancestor credential reference unchanged - `inst-hm-4a1`
   2. [ ] - `p1` - **ELSE** - `inst-hm-4b`
      1. [ ] - `p1` - Adopt the descendant credential reference - `inst-hm-4b1`
5. [ ] - `p1` - Normalize every candidate sustained rate to tokens per second, so windows of different units compare correctly - `inst-hm-5`
6. [ ] - `p1` - Set the effective sustained rate to the minimum across the descendant value and every enforced ancestor value - `inst-hm-6`
7. [ ] - `p1` - Set the effective burst capacity to the minimum across the same set, independently of the sustained rate - `inst-hm-7`
8. [ ] - `p1` - Concatenate plugin lists ancestor-first, then descendant, drawing entries from each tenant's Upstream and Route documents in turn, and keeping declaration order inside each list - `inst-hm-8`
9. [ ] - `p1` - **IF** a descendant list omits a plugin an ancestor marked `enforce` - `inst-hm-9`
   1. [ ] - `p1` - Re-insert that plugin at its ancestor position, because enforced plugins cannot be removed - `inst-hm-9a`
10. [ ] - `p1` - **IF** the effective CORS sharing mode is `inherit` - `inst-hm-10`
    1. [ ] - `p1` - Union the ancestor and descendant `allowed_origins` and `allowed_methods`, dropping duplicates - `inst-hm-10a`
11. [ ] - `p1` - **ELSE IF** the mode is `enforce` - `inst-hm-11`
    1. [ ] - `p1` - Keep the ancestor CORS values and discard descendant additions - `inst-hm-11a`
12. [ ] - `p1` - Union the ancestor and descendant tags unconditionally, drawing from each tenant's Upstream and Route documents alike, because tags carry no sharing mode and are add-only - `inst-hm-12`
13. [ ] - `p1` - Re-run `cpt-cf-oagw-algo-policy-cors-config-validation` over the merged CORS block - `inst-hm-13`
14. [ ] - `p1` - **RETURN** the effective configuration consumed by every other algorithm in this feature - `inst-hm-14`

### Plugin Chain Composition Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-plugin-chain-compose`

**Input**: The effective configuration's upstream and route plugin lists, the auth, guard, and
transform registries

**Output**: An ordered chain split into one auth slot, a guard stage, and a transform stage, or a
resolution error

**Steps**:
1. [ ] - `p1` - Read the upstream plugin items in declaration order - `inst-pcc-1`
2. [ ] - `p1` - Read the route plugin items in declaration order - `inst-pcc-2`
3. [ ] - `p1` - Concatenate the two lists upstream-first, so two upstream and two route entries execute as upstream one, upstream two, route one, route two - `inst-pcc-3`
4. [ ] - `p1` - **FOR EACH** entry in the concatenated list - `inst-pcc-4`
   1. [ ] - `p1` - **IF** the entry is the required-headers guard identifier - `inst-pcc-4a`
      1. [ ] - `p1` - Resolve it from the guard registry and append it to the guard stage - `inst-pcc-4a1`
   2. [ ] - `p1` - **ELSE IF** the entry is the request-id transform identifier - `inst-pcc-4b`
      1. [ ] - `p1` - Resolve it from the transform registry and append it to the transform stage - `inst-pcc-4b1`
   3. [ ] - `p1` - **ELSE IF** the entry is a catalog-only identifier such as `timeout`, `cors`, `logging`, or `metrics` - `inst-pcc-4c`
      1. [ ] - `p1` - **RETURN** 503 PluginNotFound, because those identifiers name core data-plane behaviour and are not registry-resolvable - `inst-pcc-4c1`
   4. [ ] - `p1` - **ELSE IF** the entry is a custom plugin identifier - `inst-pcc-4d`
      1. [ ] - `p1` - **RETURN** 503 PluginNotFound, because no Starlark runtime is enabled in this build - `inst-pcc-4d1`
   5. [ ] - `p1` - **ELSE** - `inst-pcc-4e`
      1. [ ] - `p1` - **RETURN** 503 PluginNotFound naming the unresolved identifier - `inst-pcc-4e1`
5. [ ] - `p1` - Resolve the single auth slot from the effective `auth.type` value, leaving it empty when no auth is configured - `inst-pcc-5`
6. [ ] - `p1` - **RETURN** the composed chain with its stage boundaries fixed - `inst-pcc-6`

### Plugin Chain Execution Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-plugin-chain-execute`

**Input**: The composed chain, the mutable request context, and later the response or error context

**Output**: A forwarded request and a returned response, or the first rejection the chain produced

**Steps**:
1. [ ] - `p1` - Run the auth slot exactly once, before any guard, so credentials exist for every later stage - `inst-pce-1`
2. [ ] - `p1` - **FOR EACH** guard in the guard stage, in composed order - `inst-pce-2`
   1. [ ] - `p1` - Call its request-phase check against the current request context - `inst-pce-2a`
   2. [ ] - `p1` - **IF** the guard rejects - `inst-pce-2b`
      1. [ ] - `p1` - **RETURN** the guard's status and error code without calling the upstream at all - `inst-pce-2b1`
3. [ ] - `p1` - **FOR EACH** transform in the transform stage, in composed order - `inst-pce-3`
   1. [ ] - `p1` - Call its request-phase hook, allowing it to mutate headers, path, or query - `inst-pce-3a`
4. [ ] - `p1` - Hand the mutated request to the data-plane forwarder and await the outcome - `inst-pce-4`
5. [ ] - `p1` - **IF** the forwarder produced a response - `inst-pce-5`
   1. [ ] - `p1` - **FOR EACH** guard in composed order, call its response-phase check - `inst-pce-5a`
   2. [ ] - `p1` - **IF** a guard rejects in the response phase - `inst-pce-5b`
      1. [ ] - `p1` - **RETURN** that rejection instead of the upstream response - `inst-pce-5b1`
   3. [ ] - `p1` - **FOR EACH** transform in composed order, call its response-phase hook - `inst-pce-5c`
6. [ ] - `p1` - **ELSE** - `inst-pce-6`
   1. [ ] - `p1` - **FOR EACH** transform in composed order, call its error-phase hook instead of the response hook - `inst-pce-6a`
7. [ ] - `p1` - **RETURN** the response or the error, with the composed order preserved and never reversed - `inst-pce-7`

### Auth Credential Injection Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-auth-injection`

**Input**: The effective `auth` block, the credential store handle, and the outbound request builder

**Output**: An outbound request carrying the credential, or a credential error

**Steps**:
1. [ ] - `p1` - **IF** no `auth` block is configured, or its type names the `noop` plugin - `inst-aci-1`
   1. [ ] - `p1` - **RETURN** the outbound request unchanged - `inst-aci-1a`
2. [ ] - `p1` - Look the type identifier up in the auth registry, which holds exactly `noop`, `apikey`, `oauth2_client_cred`, and `oauth2_client_cred_basic` - `inst-aci-2`
3. [ ] - `p1` - **IF** the lookup misses, which is always the case for the catalog-only `basic` and `bearer` identifiers - `inst-aci-3`
   1. [ ] - `p1` - **RETURN** 503 PluginNotFound whose detail reads "unknown auth plugin" followed by the offending identifier - `inst-aci-3a`
4. [ ] - `p1` - **IF** the resolved plugin is `apikey` - `inst-aci-4`
   1. [ ] - `p1` - Read the placement, which is either `header` or `query`, and the parameter name from the plugin configuration - `inst-aci-4a`
   2. [ ] - `p1` - Resolve the `cred://` reference through `cpt-cf-oagw-contract-cred-store` at request time, never from a stored copy - `inst-aci-4b`
   3. [ ] - `p1` - **IF** the store reports the secret does not exist - `inst-aci-4c`
      1. [ ] - `p1` - **RETURN** 500 SecretNotFound - `inst-aci-4c1`
   4. [ ] - `p1` - **ELSE IF** the store reports the secret is not accessible to the calling tenant - `inst-aci-4d`
      1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed - `inst-aci-4d1`
   5. [ ] - `p1` - **ELSE** - `inst-aci-4e`
      1. [ ] - `p1` - Place the secret in the named header, or as the named query parameter, on the outbound request only - `inst-aci-4e1`
5. [ ] - `p1` - **ELSE IF** the resolved plugin is either OAuth2 client-credentials variant - `inst-aci-5`
   1. [ ] - `p1` - Delegate to `cpt-cf-oagw-algo-policy-oauth2-token-cache` and inject the bearer value it returns - `inst-aci-5a`
6. [ ] - `p1` - Hold the resolved material only for the lifetime of the request, and never write it to the resource store - `inst-aci-6`
7. [ ] - `p1` - Exclude the material from every metric label, audit field, error detail, and response body - `inst-aci-7`
8. [ ] - `p1` - **RETURN** the outbound request carrying the credential - `inst-aci-8`

### OAuth2 Token Cache Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-oauth2-token-cache`

**Input**: The plugin configuration, the client-auth-method variant, the security context, and the
in-process token cache

**Output**: A bearer token value ready for injection, or a credential error

**Steps**:
1. [ ] - `p1` - Parse the configuration, requiring exactly one of the token endpoint or the issuer URL, plus both credential references - `inst-otc-1`
2. [ ] - `p1` - **IF** both endpoint forms are present, or both are absent - `inst-otc-2`
   1. [ ] - `p1` - **RETURN** a configuration error mapped to 400 ValidationError - `inst-otc-2a`
3. [ ] - `p1` - Build the cache key by joining the subject tenant identifier, the subject identifier, the auth-method tag, and a deterministic hash of the sorted configuration pairs with colons - `inst-otc-3`
4. [ ] - `p1` - Look the key up in the cache - `inst-otc-4`
5. [ ] - `p1` - **IF** an entry is returned and the key it stores equals the lookup key - `inst-otc-5`
   1. [ ] - `p1` - **RETURN** the cached token, so no identity-provider call is made - `inst-otc-5a`
6. [ ] - `p1` - **ELSE IF** an entry is returned whose stored key differs - `inst-otc-6`
   1. [ ] - `p1` - Treat the hit as a miss, because a hash collision must never leak another tenant's token - `inst-otc-6a`
7. [ ] - `p1` - Resolve the client identifier and the client secret through the credential store, applying the same 500 and 401 mapping as the injection algorithm - `inst-otc-7`
8. [ ] - `p1` - **TRY** - `inst-otc-8`
   1. [ ] - `p1` - API: POST to the token endpoint, placing credentials in the form body for the Form variant and in the `Authorization` header for the Basic variant - `inst-otc-8a`
9. [ ] - `p1` - **CATCH** an exchange failure - `inst-otc-9`
   1. [ ] - `p1` - **RETURN** the error without caching anything, so the next request retries the identity provider - `inst-otc-9a`
10. [ ] - `p1` - Compute the time to live as the smaller of the configured ceiling, whose default is 300 seconds, and the reported lifetime minus a 30-second safety margin - `inst-otc-10`
11. [ ] - `p1` - **IF** the computed time to live is zero or negative - `inst-otc-11`
    1. [ ] - `p1` - **RETURN** the token for immediate use without storing it - `inst-otc-11a`
12. [ ] - `p1` - **ELSE** - `inst-otc-12`
    1. [ ] - `p1` - Store an entry that carries its own key alongside the token, inside a cache whose default capacity is 10,000 entries - `inst-otc-12a`
13. [ ] - `p1` - **RETURN** the token; an upstream 401 later in the request never triggers a retry of the original call - `inst-otc-13`

### Required Headers Evaluation Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-required-headers-eval`

**Input**: The phase, the guard's configuration map, and the header set for that phase

**Output**: An allow decision, or a reject decision carrying a status and the first missing name

**Steps**:
1. [ ] - `p1` - Select `required_request_headers` in the request phase and `required_response_headers` in the response phase - `inst-rhe-1`
2. [ ] - `p1` - **IF** the selected key is absent from the configuration - `inst-rhe-2`
   1. [ ] - `p1` - **RETURN** allow, because an unconfigured phase fails open - `inst-rhe-2a`
3. [ ] - `p1` - Split the value on commas, trim each entry, lowercase it, and drop entries that became empty - `inst-rhe-3`
4. [ ] - `p1` - **IF** the resulting list is empty, which covers a value that was only commas and spaces - `inst-rhe-4`
   1. [ ] - `p1` - **RETURN** allow, because a blank-after-trim configuration is a no-op - `inst-rhe-4a`
5. [ ] - `p1` - **FOR EACH** required name, in list order - `inst-rhe-5`
   1. [ ] - `p1` - **IF** the header set has no entry matching that name case-insensitively - `inst-rhe-5a`
      1. [ ] - `p1` - **RETURN** reject with 400 in the request phase, or 502 in the response phase, error code `REQUIRED_HEADER_MISSING`, and only this first name in the detail - `inst-rhe-5a1`
6. [ ] - `p1` - **RETURN** allow, having checked presence only and never any header value - `inst-rhe-6`

### Token Bucket Admission Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-token-bucket-admission`

**Input**: The effective rate-limit block, the request's scope identifiers, and the current instant

**Output**: An admit decision with rate-limit headers, or a rejection

**Steps**:
1. [ ] - `p1` - **IF** the effective configuration carries no rate limit - `inst-tba-1`
   1. [ ] - `p1` - **RETURN** admit with no rate-limit headers - `inst-tba-1a`
2. [ ] - `p1` - Convert the sustained window enum to seconds, mapping second to 1, minute to 60, hour to 3600, and day to 86400 - `inst-tba-2`
3. [ ] - `p1` - Compute the refill rate as the sustained rate divided by those window seconds - `inst-tba-3`
4. [ ] - `p1` - Set the capacity to the configured burst capacity, defaulting to the sustained rate when it is absent - `inst-tba-4`
5. [ ] - `p1` - Build the counter key from the scope enum, which selects a constant, the tenant, the caller, the client address, or the matched route, defaulting to tenant - `inst-tba-5`
6. [ ] - `p1` - Look up or create the limiter for that key in per-instance local state, with no cross-node synchronization - `inst-tba-6`
7. [ ] - `p1` - **IF** the algorithm is `token_bucket` - `inst-tba-7`
   1. [ ] - `p1` - Refill by adding elapsed seconds times the refill rate, clamped to the capacity, then record the new instant - `inst-tba-7a`
   2. [ ] - `p1` - Set the admission test to whether the available tokens are at least the configured cost, whose default is 1 - `inst-tba-7b`
8. [ ] - `p1` - **ELSE** (the algorithm is `sliding_window`) - `inst-tba-8`
   1. [ ] - `p1` - Count the cost already charged inside the trailing window and set the admission test to whether that count plus this cost stays within the sustained rate - `inst-tba-8a`
9. [ ] - `p1` - Compute the reset instant as the epoch second when the limiter next admits a request of this cost - `inst-tba-9`
10. [ ] - `p1` - **IF** the admission test passes - `inst-tba-10`
    1. [ ] - `p1` - Charge the cost and **RETURN** admit with `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` - `inst-tba-10a`
11. [ ] - `p1` - **ELSE IF** the strategy is `reject`, which is the default - `inst-tba-11`
    1. [ ] - `p1` - **RETURN** 429 RateLimitExceeded with the three headers plus `Retry-After` in whole seconds, never below one - `inst-tba-11a`
12. [ ] - `p1` - **ELSE IF** the strategy is `queue` - `inst-tba-12`
    1. [ ] - `p1` - Wait for the reset instant, bounded by the configured proxy timeout, then re-test once - `inst-tba-12a`
    2. [ ] - `p1` - **RETURN** admit on success, or the same 429 shape when the bound elapses first - `inst-tba-12b`
13. [ ] - `p1` - **ELSE** (the strategy is `degrade`) - `inst-tba-13`
    1. [ ] - `p1` - **RETURN** admit with the context marked degraded and `X-RateLimit-Remaining` reported as zero - `inst-tba-13a`

### CORS Preflight Handling Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-policy-cors-preflight`

**Input**: The inbound request at proxy handler entry

**Output**: A 204 preflight response, or a signal to continue ordinary handling

**Steps**:
1. [ ] - `p2` - Test whether the method is `OPTIONS` - `inst-cpa-1`
2. [ ] - `p2` - Test whether an `Origin` header is present - `inst-cpa-2`
3. [ ] - `p2` - Test whether an `Access-Control-Request-Method` header is present - `inst-cpa-3`
4. [ ] - `p2` - **IF** any of the three tests fails - `inst-cpa-4`
   1. [ ] - `p2` - **RETURN** the continue signal, leaving the request to ordinary proxy handling - `inst-cpa-4a`
5. [ ] - `p2` - Echo the request origin into `Access-Control-Allow-Origin` - `inst-cpa-5`
6. [ ] - `p2` - Echo the requested method into `Access-Control-Allow-Methods` - `inst-cpa-6`
7. [ ] - `p2` - **IF** `Access-Control-Request-Headers` is present - `inst-cpa-7`
   1. [ ] - `p2` - Echo its value into `Access-Control-Allow-Headers` - `inst-cpa-7a`
8. [ ] - `p2` - Add `Access-Control-Max-Age: 86400` and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` - `inst-cpa-8`
9. [ ] - `p2` - **RETURN** 204 No Content with an empty body, having resolved no upstream, required no tenant context, and run no plugin - `inst-cpa-9`

### CORS Actual Request Validation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-policy-cors-actual-validation`

**Input**: The resolved request, its `Origin` header, and the effective CORS block

**Output**: A forward decision with response headers, or a 403 rejection

**Steps**:
1. [ ] - `p2` - **IF** the request carries no `Origin` header - `inst-cav-1`
   1. [ ] - `p2` - **RETURN** forward, because the call is not cross-origin - `inst-cav-1a`
2. [ ] - `p2` - **IF** the effective CORS block is absent or disabled - `inst-cav-2`
   1. [ ] - `p2` - **RETURN** forward with no CORS response headers, which is the deny-by-default posture a browser then enforces - `inst-cav-2a`
3. [ ] - `p2` - **FOR EACH** configured allowed origin - `inst-cav-3`
   1. [ ] - `p2` - Compare it to the request origin by exact string equality, treating a single asterisk entry as matching any origin - `inst-cav-3a`
4. [ ] - `p2` - **IF** no entry matched, including entries differing only by port or by scheme - `inst-cav-4`
   1. [ ] - `p2` - **RETURN** 403 with type `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, before any byte reaches the upstream - `inst-cav-4a`
5. [ ] - `p2` - **IF** the request method is absent from `allowed_methods`, whose default is GET and POST - `inst-cav-5`
   1. [ ] - `p2` - **RETURN** 403 with type `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` - `inst-cav-5a`
6. [ ] - `p2` - Forward the request, then set `Access-Control-Allow-Origin` to the request origin - `inst-cav-6`
7. [ ] - `p2` - **IF** `expose_headers` is configured - `inst-cav-7`
   1. [ ] - `p2` - Set `Access-Control-Expose-Headers` to that list - `inst-cav-7a`
8. [ ] - `p2` - **IF** `allow_credentials` is enabled - `inst-cav-8`
   1. [ ] - `p2` - Set `Access-Control-Allow-Credentials` to true - `inst-cav-8a`
9. [ ] - `p2` - Always add `Vary: Origin`, so a shared cache cannot serve one origin's response to another - `inst-cav-9`
10. [ ] - `p2` - **RETURN** the response carrying those headers - `inst-cav-10`

### CORS Configuration Validation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-policy-cors-config-validation`

**Input**: A CORS block, either as submitted on an upstream or route, or as produced by the merge

**Output**: A pass result, or a validation error naming the offending fields

**Steps**:
1. [ ] - `p2` - **IF** the block is absent - `inst-ccv-1`
   1. [ ] - `p2` - **RETURN** pass, because CORS is optional and disabled by default - `inst-ccv-1a`
2. [ ] - `p2` - **IF** the `enabled` field is missing - `inst-ccv-2`
   1. [ ] - `p2` - **RETURN** a validation error, because the schema marks it required - `inst-ccv-2a`
3. [ ] - `p2` - **IF** `allow_credentials` is true and `allowed_origins` contains a single asterisk entry - `inst-ccv-3`
   1. [ ] - `p2` - **RETURN** a validation error stating that credentials cannot combine with a wildcard origin - `inst-ccv-3a`
4. [ ] - `p2` - **FOR EACH** configured origin that is not the asterisk - `inst-ccv-4`
   1. [ ] - `p2` - **IF** it lacks a scheme or a host, or carries a path, query, or partial wildcard - `inst-ccv-4a`
      1. [ ] - `p2` - **RETURN** a validation error, because origin matching is exact and no pattern syntax exists - `inst-ccv-4a1`
5. [ ] - `p2` - **RETURN** pass; the same checks re-run after every merge, since a union can introduce a wildcard from an ancestor - `inst-ccv-5`

### Header Rule Application Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-policy-header-rules-apply`

**Input**: The phase, the effective `headers.request` or `headers.response` block, and the header map

**Output**: The transformed header map, or a validation error on a malformed well-known header

**Steps**:
1. [ ] - `p1` - Start from the header map the proxy-data-plane feature already stripped of routing and hop-by-hop headers - `inst-hra-1`
2. [ ] - `p1` - **IF** the phase is the request phase - `inst-hra-2`
   1. [ ] - `p1` - **IF** the passthrough mode is `none`, which is the default - `inst-hra-2a`
      1. [ ] - `p1` - Drop every inbound header, keeping only what later rules add - `inst-hra-2a1`
   2. [ ] - `p1` - **ELSE IF** the mode is `allowlist` - `inst-hra-2b`
      1. [ ] - `p1` - Keep only the names listed in `passthrough_allowlist`, compared case-insensitively - `inst-hra-2b1`
   3. [ ] - `p1` - **ELSE** (the mode is `all`) - `inst-hra-2c`
      1. [ ] - `p1` - Keep every inbound header that survived the earlier stripping - `inst-hra-2c1`
3. [ ] - `p1` - **FOR EACH** name in the `remove` list - `inst-hra-3`
   1. [ ] - `p1` - Delete all values for that name, compared case-insensitively - `inst-hra-3a`
4. [ ] - `p1` - **FOR EACH** pair in the `set` map - `inst-hra-4`
   1. [ ] - `p1` - Replace any existing values for that name with the single configured value - `inst-hra-4a`
5. [ ] - `p1` - **FOR EACH** pair in the `add` map - `inst-hra-5`
   1. [ ] - `p1` - Append the value, leaving any existing value in place so duplicates are allowed - `inst-hra-5a`
6. [ ] - `p1` - Validate well-known headers such as `Content-Length` and `Content-Type` after the rules ran - `inst-hra-6`
7. [ ] - `p1` - **IF** a well-known header is now malformed or inconsistent with the body - `inst-hra-7`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-hra-7a`
8. [ ] - `p1` - Strip any hop-by-hop header a rule re-introduced, so configuration cannot defeat the data-plane rule - `inst-hra-8`
9. [ ] - `p1` - **RETURN** the transformed map; the response phase runs the same remove, set, and add steps but has no passthrough mode - `inst-hra-9`

### Observability Emission Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-policy-observability-emit`

**Input**: The request context, the outcome, and the measured phase durations

**Output**: Updated metric series and one structured audit record

**Steps**:
1. [ ] - `p2` - Increment `oagw_requests_in_flight` labelled by host when the request enters the policy layer - `inst-obs-1`
2. [ ] - `p2` - Start a timer for each measured phase of the request - `inst-obs-2`
3. [ ] - `p2` - Normalize the method to a standard verb, or to the reserved `_OTHER` value, before using it as a label - `inst-obs-3`
4. [ ] - `p2` - Use the matched route pattern, never the raw request path, as the route label - `inst-obs-4`
5. [ ] - `p2` - On completion, decrement `oagw_requests_in_flight` and observe `oagw_request_duration_seconds` labelled by host, route, and phase - `inst-obs-5`
6. [ ] - `p2` - Increment `oagw_requests_total` labelled by host, method, route, and numeric status code - `inst-obs-6`
7. [ ] - `p2` - **IF** the outcome is an error - `inst-obs-7`
   1. [ ] - `p2` - Increment `oagw_errors_total` labelled by host, route, and error type - `inst-obs-7a`
8. [ ] - `p2` - Attach no tenant label to any series, so cardinality stays bounded - `inst-obs-8`
9. [ ] - `p2` - Emit one structured record carrying `request_id`, `tenant_id`, `method`, `path`, `status`, and `duration_ms` - `inst-obs-9`
10. [ ] - `p2` - Choose INFO for a successful request, WARN for a rate-limit rejection or emitted retry guidance, and ERROR for an upstream failure, a timeout, or an authentication failure - `inst-obs-10`
11. [ ] - `p2` - **RETURN** without ever placing a request body, a response body, or credential material in a metric or a log - `inst-obs-11`

## 4. States (CDSL)

### Token Cache Entry State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-policy-token-cache-entry`

**States**: absent, valid, expired, evicted

**Initial State**: absent

**Transitions**:
1. [ ] - `p2` - **FROM** absent **TO** valid **WHEN** a token exchange succeeds and the computed time to live is positive - `inst-tcs-1`
2. [ ] - `p2` - **FROM** absent **TO** absent **WHEN** a token exchange fails, because failed fetches are never cached - `inst-tcs-2`
3. [ ] - `p2` - **FROM** absent **TO** absent **WHEN** the computed time to live is zero or negative, so the token is injected but not stored - `inst-tcs-3`
4. [ ] - `p2` - **FROM** valid **TO** expired **WHEN** the stored time to live elapses - `inst-tcs-4`
5. [ ] - `p2` - **FROM** expired **TO** absent **WHEN** the next lookup observes expiry and drops the entry - `inst-tcs-5`
6. [ ] - `p2` - **FROM** valid **TO** evicted **WHEN** capacity pressure removes it, at which point the secret buffer is zeroed - `inst-tcs-6`
7. [ ] - `p2` - **FROM** evicted **TO** absent **WHEN** the next lookup for that key finds nothing and proceeds as a miss - `inst-tcs-7`
8. [ ] - `p2` - **FROM** valid **TO** valid **WHEN** a lookup finds a stored key different from the lookup key, so the caller is served as a miss while the entry stays untouched - `inst-tcs-8`

### Rate Limit Bucket State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-policy-rate-limit-bucket`

**States**: full, partial, depleted

**Initial State**: full

**Transitions**:
1. [ ] - `p2` - **FROM** full **TO** partial **WHEN** a request charges a cost smaller than the capacity - `inst-rbs-1`
2. [ ] - `p2` - **FROM** partial **TO** depleted **WHEN** the remaining tokens fall below the next request's cost - `inst-rbs-2`
3. [ ] - `p2` - **FROM** partial **TO** full **WHEN** refill at the sustained rate reaches the capacity clamp - `inst-rbs-3`
4. [ ] - `p2` - **FROM** depleted **TO** partial **WHEN** refill accumulates at least the next request's cost but less than the capacity - `inst-rbs-4`
5. [ ] - `p2` - **FROM** depleted **TO** depleted **WHEN** a request arrives before refill completes, producing the 429 rejection or the configured alternative strategy - `inst-rbs-5`

## 5. Definitions of Done

### Plugin Chain Composition and Execution

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-policy-plugin-chain`

The system **MUST** compose one chain per proxied request by concatenating upstream plugin items
before route plugin items, and **MUST** execute the stages in the order auth, guards, request
transforms, upstream call, then response or error transforms. A guard rejection in the request
phase **MUST** prevent the upstream call entirely. Any identifier that no registry resolves,
including catalog-only and custom identifiers, **MUST** fail with 503 PluginNotFound.

**Implements**:
- `cpt-cf-oagw-flow-policy-credential-injected-proxy`
- `cpt-cf-oagw-algo-policy-plugin-chain-compose`
- `cpt-cf-oagw-algo-policy-plugin-chain-execute`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Plugin`, `ProxyContext`

### Auth Plugin Registry and Built-ins

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-policy-auth-registry`

The system **MUST** register exactly four resolvable auth plugins — `noop`, `apikey` with header
or query placement, `oauth2_client_cred` using a form-post exchange, and
`oauth2_client_cred_basic` using a Basic-auth exchange. It **MUST** reject the catalog-only
`basic` and `bearer` identifiers with an error whose detail reads "unknown auth plugin", even
though `cpt-cf-oagw-contract-types-registry` still catalogs them.

**Implements**:
- `cpt-cf-oagw-flow-policy-credential-injected-proxy`
- `cpt-cf-oagw-algo-policy-auth-injection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Credential reference`

### Request-Time Credential Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-policy-credential-resolution`

The system **MUST** resolve every credential reference through the credential store at request
time, **MUST** return 500 SecretNotFound for a missing secret and 401 for one the calling tenant
cannot access, and **MUST NOT** store or log secret material anywhere. No metric label, audit
field, error detail, or response body may contain a resolved secret.

**Implements**:
- `cpt-cf-oagw-algo-policy-auth-injection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Credential reference`

### OAuth2 Token Cache

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-policy-oauth2-token-cache`

The system **MUST** cache access tokens in process under a key joining subject tenant, subject,
auth-method tag, and configuration hash, with a time to live of the smaller of the configured
ceiling, defaulting to 300 seconds, and the reported lifetime minus 30 seconds. Capacity
**MUST** default to 10,000 entries, each entry **MUST** carry its own key so a hash collision is
detected on hit, failed fetches **MUST NOT** be cached, and an upstream 401 **MUST NOT** cause a
retry of the original request.

**Implements**:
- `cpt-cf-oagw-algo-policy-oauth2-token-cache`
- `cpt-cf-oagw-state-policy-token-cache-entry`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Credential reference`

### Required Headers Guard

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-policy-required-headers-guard`

The system **MUST** implement the required-headers guard as the only guard identifier bindable
through a plugin list entry, reading `required_request_headers` and `required_response_headers`
independently. It **MUST** split on commas, trim, lowercase, drop empties, scan in order, and
report only the first missing header, rejecting with 400 in the request phase and 502 in the
response phase, both carrying error code `REQUIRED_HEADER_MISSING`. Absent or blank-after-trim
configuration **MUST** be a no-op.

**Implements**:
- `cpt-cf-oagw-flow-policy-required-header-rejection`
- `cpt-cf-oagw-algo-policy-required-headers-eval`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Plugin`

### Rate Limiting

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-policy-rate-limiting`

The system **MUST** enforce per-instance rate limits using a token bucket by default and a
sliding window when configured, honouring `sustained` rate and window, `burst.capacity`
defaulting to the sustained rate, `scope` defaulting to tenant, `strategy` defaulting to reject,
and `cost` defaulting to one. A rejection **MUST** be 429 with `Retry-After` and the three
`X-RateLimit-*` headers. No `budget` or `overcommit_ratio` field exists in either JSON Schema, so
the effective limit **MUST** be the plain minimum of ancestor and descendant.

**Implements**:
- `cpt-cf-oagw-flow-policy-rate-limit-exceeded`
- `cpt-cf-oagw-algo-policy-token-bucket-admission`
- `cpt-cf-oagw-state-policy-rate-limit-bucket`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Rate limiter state`

### CORS Preflight Fast Path

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-policy-cors-preflight`

The system **MUST** detect a preflight as `OPTIONS` plus `Origin` plus
`Access-Control-Request-Method`, and **MUST** answer it with a permissive 204 echoing the origin,
the requested method, and the requested headers, plus `Access-Control-Max-Age: 86400` and the
three-name `Vary` list. The fast path **MUST** resolve no upstream, require no tenant context,
and run no plugin.

**Implements**:
- `cpt-cf-oagw-flow-policy-cors-preflight`
- `cpt-cf-oagw-algo-policy-cors-preflight`

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}`
- Entities: `CORS policy`

### CORS Actual Request Enforcement

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-policy-cors-actual`

The system **MUST** validate the origin and the method of an actual cross-origin request after
upstream resolution and before forwarding, rejecting with 403 and the exact
`cors.origin_not_allowed` or `cors.method_not_allowed` GTS type. Origin matching **MUST** be
exact, port-sensitive, and protocol-sensitive, with no pattern syntax. Allowed responses **MUST**
carry the configured CORS headers and always `Vary: Origin`.

**Implements**:
- `cpt-cf-oagw-flow-policy-cors-actual-request`
- `cpt-cf-oagw-algo-policy-cors-actual-validation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `CORS policy`

### CORS Configuration Validation

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-policy-cors-config-validation`

The system **MUST** reject a CORS configuration that combines `allow_credentials` with a wildcard
origin, at configuration-validation time rather than at request time, and **MUST** re-run the
same check on the merged result so an inherited wildcard cannot slip through the union.

**Implements**:
- `cpt-cf-oagw-flow-policy-configure-policy-layer`
- `cpt-cf-oagw-algo-policy-cors-config-validation`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `CORS policy`

### Configurable Header Transformation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-policy-header-rules`

The system **MUST** apply the configured `set`, `add`, and `remove` operations plus the
`none`, `allowlist`, and `all` passthrough modes on the request side, and the `set`, `add`, and
`remove` operations on the response side, in the documented order. It **MUST NOT** duplicate the
routing-header, hop-by-hop, and authority-rewriting behaviour owned by the proxy-data-plane
feature, and **MUST** re-strip any hop-by-hop header a rule re-introduces.

**Implements**:
- `cpt-cf-oagw-algo-policy-header-rules-apply`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Header transformation rule`

### Hierarchical Configuration Merge

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-policy-hierarchical-config`

The system **MUST** merge configuration with upstream as base, route above it, and tenant above
both, honouring `private`, `inherit`, and `enforce` per field. Auth **MUST** be overridable under
`inherit` and forced under `enforce`; rate limits **MUST** take the minimum of ancestor and
descendant; plugin lists **MUST** concatenate ancestor-then-descendant with enforced entries
retained; CORS origins **MUST** union under `inherit`; tags **MUST** always union add-only, so a
descendant can add but never remove an inherited tag.

**Implements**:
- `cpt-cf-oagw-flow-policy-configure-policy-layer`
- `cpt-cf-oagw-algo-policy-hierarchical-merge`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Rate limiter state`, `CORS policy`, `Header transformation rule`

### Metrics and Audit Logging

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-policy-observability`

The system **MUST** emit `oagw_requests_total`, `oagw_request_duration_seconds`,
`oagw_requests_in_flight`, and `oagw_errors_total` with the documented OTel (OpenTelemetry) label
keys and no tenant label, and **MUST** emit one structured audit record per request carrying
`request_id`, `tenant_id`, `method`, `path`, `status`, and `duration_ms` at INFO, WARN, or ERROR.
Bodies and credential material **MUST** never appear in either output.

**Implements**:
- `cpt-cf-oagw-flow-policy-credential-injected-proxy`
- `cpt-cf-oagw-algo-policy-observability-emit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `ProxyContext`

## 6. Acceptance Criteria

Every criterion below is assertable by an automated test that drives the gear against a local
stub upstream, with no external network dependency.

- [ ] An upstream configured with the `apikey` auth plugin causes the configured header to appear on the request the stub upstream receives, with the resolved secret as its value.
- [ ] After that same request, the secret value appears in no captured log line, no metric label, no error body, and no response header returned to the client.
- [ ] An upstream whose `auth.type` names the catalog-only `basic` or `bearer` identifier fails the proxy request with a problem document whose detail contains "unknown auth plugin".
- [ ] An upstream whose credential reference names a secret the stub credential store does not hold returns 500 with the `SecretNotFound` GTS type.
- [ ] An upstream whose credential reference names a secret the calling tenant may not read returns 401.
- [ ] A chain of two upstream plugins and two route plugins records execution in upstream-first order at the stub, confirming the concatenation rule.
- [ ] With `required_request_headers` configured and one of those headers absent, the proxy returns 400 with error code `REQUIRED_HEADER_MISSING`, and the stub upstream records no request.
- [ ] With `required_response_headers` configured and the stub omitting one of them, the proxy returns 502 with error code `REQUIRED_HEADER_MISSING`.
- [ ] A required-headers value that is blank after trimming, such as one containing only commas and spaces, is a no-op and the request reaches the stub unchanged.
- [ ] A required-headers check succeeds when the client sends the header in a different letter case from the configured name.
- [ ] Sending more requests than the configured sustained rate and burst capacity allows returns 429 carrying `Retry-After`, `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset`.
- [ ] A request admitted under the same limit carries the three `X-RateLimit-*` headers with a remaining count that decreases across successive admitted requests.
- [ ] An `OPTIONS` request with `Origin` and `Access-Control-Request-Method` returns 204 with `Access-Control-Allow-Origin` echoing the sent origin and `Access-Control-Max-Age: 86400`, and the stub upstream records no request.
- [ ] An actual cross-origin request from an origin absent from `allowed_origins` returns 403 with the `cors.origin_not_allowed` GTS type, and the stub upstream records no request.
- [ ] An actual cross-origin request whose method is absent from `allowed_methods` returns 403 with the `cors.method_not_allowed` GTS type.
- [ ] An origin matching an allowed entry except for its port, or except for its scheme, is rejected with 403.
- [ ] Creating an upstream whose CORS block sets `allow_credentials` to true together with a wildcard origin returns 400 at configuration time, before any proxy request is made.
- [ ] Configured request-header `set`, `add`, and `remove` rules are visible on the request the stub upstream receives: the set name holds exactly the configured value, the added name holds both values, and the removed name is absent.
- [ ] A request-side passthrough mode of `allowlist` forwards only the allowlisted inbound headers to the stub upstream.
- [ ] Configured response-header rules are visible on the response the client receives.
- [ ] An ancestor rate limit of `enforce` combined with a stricter descendant limit produces the stricter effective limit, and the reverse pairing also produces the stricter one.
- [ ] A descendant tag list is unioned with the inherited tags, and an attempt to omit an inherited tag leaves that tag present in the effective configuration.
- [ ] A successful proxy request increments `oagw_requests_total` and observes `oagw_request_duration_seconds`, and neither series carries a tenant label.
- [ ] A proxy request emits exactly one structured audit record containing `request_id`, `tenant_id`, `method`, `path`, `status`, and `duration_ms`, and containing no request or response body.
- [ ] A second proxied request for the same tenant, subject, and `oauth2_client_cred` configuration, sent within the cached token's time to live, injects the same bearer value as the first request and causes no second call to the stub identity provider.
- [ ] An upstream naming a plugin identifier that no registry resolves fails the proxy request with 503 and the `PluginNotFound` GTS type, and the stub upstream records no request.

## 7. Additional Context (optional)

### 7.1 Out of Scope, With Reasons

- **Executing Starlark custom plugins.** No Starlark runtime is enabled in this deployment, so
  `cpt-cf-oagw-nfr-starlark-sandbox` has no runtime to sandbox. A plugin list entry naming a
  custom plugin resolves to 503 PluginNotFound rather than executing anything. Definitions are
  still stored and served by the plugin management feature.
- **Redis-backed distributed rate-limit synchronization.** No Redis dependency is enabled, so
  limiters hold per-instance local state and the effective limit is per node. ADR-0003's hybrid
  sync design is therefore not built.
- **The Redis L2 configuration cache.** Same missing dependency. Configuration is merged per
  request from the in-memory store rather than served from a second-level cache.
- **The circuit breaker.** DESIGN.md §4.7 lists it as future work and ADR-0002 states it is core
  policy rather than a plugin. The circuit-breaker clause of `cpt-cf-oagw-nfr-high-availability`
  is therefore unmet; only the baseline availability behaviour below is built.
- **A gear-mounted metrics scrape route.** DESIGN.md marks that surface admin-only, and
  aggregating it is a host-runtime concern. This feature emits the series through shared
  instrumentation hooks without owning a route.
- **The `budget` and `overcommit_ratio` rate-limit extension.** ADR-0003 proposes it, but neither
  JSON Schema carries it; both define only `sharing`, `algorithm`, `sustained`, `burst`, `scope`,
  `strategy`, and `cost`. There is no field to implement.

### 7.2 Availability and Resilience Posture

The achievable portion of `cpt-cf-oagw-nfr-high-availability` in this build is baseline
behaviour: no unhandled panic on any policy path, and a consistent RFC 9457 problem document
whenever an upstream fails, times out, or returns an unexpected shape. Timeout handling comes
from the gear-level proxy timeout, which is two seconds in the graded configuration. Consistent
with `cpt-cf-oagw-principle-no-retry`, no policy path re-issues the client's request, including
after an upstream 401 that a fresh token might have satisfied.

### 7.3 Explicit Non-Applicability

- **Database and data-lifecycle analysis**: not applicable, because this feature adds no table,
  no query, and no persisted entity. Its only durable inputs are the upstream and route documents
  owned by other features; its own state is the in-memory token cache and rate limiter.
- **Accessibility**: not applicable, because the feature exposes no user interface. Its only
  browser-facing surface is the CORS header contract, covered above.
- **Regulatory and privacy compliance**: no personal data is processed or stored by this feature.
  The audit fields are deliberately limited to identifiers, method, path, status, and duration,
  and bodies and query strings are never logged.
- **Rollout and rollback**: not applicable as a separate concern, because every behaviour here is
  driven by upstream and route configuration. Removing an `auth`, `rate_limit`, `cors`, `headers`,
  or `plugins` block restores the prior bare-proxy behaviour without a code change.
- **Data migration**: not applicable, because no schema or stored representation changes.
