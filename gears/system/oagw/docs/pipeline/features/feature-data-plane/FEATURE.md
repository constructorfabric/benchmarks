# Feature: OAGW Data Plane


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy a Request](#proxy-a-request)
  - [Rate-Limited Request (Error Path)](#rate-limited-request-error-path)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Request Pipeline Execution](#request-pipeline-execution)
  - [Credential Resolution and Injection](#credential-resolution-and-injection)
  - [Error Mapping](#error-mapping)
- [4. States (CDSL)](#4-states-cdsl)
  - [Rate Limiter Bucket State Machine](#rate-limiter-bucket-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Proxy Service](#proxy-service)
  - [Plugin Registry](#plugin-registry)
  - [Credential Resolution and Injection](#credential-resolution-and-injection-1)
  - [Rate Limiting](#rate-limiting)
  - [CORS](#cors)
  - [Error-Source Distinction](#error-source-distinction)
  - [PEP Gate](#pep-gate)
  - [SSRF Guard](#ssrf-guard)
  - [Streaming](#streaming)
  - [DP L1 Config Cache](#dp-l1-config-cache)
  - [Data-Plane Test Harness](#data-plane-test-harness)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p2` - **ID**: `cpt-cf-oagw-featstatus-data-plane`

- [x] `p2` - `cpt-cf-oagw-feature-data-plane`
## 1. Feature Context

### 1.1 Overview

The data plane is the request-execution surface: a pingora-based proxy service in `infra/proxy` that resolves the effective configuration (per feature-control-plane), enforces policy in documented order (PEP gate via the authz-resolver SDK, rate limiting, CORS, SSRF policy, auth plugins with credential injection, required-headers guard, request_id transform), streams HTTP/SSE/WebSocket responses, and maps every outcome onto the authoritative error catalog with `X-OAGW-Error-Source: gateway|upstream` per ADR 0007. It owns the DP L1 config cache (1000 entries), the in-memory rate limiters, the ADR 0008 pingora-memory-cache token cache, and the streaming/bridging of pingora into the Axum host. Recording uses structured request logging with correlation IDs and Prometheus metrics per the authoritative vocabulary (no PII, no secrets, bounded cardinality), driving the observability NFR.

### 1.2 Purpose

The authoritative ADRs 0001-0009 decide the product behavior (routing matrix, plugin semantics, rate limit algorithm, CORS rules, caching, state ownership, error-source distinction, OAuth2 plugin, required-headers guard); the pipeline PRD's DoD requires end-to-end reachability of the contract paths and per-request behavior faithful to those ADRs. Without this feature the proxy paths listed in the contract answer with the host's unhandled-path response, the PEP gate is never applied between hosts, credentials are never injected, and gateway-vs-upstream error semantics never reach clients. This feature delivers the executing half of the gear on top of feature-gear-foundation's lifecycle and feature-control-plane's authoritative configuration.

**Requirements**: `cpt-cf-oagw-fr-data-plane-proxy`, `cpt-cf-oagw-fr-credential-injection`, `cpt-cf-oagw-fr-rate-limit-enforcement`, `cpt-cf-oagw-fr-stream-proxying`, `cpt-cf-oagw-fr-plugin-execution`, `cpt-cf-oagw-fr-cors-enforcement`, `cpt-cf-oagw-fr-error-source-semantics`, `cpt-cf-oagw-fr-inbound-authz`, `cpt-cf-oagw-fr-security-policy`, `cpt-cf-oagw-fr-secret-resolution`, `cpt-cf-oagw-nfr-proxy-overhead`, `cpt-cf-oagw-nfr-availability`, `cpt-cf-oagw-nfr-concurrency-safety`, `cpt-cf-oagw-nfr-secret-hygiene`, `cpt-cf-oagw-nfr-ssrf-safety`, `cpt-cf-oagw-nfr-observability-metrics`, `cpt-cf-oagw-nfr-test-coverage`, `cpt-cf-oagw-usecase-proxy-call`

**Principles**: `cpt-cf-oagw-principle-cp-authoritative-dp-bounded`, `cpt-cf-oagw-principle-pingora-reuse`, `cpt-cf-oagw-principle-ssrf-defense-in-depth`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-proxy-client` | Issues proxied requests to the contract paths; receives proxied responses or authoritative error/problem responses |
| `cpt-cf-oagw-actor-authz-resolver` | Receives the policy decision requested on the request's effective permissions (PEP gate) before proxying |
| `cpt-cf-oagw-actor-tenant-resolver` | Maps the caller to the tenant scope used for effective configuration, policy, and the token cache key |
| `cpt-cf-oagw-actor-credential-store` | Supplies upstream credentials for the chosen auth method during credential resolution |
| `cpt-cf-oagw-actor-gateway-operator` | Writes the effective configuration (via feature-control-plane) that this feature's DP L1 cache consumes and agrees on engine settings with |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADR**: [0001-request-routing.md](../ADR/0001-request-routing.md), [0002-plugin-system.md](../ADR/0002-plugin-system.md), [0003-rate-limiting.md](../ADR/0003-rate-limiting.md), [0004-cors.md](../ADR/0004-cors.md), [0005-data-plane-caching.md](../ADR/0005-data-plane-caching.md), [0006-state-management.md](../ADR/0006-state-management.md), [0007-error-source-distinction.md](../ADR/0007-error-source-distinction.md), [0008-oauth2-client-credentials-auth-plugin.md](../ADR/0008-oauth2-client-credentials-auth-plugin.md), [0009-required-headers-guard-plugin.md](../ADR/0009-required-headers-guard-plugin.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` (lifecycle, config, route mounting), `cpt-cf-oagw-feature-control-plane` (effective configuration inputs)
- **Interfaces**: `cpt-cf-oagw-interface-oagw-rest-surface`, `cpt-cf-oagw-interface-proxy-api` (reference, host-composed as `/api/oagw/v1/...`), `cpt-cf-oagw-interface-management-api` (reference)
- **Components**: `cpt-cf-oagw-component-data-plane`
- **Sequences**: `cpt-cf-oagw-seq-proxy-call`
- **Schemas**: `upstream.v1.schema.json`, `route.v1.schema.json`

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-proxy-call`

### Proxy a Request

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-data-plane-proxy-request`

**Actor**: `cpt-cf-oagw-actor-proxy-client`

**Success Scenarios**:
- A request to `{METHOD} /oagw/v1/proxy/{alias}/{*path}` is authenticated (per the upstream's auth plugins), passes the PEP gate, rate limiter, CORS check, guards, and SSRF check, is streamed to the resolved upstream target with credentials injected, and the upstream response is streamed back with `X-OAGW-Error-Source: upstream` on upstream-sourced outcomes.
- SSE and WebSocket requests stream without buffering the whole body; the `<10ms` p95 overhead budget holds under the DoD load.

**Error Scenarios**:
- Request-scoped failures (missing auth, guard rejection, rate limit, PEP denial, SSRF block, connection failure, upstream timeout) produce the authoritative problem+json catalog with `X-OAGW-Error-Source: gateway` (gateway-sourced) or `upstream` (upstream-sourced) and correct status.

**Steps**:
1. [x] - `p1` - Client issues `API: {METHOD} /oagw/v1/proxy/{alias}/{*path}` - `inst-proxy-request`
2. [x] - `p1` - Data plane consumes the DP L1 config cache (1000 entries), refreshing on control-plane invalidation, to resolve the effective configuration for the alias - `inst-l1-resolve`
3. [x] - `p1` - **IF** the alias is unknown in the effective configuration - `inst-alias-unknown`
   1. [x] - `p1` - **RETURN** 404 with `X-OAGW-Error-Source: gateway` - `inst-404`
4. [x] - `p1` - **ELSE** proceed - `inst-alias-known`
5. [x] - `p1` - Resolve the caller to a tenant scope via the tenant-resolver SDK - `inst-tenant-resolve`
6. [x] - `p1` - Run the configured auth plugins for the tenant's effective auth method and the upstream's credential selection - `inst-auth-plugins`
7. [x] - `p1` - **IF** authentication fails (unknown API key, invalid token, missing auth) - `inst-auth-fail`
   1. [x] - `p1` - **RETURN** 401 with `X-OAGW-Error-Source: gateway` - `inst-401`
8. [x] - `p1` - **ELSE** proceed - `inst-auth-ok`
9. [x] - `p1` - Resolve credentials for the upstream (from the resolved auth method) - `inst-credential-resolve`
10. [x] - `p1` - Enforce the token-bucket rate limit (stricter-wins from the effective configuration) - `inst-rate-limit`
11. [x] - `p1` - **IF** the bucket is exhausted - `inst-rate-limit-hit`
    1. [x] - `p1` - **RETURN** 429 with `Retry-After`, `X-RateLimit-*` headers, and `X-OAGW-Error-Source: gateway` (see the rate-limited-flow expansion below) - `inst-429`
12. [x] - `p1` - **ELSE** proceed - `inst-rate-ok`
13. [x] - `p1` - Evaluate the PEP gate: ask the authz-resolver SDK for a decision on `gts.cf.core.oagw.proxy.v1~:invoke` under the ownership rule - `inst-pep`
14. [x] - `p1` - **IF** the decision is deny - `inst-pep-deny`
    1. [x] - `p1` - **RETURN** 401/403-style PEP denial with `X-OAGW-Error-Source: gateway` - `inst-pep-deny-return`
15. [x] - `p1` - **ELSE** proceed - `inst-pep-allow`
16. [x] - `p1` - Enforce CORS per ADR 0004 (preflight 204 local on exact origin match; actual request header checks) - `inst-cors`
17. [x] - `p1` - **IF** the origin fails the CORS check - `inst-cors-fail`
    1. [x] - `p1` - **RETURN** the CORS error with `X-OAGW-Error-Source: gateway` - `inst-cors-error`
18. [x] - `p1` - **ELSE** proceed - `inst-cors-ok`
19. [x] - `p1` - Apply the SSRF policy per `ssrf_policy.enabled` (HTTPS-only by default) - `inst-ssrf`
20. [x] - `p1` - **IF** the upstream target is not allowed - `inst-ssrf-block`
    1. [x] - `p1` - **RETURN** the SSRF block with `X-OAGW-Error-Source: gateway` - `inst-ssrf-block-return`
21. [x] - `p1` - **ELSE** proceed - `inst-ssrf-ok`
22. [x] - `p1` - Run the guard plugins (e.g. `required_headers`) and the `request_id` transform - `inst-guards`
23. [x] - `p1` - **IF** a guard rejects the request - `inst-guard-reject`
    1. [x] - `p1` - **RETURN** the guard rejection with `X-OAGW-Error-Source: gateway` - `inst-guard-return`
24. [x] - `p1` - **ELSE** proceed - `inst-guard-ok`
25. [x] - `p1` - Inject the resolved upstream credentials into the outbound request - `inst-inject-credentials`
26. [x] - `p1` - Stream the request to the upstream through the bridged pingora transport (pooled, load-balanced, multi-endpoint selection per ADR 0001) - `inst-forward`
27. [x] - `p1` - **TRY** capture the upstream response - `inst-upstream-try`
28. [x] - `p1` - **CATCH** upstream failure (connection refused, timeout via `proxy_timeout_secs`, TLS error) - `inst-upstream-catch`
    1. [x] - `p1` - Map to the upstream-sourced error catalog (502/503/504) - `inst-upstream-error`
    2. [x] - `p1` - **RETURN** the error problem+json with `X-OAGW-Error-Source: upstream` - `inst-upstream-error-return`
29. [x] - `p1` - **ELSE** forward the response (streaming HTTP/SSE/WebSocket, hop-by-hop headers handled, `X-OAGW-Error-Source: upstream` on upstream-sourced responses) - `inst-forward-response`
30. [x] - `p1` - **RETURN** the response to the client - `inst-return-response`

### Rate-Limited Request (Error Path)

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-data-plane-rate-limited-request`

**Actor**: `cpt-cf-oagw-actor-proxy-client`

**Success Scenarios**:
- A client whose bucket is exhausted receives the authoritative 429 problem response with `Retry-After` and `X-RateLimit-*` headers and `X-OAGW-Error-Source: gateway`; the request is never proxied upstream.

**Error Scenarios**:
- The limiter itself failing (e.g. clock anomaly) must not proxy a request outside policy; it maps to the gateway error path.

**Steps**:
1. [x] - `p1` - Client issues a request above the effective token-bucket rate - `inst-rapid-request`
2. [x] - `p1` - Rate limiter checks the bucket (dual rate: refill + burst ceiling per ADR 0003) - `inst-bucket-check`
3. [x] - `p1` - **IF** the bucket is exhausted - `inst-exhausted`
   1. [x] - `p1` - **TRY** authoring the 429 problem response - `inst-429-try`
   2. [x] - `p1` - **CATCH** error during authoring - `inst-429-catch`
      1. [x] - `p1` - **RETURN** the fallback gateway 500 - `inst-429-fallback`
   3. [x] - `p1` - **ELSE** - `inst-429-ok`
      1. [x] - `p1` - Add `Retry-After` and `X-RateLimit-Remaining`/`X-RateLimit-Limit`/`X-RateLimit-Reset` - `inst-429-headers`
      2. [x] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-429-source`
   4. [x] - `p1` - **RETURN** 429 to the client (no proxying) - `inst-429-return`
4. [x] - `p1` - **ELSE** the bucket admits the request - `inst-admitted`
   1. [x] - `p1` - **RETURN** admission control to the main proxy flow - `inst-to-main-flow`

## 3. Processes / Business Logic (CDSL)

### Request Pipeline Execution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-data-plane-pipeline-execution`

**Input**: a proxied request with its tenant, alias, and path; the effective configuration for the alias; the DP L1 cache state

**Output**: either a forwarded/streamed upstream response (with gateway or upstream error-source header as appropriate) or an authoritative gateway-sourced problem response

**Steps**:
1. [x] - `p1` - Load the effective configuration for the alias (DP L1, capacity 1000) - `inst-load-effective`
2. [x] - `p1` - **FOR EACH** auth plugin active for the tenant/upstream (noop, apikey, oauth2_client_cred, oauth2_client_cred_basic) - `inst-auth-loop`
   1. [x] - `p1` - Execute the plugin against the request identity - `inst-auth-plugin-exec`
3. [x] - `p1` - **IF** the effective auth method requires but no auth succeeded - `inst-auth-missing`
   1. [x] - `p1` - **RETURN** gateway 401 - `inst-auth-401`
4. [x] - `p1` - **ELSE** proceed - `inst-auth-done`
5. [x] - `p1` - Check the token bucket (stricter-wins rate from the effective config) - `inst-check-bucket`
6. [x] - `p1` - **IF** exhausted - `inst-bucket-exhausted`
   1. [x] - `p1` - **RETURN** gateway 429 with `Retry-After` and `X-RateLimit-*` - `inst-bucket-429`
7. [x] - `p1` - **ELSE** consume a token - `inst-consume-token`
8. [x] - `p1` - Request the PEP decision (`gts.cf.core.oagw.proxy.v1~:invoke` + ownership rule) - `inst-pep-request`
9. [x] - `p1` - **IF** deny - `inst-pep-denied`
   1. [x] - `p1` - **RETURN** the gateway PEP denial (authorized error, no upstream call) - `inst-pep-deny`
10. [x] - `p1` - **ELSE** proceed - `inst-pep-passed`
11. [x] - `p1` - Run CORS per ADR 0004 (exact origin match, `Vary: Origin`, preflight 204 local) - `inst-run-cors`
12. [x] - `p1` - Run the SSRF policy (HTTPS-only default, `allow_http_upstream` opt-in) - `inst-run-ssrf`
13. [x] - `p1` - Run the guard plugins and the `request_id` transform - `inst-run-guards`
14. [x] - `p1` - **IF** any guard rejects - `inst-guard-blocked`
    1. [x] - `p1` - **RETURN** the gateway guard rejection - `inst-guard-block`
15. [x] - `p1` - **ELSE** proceed - `inst-guards-passed`
16. [x] - `p1` - Resolve and inject upstream credentials (see credential resolution) - `inst-inject`
17. [x] - `p1` - Forward and stream via pingora (pooled, load-balanced; SSE/WebSocket passthrough) - `inst-pingora-forward`
18. [x] - `p1` - **TRY** capture the upstream response - `inst-forward-try`
19. [x] - `p1` - **CATCH** upstream error - `inst-forward-catch`
    1. [x] - `p1` - Map to 502/503/504 per the error catalog - `inst-forward-map`
    2. [x] - `p1` - **RETURN** the upstream-sourced problem response - `inst-forward-map-return`
20. [x] - `p1` - **ELSE** stream the response back with correct hop-by-hop handling and error-source header - `inst-stream-response`
21. [x] - `p1` - **RETURN** the response - `inst-done`

### Credential Resolution and Injection

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-data-plane-credential-resolution`

**Input**: the upstream's effective auth method, the request identity, the credential store

**Output**: an injectable credential (for the authorized method), or an authorized gateway-sourced authentication failure

**Steps**:
1. [x] - `p1` - Determine the auth method from the effective configuration (credential-store backed) - `inst-determine-method`
2. [x] - `p1` - **IF** the method is `noop` - `inst-noop`
   1. [x] - `p1` - **RETURN** no credential to inject - `inst-noop-done`
3. [x] - `p1` - **ELSE IF** the method is `apikey` - `inst-apikey`
   1. [x] - `p1` - Look up the API key for the tenant/upstream from the credential store - `inst-apikey-lookup`
4. [x] - `p1` - **ELSE IF** the method is OAuth2 (`oauth2_client_cred`, `oauth2_client_cred_basic`) - `inst-oauth2`
   1. [x] - `p1` - Look up the token cache (pingora-memory-cache keyed `tenant:subject:auth_method:config_hash`) - `inst-token-cache`
   2. [x] - `p1` - **IF** a valid cached token exists - `inst-token-cached`
      1. [x] - `p1` - Verify `CachedToken` (subject, not expired) and reuse it - `inst-token-reuse`
   3. [x] - `p1` - **ELSE** exchange client credentials for a token - `inst-token-exchange`
      1. [x] - `p1` - Cache with TTL `min(300, expires_in - 30s)` and capacity 10 000 - `inst-token-store`
5. [x] - `p1` - **IF** the credential or token could not be resolved - `inst-cred-fail`
   1. [x] - `p1` - **RETURN** gateway 401 - `inst-cred-401`
6. [x] - `p1` - **ELSE** - `inst-cred-ok`
   1. [x] - `p1` - **RETURN** the credential for injection (header for `oauth2_client_cred`/`apikey`; header+basic distinction for `oauth2_client_cred_basic`) - `inst-cred-return`

### Error Mapping

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-data-plane-error-mapping`

**Input**: a failure observed anywhere in the pipeline (policy, guard, auth, PEP, SSRF, transport, timeout, TLS)

**Output**: an RFC 9457 problem+json response with the authoritative status and `X-OAGW-Error-Source: gateway|upstream`

**Steps**:
1. [x] - `p1` - Classify the failure source - `inst-classify`
2. [x] - `p1` - **IF** the failure originates in the gateway (config, policy, guards, auth, PEP, SSRF, limiter) - `inst-gateway-source`
   1. [x] - `p1` - Map to the gateway range (400/401/404/409/413/429/500) per the error catalog - `inst-gateway-map`
   2. [x] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-gateway-source-header`
3. [x] - `p1` - **ELSE IF** the failure originates upstream (connection, timeout via `proxy_timeout_secs`, 5xx, TLS) - `inst-upstream-source`
   1. [x] - `p1` - Map to the upstream range (502/503/504) per the error catalog - `inst-upstream-map`
   2. [x] - `p1` - Set `X-OAGW-Error-Source: upstream` - `inst-upstream-source-header`
4. [x] - `p1` - **ELSE** treat as internal gateway error (500) - `inst-internal`
   1. [x] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-internal-source`
5. [x] - `p1` - Author the RFC 9457 problem body with the GTS error type - `inst-problem-body`
6. [x] - `p1` - **RETURN** the problem response to the client - `inst-problem-return`

## 4. States (CDSL)

### Rate Limiter Bucket State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-data-plane-rate-limiter-bucket`

**States**: `Ready`, `Exhausted`

**Initial State**: `Ready`

**Transitions**:
1. [x] - `p1` - **FROM** Ready **TO** Exhausted **WHEN** tokens drop below the admission threshold (dual-rate refill cannot keep up with demand) - `inst-exhaust`
2. [x] - `p1` - **FROM** Exhausted **TO** Ready **WHEN** the refill restores tokens above the threshold - `inst-refill`
3. [x] - `p1` - **FROM** Ready **TO** Ready **WHEN** a request consumes one token while the bucket remains above threshold - `inst-consume`
4. [x] - `p1` - **FROM** Exhausted **TO** Exhausted **WHEN** further requests are rejected while tokens remain below threshold (each returns 429) - `inst-remain-exhausted`

**Note**: `Exhausted` transitions are observed under the effective configuration's stricter-wins rate per ADR 0003; the bucket is DP-owned state per ADR 0006.

## 5. Definitions of Done

### Proxy Service

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-proxy-service`

The system **MUST** serve `{METHOD} /oagw/v1/proxy/{alias}/{*path}` through the bridged pingora data plane in `infra/proxy`, supporting pooled and load-balanced transport with multi-endpoint selection per ADR 0001, honoring `proxy_timeout_secs`, and streaming responses with correct hop-by-hop handling.

**Implements**: `cpt-cf-oagw-flow-data-plane-proxy-request`, `cpt-cf-oagw-algo-data-plane-pipeline-execution`

**Constraints**: `cpt-cf-oagw-constraint-no-api-segment`, `cpt-cf-oagw-constraint-locked-deps`, `cpt-cf-oagw-nfr-proxy-overhead`

**Touches**: API: `{METHOD} /oagw/v1/proxy/{alias}/{*path}` / Entities: `ProxyRequest`, `ProxyResponse`

### Plugin Registry

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-plugin-registry`

The system **MUST** register and execute auth plugins (noop, apikey, oauth2_client_cred, oauth2_client_cred_basic; basic/bearer catalog-only) and the guard/transform plugins (`required_headers`, `request_id`) in the effective configuration's binding order, with unknown or unregistered plugin types rejected over the management surface (feature-control-plane) and never executed.

**Implements**: `cpt-cf-oagw-algo-data-plane-pipeline-execution`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: Entities: `AuthPluginRegistry`, `GuardPluginRegistry`

### Credential Resolution and Injection

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-credential-injection`

The system **MUST** resolve upstream credentials for the effective auth method via the credential store, inject them into the outbound request, and for OAuth2 methods manage the ADR 0008 pingora-memory-cache token cache (capacity 10 000, key `tenant:subject:auth_method:config_hash`, TTL `min(300, expires_in - 30s)`), so no credential or token leaves the gear unmanaged.

**Implements**: `cpt-cf-oagw-algo-data-plane-credential-resolution`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`, `cpt-cf-oagw-nfr-secret-hygiene`

**Touches**: Entities: `CachedToken`, `CredentialResolver`

### Rate Limiting

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-rate-limiter`

The system **MUST** enforce the token-bucket (dual refill rate + burst ceiling per ADR 0003) with stricter-wins merging from the effective configuration, returning 429 with `Retry-After` and `X-RateLimit-*` headers and `X-OAGW-Error-Source: gateway` when the bucket is exhausted, per state `cpt-cf-oagw-state-data-plane-rate-limiter-bucket`.

**Implements**: `cpt-cf-oagw-flow-data-plane-rate-limited-request`, `cpt-cf-oagw-state-data-plane-rate-limiter-bucket`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: Entities: `RateLimiter`, `RateBucket`

### CORS

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-cors`

The system **MUST** enforce CORS per ADR 0004 (preflight answered 204 locally on exact origin match, `Vary: Origin` on responses, allow-list from the effective configuration's CORS union), with failures mapped to the gateway error path.

**Implements**: `cpt-cf-oagw-algo-data-plane-pipeline-execution`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: Entities: `CorsPolicy`

### Error-Source Distinction

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-error-source`

The system **MUST** emit every error as RFC 9457 problem+json with a GTS error type and set `X-OAGW-Error-Source: gateway` for gateway-sourced outcomes (400/401/404/409/413/429/500) and `X-OAGW-Error-Source: upstream` for upstream-sourced outcomes (502/503/504) per ADR 0007, with no mixing.

**Implements**: `cpt-cf-oagw-algo-data-plane-error-mapping`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: Entities: `ErrorType`, `ProblemDetails`

### PEP Gate

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-pep-gate`

The system **MUST** request the authz-resolver SDK decision for `gts.cf.core.oagw.proxy.v1~:invoke` under the ownership rule before any upstream call and short-circuit on deny with the gateway error path (no proxying on deny).

**Implements**: `cpt-cf-oagw-algo-data-plane-pipeline-execution`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: API: (SDK) authz-resolver decision / Entities: `PepDecision`

### SSRF Guard

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-ssrf-guard`

The system **MUST** enforce the SSRF policy per `ssrf_policy.enabled` (HTTPS-only by default; `allow_http_upstream` opts into HTTP) before connection, blocking disallowed targets (default deny) and returning the gateway error path for blocked targets, per `cpt-cf-oagw-principle-ssrf-defense-in-depth`.

**Implements**: `cpt-cf-oagw-algo-data-plane-pipeline-execution`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: Entities: `SsrfPolicy`

### Streaming

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-streaming`

The system **MUST** stream HTTP, SSE, and WebSocket responses without buffering whole bodies (per `cpt-cf-oagw-fr-stream-proxying`) and keep the proxy overhead within the `<10ms` p95 budget under DoD load (per `cpt-cf-oagw-nfr-proxy-overhead`).

**Implements**: `cpt-cf-oagw-flow-data-plane-proxy-request`, `cpt-cf-oagw-algo-data-plane-pipeline-execution`

**Constraints**: `cpt-cf-oagw-nfr-proxy-overhead`

**Touches**: Entities: `ProxyStream`

### DP L1 Config Cache

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-l1-config-cache`

The system **MUST** maintain the DP L1 config cache (capacity 1000) per ADR 0005/0006, refresh/evict on control-plane invalidation, and never serve an effective configuration stale relative to acknowledged writes.

**Implements**: `cpt-cf-oagw-flow-data-plane-proxy-request`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: Entities: `ConfigCacheMgr`

### Data-Plane Test Harness

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-data-plane-test-harness`

The system **MUST** test the full round-trip, credential resolution and caching, rate-limit rejection, guard rejection, CORS, SSRF, error mapping (gateway vs upstream), streaming, and PEP gating at the crate level under the workspace lint denials, and MUST NOT author any code under `testing/e2e/gears/oagw/`.

**Implements**: `cpt-cf-oagw-flow-data-plane-proxy-request`, `cpt-cf-oagw-algo-data-plane-error-mapping`

**Constraints**: `cpt-cf-oagw-constraint-workspace-lints`, `cpt-cf-oagw-constraint-toolchain`

**Touches**: Entities: `ProxyRequest`, `CachedToken`, `RateLimiter`, `PepDecision`

## 6. Acceptance Criteria

- [x] A full round-trip through `{METHOD} /oagw/v1/proxy/{alias}/{*path}` reaches the upstream and streams the response back with correct hop-by-hop handling and error-source header.
- [x] Credential resolution injects the configured credential; OAuth2 tokens are cached per ADR 0008 (key/TTL/capacity) and reused, with refresh before expiry.
- [x] Exceeding the effective rate returns 429 with `Retry-After`, `X-RateLimit-*`, and `X-OAGW-Error-Source: gateway`; no request is proxied above the limit.
- [x] A rejected guard (e.g. missing required header) returns the authoritative gateway rejection and never reaches the upstream.
- [x] CORS preflight returns 204 locally on exact origin match; disallowed origins are rejected; `Vary: Origin` is present on actual responses.
- [x] SSRF policy blocks disallowed targets (HTTPS-only default; HTTP only when `allow_http_upstream` is set).
- [x] Error mapping yields the correct status and `X-OAGW-Error-Source` per ADR 0007 across 404 (unknown alias), 401 (auth/PEP), 413/409 (gateway), 502/503 (upstream), and 504 (timeout per `proxy_timeout_secs`) cases.
- [ ] SSE and WebSocket requests are streamed without whole-body buffering.
- [ ] The proxy overhead stays within the `<10ms` p95 budget under the DoD load, and the gear sustains the concurrency NFR.
- [x] Every crate-level data-plane test passes on the configured toolchain; `testing/e2e/gears/oagw/` receives no code from this change.
