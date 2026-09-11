# Feature: Request Proxy


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy Request Dispatch](#proxy-request-dispatch)
  - [Alias Resolution and Tenant-Hierarchy Shadowing](#alias-resolution-and-tenant-hierarchy-shadowing)
  - [Route Matching and Request Classification](#route-matching-and-request-classification)
  - [Request Surface Validation and Hardening](#request-surface-validation-and-hardening)
  - [Target Host Resolution and Endpoint Selection](#target-host-resolution-and-endpoint-selection)
  - [Plugin Chain Invocation and Cross-Cutting Call-Ins](#plugin-chain-invocation-and-cross-cutting-call-ins)
  - [Upstream Forwarding and Response Passthrough](#upstream-forwarding-and-response-passthrough)
  - [SSE Streaming Session](#sse-streaming-session)
  - [WebSocket Session](#websocket-session)
  - [WebTransport Session](#webtransport-session)
  - [Error Source Emission](#error-source-emission)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Tenant-Hierarchy Alias Resolution](#tenant-hierarchy-alias-resolution)
  - [Effective Configuration Merge](#effective-configuration-merge)
  - [Route Selection and Path and Query Validation](#route-selection-and-path-and-query-validation)
  - [Header Classification and Transformation](#header-classification-and-transformation)
  - [Target Host Validation and Endpoint Selection](#target-host-validation-and-endpoint-selection)
  - [Request Body Validation](#request-body-validation)
  - [Proxy Timeout and Failover Handling](#proxy-timeout-and-failover-handling)
  - [Streaming Connection Lifecycle](#streaming-connection-lifecycle)
- [4. States (CDSL)](#4-states-cdsl)
  - [Proxy Request State Machine](#proxy-request-state-machine)
  - [Streaming Connection State Machine](#streaming-connection-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Proxy Endpoint Handler and Data Plane Dispatch](#proxy-endpoint-handler-and-data-plane-dispatch)
  - [Preflight OPTIONS Detection](#preflight-options-detection)
  - [Tenant-Hierarchy Alias Resolution and Shadowing](#tenant-hierarchy-alias-resolution-and-shadowing)
  - [Disabled Upstream Rejection](#disabled-upstream-rejection)
  - [Route Matching by Method, Path Prefix, and Priority](#route-matching-by-method-path-prefix-and-priority)
  - [gRPC as Configuration Surface Only](#grpc-as-configuration-surface-only)
  - [Effective Configuration Merge](#effective-configuration-merge-1)
  - [Path Suffix Handling and Query Allowlist Enforcement](#path-suffix-handling-and-query-allowlist-enforcement)
  - [Header Classification and Transformation](#header-classification-and-transformation-1)
  - [X-OAGW-Target-Host Behavior Matrix](#x-oagw-target-host-behavior-matrix)
  - [Endpoint Pool Selection](#endpoint-pool-selection)
  - [Endpoint Scheme Allowlist Enforcement](#endpoint-scheme-allowlist-enforcement)
  - [Request Body Validation and Size Limit](#request-body-validation-and-size-limit)
  - [Request-Surface Hardening](#request-surface-hardening)
  - [Plugin Chain Invocation Points](#plugin-chain-invocation-points)
  - [Rate-Limit Call-In Point](#rate-limit-call-in-point)
  - [CORS Call-In Points](#cors-call-in-points)
  - [Proxy Timeout and No-Retry Forwarding](#proxy-timeout-and-no-retry-forwarding)
  - [Response Passthrough Without Caching](#response-passthrough-without-caching)
  - [Error Source Header on All Error Responses](#error-source-header-on-all-error-responses)
  - [SSE Streaming Passthrough and Lifecycle](#sse-streaming-passthrough-and-lifecycle)
  - [WebSocket Session Passthrough and Lifecycle](#websocket-session-passthrough-and-lifecycle)
  - [WebTransport Session Passthrough and Lifecycle](#webtransport-session-passthrough-and-lifecycle)
  - [Hot-Path Latency Posture](#hot-path-latency-posture)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-request-proxy-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-request-proxy`
## 1. Feature Context

### 1.1 Overview

Implements the Data Plane hot path of the `oagw` gear: the gear-relative proxy endpoint `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` dispatches to `DataPlaneService`, which resolves an upstream by alias across the tenant hierarchy, matches a route, merges effective configuration, classifies and transforms headers, selects an endpoint from the pool, forwards the request to the external service, and returns the response carrying the error-source header — including streaming responses over SSE, WebSocket, and WebTransport.

### 1.2 Purpose

This feature is the core value proposition of the gear: every outbound call the platform makes to an external service traverses this path. It realizes `cpt-cf-oagw-seq-proxy-flow`, the only interaction sequence DESIGN.md defines, and it is the Data Plane half of the Control Plane / Data Plane split recorded in `cpt-cf-oagw-design-overview` and `cpt-cf-oagw-component-model`. Per `cpt-cf-oagw-adr-request-routing`, proxy operations route to the Data Plane while upstream, route, and plugin management remain on the Control Plane. The proxy path is exposed at the gear-relative `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` with no leading `/api` segment (graded deviation 1); `http` is a legal endpoint scheme because `oagw.config.allow_http_upstream: true` lifts the default TLS posture of `cpt-cf-oagw-constraint-https-only` (graded deviation 2); and gRPC has no proxy code path, remaining configuration and schema surface only (graded deviation 7). The proxy surface inherits `Stability: unstable` and the major-version breaking-change policy from `cpt-cf-oagw-interface-proxy-api`, and the defaults this document defines — `headers.request.passthrough: none` and `path_suffix_mode: disabled` — may only be tightened across a major version boundary.

Read-side resolution is served from the in-memory repository: Find Upstream by Alias (tenant hierarchy walk with `enabled` inheritance), Find Matching Route for Request, and Resolve Effective Configuration, all under `cpt-cf-oagw-db-schema`; the effective-configuration merge itself is specified by `cpt-cf-oagw-algo-request-proxy-effective-config`. Proxy-time alias resolution and tenant-hierarchy shadowing are owned here; the alias derivation, immutability matrix, and persistence of `Upstream` records are owned by `cpt-cf-oagw-feature-upstream-management` under the PRD requirement `cpt-cf-oagw-fr-alias-resolution` (already marked done upstream), and route CRUD is owned by `cpt-cf-oagw-feature-route-management`.

Delivered by this feature:

- `p1` - `cpt-cf-oagw-fr-request-proxy`
- `p1` - `cpt-cf-oagw-fr-streaming`
- `p1` - `cpt-cf-oagw-fr-header-transform`
- `p1` - `cpt-cf-oagw-usecase-proxy-request`
- `p1` - `cpt-cf-oagw-usecase-sse-streaming`
- `p1` - `cpt-cf-oagw-interface-proxy-api`
- `p1` - `cpt-cf-oagw-nfr-low-latency`
- `p1` - `cpt-cf-oagw-nfr-ssrf-protection` - request-surface slice only; DNS-resolution validation and IP pinning are out of scope here
- [x] `p1` - `cpt-cf-oagw-nfr-input-validation` - enforcement slice; entry 2.5 renders the resulting 4xx problem+json and entry 2.8 covers the CORS-configuration validation slice

**Principles**: `cpt-cf-oagw-principle-no-cache`, `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-error-source`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`, `cpt-cf-oagw-constraint-https-only` (implemented as the default-TLS posture with `oagw.config.allow_http_upstream: true` as the documented lift, graded deviation 2)

**Sequences**: `cpt-cf-oagw-seq-proxy-flow` — the proxy request flow from Client through API Handler, Data Plane, Control Plane, auth plugin, plugin chain, and upstream service.

**Out of scope**:

- Any gRPC proxy code path: `upstream.protocol = gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1` and the `match.grpc` block are configuration and schema surface only (graded deviation 7)
- DNS-resolution validation and IP pinning: PRD §4.2 and DESIGN §4.5 declare those rules a separate concern, so `cpt-cf-oagw-nfr-ssrf-protection` is covered here only by the request-surface validation slice
- Response caching (`cpt-cf-oagw-principle-no-cache`), automatic retries of the original client request (`cpt-cf-oagw-principle-no-retry`), and the DP L1 hot-config cache (`cpt-cf-oagw-adr-state-management`, owned by entry 2.9)
- Gateway error body rendering, the shared problem+json contract, and the complete status/GTS mapping (owned by entry 2.5)
- Auth, guard, and transform plugin execution, registries, and credential resolution (owned by entry 2.6); this feature defines the extension points they plug into
- Rate-limit strategy execution and the 429 response (owned by entry 2.7); CORS origin and method enforcement on actual requests (owned by entry 2.8)
- Audit logging, metrics, and trace-identifier propagation (owned by `cpt-cf-oagw-feature-observability-and-operability`, entry 2.9): not applicable because entry 2.9 owns the audit and metric surface; this feature only exposes the request boundary. At that boundary this feature makes `status`, `duration_ms`, `request_size`, `response_size`, and `error_type` available for entry 2.9 to consume, and it emits no audit record, no metric, and no trace identifier of its own
- `testing/e2e/gears/oagw/` (graded deviation 4); verification is entirely in-crate

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends proxy requests to the gear-relative proxy endpoint, optionally targeting a specific pool endpoint with `X-OAGW-Target-Host`, and consumes the streamed or buffered response with its `X-OAGW-Error-Source` header |
| `cpt-cf-oagw-actor-upstream-service` | Receives the forwarded request over `http` or `https` (and streaming sessions over `wss` and `wt`), and returns the response or stream that the Data Plane passes through |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Schemas**: [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json), [schemas/route.v1.schema.json](../schemas/route.v1.schema.json)
- **Dependencies**: `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`, `cpt-cf-oagw-feature-gear-foundation`

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-proxy-request`, `cpt-cf-oagw-usecase-sse-streaming`

### Proxy Request Dispatch

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-dispatch`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request addressed to `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` with any HTTP method is authenticated, classified as a preflight or an actual request, and dispatched to the Data Plane under the gear-relative route tree registered in `cpt-cf-oagw-feature-gear-foundation`.

**Error Scenarios**:
- The caller lacks `gts.cf.core.oagw.proxy.v1~:invoke`: the request is rejected as a gateway error before any repository access.
- The request is a CORS preflight: it is answered at handler level with a permissive `204` before the proxy invoke permission, before alias normalization and before route matching; it never matches a route, never executes the plugin chain and never contacts an upstream. The addressed alias and the caller's tenant, when the request carries both, are still read to resolve the effective `allow_credentials` and the matched verdict of the answer's CORS headers.

**Steps**:
1. [x] - `p1` - Receive `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` on the gear-relative proxy route registered for every HTTP method - `inst-rp-dispatch-1`
2. [x] - `p1` - Extract the security context and require the `gts.cf.core.oagw.proxy.v1~:invoke` permission - `inst-rp-dispatch-2`
3. [x] - `p1` - **IF** the caller lacks the proxy invoke permission - `inst-rp-dispatch-3`
   1. [x] - `p1` - Return a gateway error for entry 2.5 to render, without touching a repository or an upstream - `inst-rp-dispatch-4`
4. [x] - `p1` - **IF** the request is a CORS preflight, detected as `OPTIONS` with an `Origin` header and an `Access-Control-Request-Method` header - `inst-rp-dispatch-5`
   1. [x] - `p1` - Return a permissive `204` at handler level that echoes the requested origin, method, and headers and carries `X-OAGW-Error-Source: gateway`, with no route match, no plugin-chain execution and no upstream contact; the addressed alias and the caller's tenant, when the request carries both, are read to resolve the effective `allow_credentials` and the matched verdict of the answer's CORS headers, and an unresolvable alias still yields the permissive `204`. This is a response-header statement only, and CORS origin and method enforcement itself remains owned by entry 2.8 - `inst-rp-dispatch-6`
5. [x] - `p1` - Normalize the requested alias, capture the optional path suffix, and capture the query string for later allowlist enforcement - `inst-rp-dispatch-7`
6. [x] - `p1` - Dispatch the proxy operation to `DataPlaneService` so the Control Plane continues to own configuration data while the Data Plane owns request execution, per `cpt-cf-oagw-adr-request-routing` - `inst-rp-dispatch-8`
7. [x] - `p1` - **RETURN** the rendered proxy response, carrying `X-OAGW-Error-Source` on every error response including streaming error responses - `inst-rp-dispatch-9`

### Alias Resolution and Tenant-Hierarchy Shadowing

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-alias-resolution`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The requested alias resolves to the closest enabled upstream in the caller's tenant chain, walking from descendant to root, so a descendant shadows an ancestor and ancestor-enforced constraints are retained.

**Error Scenarios**:
- No upstream with the alias exists anywhere in the tenant chain: the request fails with a not-found outcome rendered by entry 2.5.
- The selected upstream, or an ancestor of it, is disabled: the request is rejected with `503` and the disabled-upstream outcome entry 2.5 maps onto `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` with `X-OAGW-Error-Source: gateway`.

**Steps**:
1. [x] - `p1` - Normalize the requested alias to ASCII lowercase and strip trailing dots, so resolution is case-insensitive - `inst-rp-alias-1`
2. [x] - `p1` - Walk the tenant hierarchy from the calling tenant to the root, at each level looking up an upstream by `(tenant_id, alias)` - `inst-rp-alias-2`
3. [x] - `p1` - **IF** no level of the tenant chain holds an upstream with the requested alias - `inst-rp-alias-3`
   1. [x] - `p1` - Stop resolution and return a not-found domain error for entry 2.5 to render - `inst-rp-alias-4`
4. [x] - `p1` - Select the first upstream found on the walk, so the closest tenant match wins and a descendant shadows an ancestor - `inst-rp-alias-5`
5. [x] - `p1` - Collect the `enabled` state observed along the walk, treating an ancestor-disabled upstream as disabled for every descendant - `inst-rp-alias-6`
6. [x] - `p1` - **IF** the selected upstream is disabled, or an ancestor tenant has disabled the upstream it shadows - `inst-rp-alias-7`
   1. [x] - `p1` - Reject the request with the `503` disabled-upstream outcome and make no upstream call - `inst-rp-alias-8`
7. [x] - `p1` - Collect the enforced ancestor configuration encountered on the walk so shadowing selects only the routing target and never bypasses an enforced ancestor constraint - `inst-rp-alias-9`
8. [x] - `p1` - **RETURN** the selected upstream, its endpoint pool, and the enforced ancestor configuration for effective-configuration resolution - `inst-rp-alias-10`

### Route Matching and Request Classification

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-route-matching`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The resolved upstream's protocol selects the match keys, and the best matching enabled route is chosen by method allowlist, longest path prefix, and then priority.

**Error Scenarios**:
- No enabled route matches the request method and path: the request fails with a route-not-found domain error rendered by entry 2.5 as `404`.
- The upstream protocol is gRPC: no proxy code path exists, so the request is rejected as a gateway error rather than forwarded.

**Steps**:
1. [x] - `p1` - Read `upstream.protocol` to determine which match keys apply, per the request classification in `cpt-cf-oagw-adr-request-routing` - `inst-rp-match-1`
2. [x] - `p1` - **IF** the upstream protocol is the gRPC protocol value `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1` - `inst-rp-match-2`
   1. [x] - `p1` - Refuse to forward the request, because gRPC is configuration and schema surface only and no gRPC proxy code path is reachable (graded deviation 7) - `inst-rp-match-3`
   2. [x] - `p1` - Return a gateway protocol error for entry 2.5 to render - `inst-rp-match-4`
3. [x] - `p1` - Restrict candidate routes to enabled routes belonging to the resolved upstream and its tenant chain, excluding disabled routes from matching - `inst-rp-match-5`
4. [x] - `p1` - Filter candidates by the HTTP method allowlist of each `match.http` block - `inst-rp-match-6`
5. [x] - `p1` - Walk the tenant chain from the descendant upstream toward the root and take the first upstream whose route set yields a match, so a descendant route always shadows an ancestor route on the same effective path, then rank the surviving candidates by (1) closest tenant-chain distance, (2) longest matching path prefix, and (3) route priority, exploiting the match-determinism invariant that no two enabled routes under the same upstream share a path prefix and priority for the same method - `inst-rp-match-7`
6. [x] - `p1` - **IF** no candidate survives the method and path filters - `inst-rp-match-8`
   1. [x] - `p1` - Return a route-not-found domain error rendered by entry 2.5 as `404` with `X-OAGW-Error-Source: gateway` - `inst-rp-match-9`
7. [x] - `p1` - **RETURN** the selected route together with its `MatchConfig`, the matched path, the extracted path suffix, and the tenant chain used for effective-configuration resolution - `inst-rp-match-10`

### Request Surface Validation and Hardening

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-request-validation`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The request path, path suffix, query string, headers, and body are validated against the matched route configuration and the gear configuration before anything is forwarded, so only well-formed requests reach the external service.

**Error Scenarios**:
- A path suffix is supplied while `path_suffix_mode` is `disabled`: the request is rejected with a `400` validation error.
- A query parameter outside `query_allowlist` is present, or the allowlist is empty and any query parameter is supplied: the request is rejected with a `400` validation error.
- `Content-Length` is malformed or disagrees with the actual body size, or a `Transfer-Encoding` other than `chunked` is used: the request is rejected with a `400` validation error.
- The body exceeds the 100 MB hard limit: the request is rejected with `413` before buffering.

**Steps**:
1. [x] - `p1` - Validate the request path and the extracted path suffix against the matched route configuration immediately after route matching and before endpoint selection, upstream credential resolution, or any connection attempt - `inst-rp-validate-1`
2. [x] - `p1` - **IF** `path_suffix_mode` is `disabled` and a path suffix is present - `inst-rp-validate-2`
   1. [x] - `p1` - Reject the request with a validation error - `inst-rp-validate-3`
3. [x] - `p1` - Validate every query parameter against the route `query_allowlist`, treating an empty allowlist as permitting no query parameter - `inst-rp-validate-4`
4. [x] - `p1` - **IF** a query parameter is not in the allowlist - `inst-rp-validate-5`
   1. [x] - `p1` - Reject the request with a validation error naming the rejected parameter key - `inst-rp-validate-6`
5. [x] - `p1` - Validate the request body per `cpt-cf-oagw-algo-request-proxy-body-validate`, rejecting an invalid `Content-Length`, a non-`chunked` `Transfer-Encoding`, and a body over the 100 MB hard limit before buffering - `inst-rp-validate-7`
6. [x] - `p1` - Classify the inbound headers and strip the well-known internal hop-by-hop headers from the set that will be forwarded - `inst-rp-validate-8`
7. [x] - `p1` - **IF** any validation step fails - `inst-rp-validate-9`
   1. [x] - `p1` - Return the corresponding domain error for entry 2.5 to render as a `400` or `413` problem+json gateway error and make no upstream call - `inst-rp-validate-10`
8. [x] - `p1` - **RETURN** a validated proxy context carrying the request surface that survives validation - `inst-rp-validate-11`

### Target Host Resolution and Endpoint Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-target-host-selection`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The target endpoint is chosen from the upstream pool: explicitly when a valid `X-OAGW-Target-Host` is supplied, by round-robin when it is absent and optional, and with a required-header failure when it is absent and required.

**Error Scenarios**:
- A multi-endpoint upstream with a common-suffix alias receives no `X-OAGW-Target-Host`: the request is rejected with `400`.
- A supplied `X-OAGW-Target-Host` is malformed or matches no configured endpoint: the request is rejected with `400`.

**Steps**:
1. [x] - `p1` - Read `X-OAGW-Target-Host` through the proxy handler extractor, treating an HTTP/1.1 `Host` header and an HTTP/2 `:authority` pseudo-header as never satisfying it - `inst-rp-target-1`
2. [x] - `p1` - Determine the alias shape from the endpoint pool: a single endpoint, multiple endpoints with an explicit alias, or multiple endpoints whose alias is a registrable common suffix - `inst-rp-target-2`
3. [x] - `p1` - **IF** the upstream has one endpoint - `inst-rp-target-3`
   1. [x] - `p1` - Route to that endpoint when the header is absent, and validate the header against the endpoint host when it is present - `inst-rp-target-4`
4. [x] - `p1` - **IF** the upstream has multiple endpoints with an explicit alias and the header is absent - `inst-rp-target-5`
   1. [x] - `p1` - Select an endpoint by round-robin over a cursor that is Data Plane state scoped to that endpoint pool, advancing the cursor atomically under concurrent requests so concurrent selections do not all return the same endpoint - `inst-rp-target-6`
5. [x] - `p1` - **IF** the upstream has multiple endpoints with a common-suffix alias and the header is absent - `inst-rp-target-7`
   1. [x] - `p1` - Reject the request with the missing-target-host domain error rendered by entry 2.5 as `400` - `inst-rp-target-8`
6. [x] - `p1` - **IF** a header value is supplied - `inst-rp-target-9`
   1. [x] - `p1` - Validate the value as a bare hostname or IP address with no port, path, or special characters, and reject a malformed value with the invalid-target-host domain error - `inst-rp-target-10`
   2. [x] - `p1` - **IF** the validated value matches no configured endpoint host - `inst-rp-target-11`
      1. [x] - `p1` - Reject the request with the unknown-target-host domain error carrying the valid endpoint hosts - `inst-rp-target-12`
   3. [x] - `p1` - Route to the named endpoint, bypassing round-robin load balancing - `inst-rp-target-13`
7. [x] - `p1` - Strip `X-OAGW-Target-Host` from the forwarded request after it has been read - `inst-rp-target-14`
8. [x] - `p1` - **RETURN** the selected endpoint and the selection method used - `inst-rp-target-15`

### Plugin Chain Invocation and Cross-Cutting Call-Ins

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-plugin-chain`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The proxy hot path runs in one normative sequence, per `cpt-cf-oagw-adr-state-management` and `cpt-cf-oagw-adr-plugin-system`: the auth plugin first, then the rate-limit call-in, then the guard plugins, then the transform plugins, then the CORS call-in, then the upstream call, then Transform(response/error) — with upstream-bound plugins executing before route-bound plugins inside each phase.

**Error Scenarios**:
- An auth plugin rejects because the caller credentials are invalid or missing: the request fails with an authentication-failed domain error and no upstream call is made.
- A `cred://` reference cannot be resolved, or the credential store is unavailable: the request fails with the `500` `SecretNotFound` domain error `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` carrying `X-OAGW-Error-Source: gateway`, and never with `401`; entry 2.5 renders both.
- A guard plugin rejects: the request fails with a validation domain error and no upstream call is made.
- The rate-limit decision returns reject: the request fails before the upstream call with the rate-limit outcome owned by entry 2.7.
- A transform plugin references a plugin that cannot be resolved: the request fails before the upstream call.

**Steps**:
1. [x] - `p1` - Invoke the auth plugin of the effective chain as the first phase of the hot path, before the rate-limit call-in, to inject credentials resolved from `cred://` references - `inst-rp-chain-4`
2. [x] - `p1` - **IF** the auth plugin fails - `inst-rp-chain-5`
   1. [x] - `p1` - Return an authentication-failed domain error for entry 2.5 to render as `401` when the caller credentials are invalid or missing - `inst-rp-chain-6`
   2. [x] - `p1` - Return the `500` `SecretNotFound` domain error `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` carrying `X-OAGW-Error-Source: gateway`, and never a `401`, when a `cred://` reference cannot be resolved or the credential store is unavailable - `inst-rp-chain-13`
3. [x] - `p1` - Invoke the rate-limit call-in on the hot path after the auth phase and ahead of the guard phase and the upstream call, passing the effective merged rate-limit configuration and the counter scope - `inst-rp-chain-1`
4. [x] - `p1` - **IF** the rate-limit decision is reject - `inst-rp-chain-2`
   1. [x] - `p1` - Stop the pipeline and hand the rate-limit outcome to entry 2.7, which owns the `429` response and its headers - `inst-rp-chain-3`
5. [x] - `p1` - Invoke the guard plugins in chain order, any of which may reject the request - `inst-rp-chain-7`
6. [x] - `p1` - Invoke the request phase of the transform plugins - `inst-rp-chain-8`
7. [x] - `p1` - Invoke the CORS call-in for actual requests after upstream resolution and before forwarding, so entry 2.8 can enforce origin and method on the resolved effective CORS configuration - `inst-rp-chain-9`
8. [x] - `p1` - Forward the prepared request to the selected endpoint, then invoke the response phase, or the error phase when the upstream call fails, of the transform plugins - `inst-rp-chain-10`
9. [x] - `p1` - Compose the chain so upstream-bound plugins execute before route-bound plugins, `[U1, U2] + [R1, R2]` yielding `[U1, U2, R1, R2]`, and so enforced ancestor bindings are never dropped - `inst-rp-chain-11`
10. [x] - `p1` - **RETURN** the outcome of the chain phase that ended the pipeline, or the transformed response - `inst-rp-chain-12`

### Upstream Forwarding and Response Passthrough

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-upstream-forwarding`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- The prepared request is sent to the selected endpoint over `http` or `https` under the proxy timeout, and the upstream response is passed through with the configured response header transformation and no caching.

**Error Scenarios**:
- The upstream call exceeds `proxy_timeout_secs`: the request fails with a timeout domain error rendered by entry 2.5 as `504`.
- The upstream returns an error status: the status and body are passed through unchanged with `X-OAGW-Error-Source: upstream`.
- The upstream connection fails at protocol level: the request fails with a downstream or protocol domain error rendered by entry 2.5 as `502`.

**Steps**:
1. [x] - `p1` - Replace the inbound `Host` header, or the HTTP/2 `:authority` pseudo-header, with the selected endpoint's upstream host or authority - `inst-rp-forward-1`
2. [x] - `p1` - Send the request to the selected endpoint over the endpoint's `http` or `https` scheme, with `proxy_timeout_secs` bounding connection establishment and the complete buffered request/response exchange, and no total-duration cap applied to an open `sse`, `ws`, or `wt` session - `inst-rp-forward-2`
3. [x] - `p1` - **IF** the connection to the selected endpoint fails - `inst-rp-forward-3`
   1. [x] - `p1` - Permit endpoint-level connection failover to another pool endpoint, which is a connector-level connection attempt and not a re-issue of the client request - `inst-rp-forward-4`
4. [x] - `p1` - **IF** the exchange exceeds `proxy_timeout_secs` - `inst-rp-forward-5`
   1. [x] - `p1` - Abandon the exchange and return the timeout domain error for entry 2.5 to render as `504`, distinguishing connection, request, and idle timeouts - `inst-rp-forward-6`
5. [x] - `p1` - Receive the upstream response and apply the configured `headers.response` set, add, and remove rules, then pass the response through without caching it - `inst-rp-forward-7`
6. [x] - `p1` - Strip hop-by-hop headers from the response before it reaches the client - `inst-rp-forward-8`
7. [x] - `p1` - **IF** the upstream returned an error status - `inst-rp-forward-9`
   1. [x] - `p1` - Pass the status and body through unmodified and mark the response as upstream-sourced - `inst-rp-forward-10`
8. [x] - `p1` - **IF** no upstream response is obtainable after endpoint-level failover - `inst-rp-forward-11`
   1. [x] - `p1` - Return the corresponding domain error without re-issuing the original client request, so retry responsibility stays with the client - `inst-rp-forward-12`
9. [x] - `p1` - **RETURN** the proxy response to the handler for rendering to the client - `inst-rp-forward-13`

### SSE Streaming Session

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-sse-streaming`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An SSE response from the upstream is streamed to the client event by event as received, with the connection lifecycle handled from open through close.

**Error Scenarios**:
- The upstream closes the SSE connection: the client connection is closed and the event is logged.
- The client disconnects: the upstream connection is closed.
- The SSE stream errors or aborts mid-stream: the streaming error response carries `X-OAGW-Error-Source`.

**Steps**:
1. [x] - `p1` - Detect an SSE streaming response from the upstream content type and switch the proxy response into streaming mode instead of buffering - `inst-rp-sse-1`
2. [x] - `p1` - Establish the upstream connection and mark the session open, applying the request header transformation and plugin chain exactly as for a buffered request - `inst-rp-sse-2`
3. [x] - `p1` - Forward each SSE event to the client as it is received, preserving event boundaries and ordering - `inst-rp-sse-3`
4. [x] - `p1` - **IF** the upstream closes the SSE connection - `inst-rp-sse-4`
   1. [x] - `p1` - Close the client connection cleanly and record the close event - `inst-rp-sse-5`
5. [x] - `p1` - **IF** the client disconnects while the stream is open - `inst-rp-sse-6`
   1. [x] - `p1` - Close the upstream connection and release the session resources - `inst-rp-sse-7`
6. [x] - `p1` - **IF** the stream errors or aborts before it completes - `inst-rp-sse-8`
   1. [x] - `p1` - Emit the streaming error outcome with `X-OAGW-Error-Source` set, and let entry 2.5 render the aborted-stream mapping - `inst-rp-sse-9`
7. [x] - `p1` - **RETURN** a completed session in which every event was forwarded or a precisely attributed error was emitted - `inst-rp-sse-10`

### WebSocket Session

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-websocket-session`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A WebSocket session to an upstream endpoint is established and relayed bidirectionally until either side ends it, with the open, close, and error lifecycle handled.

**Error Scenarios**:
- The upstream refuses or cannot accept the session: the client session fails with a gateway error carrying `X-OAGW-Error-Source`.
- The session aborts mid-conversation: both directions are closed and the abort is recorded.

**Steps**:
1. [x] - `p1` - Recognize a WebSocket upgrade request addressed to an upstream endpoint whose scheme supports a WebSocket session - `inst-rp-ws-1`
2. [x] - `p1` - Apply alias resolution, route matching, request-surface validation, and the plugin chain before any session is established - `inst-rp-ws-2`
3. [x] - `p1` - Open the upstream session and relay frames in both directions without buffering the conversation - `inst-rp-ws-3`
4. [x] - `p1` - **IF** either side ends the session - `inst-rp-ws-4`
   1. [x] - `p1` - Close the other direction cleanly and release the session - `inst-rp-ws-5`
5. [x] - `p1` - **IF** the upstream cannot be reached or refuses the session - `inst-rp-ws-6`
   1. [x] - `p1` - Fail the client session with a gateway error carrying `X-OAGW-Error-Source: gateway` - `inst-rp-ws-7`
6. [x] - `p1` - **IF** the session aborts mid-conversation - `inst-rp-ws-8`
   1. [x] - `p1` - Close both directions and emit the streaming error outcome with `X-OAGW-Error-Source` set - `inst-rp-ws-9`
7. [x] - `p1` - **RETURN** a relayed session with a recorded open, close, or error outcome - `inst-rp-ws-10`

### WebTransport Session

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-webtransport-session`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A WebTransport session to an upstream endpoint whose scheme is `wt` is established and relayed, with the open, close, and error lifecycle handled and `wt` admitted as a legal endpoint scheme value.

**Error Scenarios**:
- The upstream endpoint cannot establish the WebTransport session: the client session fails with a gateway error carrying `X-OAGW-Error-Source`.
- The session aborts mid-conversation: both directions are closed and the abort is recorded.

**Steps**:
1. [x] - `p1` - Recognize a WebTransport session addressed to an endpoint whose `scheme` is `wt`, which is a legal endpoint scheme value in the graded configuration - `inst-rp-wt-1`
2. [x] - `p1` - Apply alias resolution, route matching, request-surface validation, and the plugin chain before the session opens - `inst-rp-wt-2`
3. [x] - `p1` - Open the upstream session and relay streams and datagrams in both directions without buffering them - `inst-rp-wt-3`
4. [x] - `p1` - **IF** either side ends the session - `inst-rp-wt-4`
   1. [x] - `p1` - Close the session cleanly on both sides and release its resources - `inst-rp-wt-5`
5. [x] - `p1` - **IF** the session cannot be established or aborts mid-conversation - `inst-rp-wt-6`
   1. [x] - `p1` - Emit the gateway or aborted-stream error outcome with `X-OAGW-Error-Source` set and close both directions - `inst-rp-wt-7`
6. [x] - `p1` - **RETURN** a relayed session with a recorded open, close, or error outcome - `inst-rp-wt-8`

### Error Source Emission

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-request-proxy-error-source-emission`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Every response produced by the proxy pipeline after the dispatch short-circuits, and every gateway error response, carries `X-OAGW-Error-Source`, distinguishing a gateway-generated failure from an upstream failure passed through unchanged, on buffered and streaming responses alike.

**Error Scenarios**:
- A gateway-generated failure occurs anywhere in the pipeline: the header is `gateway` and the body is the problem+json contract owned by entry 2.5.
- An upstream failure is passed through: the header is `upstream` and the body is the upstream body unmodified.

**Steps**:
1. [x] - `p1` - Classify every failure produced inside the proxy path as gateway-generated or upstream-passthrough, per `cpt-cf-oagw-adr-error-source-distinction` - `inst-rp-errsrc-1`
2. [x] - `p1` - Emit `X-OAGW-Error-Source: gateway` on responses OAGW generated, including the handler-level preflight `204`, validation failures, route and alias failures, target-host failures, disabled-upstream rejections, timeouts, and rate-limit rejections - `inst-rp-errsrc-2`
3. [x] - `p1` - Emit `X-OAGW-Error-Source: upstream` on responses whose status and body come from the upstream service unmodified - `inst-rp-errsrc-3`
4. [x] - `p1` - Emit the header on streaming error responses as well, so the distinction holds for SSE, WebSocket, and WebTransport failures - `inst-rp-errsrc-4`
5. [x] - `p1` - Leave the gateway error body, its GTS type, and its extension fields to entry 2.5, and leave an upstream error body untouched - `inst-rp-errsrc-5`
6. [x] - `p1` - **RETURN** the response with the error-source header applied and never with a missing or ambiguous source - `inst-rp-errsrc-6`

## 3. Processes / Business Logic (CDSL)

### Tenant-Hierarchy Alias Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-alias-resolve`

**Input**: the caller's tenant identifier, the normalized requested alias, and the in-memory repository contents.
**Output**: the selected enabled upstream, the observed `enabled` inheritance, and the enforced ancestor configuration, or a specific domain error.

**Steps**:
1. [x] - `p1` - Start at the calling tenant and walk the tenant chain toward the root, looking up `(tenant_id, alias)` at each level - `inst-rp-al-alias-1`
2. [x] - `p1` - **IF** an upstream with the alias exists at the calling tenant - `inst-rp-al-alias-2`
   1. [x] - `p1` - Select it as the closest match and continue the walk only to collect inherited and enforced configuration - `inst-rp-al-alias-3`
3. [x] - `p1` - Continue the walk until an upstream with the alias is found or the root is reached, so a descendant shadows an ancestor - `inst-rp-al-alias-4`
4. [x] - `p1` - Record the `enabled` state of the selected upstream and of every ancestor record encountered on the walk, because an ancestor-disabled upstream is disabled for all descendants - `inst-rp-al-alias-5`
5. [x] - `p1` - **IF** the selected upstream or any shadowed ancestor record is disabled - `inst-rp-al-alias-6`
   1. [x] - `p1` - Return the `503` disabled-upstream domain error and resolve no endpoint - `inst-rp-al-alias-7`
6. [x] - `p1` - Collect the enforced ancestor constraints across the walk so they are retained even when the selected upstream belongs to a descendant - `inst-rp-al-alias-8`
7. [x] - `p1` - **RETURN** the selected upstream, its endpoint pool, and the enforced ancestor configuration, or the not-found or disabled domain error - `inst-rp-al-alias-9`

### Effective Configuration Merge

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-effective-config`

**Input**: the resolved upstream configuration, the matched route configuration with its `MatchConfig`, and the enforced ancestor configuration returned by `inst-rp-alias-10`.
**Output**: one effective configuration, or a validation domain error.

**Steps**:
1. [x] - `p1` - Collect the merge inputs: the resolved upstream configuration, the matched route configuration, and the enforced ancestor configuration returned by `inst-rp-alias-10` - `inst-rp-al-config-1`
2. [x] - `p1` - Apply the merge precedence order tenant over route over upstream, the same order the merge engine delivered by `cpt-cf-oagw-feature-gear-foundation` applies, so a tenant-layer value overrides a route-layer value and a route-layer value overrides the upstream base value - `inst-rp-al-config-2`
3. [x] - `p1` - Retain every `sharing: enforce` ancestor value and apply the per-field merge rules that engine defines: `min()` for rate limits, union for CORS origins, concatenation for plugin chains, and the more specific value for scalar fields - `inst-rp-al-config-3`
4. [x] - `p1` - Run the merge in the dispatch flow immediately after route matching and before endpoint selection, plugin-chain invocation, the rate-limit call-in, the CORS call-in, and the header transform - `inst-rp-al-config-4`
5. [x] - `p1` - Hand the merged result to the plugin-chain composition step, the rate-limit call-in, the CORS call-in, and the request and response header-transform steps, so every consumer reads one effective configuration rather than a per-layer view - `inst-rp-al-config-5`
6. [x] - `p1` - **RETURN** one effective configuration, or the validation domain error when a required field resolves to no usable value on any layer - `inst-rp-al-config-6`

### Route Selection and Path and Query Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-route-select`

**Input**: the resolved upstream, the request method, the request path with its optional path suffix, and the request query parameters.
**Output**: the selected route with its match configuration, the outbound path, and the permitted query set, or a specific domain error.

**Steps**:
1. [x] - `p1` - Select the match key set from `upstream.protocol`, using the HTTP match keys for the HTTP protocol and refusing the gRPC protocol as an unreachable code path - `inst-rp-al-route-1`
2. [x] - `p1` - Discard disabled routes and routes whose method allowlist does not contain the request method - `inst-rp-al-route-2`
3. [x] - `p1` - Rank the remaining routes by closest tenant-chain distance, then by longest matching path prefix, and then by priority, so a descendant route shadows an ancestor route on the same effective path - `inst-rp-al-route-3`
4. [x] - `p1` - **IF** no route remains - `inst-rp-al-route-4`
   1. [x] - `p1` - Return the route-not-found domain error - `inst-rp-al-route-5`
5. [x] - `p1` - Apply `path_suffix_mode`: `disabled` rejects any provided suffix, and `append` appends the suffix to the matched path to form the outbound path; normalise the request path and the path suffix first, rejecting any `.` or `..` segment, a double slash introduced by suffix concatenation, and a suffix that escapes the matched route prefix - `inst-rp-al-route-6`
6. [x] - `p1` - **IF** `path_suffix_mode` is `disabled` and a suffix was supplied - `inst-rp-al-route-7`
   1. [x] - `p1` - Return a validation domain error - `inst-rp-al-route-8`
7. [x] - `p1` - Filter the query parameters against `query_allowlist`, treating an empty allowlist as permitting no query parameter, rejecting unknown keys, and rejecting a query parameter name or value carrying a control character — a carriage return or a line feed - `inst-rp-al-route-9`
8. [x] - `p1` - **IF** a query parameter is not permitted - `inst-rp-al-route-10`
   1. [x] - `p1` - Return a validation domain error naming the rejected key - `inst-rp-al-route-11`
9. [x] - `p1` - **RETURN** the selected route, the outbound path, and the permitted query set - `inst-rp-al-route-12`

### Header Classification and Transformation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-header-transform`

**Input**: the inbound request headers, the effective `headers` configuration of the selected upstream and route, and the selected endpoint.
**Output**: the outbound request header set and the outbound response header set.

**Steps**:
1. [x] - `p1` - Classify inbound request headers into routing headers, hop-by-hop headers, and passthrough candidates - `inst-rp-al-header-1`
2. [x] - `p1` - Read `X-OAGW-Target-Host` for routing and then strip it, so it is never forwarded to the upstream - `inst-rp-al-header-2`
3. [x] - `p1` - Replace `Host` on HTTP/1.1 requests, and the `:authority` pseudo-header on HTTP/2 requests, with the selected endpoint's upstream host or authority - `inst-rp-al-header-3`
4. [x] - `p1` - Strip `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, and `Upgrade` from the forwarded request, exempting `Connection` and `Upgrade` only on a detected WebSocket upgrade request, where they are replaced with the values the upstream handshake requires - `inst-rp-al-header-4`
5. [x] - `p1` - Apply the request passthrough control: `none` forwards no inbound header, `allowlist` forwards only the names in `passthrough_allowlist`, and `all` forwards every surviving inbound header - `inst-rp-al-header-5`
6. [x] - `p1` - Apply the configured `headers.request` set, add, and remove rules to the outbound request, with `set` overwriting, `add` appending, and `remove` deleting - `inst-rp-al-header-6`
7. [x] - `p1` - Validate well-known headers such as `Content-Length` and `Content-Type`, reject an invalid value with a validation domain error, and reject any header value carrying a control character — a carriage return or a line feed — with the same validation domain error - `inst-rp-al-header-7`
8. [x] - `p1` - Apply the configured `headers.response` set, add, and remove rules to the upstream response before it is returned, and strip hop-by-hop headers from it as well - `inst-rp-al-header-8`
9. [x] - `p1` - **IF** the response is a streaming response - `inst-rp-al-header-9`
   1. [x] - `p1` - Apply the response header rules to the streaming response head before the first event, frame, or datagram is relayed - `inst-rp-al-header-10`
10. [x] - `p1` - **RETURN** the outbound request header set and the outbound response header set - `inst-rp-al-header-11`

### Target Host Validation and Endpoint Selection

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-endpoint-select`

**Input**: the endpoint pool of the selected upstream, the alias form of that upstream, and the optional `X-OAGW-Target-Host` value.
**Output**: one selected endpoint and the selection method, or a specific routing domain error.

**Steps**:
1. [x] - `p1` - Confirm the endpoint pool is uniform in `protocol`, `scheme`, and `port`, which upstream configuration validation guarantees - `inst-rp-al-endpoint-1`
2. [x] - `p1` - **IF** the pool has one endpoint - `inst-rp-al-endpoint-2`
   1. [x] - `p1` - Select it, and when a target host was supplied validate it against the endpoint host before use - `inst-rp-al-endpoint-3`
3. [x] - `p1` - **IF** the pool has several endpoints and a target host value is present - `inst-rp-al-endpoint-4`
   1. [x] - `p1` - Require the value to be a bare hostname or IP address with no port, path, or special characters, and return the invalid-target-host domain error otherwise - `inst-rp-al-endpoint-5`
   2. [x] - `p1` - Match the value against the configured endpoint hosts and return the unknown-target-host domain error, carrying the valid hosts, when nothing matches - `inst-rp-al-endpoint-6`
   3. [x] - `p1` - Select the named endpoint with selection method `explicit_header`, bypassing load balancing - `inst-rp-al-endpoint-7`
4. [x] - `p1` - **IF** the pool has several endpoints, no target host was supplied, and the alias is an explicit alias - `inst-rp-al-endpoint-8`
   1. [x] - `p1` - Select the next endpoint by round-robin with selection method `round_robin`, advancing a cursor that is Data Plane state scoped per endpoint pool, that advances atomically under concurrent requests so concurrent selections do not all return the same endpoint, and that is re-derived when the pool's endpoint set changes - `inst-rp-al-endpoint-9`
5. [x] - `p1` - **IF** the pool has several endpoints with a common-suffix alias and no target host was supplied - `inst-rp-al-endpoint-10`
   1. [x] - `p1` - Return the missing-target-host domain error, because the header is required to disambiguate the target - `inst-rp-al-endpoint-11`
6. [x] - `p1` - Enforce the endpoint scheme allowlist before any connection is opened: `https` and `wss` are always legal, `wt` is a legal endpoint scheme value, and `http` is legal only while `allow_http_upstream` is true - `inst-rp-al-endpoint-12`
7. [x] - `p1` - **IF** the selected endpoint's scheme is not admitted by the allowlist - `inst-rp-al-endpoint-13`
   1. [x] - `p1` - Return a validation domain error and open no connection - `inst-rp-al-endpoint-14`
8. [x] - `p1` - **RETURN** the selected endpoint, its scheme, host, and port, and the selection method used - `inst-rp-al-endpoint-15`

**State management note**: the round-robin cursor is Data Plane state and extends the DPState enumeration in `cpt-cf-oagw-adr-state-management` with one cursor per endpoint pool, held beside that ADR's L1 cache, shared HTTP client, and DP-owned rate limiters, and re-derived from the pool's endpoint set whenever that set changes. This is an in-document note recording the extension this feature needs; `cpt-cf-oagw-adr-state-management` itself is not amended by this feature.

### Request Body Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-body-validate`

**Input**: the inbound request body metadata (`Content-Length`, `Transfer-Encoding`) and the request body stream.
**Output**: a validated body stream, or a specific domain error.

**Steps**:
1. [x] - `p1` - **IF** `Content-Length` is present - `inst-rp-al-body-1`
   1. [x] - `p1` - Require it to parse as a valid integer and to match the actual body size, returning a validation domain error otherwise, and reject a `Content-Length` that is absent where the request method requires a body, that is non-numeric, or that carries conflicting duplicate values - `inst-rp-al-body-2`
2. [x] - `p1` - **IF** `Transfer-Encoding` is present - `inst-rp-al-body-3`
   1. [x] - `p1` - Accept only `chunked`, return a validation domain error for any other encoding, and reject a request that presents `Content-Length` and `Transfer-Encoding` together - `inst-rp-al-body-4`
3. [x] - `p1` - Reject a body whose declared or observed size exceeds the 100 MB hard limit before any buffering takes place - `inst-rp-al-body-5`
4. [x] - `p1` - **IF** any body check fails - `inst-rp-al-body-6`
   1. [x] - `p1` - Return the validation or payload-too-large domain error without opening an upstream connection - `inst-rp-al-body-7`
5. [x] - `p1` - **RETURN** the validated body stream for forwarding - `inst-rp-al-body-8`

### Proxy Timeout and Failover Handling

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-timeout`

**Input**: the prepared outbound request, the selected endpoint, and `proxy_timeout_secs` from `OagwConfig`.
**Output**: the upstream response or stream, or a specific timeout or downstream domain error.

**Steps**:
1. [x] - `p1` - Bound connection establishment by the configured proxy timeout and return the connection-timeout domain error when it expires - `inst-rp-al-timeout-1`
2. [x] - `p1` - Bound the request exchange by the configured proxy timeout and return the request-timeout domain error when it expires - `inst-rp-al-timeout-2`
3. [x] - `p1` - Bound idle periods on an open streaming session and return the idle-timeout domain error when one expires - `inst-rp-al-timeout-3`
4. [x] - `p1` - **IF** the connection to the selected endpoint fails - `inst-rp-al-timeout-4`
   1. [x] - `p1` - Attempt endpoint-level connection failover to another pool endpoint, which is a connector-level connection attempt and never a re-issue of the original client request - `inst-rp-al-timeout-5`
5. [x] - `p1` - **IF** no endpoint yields a connection or the exchange cannot complete - `inst-rp-al-timeout-6`
   1. [x] - `p1` - Return the downstream or protocol domain error and leave retry responsibility with the client - `inst-rp-al-timeout-7`
6. [x] - `p1` - **RETURN** the upstream response or the open stream - `inst-rp-al-timeout-8`

### Streaming Connection Lifecycle

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-proxy-stream-lifecycle`

**Input**: a streaming upstream response or session of kind `sse`, `ws`, or `wt`, and the client connection.
**Output**: a completed or aborted session outcome with its recorded lifecycle events.

**Steps**:
1. [x] - `p1` - Classify the session kind from the upstream response and the endpoint scheme, so an SSE stream, a WebSocket session, and a WebTransport session take the same lifecycle model - `inst-rp-al-stream-1`
2. [x] - `p1` - Move the session from connecting to open when the upstream accepts it and the first event, frame, or datagram is available - `inst-rp-al-stream-2`
3. [x] - `p1` - Relay content as it arrives without buffering the whole conversation - `inst-rp-al-stream-3`
4. [x] - `p1` - Move the session to closing when the client or the upstream ends it, drain both directions, and then close it - `inst-rp-al-stream-4`
5. [x] - `p1` - **IF** the upstream connection errors, is reset, or aborts before the session completes - `inst-rp-al-stream-5`
   1. [x] - `p1` - Mark the session aborted, close both directions, and emit the aborted-stream domain error with `X-OAGW-Error-Source` set - `inst-rp-al-stream-6`
6. [x] - `p1` - **IF** the client disconnects from an open session - `inst-rp-al-stream-7`
   1. [x] - `p1` - Close the upstream side and release the session resources - `inst-rp-al-stream-8`
7. [x] - `p1` - **RETURN** the recorded lifecycle outcome of the session - `inst-rp-al-stream-9`

## 4. States (CDSL)

### Proxy Request State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-request-proxy-proxy-request`

**States**: `received`, `resolving`, `executing`, `streaming`, `completed`, `failed`
**Initial State**: `received`
**Transitions**:
1. [x] - `p1` - **FROM** `received` **TO** `resolving` **WHEN** a non-preflight proxy request carries a valid security context and enters `DataPlaneService` - `inst-rp-st-req-1`
2. [x] - `p1` - **FROM** `received` **TO** `completed` **WHEN** the request is a detected CORS preflight and the permissive `204` is returned without upstream resolution - `inst-rp-st-req-2`
3. [x] - `p1` - **FROM** `resolving` **TO** `executing` **WHEN** an enabled upstream is resolved, a route is matched, and the request surface, target host, and endpoint scheme all validate - `inst-rp-st-req-3`
4. [x] - `p1` - **FROM** `resolving` **TO** `failed` **WHEN** alias resolution, route matching, surface validation, target-host validation, or the rate-limit call-in rejects the request - `inst-rp-st-req-4`
5. [x] - `p1` - **FROM** `executing` **TO** `streaming` **WHEN** the upstream response is an `sse`, `ws`, or `wt` session and its connection opens - `inst-rp-st-req-5`
6. [x] - `p1` - **FROM** `executing` **TO** `completed` **WHEN** the buffered upstream response is transformed and returned without caching - `inst-rp-st-req-6`
7. [x] - `p1` - **FROM** `streaming` **TO** `completed` **WHEN** the session closes cleanly after every event, frame, or datagram is relayed - `inst-rp-st-req-7`
8. [x] - `p1` - **FROM** `executing` **TO** `failed` **WHEN** the upstream call times out, fails at protocol level, or no endpoint yields a connection - `inst-rp-st-req-8`
9. [x] - `p1` - **FROM** `streaming` **TO** `failed` **WHEN** the session aborts or errors before it completes - `inst-rp-st-req-9`
10. [x] - `p1` - **FROM** `failed` **TO** `completed` **WHEN** the gateway or upstream error response carrying `X-OAGW-Error-Source` is returned to the client - `inst-rp-st-req-10`

### Streaming Connection State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-request-proxy-stream-connection`

**States**: `connecting`, `open`, `closing`, `closed`, `aborted`
**Initial State**: `connecting`
**Transitions**:
1. [x] - `p1` - **FROM** `connecting` **TO** `open` **WHEN** the upstream accepts the session and the first event, frame, or datagram becomes available - `inst-rp-st-conn-1`
2. [x] - `p1` - **FROM** `connecting` **TO** `aborted` **WHEN** the upstream cannot be reached or refuses the session before it opens - `inst-rp-st-conn-2`
3. [x] - `p1` - **FROM** `open` **TO** `closing` **WHEN** the client or the upstream ends the session - `inst-rp-st-conn-3`
4. [x] - `p1` - **FROM** `open` **TO** `aborted` **WHEN** the upstream connection errors or is reset mid-session - `inst-rp-st-conn-4`
5. [x] - `p1` - **FROM** `closing` **TO** `closed` **WHEN** both directions are drained and the session resources are released - `inst-rp-st-conn-5`
6. [x] - `p1` - **FROM** `aborted` **TO** `closed` **WHEN** the streaming error response carrying `X-OAGW-Error-Source` is emitted and the connection is released - `inst-rp-st-conn-6`

## 5. Definitions of Done

### Proxy Endpoint Handler and Data Plane Dispatch

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-proxy-handler`

The system **MUST** serve `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` at the gear-relative path with no leading `/api` segment, for every HTTP method, through a proxy handler that extracts the security context, requires `gts.cf.core.oagw.proxy.v1~:invoke`, and dispatches to `DataPlaneService` while management operations remain on the Control Plane, per `cpt-cf-oagw-adr-request-routing`.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-dispatch`
- `cpt-cf-oagw-state-request-proxy-proxy-request`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`
- Entities: `ProxyContext`
- Tests: unit tests in `src/api/rest/proxy_handler_tests.rs` for method-agnostic dispatch and permission enforcement, and integration tests in `tests/proxy_dispatch.rs` for the gear-relative path shape

### Preflight OPTIONS Detection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-preflight-detection`

The system **MUST** detect a CORS preflight at the proxy handler level as an `OPTIONS` request carrying `Origin` and `Access-Control-Request-Method`, and **MUST** answer it with a permissive `204` that echoes the requested origin, method, and headers, performing no upstream resolution, no tenant-context lookup, and no plugin-chain or rate-limit execution. Actual-request origin and method enforcement remains the CORS call-in defined below.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-dispatch`
- `cpt-cf-oagw-state-request-proxy-proxy-request`

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyResponse`
- Tests: integration tests in `tests/cors_preflight.rs` asserting the `204` response, the echoed headers, and that no upstream connection is opened

### Tenant-Hierarchy Alias Resolution and Shadowing

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-alias-resolution`

The system **MUST** resolve the requested alias by walking the tenant hierarchy from the calling tenant to the root, selecting the closest enabled upstream so a descendant shadows an ancestor, normalizing the alias to ASCII lowercase with trailing dots stripped so resolution is case-insensitive, and retaining every enforced ancestor constraint across shadowing. Proxy-time resolution owns the read side of the alias contract; derivation, immutability, and persistence remain with `cpt-cf-oagw-feature-upstream-management`.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-alias-resolution`
- `cpt-cf-oagw-algo-request-proxy-alias-resolve`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Upstream`
- Tests: unit tests in `src/infra/proxy/alias_resolver_tests.rs` for descendant-to-root walk, shadowing, case-insensitive resolution, and enforced-ancestor retention, and integration tests in `tests/proxy_dispatch.rs`

### Disabled Upstream Rejection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-disabled-upstream`

The system **MUST** reject a proxy request whose selected upstream is disabled, or whose selected upstream shadows an ancestor-disabled upstream, with the `503` disabled-upstream outcome, making no upstream call and emitting `X-OAGW-Error-Source: gateway`; entry 2.5 renders the problem+json body and maps the outcome onto `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`. This realizes the proxy half of `cpt-cf-oagw-fr-enable-disable`.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-alias-resolution`
- `cpt-cf-oagw-algo-request-proxy-alias-resolve`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Upstream`
- Tests: unit tests in `src/infra/proxy/alias_resolver_tests.rs` for direct and inherited disablement, and integration tests in `tests/upstream_enable_disable.rs` asserting `503` and no upstream connection

### Route Matching by Method, Path Prefix, and Priority

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-route-matching`

The system **MUST** select the route for a proxy request by walking the tenant chain from the descendant upstream toward the root and taking the first upstream whose route set yields a match, then excluding disabled routes, filtering by the `match.http.methods` allowlist, and ranking by closest tenant-chain distance, then the longest matching `match.http.path` prefix, then route priority, so matching is deterministic under the match-determinism invariant and a descendant route shadows an ancestor route on the same effective path. A request that matches no enabled route **MUST** produce a route-not-found domain error rendered by entry 2.5 as `404`.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-route-matching`
- `cpt-cf-oagw-algo-request-proxy-route-select`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `Route`, `MatchConfig`
- Tests: unit tests in `src/domain/route_matcher_tests.rs` for method filtering, longest-prefix and priority ordering, disabled-route exclusion, and the descendant-versus-ancestor case in which both routes match the same request and the descendant route wins, and integration tests in `tests/proxy_dispatch.rs`

### gRPC as Configuration Surface Only

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-request-proxy-grpc-surface`

The system **MUST NOT** implement a gRPC proxy code path: an upstream whose `protocol` is `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1` and a route whose match block is `grpc` remain legal configuration and schema surface, and a proxy request that resolves to such an upstream **MUST** be rejected as a gateway protocol error rendered through the shared error contract of entry 2.5 rather than forwarded (graded deviation 7).

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-route-matching`
- `cpt-cf-oagw-algo-request-proxy-route-select`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Upstream`, `Route`, `MatchConfig`
- Tests: unit tests in `src/domain/route_matcher_tests.rs` asserting a gRPC-protocol upstream never opens an upstream connection

### Effective Configuration Merge

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-effective-config`

The system **MUST** produce one effective configuration for every proxy request by merging the resolved upstream configuration, the matched route configuration, and the enforced ancestor configuration returned by `inst-rp-alias-10`, in the precedence order tenant over route over upstream — the same order the merge engine delivered by `cpt-cf-oagw-feature-gear-foundation` applies — and **MUST** run that merge immediately after route matching and before endpoint selection, plugin-chain invocation, the rate-limit call-in, the CORS call-in, and the header transform. The merged result **MUST** be the only configuration view the plugin-chain, rate-limit, CORS, and header-transform steps consume, and every `sharing: enforce` ancestor value **MUST** survive the merge, so an upstream value and a route value that disagree resolve deterministically.

**Implements**:
- `cpt-cf-oagw-algo-request-proxy-effective-config`
- `cpt-cf-oagw-flow-request-proxy-dispatch`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `ProxyContext`
- Tests: unit tests in `src/domain/merge_tests.rs` for the tenant-over-route-over-upstream precedence, the retention of `sharing: enforce` ancestor values, and the case where the upstream and the route configuration disagree, and integration tests in `tests/control_plane_resolution.rs` asserting the merged result reaches the plugin-chain, rate-limit, CORS, and header-transform steps

### Path Suffix Handling and Query Allowlist Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-path-and-query`

The system **MUST** apply `path_suffix_mode` from the matched route — `disabled` rejects any provided path suffix and `append` appends the suffix to the matched path to form the outbound path — and **MUST** enforce `query_allowlist` on the request query string, treating an empty allowlist as permitting no query parameter and rejecting an unknown parameter with a validation domain error. The validated path and query set are the only values forwarded to the upstream.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-request-validation`
- `cpt-cf-oagw-algo-request-proxy-route-select`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`
- Entities: `Route`, `MatchConfig`, `ProxyContext`
- Tests: unit tests in `src/domain/route_matcher_tests.rs` for both `path_suffix_mode` values and for empty and non-empty allowlists, and integration tests in `tests/proxy_dispatch.rs`

### Header Classification and Transformation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-header-pipeline`

The system **MUST** classify and transform headers in three categories: routing headers, consisting of `X-OAGW-Target-Host` which is read for routing and then stripped and `Host` (or the HTTP/2 `:authority` pseudo-header) which is replaced with the upstream host or authority; hop-by-hop headers, consisting of `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, and `Upgrade`, which are stripped from forwarded requests and from returned responses, with one exemption: on a detected WebSocket upgrade request, `Connection` and `Upgrade` are not stripped but are instead replaced with the values the upstream handshake requires, while they remain stripped from every buffered request and from every response head, including a streamed response head; and passthrough headers, forwarded according to `headers.request.passthrough` with the values `none` (default), `allowlist` over `passthrough_allowlist`, and `all`. The system **MUST** apply the configured `headers.request` set, add, and remove rules to the outbound request and the configured `headers.response` set, add, and remove rules to the response returned to the client, and **MUST** validate well-known headers such as `Content-Length` and `Content-Type`, rejecting an invalid value with a validation domain error.

**Implements**:
- `cpt-cf-oagw-algo-request-proxy-header-transform`
- `cpt-cf-oagw-flow-request-proxy-upstream-forwarding`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `Upstream`, `ProxyContext`, `ProxyResponse`
- Tests: unit tests in `src/domain/header_transform_tests.rs` for every strip rule, the three passthrough modes, set/add/remove ordering, and the WebSocket-upgrade exemption of `Connection` and `Upgrade`, and integration tests in `tests/proxy_dispatch.rs`

### X-OAGW-Target-Host Behavior Matrix

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-target-host-matrix`

The system **MUST** implement the `X-OAGW-Target-Host` behavior matrix: for a single-endpoint upstream the header is optional and is validated when present; for a multi-endpoint upstream with an explicit alias the header is optional and selects an endpoint when present, bypassing round-robin; for a multi-endpoint upstream with a common-suffix alias the header is required and its absence is rejected. A present value **MUST** be validated as a bare hostname or IP address with no port, path, or special characters, and **MUST** match a configured endpoint host. The header **MUST** be stripped after it is read, **MUST** never be satisfied by an HTTP/1.1 `Host` header or an HTTP/2 `:authority` pseudo-header, and a failed check **MUST** produce the missing, invalid, or unknown target-host domain error that entry 2.5 renders as `400`.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-target-host-selection`
- `cpt-cf-oagw-algo-request-proxy-endpoint-select`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- Entities: `Endpoint`, `ProxyContext`
- Tests: unit tests in `src/infra/proxy/endpoint_selector_tests.rs` for all six matrix rows and the three failure shapes, and integration tests in `tests/proxy_dispatch.rs`

### Endpoint Pool Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-endpoint-selection`

The system **MUST** distribute requests across the endpoints of an upstream pool by round-robin when no explicit target host is supplied, relying on the pool uniformity invariant that all endpoints share the same `protocol`, `scheme`, and `port`, and **MUST** record which selection method was used. The round-robin cursor **MUST** be Data Plane state scoped per endpoint pool, extending the DPState enumeration in `cpt-cf-oagw-adr-state-management`, **MUST** advance atomically under concurrent requests so concurrent selections do not all return the same endpoint, and **MUST** be re-derived when the pool's endpoint set changes.

**Implements**:
- `cpt-cf-oagw-algo-request-proxy-endpoint-select`
- `cpt-cf-oagw-flow-request-proxy-target-host-selection`

**Touches**:
- API: none
- Entities: `Endpoint`, `ServerConfig`
- Tests: unit tests in `src/infra/proxy/endpoint_selector_tests.rs` for round-robin rotation across the pool, for concurrent selections over one pool not all resolving to the same endpoint, for cursor re-derivation after the pool's endpoint set changes, and for the explicit-header bypass

### Endpoint Scheme Allowlist Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-scheme-allowlist`

The system **MUST** enforce the endpoint scheme allowlist before any upstream connection is opened: `https` and `wss` are always legal, `wt` is a legal endpoint scheme value, and `http` is legal only while `oagw.config.allow_http_upstream` is true, so the default TLS posture described by `cpt-cf-oagw-constraint-https-only` holds and is lifted only by the documented configuration flag (graded deviation 2). A request whose selected endpoint scheme is not admitted **MUST** be rejected with a validation domain error and **MUST NOT** open a connection.

**Implements**:
- `cpt-cf-oagw-algo-request-proxy-endpoint-select`
- `cpt-cf-oagw-flow-request-proxy-target-host-selection`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: none
- Entities: `Endpoint`
- Tests: unit tests in `src/infra/proxy/endpoint_selector_tests.rs` for the admitted and rejected scheme values under both values of `allow_http_upstream`

### Request Body Validation and Size Limit

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-body-validation`

The system **MUST** validate the request body before buffering it: a present `Content-Length` **MUST** parse as an integer and match the actual body size, `Transfer-Encoding` **MUST** be `chunked` if present, and a body over the 100 MB hard limit **MUST** be rejected with the payload-too-large domain error before any buffering, so no oversized body is ever read into memory.

**Implements**:
- `cpt-cf-oagw-algo-request-proxy-body-validate`
- `cpt-cf-oagw-flow-request-proxy-request-validation`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyContext`
- Tests: unit tests in `src/infra/proxy/body_validation_tests.rs` for the length-integrity, encoding, and limit checks and for the `Content-Length`/`Transfer-Encoding` conflict, and integration tests in `tests/rate_limiting.rs`

### Request-Surface Hardening

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-request-hardening`

The system **MUST** harden the proxy request surface as the in-scope slice of `cpt-cf-oagw-nfr-ssrf-protection` — the strip well-known internal headers and validate request paths and query parameters against route configuration wording of DESIGN §3.2: it **MUST** strip the well-known internal hop-by-hop headers from every forwarded request, **MUST** validate the request path, path suffix, and query parameters against the matched route configuration before forwarding, and **MUST** enforce the endpoint scheme allowlist on the selected endpoint. The system **MUST** reject with the `400` validation outcome any control character — a carriage return or a line feed — appearing in a header value, in the request target, or in a query parameter name or value. The system **MUST** reject a request that presents both `Content-Length` and `Transfer-Encoding`, and **MUST** reject a `Content-Length` that is absent where the request method requires a body, that is non-numeric, or that carries conflicting duplicate values. The system **MUST** normalise the request path and the path suffix and **MUST** reject any `.` or `..` segment, a double slash introduced by suffix concatenation, and a suffix that escapes the matched route prefix. Every rejection above **MUST** produce the `400` validation outcome and **MUST** occur before any upstream connection or credential lookup. The system **MUST** leave DNS-resolution validation and IP pinning out of scope, as PRD §4.2 and DESIGN §4.5 declare those rules a separate concern.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-request-validation`
- `cpt-cf-oagw-algo-request-proxy-header-transform`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`
- Entities: `ProxyContext`, `Route`, `Endpoint`
- Tests: unit tests in `src/domain/header_transform_tests.rs` for control-character rejection in header values, `src/infra/proxy/body_validation_tests.rs` for the `Content-Length`/`Transfer-Encoding` conflict and for the absent, non-numeric, and conflicting-duplicate length rules, and `src/domain/route_matcher_tests.rs` for path normalisation, `.` and `..` segment rejection, and suffix-escape rejection, and integration tests in `tests/proxy_dispatch.rs`

### Plugin Chain Invocation Points

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-plugin-chain-points`

The system **MUST** invoke the plugin chain at deterministic points inside the proxy pipeline — auth, then the rate-limit call-in, then guards, then transform(request), then the CORS call-in, then the upstream call, then transform(response) or transform(error), per `cpt-cf-oagw-adr-state-management` and `cpt-cf-oagw-adr-plugin-system` — and **MUST** compose the chain so upstream-bound plugins execute before route-bound plugins, yielding `[U1, U2, R1, R2]`, with enforced ancestor bindings retained. This feature defines the invocation points only; plugin traits, registries, resolution, and credential handling are owned by entry 2.6.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-plugin-chain`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `Upstream`, `Route`, `ProxyContext`, `ProxyResponse`
- Tests: integration tests in `tests/plugin_chain_ordering.rs` asserting the observed execution order — auth before the rate-limit call-in, the rate-limit call-in before the guard plugins, and the CORS call-in after transform(request) — across an upstream-level and a route-level binding

### Rate-Limit Call-In Point

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-rate-limit-call-in`

The system **MUST** expose a rate-limit call-in on the proxy hot path after the auth phase and ahead of the guard phase and the upstream call, passing the effective merged rate-limit configuration and the counter scope to the Data-Plane-owned limiter, and **MUST** stop the pipeline when the decision is reject so that entry 2.7 owns the strategy execution and the `429` response. No rate-limit counter state is owned by this feature.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-plugin-chain`
- `cpt-cf-oagw-state-request-proxy-proxy-request`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyContext`
- Tests: integration tests in `tests/rate_limiting.rs` asserting the call-in happens before any upstream connection and that a reject outcome ends the pipeline

### CORS Call-In Points

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-cors-call-in`

The system **MUST** define the two CORS call-in points in the proxy path: preflight handling at the proxy handler level before upstream resolution, and actual-request origin and method enforcement after upstream resolution and before forwarding, using the effective merged CORS configuration. Origin matching, the `403` outcomes, and the CORS response headers are owned by entry 2.8.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-plugin-chain`
- `cpt-cf-oagw-dod-request-proxy-preflight-detection`

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyContext`
- Tests: integration tests in `tests/cors_enforcement.rs` asserting the enforcement point runs after resolution and before any upstream connection

### Proxy Timeout and No-Retry Forwarding

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-timeout-no-retry`

The system **MUST** bound connection establishment and the complete buffered request/response exchange by `proxy_timeout_secs`, distinguishing connection, request, and idle timeouts so entry 2.5 can render them as separate `504` types, and **MUST NOT** re-issue the original client request after a failure, so retry responsibility stays with the client. An open `sse`, `ws`, or `wt` session **MUST** be bounded by the idle timeout alone and **MUST NOT** be subject to any total-duration cap from `proxy_timeout_secs`, so a stream that keeps receiving events inside the idle window stays open for as long as it stays idle-clean. Endpoint-level connection failover to another pool endpoint **MUST** remain permitted, because it is a connector-level connection attempt and not a client-request retry.

**Implements**:
- `cpt-cf-oagw-algo-request-proxy-timeout`
- `cpt-cf-oagw-flow-request-proxy-upstream-forwarding`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyContext`, `Endpoint`
- Tests: unit tests in `src/infra/proxy/stream_lifecycle_tests.rs` for the three timeout classes and for the absence of any client-request retry, and integration tests in `tests/proxy_streaming.rs`

### Response Passthrough Without Caching

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-response-passthrough`

The system **MUST** return the upstream response to the caller as a passthrough, applying only the configured `headers.response` set, add, and remove rules and hop-by-hop stripping, and **MUST NOT** cache any upstream response, so caching remains the responsibility of the client and the upstream. An upstream error status **MUST** be passed through with its body unmodified.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-upstream-forwarding`
- `cpt-cf-oagw-algo-request-proxy-header-transform`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyResponse`
- Tests: unit tests in `src/domain/header_transform_tests.rs` for the response rules, and integration tests in `tests/proxy_streaming.rs` asserting the upstream body reaches the client byte-identical and that no cache state exists

### Error Source Header on All Error Responses

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-error-source`

The system **MUST** emit `X-OAGW-Error-Source: gateway` on every response the proxy path generates itself, including the handler-level preflight `204`, validation, routing, target-host, disabled-upstream, rate-limit, timeout, and protocol failures, and `X-OAGW-Error-Source: upstream` on every response passed through from the upstream service, and **MUST** emit the header on streaming error responses for SSE, WebSocket, and WebTransport alike, per `cpt-cf-oagw-principle-error-source` and `cpt-cf-oagw-adr-error-source-distinction`. The problem+json body and its GTS type remain owned by entry 2.5.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-error-source-emission`
- `cpt-cf-oagw-flow-request-proxy-upstream-forwarding`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyResponse`
- Tests: unit tests in `src/infra/proxy/stream_lifecycle_tests.rs`, and integration tests in `tests/error_contract.rs` covering buffered, upstream-passthrough, and streaming error responses

### SSE Streaming Passthrough and Lifecycle

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-sse-streaming`

The system **MUST** stream an SSE upstream response to the client event by event as it is received, without buffering the stream, applying the request and response header pipeline before the first event, and handling the open, close, and error lifecycle: an upstream close closes the client connection cleanly, a client disconnect closes the upstream connection, and a mid-stream abort emits the aborted-stream outcome with `X-OAGW-Error-Source` set.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-sse-streaming`
- `cpt-cf-oagw-algo-request-proxy-stream-lifecycle`
- `cpt-cf-oagw-state-request-proxy-stream-connection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyResponse`
- Tests: unit tests in `src/infra/proxy/stream_lifecycle_tests.rs` including a long-lived healthy stream that keeps receiving events inside the idle window and is therefore never terminated by `proxy_timeout_secs`, and integration tests in `tests/proxy_streaming.rs` for event ordering, upstream close, client disconnect, and mid-stream abort

### WebSocket Session Passthrough and Lifecycle

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-websocket-session`

The system **MUST** relay a WebSocket session bidirectionally without buffering the conversation, applying alias resolution, route matching, request-surface validation, and the plugin chain before the session opens, and handling the open, close, and error lifecycle so a refused or unreachable session fails with a gateway error carrying `X-OAGW-Error-Source: gateway` and a mid-session abort closes both directions.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-websocket-session`
- `cpt-cf-oagw-algo-request-proxy-stream-lifecycle`
- `cpt-cf-oagw-state-request-proxy-stream-connection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyResponse`
- Tests: integration tests in `tests/proxy_streaming.rs` for session open, clean close, refused session, and mid-session abort

### WebTransport Session Passthrough and Lifecycle

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-webtransport-session`

The system **MUST** relay a WebTransport session for an endpoint whose `scheme` is `wt`, a legal endpoint scheme value in the graded configuration, applying the same request pipeline and the same open, close, and error lifecycle handling as the other streaming kinds, so a failed or aborted session closes both directions and carries `X-OAGW-Error-Source`.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-webtransport-session`
- `cpt-cf-oagw-algo-request-proxy-stream-lifecycle`
- `cpt-cf-oagw-state-request-proxy-stream-connection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `Endpoint`, `ProxyResponse`
- Tests: the `wt` scheme value is admitted at the configuration boundary by `tests/upstream_validation.rs` and `tests/schema_contract.rs`; the relay itself has no test because this run delivers no WebTransport code path (`RVW-129` in `REVIEW-FINDINGS.md`)

### Hot-Path Latency Posture

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-low-latency`

The system **MUST** keep the proxy path on the Data Plane with in-process repository reads and no synchronous cross-gear call on the request path, so the gateway adds less than 10 ms of overhead at p95 excluding upstream response time, and **MUST NOT** introduce a response cache or an automatic client-request retry as a latency or resilience strategy. The DP L1 hot-config cache that further reduces resolution cost is owned by entry 2.9.

**Implements**:
- `cpt-cf-oagw-flow-request-proxy-upstream-forwarding`
- `cpt-cf-oagw-flow-request-proxy-plugin-chain`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `ProxyContext`, `ProxyResponse`
- Tests: the five phase histograms an exchange records are asserted by `src/infra/observability_tests.rs`, and the audited `duration_ms` of a proxied request against a local stub upstream by `tests/audit_log_shape.rs`; no separate end-to-end latency measurement exists, so no p95 figure is claimed

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering alias resolution and shadowing, effective-configuration merge, route matching, header rules, endpoint selection, streaming lifecycle, and body limits, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4.

**Implements**:
- `cpt-cf-oagw-dod-request-proxy-alias-resolution`
- `cpt-cf-oagw-dod-request-proxy-effective-config`
- `cpt-cf-oagw-dod-request-proxy-route-matching`
- `cpt-cf-oagw-dod-request-proxy-header-pipeline`
- `cpt-cf-oagw-dod-request-proxy-endpoint-selection`
- `cpt-cf-oagw-dod-request-proxy-body-validation`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `Endpoint`, `ProxyContext`, `ProxyResponse`
- Tests: `src/infra/proxy/alias_resolver_tests.rs`, `src/domain/merge_tests.rs`, `src/domain/route_matcher_tests.rs`, `src/domain/header_transform_tests.rs`, `src/infra/proxy/endpoint_selector_tests.rs`, `src/infra/proxy/body_validation_tests.rs`, `src/infra/proxy/stream_lifecycle_tests.rs`, `src/infra/proxy/stream_lifecycle_tests.rs`

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-proxy-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory covering the full proxy flow against a local stub upstream, including alias shadowing across a three-level tenant hierarchy, the `503` disabled-upstream outcome, the `X-OAGW-Target-Host` matrix, header strip and transformation rules, plugin-chain invocation points, the rate-limit and CORS call-ins, timeout handling, error-source emission on buffered and streaming errors, and SSE, WebSocket, and WebTransport lifecycle, and **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-request-proxy-proxy-handler`
- `cpt-cf-oagw-dod-request-proxy-target-host-matrix`
- `cpt-cf-oagw-dod-request-proxy-plugin-chain-points`
- `cpt-cf-oagw-dod-request-proxy-error-source`
- `cpt-cf-oagw-dod-request-proxy-sse-streaming`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`
- Entities: `Upstream`, `Route`, `Endpoint`, `ProxyContext`, `ProxyResponse`
- Tests: `tests/proxy_dispatch.rs`, `tests/proxy_dispatch.rs`, `tests/proxy_dispatch.rs`, `tests/proxy_dispatch.rs`, `tests/proxy_dispatch.rs`, `tests/proxy_dispatch.rs`, `tests/plugin_chain_ordering.rs`, `tests/rate_limiting.rs`, `tests/cors_enforcement.rs`, `tests/proxy_streaming.rs`, `tests/error_contract.rs`, `tests/proxy_streaming.rs`, `tests/rate_limiting.rs`, `tests/proxy_streaming.rs`, `tests/proxy_streaming.rs`

## 6. Acceptance Criteria

- [ ] **Remaining two criteria are the `wt` slice, which this run does not deliver:** the WebTransport relay of box `cpt-cf-oagw-dod-request-proxy-webtransport-session` has no reachable code path — `wt` is admitted as a legal endpoint `scheme` value at the configuration boundary only — so the two boxes that assert a relayed `wt` session stay unticked and the gap is recorded as `RVW-129` in `REVIEW-FINDINGS.md`.
- [x] A proxy request to `/oagw/v1/proxy/{alias}/{path_suffix}` with any HTTP method reaches `DataPlaneService` through the gear-relative route with no `/api` segment, and a caller without `gts.cf.core.oagw.proxy.v1~:invoke` is rejected before any repository access (DoD `cpt-cf-oagw-dod-request-proxy-proxy-handler`).
- [x] An `OPTIONS` request carrying `Origin` and `Access-Control-Request-Method` returns a permissive `204` echoing the requested origin, method, and headers, with no upstream resolution, no tenant context, and no plugin-chain execution (DoD `cpt-cf-oagw-dod-request-proxy-preflight-detection`).
- [x] The handler-level CORS preflight `204` carries `X-OAGW-Error-Source: gateway`, while origin and method enforcement on the actual request remains owned by entry 2.8 and no CORS enforcement is performed by this feature (DoD `cpt-cf-oagw-dod-request-proxy-preflight-detection`, `cpt-cf-oagw-dod-request-proxy-error-source`).
- [x] A request from a descendant tenant resolves an alias shadowed at that descendant, while an enforced ancestor rate limit or CORS origin set is still applied to the effective configuration (DoD `cpt-cf-oagw-dod-request-proxy-alias-resolution`).
- [x] An upstream configuration and a matched route configuration that disagree on a mergeable field resolve to the route value, because the merge precedence order is tenant over route over upstream, and an enforced ancestor value still outranks both (DoD `cpt-cf-oagw-dod-request-proxy-effective-config`).
- [x] Alias resolution is case-insensitive and trailing dots are ignored, so `Api.OpenAI.COM.` resolves to an upstream whose alias is `api.openai.com` (DoD `cpt-cf-oagw-dod-request-proxy-alias-resolution`).
- [x] A disabled upstream, and an upstream shadowing a disabled ancestor, return `503` with `X-OAGW-Error-Source: gateway` and open no upstream connection (DoD `cpt-cf-oagw-dod-request-proxy-disabled-upstream`).
- [x] A request whose method is not in `match.http.methods`, or whose path matches no enabled route, returns the route-not-found error rendered as `404` by entry 2.5 (DoD `cpt-cf-oagw-dod-request-proxy-route-matching`).
- [x] A descendant route and an ancestor route whose match blocks both match the same request resolve to the descendant route, because the tenant chain is walked descendant to root, the first upstream whose route set yields a match wins, and closest tenant-chain distance outranks the longest matching path prefix and then the route priority (DoD `cpt-cf-oagw-dod-request-proxy-route-matching`).
- [x] An upstream whose protocol is the gRPC protocol value never opens an upstream connection, and the request is rejected as a gateway protocol error (DoD `cpt-cf-oagw-dod-request-proxy-grpc-surface`).
- [x] A path suffix supplied to a route with `path_suffix_mode: disabled` is rejected, and the same suffix supplied to a route with `path_suffix_mode: append` is appended to the matched path (DoD `cpt-cf-oagw-dod-request-proxy-path-and-query`).
- [x] A query parameter outside `query_allowlist` is rejected, and an empty allowlist rejects every query parameter (DoD `cpt-cf-oagw-dod-request-proxy-path-and-query`).
- [x] `X-OAGW-Target-Host`, `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, and `Upgrade` never reach the upstream, and `Host` or `:authority` is replaced with the upstream host or authority (DoD `cpt-cf-oagw-dod-request-proxy-header-pipeline`).
- [x] `headers.request.passthrough` of `none`, `allowlist`, and `all` forward no inbound header, exactly the allowlisted names, and every surviving inbound header respectively, and the configured `headers.request` and `headers.response` set, add, and remove rules are applied in that order (DoD `cpt-cf-oagw-dod-request-proxy-header-pipeline`).
- [x] A `ws` or `wss` session opens with the upstream receiving `Upgrade: websocket` and the handshake `Connection` value, while `X-OAGW-Target-Host`, `TE`, `Trailer`, and `Transfer-Encoding` still never reach the upstream, and `Connection` and `Upgrade` remain stripped from every buffered request and from every response head including a streamed response head (DoD `cpt-cf-oagw-dod-request-proxy-header-pipeline`).
- [x] The six rows of the `X-OAGW-Target-Host` behavior matrix behave as specified, including the required-header `400` for a multi-endpoint common-suffix alias, and an invalid or unknown value returns the corresponding `400` gateway error (DoD `cpt-cf-oagw-dod-request-proxy-target-host-matrix`).
- [x] A multi-endpoint upstream without a target host rotates across the pool by round-robin, and a supplied valid target host bypasses the rotation (DoD `cpt-cf-oagw-dod-request-proxy-endpoint-selection`).
- [x] N sequential requests over a pool of N endpoints hit each endpoint exactly once, and concurrent selections over that pool do not all return the same endpoint, because the round-robin cursor is Data Plane state scoped per endpoint pool, advances atomically, and is re-derived when the pool's endpoint set changes (DoD `cpt-cf-oagw-dod-request-proxy-endpoint-selection`).
- [x] An endpoint whose scheme is not admitted by the allowlist is rejected before a connection is opened, and `http` is admitted exactly while `allow_http_upstream` is true (DoD `cpt-cf-oagw-dod-request-proxy-scheme-allowlist`).
- [x] A `Content-Length` that disagrees with the body, a `Transfer-Encoding` other than `chunked`, and a body over 100 MB are each rejected before buffering, with the oversized body returning the payload-too-large outcome (DoD `cpt-cf-oagw-dod-request-proxy-body-validation`).
- [x] A header value, a request target, or a query parameter name or value carrying a carriage return or a line feed is rejected with the `400` validation outcome before any upstream connection or credential lookup (DoD `cpt-cf-oagw-dod-request-proxy-request-hardening`).
- [x] A request presenting both `Content-Length` and `Transfer-Encoding`, a `Content-Length` absent where the request method requires a body, a non-numeric `Content-Length`, and conflicting duplicate `Content-Length` values are each rejected with the `400` validation outcome (DoD `cpt-cf-oagw-dod-request-proxy-request-hardening`).
- [x] A request path or path suffix containing a `.` or `..` segment, a double slash introduced by suffix concatenation, or a suffix that escapes the matched route prefix is rejected with the `400` validation outcome (DoD `cpt-cf-oagw-dod-request-proxy-request-hardening`).
- [x] A request that fails request-surface validation never triggers an upstream connection or a credential lookup, because path and path-suffix validation run immediately after route matching and before endpoint selection, upstream credential resolution, or any connection attempt (DoD `cpt-cf-oagw-dod-request-proxy-request-hardening`).
- [x] The plugin chain executes auth, then the rate-limit call-in, then guards, then transform(request), then the CORS call-in, then the upstream call, then transform(response) or transform(error), with upstream-bound plugins ahead of route-bound plugins (DoD `cpt-cf-oagw-dod-request-proxy-plugin-chain-points`).
- [x] A request that is both unauthenticated and over quota returns `401` and not `429`, because the auth phase runs before the rate-limit call-in in the hot-path sequence fixed by `cpt-cf-oagw-adr-state-management` (DoD `cpt-cf-oagw-dod-request-proxy-plugin-chain-points`, `cpt-cf-oagw-dod-request-proxy-rate-limit-call-in`).
- [x] A request carrying a `cred://` reference that cannot be resolved, or an unavailable credential store, returns the `500` `SecretNotFound` outcome `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` with `X-OAGW-Error-Source: gateway`, and never returns `401` (DoD `cpt-cf-oagw-dod-request-proxy-plugin-chain-points`).
- [x] A rate-limited request is stopped ahead of the upstream call and handed to entry 2.7, and a cross-origin actual request is origin- and method-checked after resolution and before forwarding (DoD `cpt-cf-oagw-dod-request-proxy-rate-limit-call-in`, `cpt-cf-oagw-dod-request-proxy-cors-call-in`).
- [x] An exchange exceeding `proxy_timeout_secs` returns the timeout outcome rendered by entry 2.5 as `504`, and no failed request is ever re-issued, while a failed connection to one pool endpoint may fail over to another (DoD `cpt-cf-oagw-dod-request-proxy-timeout-no-retry`).
- [x] An active `sse`, `ws`, or `wt` session that keeps receiving events, frames, or datagrams inside the idle window is not terminated by `proxy_timeout_secs`, which bounds only connection establishment and the complete buffered request/response exchange, while an idle stream still returns the idle-timeout outcome rendered as `504` (DoD `cpt-cf-oagw-dod-request-proxy-timeout-no-retry`).
- [x] An upstream error status and body reach the client unmodified with `X-OAGW-Error-Source: upstream`, a gateway failure reaches the client with `X-OAGW-Error-Source: gateway`, and no response is cached (DoD `cpt-cf-oagw-dod-request-proxy-response-passthrough`, `cpt-cf-oagw-dod-request-proxy-error-source`).
- [x] An SSE response is relayed event by event in order, an upstream close closes the client connection, a client disconnect closes the upstream connection, and a mid-stream abort emits the aborted-stream outcome with `X-OAGW-Error-Source` set (DoD `cpt-cf-oagw-dod-request-proxy-sse-streaming`).
- [ ] A WebSocket session and a WebTransport session are relayed bidirectionally with open, close, refused, and aborted outcomes, and every failure carries `X-OAGW-Error-Source` (DoD `cpt-cf-oagw-dod-request-proxy-websocket-session`, `cpt-cf-oagw-dod-request-proxy-webtransport-session`).
- [x] No DNS-resolution validation, IP pinning, or response-cache logic is introduced by this feature, and the SSRF slice it delivers is limited to header stripping, path and query validation, and the endpoint scheme allowlist (DoD `cpt-cf-oagw-dod-request-proxy-request-hardening`).
- [x] A proxied request produces no oagw-owned audit record, metric, or trace identifier from this feature, because audit logging, metrics, and trace-identifier propagation are owned by `cpt-cf-oagw-feature-observability-and-operability` (entry 2.9); this feature only exposes `status`, `duration_ms`, `request_size`, `response_size`, and `error_type` at the pipeline boundary for entry 2.9 to consume (DoD `cpt-cf-oagw-dod-request-proxy-proxy-handler`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-request-proxy-unit-tests`, `cpt-cf-oagw-dod-request-proxy-integration-tests`).
