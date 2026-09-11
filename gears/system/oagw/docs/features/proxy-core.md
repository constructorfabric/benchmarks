# Feature: Proxy Request Resolution and Forwarding


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy Request Forwarded to Upstream](#proxy-request-forwarded-to-upstream)
  - [Proxy Request Whose Alias or Route Does Not Resolve](#proxy-request-whose-alias-or-route-does-not-resolve)
  - [Proxy Request to a Disabled Upstream](#proxy-request-to-a-disabled-upstream)
  - [Proxy Request Rejected by Target-Host Selection](#proxy-request-rejected-by-target-host-selection)
  - [Proxy Request Rejected by Inbound Validation](#proxy-request-rejected-by-inbound-validation)
  - [Upstream Error Response Relayed Verbatim](#upstream-error-response-relayed-verbatim)
  - [Proxy Request Whose Upstream Is Unreachable or Too Slow](#proxy-request-whose-upstream-is-unreachable-or-too-slow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Parse and Classify the Inbound Proxy Request](#parse-and-classify-the-inbound-proxy-request)
  - [Read the Resolved Configuration for the Request](#read-the-resolved-configuration-for-the-request)
  - [Resolve the Alias Through the Tenant Chain](#resolve-the-alias-through-the-tenant-chain)
  - [Match the Route Within the Resolved Upstream](#match-the-route-within-the-resolved-upstream)
  - [Merge the Effective Configuration Across the Hierarchy](#merge-the-effective-configuration-across-the-hierarchy)
  - [Select the Target Endpoint from the Pool](#select-the-target-endpoint-from-the-pool)
  - [Apply the Inbound Guard Rules](#apply-the-inbound-guard-rules)
  - [Validate the Request Body](#validate-the-request-body)
  - [Transform the Headers in Both Directions](#transform-the-headers-in-both-directions)
  - [Forward the Request to the Selected Endpoint](#forward-the-request-to-the-selected-endpoint)
  - [Relay the Upstream Response to the Caller](#relay-the-upstream-response-to-the-caller)
  - [Map a Gateway Failure to Its Response](#map-a-gateway-failure-to-its-response)
  - [Observe the Proxy Request](#observe-the-proxy-request)
- [4. States (CDSL)](#4-states-cdsl)
  - [Proxy Request Lifecycle State Machine](#proxy-request-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Proxy Endpoint Registration](#proxy-endpoint-registration)
  - [Alias Resolution Through the Tenant Chain](#alias-resolution-through-the-tenant-chain)
  - [Route Matching Within the Resolved Upstream](#route-matching-within-the-resolved-upstream)
  - [Enable and Disable Enforcement at Proxy Time](#enable-and-disable-enforcement-at-proxy-time)
  - [Hierarchical Configuration Merge](#hierarchical-configuration-merge)
  - [Resolved-Configuration Read Path and Invalidation](#resolved-configuration-read-path-and-invalidation)
  - [Target-Host Behaviour Matrix](#target-host-behaviour-matrix)
  - [Header Handling in Three Categories](#header-handling-in-three-categories)
  - [Body Validation Before Buffering](#body-validation-before-buffering)
  - [Guard Rules for Path Suffix and Query](#guard-rules-for-path-suffix-and-query)
  - [Request Forwarding and Response Relay](#request-forwarding-and-response-relay)
  - [Plaintext Upstream Connection Gate](#plaintext-upstream-connection-gate)
  - [Request Deadline and No Retries](#request-deadline-and-no-retries)
  - [Gateway and Upstream Error Split](#gateway-and-upstream-error-split)
  - [Base Observability for the Proxy Path](#base-observability-for-the-proxy-path)
  - [Named Extension Points for the Layered Features](#named-extension-points-for-the-layered-features)
  - [Cross-Cutting Qualities of the Proxy Path](#cross-cutting-qualities-of-the-proxy-path)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-proxy-core-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-proxy-core`
## 1. Feature Context

### 1.1 Overview

This feature implements the single OAGW data-plane request path: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` is resolved to an upstream and a route through the tenant hierarchy, validated, transformed, forwarded to the selected upstream endpoint over plain HTTP/HTTPS, and the upstream response is relayed back to the caller — with the gateway-versus-upstream error split and base observability applied to every outcome.

### 1.2 Purpose

Every other data-plane capability extends this one path rather than duplicating it, so this feature owns the parts that all of them share: alias resolution with shadowing across the tenant chain, longest-path-prefix plus priority route matching, the hierarchical configuration merge, endpoint-pool selection including the full `X-OAGW-Target-Host` behaviour matrix, the three header categories with `Host` / HTTP/2 `:authority` replacement, body validation, request forwarding under a configured deadline, and the RFC 9457 gateway-error envelope versus verbatim upstream passthrough.

**Requirements** (defined in `PRD.md` with checkboxes — referenced here in the same form, with the three requirements the frozen PRD already marks complete carried over as `[x]`):

- [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
- [x] `p1` - `cpt-cf-oagw-fr-alias-resolution`
- [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
- [x] `p1` - `cpt-cf-oagw-fr-config-layering`
- [x] `p1` - `cpt-cf-oagw-fr-hierarchical-config`
- [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
- [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
- [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
- [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
- [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
- [ ] `p1` - `cpt-cf-oagw-nfr-observability`
- [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`
- [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`

**Principles** (defined in `DESIGN.md` as bare `**ID**` lines — referenced without checkboxes): `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-no-cache`, `cpt-cf-oagw-principle-tenant-scope`, `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-rfc9457`.

**Constraints** (same form): `cpt-cf-oagw-constraint-body-limit`, `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-https-only`.

**Design elements and decisions** (same form): `cpt-cf-oagw-component-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-seq-proxy-flow`, `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-error-source-distinction`, `cpt-cf-oagw-adr-state-management`, `cpt-cf-oagw-adr-data-plane-caching`.

**Note on design elements cited beyond this feature's DECOMPOSITION entry.** Six of the elements listed above — `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-adr-error-source-distinction`, `cpt-cf-oagw-adr-state-management` and `cpt-cf-oagw-adr-data-plane-caching` — do not appear in the Design Principles Covered or Design Components lists of DECOMPOSITION entry 2.5, yet this feature substantively touches all six and cites them deliberately rather than decoratively. The reason is that entry 2.5 enumerates the elements that *originate* with this feature, while two of its owned behaviours are the concrete rendering of elements enumerated elsewhere. First, this feature is the only place where the gateway-versus-upstream error split becomes observable: it decides, per response, whether `X-OAGW-Error-Source` reads `gateway` with an RFC 9457 envelope or `upstream` with a verbatim body, which is exactly the contract the error-source principle, the RFC 9457 principle and the error-source-distinction ADR define and which entry 2.1 attributes to the shared renderer. Second, this feature is the sole reader of the resolved-configuration cache: it consumes the Control-Plane/Data-Plane split and the no-TTL, explicit-invalidation read path that the state-management and data-plane-caching ADRs decide and that entry 2.1 establishes, and it resolves requests against the domain model those ADRs and the domain-model element describe. Citing the six here records that dependency at the point where it is exercised; the authoritative enumeration stays in `DECOMPOSITION.md`, which is not modified by this feature's documentation.

**Two overrides that supersede the tabulated source documents and are binding for this feature:**

1. **Route registration is gear-relative at `/oagw/v1/...`, with no leading `/api`.** `PRD.md` and `DESIGN.md` tabulate `/api/oagw/v1/proxy/...`; that is the absolute path behind a host gateway whose prefix is `/api`, which the graded server configuration does not set. The proxy path implemented here is `/oagw/v1/proxy/{alias}[/{path_suffix}]`, and that is the form used in this document, in the problem-details `instance` field, and in the `http.route` metric label.
2. **`http` is a legal endpoint scheme, and the graded configuration permits plaintext upstream connections.** `cpt-cf-oagw-constraint-https-only` states the *default* HTTPS-only posture; the `allow_http_upstream` gear-configuration flag lifts it. Whether a scheme may be *declared* on an upstream record is settled at the management layer by DECOMPOSITION entry 2.2 and is not this feature's concern. Whether OAGW actually *opens* a plaintext connection is settled here: with `allow_http_upstream: true` (the graded value in `config/e2e-local.yaml`) a plaintext connection is opened and the request is forwarded; with `allow_http_upstream: false` the attempt is refused before any socket is opened and rendered as a gateway error.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxy request to `/oagw/v1/proxy/{alias}[/{path_suffix}]`, optionally supplying `X-OAGW-Target-Host`, and consumes the relayed upstream response or the gateway error |
| `cpt-cf-oagw-actor-upstream-service` | Receives the forwarded outbound request and returns the response that is relayed back verbatim |
| `cpt-cf-oagw-actor-platform-operator` | Owns the upstream/route configuration and the gear configuration (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`) that this path reads, and consumes its audit log and metrics |
| `cpt-cf-oagw-actor-tenant-admin` | Owns the descendant-tenant configuration whose shadowing and sharing-mode semantics this path resolves and merges |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.5
- **ADRs**: [0001 Request Routing](../ADR/0001-request-routing.md), [0005 Control Plane Caching](../ADR/0005-data-plane-caching.md), [0006 State Management](../ADR/0006-state-management.md), [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md)
- **Schemas**: [upstream.v1.schema.json](../schemas/upstream.v1.schema.json), [route.v1.schema.json](../schemas/route.v1.schema.json)
- **Dependencies** (must be implemented first; referenced in the checkbox form their DECOMPOSITION definitions use):
  - [ ] `p1` - `cpt-cf-oagw-feature-gear-foundation`
  - [ ] `p1` - `cpt-cf-oagw-feature-upstream-management`
  - [ ] `p1` - `cpt-cf-oagw-feature-route-management`
- **Dependent features that extend this path through its named extension points** (out of scope here):
  - [ ] `p1` - `cpt-cf-oagw-feature-proxy-streaming`
  - [ ] `p2` - `cpt-cf-oagw-feature-cors-handling`
  - [ ] `p2` - `cpt-cf-oagw-feature-rate-limiting`
  - [ ] `p2` - `cpt-cf-oagw-feature-plugin-execution`

**Explicitly out of scope for this feature** (per DECOMPOSITION entry 2.5): SSE/WebSocket streaming semantics and connection lifecycle; CORS preflight and origin/method validation; rate-limit evaluation and the `429`/`X-RateLimit-*` contract; Auth/Guard/Transform plugin invocation; gRPC request classification and forwarding; DNS resolution and IP-pinning SSRF controls beyond the scheme allowlist; circuit breaker and automatic retries.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-proxy-request`

The flows below are split by materially different observable outcome so that each is independently testable. All of them share the same first phase; the algorithms in section 3 hold the shared logic and are called by identifier.

### Proxy Request Forwarded to Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-request-forwarded`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A `GET` to `/oagw/v1/proxy/{alias}/{path_suffix}` with an allowlisted query parameter reaches the upstream at the route path with the suffix appended, and the upstream's `200` status, headers and body are relayed to the caller with `X-OAGW-Error-Source: upstream`.
- A `POST` with a JSON body under the size limit reaches the upstream with the body byte-for-byte intact and a recomputed `Content-Length`.
- A single-endpoint upstream is reached without `X-OAGW-Target-Host`; a multi-endpoint explicit-alias upstream is reached by round robin without the header, or at a named endpoint with it.
- A plaintext (`http`) endpoint is reached because `allow_http_upstream` is `true`.

**Error Scenarios**:
- Any resolution, validation, endpoint-selection or forwarding failure leaves this flow for one of the six flows below; no partial response is ever emitted to the caller.

**Steps**:
1. [x] - `p1` - Actor sends `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` with a bearer token carrying `gts.cf.core.oagw.proxy.v1~:invoke`; platform middleware authenticates it and supplies the SecurityContext (tenant and principal) before this handler runs - `inst-proxy-fwd-receive`
2. [x] - `p1` - API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` (gear-relative, no `/api` prefix) is dispatched to the Data Plane per `cpt-cf-oagw-adr-request-routing` - `inst-proxy-fwd-dispatch`
3. [x] - `p1` - Assign the request correlation identifier and open the audit/metric scope by calling `cpt-cf-oagw-algo-proxy-observe-request` - `inst-proxy-fwd-correlate`
4. [x] - `p2` - **EXTENSION POINT (no-op in this feature)** pre-resolution request-classification hook: the CORS preflight fast path of DECOMPOSITION entry 2.7 short-circuits here, before any upstream resolution or tenant walk; this feature performs no classification other than the parse in the next step - `inst-proxy-fwd-preflight-hook`
5. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-parse-request` to split the URL into normalized alias, path suffix and query pairs and to run inbound header hygiene - `inst-proxy-fwd-parse`
6. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-read-resolved-config`, which resolves the alias (`cpt-cf-oagw-algo-proxy-resolve-alias`), matches the route (`cpt-cf-oagw-algo-proxy-match-route`) and merges the effective configuration (`cpt-cf-oagw-algo-proxy-merge-config`) - `inst-proxy-fwd-resolve`
7. [x] - `p2` - **EXTENSION POINT (no-op in this feature)** post-resolution policy hook: entry 2.7 validates the request origin and method against the merged `cors` configuration here, and entry 2.8 evaluates the rate-limit budget here, both after resolution and before forwarding. The relative order at this hook is fixed and not left to the attaching features: CORS origin/method validation runs first, then rate-limit evaluation, then the plugin chain of step 12 (Auth, then Guards, then Transform `on_request`), then forwarding. A request that is simultaneously CORS-invalid and over its rate-limit budget therefore receives the CORS rejection, and its rate-limit budget is not consumed. The CORS preflight fast path is not part of this ordering at all: it short-circuits at step 4, before any resolution has happened - `inst-proxy-fwd-policy-hook`
8. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-apply-guard-rules` to enforce `path_suffix_mode` and the query allowlist and to compute the effective outbound path and query - `inst-proxy-fwd-guards`
9. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-validate-body` to check `Content-Length`, `Transfer-Encoding` and the hard size limit - `inst-proxy-fwd-validate-body`
10. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-select-endpoint` to pick the target endpoint from the pool per the `X-OAGW-Target-Host` behaviour matrix - `inst-proxy-fwd-select-endpoint`
11. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-transform-headers` in the request direction to strip routing and hop-by-hop headers, apply the upstream `headers.request` rules, and set `Host` / `:authority` from the selected endpoint - `inst-proxy-fwd-transform-headers`
12. [x] - `p2` - **EXTENSION POINT (no-op in this feature)** pre-call plugin hook: entry 2.9 runs Auth, then Guards, then Transform `on_request` here, in that order, upstream-bound plugins before route-bound ones - `inst-proxy-fwd-plugin-request-hook`
13. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-forward-request` to open the connection to the selected endpoint under the `proxy_timeout_secs` deadline and send the outbound request exactly once - `inst-proxy-fwd-forward`
14. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-relay-response` to transform the response headers, set `X-OAGW-Error-Source: upstream`, and relay the upstream status, headers and body to the caller unchanged - `inst-proxy-fwd-relay`
15. [x] - `p2` - **EXTENSION POINT (no-op in this feature)** post-call plugin hook: entry 2.9 runs Transform `on_response` here - `inst-proxy-fwd-plugin-response-hook`
16. [x] - `p1` - Emit the `INFO` audit record and the base metrics for the completed request via `cpt-cf-oagw-algo-proxy-observe-request` - `inst-proxy-fwd-observe`
17. [x] - `p1` - **RETURN** the upstream response (status, transformed headers, verbatim body) with `X-OAGW-Error-Source: upstream` - `inst-proxy-fwd-return`

### Proxy Request Whose Alias or Route Does Not Resolve

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-alias-unresolved`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- None: every path through this flow ends in `404 RouteNotFound`.

**Error Scenarios**:
- No upstream with the normalized alias exists at any tenant in the caller's chain.
- An upstream resolves but no enabled route under it matches the request method.
- An upstream resolves but no enabled route's `match.http.path` is a prefix of the inbound path suffix.
- The resolved upstream's `protocol` is the gRPC identifier, for which no HTTP match keys and no proxy code path exist in this round.

**Steps**:
1. [x] - `p1` - Actor sends the proxy request; steps 1 to 5 of `cpt-cf-oagw-flow-proxy-request-forwarded` run unchanged - `inst-proxy-nf-receive`
2. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-read-resolved-config` - `inst-proxy-nf-resolve`
3. [x] - `p1` - **IF** `cpt-cf-oagw-algo-proxy-resolve-alias` returns no upstream for the normalized alias in the whole tenant chain - `inst-proxy-nf-if-no-upstream`
   1. [x] - `p1` - Map to `404 RouteNotFound` via `cpt-cf-oagw-algo-proxy-map-error`, without contacting any upstream - `inst-proxy-nf-map-no-upstream`
4. [x] - `p1` - **ELSE IF** `cpt-cf-oagw-algo-proxy-match-route` returns no candidate (method not in any candidate route's `match.http.methods`, no `match.http.path` prefix match, every candidate route disabled, or the upstream `protocol` is gRPC) - `inst-proxy-nf-if-no-route`
   1. [x] - `p1` - Map to `404 RouteNotFound` via `cpt-cf-oagw-algo-proxy-map-error` - `inst-proxy-nf-map-no-route`
5. [x] - `p1` - Emit the failure audit record with `error_type` set to the mapped error name and increment `oagw_errors_total` - `inst-proxy-nf-observe`
6. [x] - `p1` - **RETURN** `404` `application/problem+json` with `type` `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` and `X-OAGW-Error-Source: gateway` - `inst-proxy-nf-return`

### Proxy Request to a Disabled Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-upstream-disabled`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- None: every path through this flow ends in `503`.

**Error Scenarios**:
- The selected upstream has `enabled: false`.
- An ancestor tenant's upstream with the same alias has `enabled: false`, so the cascade disables the descendant's shadowing upstream as well and the descendant cannot re-enable it.

**Steps**:
1. [x] - `p1` - Actor sends the proxy request; steps 1 to 5 of `cpt-cf-oagw-flow-proxy-request-forwarded` run unchanged - `inst-proxy-dis-receive`
2. [x] - `p1` - Call `cpt-cf-oagw-algo-proxy-resolve-alias`, which selects the closest-tenant upstream declaring the alias and computes its effective enabled state across the chain - `inst-proxy-dis-resolve`
3. [x] - `p1` - **IF** the effective enabled state is `false` - `inst-proxy-dis-if-disabled`
   1. [x] - `p1` - Abandon the request before route matching, endpoint selection and any upstream connection - `inst-proxy-dis-abandon`
   2. [x] - `p1` - Map to `503 LinkUnavailable` via `cpt-cf-oagw-algo-proxy-map-error` - `inst-proxy-dis-map`
4. [x] - `p1` - Emit the failure audit record with `status` `503` and the mapped `error_type` - `inst-proxy-dis-observe`
5. [x] - `p1` - **RETURN** `503` `application/problem+json` with `type` `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` and `X-OAGW-Error-Source: gateway` - `inst-proxy-dis-return`

### Proxy Request Rejected by Target-Host Selection

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-target-host-rejected`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- None: every path through this flow ends in one of the three documented `400` responses.

**Error Scenarios**:
- `MissingTargetHost`: the resolved upstream has two or more endpoints and its alias was derived from a registrable common suffix, so the header is required and was not supplied.
- `InvalidTargetHost`: the supplied header value is not a bare hostname or IP literal (it carries a port, a path, a scheme, userinfo, whitespace, or control characters).
- `UnknownTargetHost`: the supplied header value is well formed but matches no endpoint host configured on the resolved upstream.

**Steps**:
1. [ ] - `p1` - Actor sends the proxy request, optionally with `X-OAGW-Target-Host`; resolution, guard rules and body validation complete successfully - `inst-proxy-th-receive`
2. [ ] - `p1` - Call `cpt-cf-oagw-algo-proxy-select-endpoint` - `inst-proxy-th-select`
3. [ ] - `p1` - **IF** the algorithm reports a missing required header - `inst-proxy-th-if-missing`
   1. [ ] - `p1` - Map to `400 MissingTargetHost` with `type` `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` and the `alias` and `valid_hosts` extension fields listing every configured endpoint host - `inst-proxy-th-map-missing`
4. [ ] - `p1` - **ELSE IF** the algorithm reports a malformed header value - `inst-proxy-th-if-invalid`
   1. [ ] - `p1` - Map to `400 InvalidTargetHost` with `type` `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1` and the `invalid_value` extension field carrying the rejected value - `inst-proxy-th-map-invalid`
5. [ ] - `p1` - **ELSE IF** the algorithm reports a well-formed value matching no endpoint - `inst-proxy-th-if-unknown`
   1. [ ] - `p1` - Map to `400 UnknownTargetHost` with `type` `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1` and the `invalid_value` and `valid_hosts` extension fields - `inst-proxy-th-map-unknown`
6. [ ] - `p1` - Include the `upstream_id` and `instance` (the inbound `/oagw/v1/proxy/...` path) extension fields in all three bodies, and do not advance the round-robin cursor - `inst-proxy-th-context`
7. [ ] - `p1` - Emit the failure audit record and increment `oagw_errors_total` with the mapped `error_type` - `inst-proxy-th-observe`
8. [ ] - `p1` - **RETURN** `400` `application/problem+json` with `X-OAGW-Error-Source: gateway`, having opened no upstream connection - `inst-proxy-th-return`

### Proxy Request Rejected by Inbound Validation

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-request-invalid`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- None: every path through this flow ends in `400` or `413`.

**Error Scenarios**:
- A path suffix is supplied while the matched route sets `path_suffix_mode: disabled`.
- A query parameter name is not in the matched route's `match.http.query_allowlist` (including the empty-allowlist case, which admits no parameters at all).
- `Content-Length` is not a well-formed non-negative integer, or does not equal the number of body bytes actually received.
- `Transfer-Encoding` names anything other than the single token `chunked`, or is present together with `Content-Length`.
- A header name or value contains CR, LF or NUL.
- The declared or accumulated body size exceeds the 100 MB hard limit.

**Steps**:
1. [ ] - `p1` - Actor sends the proxy request; the alias and route resolve successfully - `inst-proxy-inv-receive`
2. [ ] - `p1` - Call `cpt-cf-oagw-algo-proxy-apply-guard-rules` and `cpt-cf-oagw-algo-proxy-validate-body` - `inst-proxy-inv-validate`
3. [ ] - `p1` - **IF** the size limit is exceeded - `inst-proxy-inv-if-too-large`
   1. [ ] - `p1` - Map to `413 PayloadTooLarge` with `type` `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`, having rejected the request before buffering the body - `inst-proxy-inv-map-too-large`
4. [ ] - `p1` - **ELSE** - `inst-proxy-inv-else`
   1. [ ] - `p1` - Map to `400 ValidationError` with `type` `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` and a `detail` naming the failed check without echoing header or body content - `inst-proxy-inv-map-validation`
5. [ ] - `p1` - Emit the failure audit record; never log the rejected body, query string or header values - `inst-proxy-inv-observe`
6. [ ] - `p1` - **RETURN** the gateway problem-details response with `X-OAGW-Error-Source: gateway`, having opened no upstream connection - `inst-proxy-inv-return`

### Upstream Error Response Relayed Verbatim

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-upstream-error-relayed`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- The upstream answers `4xx` or `5xx`; the caller receives exactly that status, the upstream's body byte-for-byte, the upstream's headers minus hop-by-hop and minus the configured `headers.response.remove` set, and `X-OAGW-Error-Source: upstream`.
- The upstream answers with a non-JSON or empty body; it is still relayed unchanged and is never re-rendered as `application/problem+json`.

**Error Scenarios**:
- The upstream's `Content-Length` does not match the bytes actually delivered, which is a gateway-side failure and leaves this flow for `cpt-cf-oagw-flow-proxy-upstream-unreachable`.

**Steps**:
1. [ ] - `p1` - The upstream service returns a response with a non-2xx status to the forwarding step - `inst-proxy-uerr-receive`
2. [ ] - `p1` - Classify the response as upstream-sourced: OAGW originated no error, so no problem-details envelope is constructed - `inst-proxy-uerr-classify`
3. [ ] - `p1` - Call `cpt-cf-oagw-algo-proxy-relay-response`, preserving the status code and the body without inspection, re-encoding or truncation - `inst-proxy-uerr-relay`
4. [ ] - `p1` - Set `X-OAGW-Error-Source: upstream`, replacing any same-named header the upstream itself supplied so the value always reflects OAGW's own classification - `inst-proxy-uerr-error-source`
5. [ ] - `p1` - Emit the audit record at `ERROR` level with the upstream `status` and `error_type` set to the upstream-error classification, and increment `oagw_requests_total` with the numeric upstream status - `inst-proxy-uerr-observe`
6. [ ] - `p1` - **RETURN** the upstream status, transformed headers and verbatim body - `inst-proxy-uerr-return`

### Proxy Request Whose Upstream Is Unreachable or Too Slow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-upstream-unreachable`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- None: every path through this flow ends in a gateway `502`, `503` or `504`.

**Error Scenarios**:
- `502 ProtocolError`: the upstream violated the HTTP protocol, returned an unparseable response, or the selected endpoint scheme is plaintext while `allow_http_upstream` is `false`.
- `502 DownstreamError`: response headers were received but the body could not be relayed intact (fewer or more bytes than the upstream's own `Content-Length`).
- `503 LinkUnavailable`: the connection was refused, reset before response headers, or could not be established.
- `504 ConnectionTimeout` / `RequestTimeout` / `IdleTimeout`: the `proxy_timeout_secs` deadline expired during connection establishment, while waiting for response headers, or while the response body was in flight.

**Steps**:
1. [ ] - `p1` - Actor sends the proxy request; resolution, validation, endpoint selection and header transformation all succeed - `inst-proxy-unr-receive`
2. [ ] - `p1` - Call `cpt-cf-oagw-algo-proxy-forward-request` - `inst-proxy-unr-forward`
3. [ ] - `p1` - **TRY** - `inst-proxy-unr-try`
   1. [ ] - `p1` - Establish the connection and exchange the request and response within the configured deadline - `inst-proxy-unr-exchange`
4. [ ] - `p1` - **CATCH** connection, protocol, relay or deadline failure - `inst-proxy-unr-catch`
   1. [ ] - `p1` - Abort the upstream connection and discard any partial response without emitting it to the caller - `inst-proxy-unr-abort`
   2. [ ] - `p1` - Make no second attempt at the client request, per `cpt-cf-oagw-principle-no-retry` - `inst-proxy-unr-no-retry`
   3. [ ] - `p1` - Map the failure to its status and GTS `type` via `cpt-cf-oagw-algo-proxy-map-error` - `inst-proxy-unr-map`
5. [ ] - `p1` - Emit the audit record at `ERROR` level with `error_type` set to the mapped error name and `duration_ms` measured to the point of failure - `inst-proxy-unr-observe`
6. [ ] - `p1` - **RETURN** the `502`, `503` or `504` `application/problem+json` response with the `upstream_id`, `host`, `path` and `trace_id` extension fields and `X-OAGW-Error-Source: gateway` - `inst-proxy-unr-return`

## 3. Processes / Business Logic (CDSL)

### Parse and Classify the Inbound Proxy Request

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-parse-request`

**Input**: The inbound request line `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`, its header map, and the SecurityContext supplied by platform middleware.

**Output**: A proxy request context holding the normalized alias, the raw path suffix (empty when absent), the ordered query name/value pairs, the request method, and the sanitized inbound header map; or a `400` gateway error.

**Steps**:
1. [x] - `p1` - Take `tenant_id` and `principal_id` exclusively from the SecurityContext; never read them from a request header, query parameter or body, per `cpt-cf-oagw-principle-tenant-scope` and `cpt-cf-oagw-nfr-multi-tenancy` - `inst-proxy-parse-tenant`
2. [x] - `p1` - Split the path after the `/oagw/v1/proxy/` prefix at the first `/`: the first segment is the alias, the remainder (which may itself contain `/`) is the path suffix - `inst-proxy-parse-split`
3. [x] - `p1` - Percent-decode the alias segment, then normalize it to ASCII lowercase and strip trailing dots, matching the normalization the management layer applies at write time - `inst-proxy-parse-normalize-alias`
4. [x] - `p1` - **IF** the alias segment is empty, or does not match the alias grammar `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` after normalization - `inst-proxy-parse-if-bad-alias`
   1. [x] - `p1` - **RETURN** `400 RouteError` (GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`) without a tenant walk - `inst-proxy-parse-return-bad-alias`
5. [x] - `p1` - Preserve the path suffix without normalizing away its meaning: reject a suffix containing a `.` or `..` segment, a NUL byte, or an encoded `/` with `400 ValidationError`, so the outbound path cannot be steered outside the matched route (`cpt-cf-oagw-nfr-ssrf-protection`) - `inst-proxy-parse-suffix`
6. [x] - `p1` - Parse the query string into an ordered list of name/value pairs, preserving inbound order and duplicate names for later allowlist evaluation - `inst-proxy-parse-query`
7. [x] - `p1` - **FOR EACH** inbound header name/value pair - `inst-proxy-parse-foreach-header`
   1. [x] - `p1` - Reject the request with `400 ValidationError` when the name or value contains CR, LF or NUL, per the strict-header-parsing posture that guards against request smuggling - `inst-proxy-parse-header-hygiene`
8. [x] - `p1` - Classify the request as HTTP-protocol proxying; gRPC classification has no code path in this round and is handled as a non-match during route matching - `inst-proxy-parse-classify`
9. [x] - `p1` - **RETURN** the proxy request context - `inst-proxy-parse-return`

### Read the Resolved Configuration for the Request

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-read-resolved-config`

**Input**: The proxy request context (tenant, normalized alias, method, path suffix).

**Output**: The effective upstream configuration and the matched route configuration, or the resolution error that terminates the request.

**Steps**:
1. [x] - `p1` - Build the Data Plane cache key set documented for the read path: `upstream:{tenant_id}:{alias}` and `route:{upstream_id}:{method}:{path_prefix}`, per `cpt-cf-oagw-adr-data-plane-caching` - `inst-proxy-read-keys`
2. [x] - `p1` - **IF** the Data Plane L1 cache holds a resolved entry for the key set - `inst-proxy-read-if-hit`
   1. [x] - `p1` - Use the cached `(effective upstream, matched route)` pair and skip the Control Plane call, per `cpt-cf-oagw-adr-state-management` - `inst-proxy-read-hit`
3. [x] - `p1` - **ELSE** - `inst-proxy-read-else`
   1. [x] - `p1` - Issue a single Control Plane resolution call that performs one tenant-hierarchy walk covering both alias shadowing (`cpt-cf-oagw-algo-proxy-resolve-alias`) and route matching (`cpt-cf-oagw-algo-proxy-match-route`) - `inst-proxy-read-cp-call`
   2. [x] - `p1` - Have the Control Plane merge the effective configuration via `cpt-cf-oagw-algo-proxy-merge-config`, reading through its own L1, then the optional L2, then the database - `inst-proxy-read-cp-merge`
   3. [x] - `p1` - Insert the resolved pair into the Data Plane L1 cache (bounded LRU, no TTL) - `inst-proxy-read-populate`
4. [x] - `p1` - Honour the invalidation contract: cache entries carry no expiry and persist until explicitly flushed, so expose the flush entry point that a Control Plane configuration write invokes after it has flushed its own layers; never rely on TTL expiry for correctness - `inst-proxy-read-invalidation`
5. [x] - `p1` - Cache only configuration, never upstream response bodies or status codes, per `cpt-cf-oagw-principle-no-cache` - `inst-proxy-read-no-response-cache`
6. [x] - `p2` - Record the cache outcome as a `phase` label value on `oagw_request_duration_seconds` so the resolution cost tracked by `cpt-cf-oagw-nfr-low-latency` is separable from upstream time - `inst-proxy-read-phase-metric`
7. [x] - `p1` - **RETURN** the effective upstream and matched route, or propagate the resolution error - `inst-proxy-read-return`

### Resolve the Alias Through the Tenant Chain

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-resolve-alias`

**Input**: The normalized alias and the calling tenant's ancestor chain (descendant to root), supplied by the platform tenant hierarchy.

**Output**: The selected upstream (routing target), its owning tenant, the ordered chain of same-alias ancestor upstreams, and the effective enabled state; or "no upstream".

**Steps**:
1. [x] - `p1` - Order the tenant chain from the calling tenant (distance 0) outward to the root - `inst-proxy-alias-order-chain`
2. [x] - `p1` - **FOR EACH** tenant in the chain, descendant to root - `inst-proxy-alias-foreach-tenant`
   1. [x] - `p1` - Look up the upstream by the unique `(tenant_id, alias)` key using tenant-scoped reads only; comparison is case-insensitive because both sides are normalized - `inst-proxy-alias-lookup`
   2. [x] - `p1` - Collect every match found in the chain, retaining tenant distance for each - `inst-proxy-alias-collect`
3. [x] - `p1` - **IF** the collected set is empty - `inst-proxy-alias-if-empty`
   1. [x] - `p1` - **RETURN** "no upstream", which the caller renders as `404 RouteNotFound` - `inst-proxy-alias-return-empty`
4. [x] - `p1` - Select the match with the smallest tenant distance as the routing target: the closest declaration wins and shadows its ancestors - `inst-proxy-alias-select-closest`
5. [x] - `p1` - Compute the effective enabled state as the logical AND of the selected upstream's `enabled` and the `enabled` of every same-alias upstream at an ancestor tenant in the chain, so an ancestor's disable cascades and a descendant cannot re-enable it - `inst-proxy-alias-effective-enabled`
6. [x] - `p1` - Treat a disabled selection as a terminal `503`, not as a reason to fall through to an ancestor: shadowing is decided by declaration, so a disabled upstream never silently redirects traffic to a different tenant's target. This is the deterministic reading that satisfies both the closest-match-wins rule and the `503`-on-disabled rule - `inst-proxy-alias-disabled-terminal`
7. [x] - `p1` - Retain the same-alias ancestor chain in the result, because ancestor configuration marked `sharing: enforce` stays active after shadowing and is required by the configuration merge - `inst-proxy-alias-retain-ancestors`
8. [x] - `p1` - **RETURN** the selected upstream, its tenant, the ancestor chain and the effective enabled state - `inst-proxy-alias-return`

### Match the Route Within the Resolved Upstream

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-match-route`

**Input**: The selected upstream and its same-alias ancestor chain, the request method, and the inbound path suffix.

**Output**: The matched route and the matched `match.http.path` prefix, or "no route".

**Steps**:
1. [x] - `p1` - **IF** the selected upstream's `protocol` is not the HTTP protocol identifier `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1` - `inst-proxy-route-if-not-http`
   1. [x] - `p1` - **RETURN** "no route": the gRPC protocol selects gRPC match keys for which no proxy code path exists in this round, so the request cannot match - `inst-proxy-route-return-not-http`
2. [x] - `p1` - Collect candidate routes: the routes owned by the selected upstream, plus routes inherited from the same-alias ancestor upstreams in the chain, each tagged with its tenant distance - `inst-proxy-route-collect`
3. [x] - `p1` - Exclude every route whose `enabled` field is `false`; a disabled route is invisible to matching rather than an error - `inst-proxy-route-exclude-disabled`
4. [x] - `p1` - Normalize the inbound path suffix to a leading-slash path expression, using `/` when no suffix was supplied - `inst-proxy-route-normalize-path`
5. [x] - `p1` - **FOR EACH** candidate route - `inst-proxy-route-foreach`
   1. [x] - `p1` - Discard the candidate when the request method is not listed in its `match.http.methods`; the method allowlist is a match key, so a disallowed method produces a non-match rather than a distinct rejection - `inst-proxy-route-method-key`
   2. [x] - `p1` - Discard the candidate when its `match.http.path` is not a segment-boundary prefix of the normalized inbound path expression - `inst-proxy-route-prefix-key`
6. [x] - `p1` - **IF** no candidate survives - `inst-proxy-route-if-none`
   1. [x] - `p1` - **RETURN** "no route", which the caller renders as `404 RouteNotFound` - `inst-proxy-route-return-none`
7. [x] - `p1` - Order the survivors by the deterministic key: longest matching `match.http.path` first, then smallest tenant distance (a descendant route outranks an inherited ancestor route at equal prefix length), then highest numeric `priority` - `inst-proxy-route-order`
8. [x] - `p1` - Fix the `priority` tie-break direction as descending numeric value, since neither the design nor the route schema states a direction; ties beyond this key cannot occur because route management rejects two enabled routes under one upstream sharing `(path prefix, priority)` for the same method - `inst-proxy-route-priority-direction`
9. [x] - `p1` - **RETURN** the first ordered candidate together with the matched prefix length, which the outbound path computation consumes - `inst-proxy-route-return`

### Merge the Effective Configuration Across the Hierarchy

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-merge-config`

**Input**: The selected upstream, its same-alias ancestor chain with each field's `sharing` mode, the matched route, and the calling tenant's permissions.

**Output**: One effective configuration object carrying the merged `auth`, `rate_limit`, `plugins`, `cors`, `headers` and `tags` values used by this path and by the features layered on it.

**Steps**:
1. [x] - `p1` - Apply the field merge in the documented priority order Upstream (base) < Route < Tenant, so a tenant-level value outranks a route-level value and both outrank the upstream base - `inst-proxy-merge-order`
2. [x] - `p1` - Walk the ancestor chain root-to-child when folding each field, so that the nearest declaration is applied last and enforced ancestor declarations are visible at every step - `inst-proxy-merge-walk`
3. [x] - `p1` - **FOR EACH** ancestor declaration of the `auth` field - `inst-proxy-merge-foreach-auth`
   1. [x] - `p1` - **IF** `sharing` is `private`, hide the declaration from the descendant, which must then supply its own `auth` or proceed without one - `inst-proxy-merge-auth-private`
   2. [x] - `p1` - **IF** `sharing` is `inherit`, let a descendant declaration override it when the calling tenant holds `oagw:upstream:override_auth`, and otherwise keep the ancestor value - `inst-proxy-merge-auth-inherit`
   3. [x] - `p1` - **IF** `sharing` is `enforce`, keep the ancestor value and discard any descendant override - `inst-proxy-merge-auth-enforce`
4. [x] - `p1` - Compute the effective rate-limit budget as `min(selected_rate, route_rate, all ancestor enforced rates)`, so the stricter limit always wins and a descendant can only tighten it (subject to `oagw:upstream:override_rate`); the computation belongs here, while token-bucket evaluation and the `429` contract belong to DECOMPOSITION entry 2.8 - `inst-proxy-merge-rate`
5. [x] - `p1` - Concatenate the plugin chain as `ancestor.plugins + descendant.plugins`, and within one level as upstream-bound items followed by route-bound items so `[U1, U2] + [R1, R2]` yields `[U1, U2, R1, R2]`; a descendant may append only with `oagw:upstream:add_plugins` and can never remove an enforced entry. The chain is materialized here; invoking it belongs to DECOMPOSITION entry 2.9 - `inst-proxy-merge-plugins`
6. [x] - `p1` - Merge `cors` with the same sharing-mode plumbing: `inherit` unions the ancestor and descendant origin sets, `enforce` forces the ancestor value, `private` hides it. The merged value is produced here; preflight handling, origin/method validation and the credentials-with-wildcard runtime rejection belong to DECOMPOSITION entry 2.7 - `inst-proxy-merge-cors`
7. [x] - `p1` - Union the `tags` of every visible level (`effective_tags = union(ancestor_tags, descendant_tags)`); tags have no sharing mode and a descendant can add but never remove an inherited tag - `inst-proxy-merge-tags`
8. [x] - `p1` - Take the effective header rules verbatim from the selected upstream's `headers` field: the upstream schema gives `headers` no `sharing` mode and the route schema has no header counterpart, so no cross-level merge is performed for this field - `inst-proxy-merge-headers`
9. [x] - `p1` - Take the request deadline from the gear configuration key `proxy_timeout_secs` rather than from any per-resource field, since no upstream or route schema field carries a timeout - `inst-proxy-merge-timeout`
10. [x] - `p1` - **RETURN** the effective configuration object, which is the value cached by the resolved-configuration read path - `inst-proxy-merge-return`

### Select the Target Endpoint from the Pool

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-select-endpoint`

**Input**: The effective upstream (its `server.endpoints[]` and stored `alias`) and the inbound `X-OAGW-Target-Host` header, if present.

**Output**: The selected endpoint (scheme, host, port) and the selection method (`explicit_header`, `round_robin` or `default`); or one of the three `400` target-host errors.

**Steps**:
1. [x] - `p1` - Classify the alias by re-running the derivation classification the management layer applies at write time: the alias is common-suffix-derived when the endpoint set holds two or more hostnames sharing a registrable common suffix and the stored alias equals that derived value; otherwise it is single-endpoint, explicit, or IP-based - `inst-proxy-ep-classify-alias`
2. [x] - `p1` - **IF** `X-OAGW-Target-Host` is present - `inst-proxy-ep-if-header`
   1. [x] - `p1` - Validate the value as a bare hostname (RFC 1123: at most 253 characters, labels of 1 to 63 ASCII alphanumerics and hyphens, no leading or trailing hyphen, a trailing dot tolerated and stripped) or an IP literal; reject a value carrying a port, path, scheme, userinfo, whitespace or control characters as `InvalidTargetHost` - `inst-proxy-ep-validate-format`
   2. [x] - `p1` - Normalize the accepted value to ASCII lowercase with trailing dots stripped and compare it against every endpoint `host`; no match is `UnknownTargetHost`, whose body lists every configured host in `valid_hosts` - `inst-proxy-ep-match-endpoint`
   3. [x] - `p1` - **RETURN** the matched endpoint with selection method `explicit_header`, bypassing round robin; this holds for a single-endpoint upstream, an explicit-alias pool and a common-suffix pool alike - `inst-proxy-ep-return-explicit`
3. [x] - `p1` - **ELSE IF** the upstream has exactly one endpoint - `inst-proxy-ep-if-single`
   1. [x] - `p1` - **RETURN** that endpoint with selection method `default`; the header is optional for a single-endpoint upstream - `inst-proxy-ep-return-single`
4. [x] - `p1` - **ELSE IF** the upstream has two or more endpoints and its alias is common-suffix-derived - `inst-proxy-ep-if-suffix`
   1. [x] - `p1` - **RETURN** `MissingTargetHost`: the header is required to disambiguate a pool whose alias is the shared suffix rather than any one endpoint - `inst-proxy-ep-return-missing`
5. [x] - `p1` - **ELSE** (two or more endpoints under an explicit or IP-based alias, header absent) - `inst-proxy-ep-else-rr`
   1. [x] - `p1` - Select the next endpoint by round robin over `server.endpoints[]` in declaration order, advancing the cursor exactly once per selection, and **RETURN** it with selection method `round_robin` - `inst-proxy-ep-return-rr`
6. [x] - `p1` - Keep the round-robin cursor as per-upstream in-process Data Plane state: it is not persisted, not shared between instances, and resets to the first endpoint on restart; no endpoint health checking is performed because circuit breaking is excluded from this round - `inst-proxy-ep-cursor-state`
7. [x] - `p1` - Do not advance the cursor when selection fails with any of the three `400` errors, so a rejected request cannot perturb the distribution of accepted ones - `inst-proxy-ep-cursor-no-advance`
8. [x] - `p1` - Validate the header format even when the value cannot change the outcome, so that a malformed value is always reported rather than silently ignored, and reject a well-formed value that matches no endpoint even on a single-endpoint upstream rather than forwarding to a target the caller did not name. ADR-0001 contradicts itself on precisely this case: its Appendix A Example 1 calls `X-OAGW-Target-Host` "optional (ignored if provided)" for a single-endpoint upstream, while its own Behavior Matrix marks the same header "validated if present" for that same case. The conflict is flagged here rather than silently resolved, and this feature resolves it toward the matrix reading — validate and reject — because ignoring a header the caller deliberately supplied would forward the request to a host the caller did not name, which the input-validation and SSRF postures both refuse; "optional" is therefore read as governing only whether the header must be *present*, never whether a present value may be disregarded - `inst-proxy-ep-always-validate`
9. [x] - `p1` - Rely on the pool invariant that every endpoint shares `protocol`, `scheme` and `port` (enforced at write time by upstream management), so selection changes only the host component - `inst-proxy-ep-pool-invariant`
10. [x] - `p2` - Increment `oagw_routing_target_host_used{upstream_id, endpoint_host}` when the header drove the selection, and `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` on every selection - `inst-proxy-ep-metrics`

### Apply the Inbound Guard Rules

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-apply-guard-rules`

**Input**: The proxy request context (path suffix, ordered query pairs, method) and the matched route with its matched prefix length.

**Output**: The effective outbound path and outbound query string, or a `400` gateway error.

**Steps**:
1. [x] - `p1` - **IF** the matched route sets `path_suffix_mode: disabled` and a non-empty path suffix was supplied - `inst-proxy-guard-if-suffix-disabled`
   1. [x] - `p1` - **RETURN** `400 ValidationError`, naming the rejected use of a path suffix in `detail` - `inst-proxy-guard-return-suffix-disabled`
2. [x] - `p1` - **IF** the matched route sets `path_suffix_mode: disabled` and no suffix was supplied - `inst-proxy-guard-if-suffix-none`
   1. [x] - `p1` - Set the outbound path to `match.http.path` exactly - `inst-proxy-guard-path-exact`
3. [x] - `p1` - **ELSE** (`path_suffix_mode: append`, the schema default) - `inst-proxy-guard-else-append`
   1. [x] - `p1` - Set the outbound path to `match.http.path` concatenated with the part of the inbound path expression that follows the matched prefix, collapsing any duplicated separator so exactly one `/` joins them - `inst-proxy-guard-path-append`
   2. [x] - `p1` - Fix the degenerate cases explicitly: when the matched prefix equals the whole inbound path expression the remainder is empty and the outbound path is `match.http.path`; when `match.http.path` is `/` the whole suffix is appended. This is the deterministic reading that reconciles longest-path-prefix matching with the append rule - `inst-proxy-guard-path-degenerate`
4. [x] - `p1` - **FOR EACH** inbound query name/value pair, in inbound order - `inst-proxy-guard-foreach-query`
   1. [x] - `p1` - **IF** the parameter name is absent from `match.http.query_allowlist` (exact, case-sensitive comparison; an empty or absent allowlist admits no parameter at all) - `inst-proxy-guard-if-query-unknown`
      1. [x] - `p1` - **RETURN** `400 ValidationError`, naming the rejected parameter name and no value - `inst-proxy-guard-return-query-unknown`
   2. [x] - `p1` - **ELSE** append the pair to the outbound query, preserving order and duplicates - `inst-proxy-guard-query-keep`
5. [x] - `p1` - Never silently drop a query parameter: unknown names are rejected, so the observable effect is that only allowlisted parameters ever reach an upstream, which is what the transformation rule "passthrough allowed params" describes - `inst-proxy-guard-no-silent-drop`
6. [x] - `p1` - Pass the request method through unchanged; it was already validated as a route match key - `inst-proxy-guard-method-passthrough`
7. [x] - `p1` - **RETURN** the effective outbound path and query - `inst-proxy-guard-return`

### Validate the Request Body

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-validate-body`

**Input**: The inbound `Content-Length` and `Transfer-Encoding` headers and the inbound body stream.

**Output**: The forwardable body and its exact byte length, or a `400` or `413` gateway error.

**Steps**:
1. [x] - `p1` - **IF** both `Content-Length` and `Transfer-Encoding` are present - `inst-proxy-body-if-cl-te`
   1. [x] - `p1` - **RETURN** `400 ValidationError`: the combination is rejected as a request-smuggling vector - `inst-proxy-body-return-cl-te`
2. [x] - `p1` - **IF** `Transfer-Encoding` is present - `inst-proxy-body-if-te`
   1. [x] - `p1` - Accept only the single token `chunked`, compared case-insensitively; a list of codings or any other coding (for example `gzip` or `identity`) **RETURN**s `400 ValidationError` - `inst-proxy-body-te-chunked-only`
   2. [x] - `p1` - Meter the chunked stream while reading and abort with `413 PayloadTooLarge` as soon as the accumulated size exceeds the limit, so an oversized body is never fully buffered - `inst-proxy-body-te-meter`
3. [x] - `p1` - **IF** `Content-Length` is present - `inst-proxy-body-if-cl`
   1. [x] - `p1` - Require a single well-formed non-negative decimal integer with no sign, whitespace or conflicting duplicate values; anything else **RETURN**s `400 ValidationError` - `inst-proxy-body-cl-format`
   2. [x] - `p1` - **IF** the declared length exceeds the 100 MB hard limit of `cpt-cf-oagw-constraint-body-limit` - `inst-proxy-body-if-declared-too-large`
      1. [x] - `p1` - **RETURN** `413 PayloadTooLarge` immediately, before reading or buffering any body byte and before opening an upstream connection - `inst-proxy-body-return-declared-too-large`
   3. [x] - `p1` - After reading, require the actual byte count to equal the declared length; a mismatch in either direction **RETURN**s `400 ValidationError` - `inst-proxy-body-cl-actual`
4. [x] - `p1` - Treat the 100 MB limit as a fixed platform constraint that no upstream, route or tenant configuration can raise - `inst-proxy-body-limit-fixed`
5. [x] - `p1` - Leave the body content itself untouched: this path performs no decoding, re-encoding, schema validation or content-type inspection, which additional guard plugins own. Inbound request-body streaming is out of scope for this decomposition round, so this algorithm exposes no unbuffered-body extension point and always buffers the accepted body up to the limit: DECOMPOSITION entry 2.6 covers upstream-response event streaming and WebSocket frame relay only, and attaches nothing here - `inst-proxy-body-passthrough`
6. [x] - `p1` - **RETURN** the forwardable body and its exact byte length - `inst-proxy-body-return`

### Transform the Headers in Both Directions

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-transform-headers`

**Input**: The sanitized inbound header map or the upstream response header map, the effective `headers` configuration, the selected endpoint, and the negotiated HTTP version.

**Output**: The outbound request header map, or the outbound response header map.

**Steps**:
1. [x] - `p1` - Category 1, routing headers: remove `X-OAGW-Target-Host` after endpoint selection has consumed it, so it is never forwarded to an upstream - `inst-proxy-hdr-routing-strip`
2. [x] - `p1` - Category 2, hop-by-hop headers: unconditionally remove `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding` and `Upgrade` from the request, and the same set from the upstream response, in both cases before any configured rule is applied - `inst-proxy-hdr-hop-strip`
3. [x] - `p1` - Category 3, passthrough headers, request direction: apply `headers.request.passthrough` to what remains — `none` drops every remaining inbound header, `allowlist` keeps only names listed in `headers.request.passthrough_allowlist` (compared case-insensitively), `all` keeps every remaining inbound header - `inst-proxy-hdr-passthrough`
4. [x] - `p1` - Apply the configured request rules in the fixed order `remove`, then `set`, then `add`, so that gateway configuration always outranks the inbound request: `remove` deletes named headers case-insensitively, `set` overwrites or inserts a single value, `add` appends and may create duplicates - `inst-proxy-hdr-request-rules`
5. [x] - `p1` - Replace the authority after the configured rules have been applied: on HTTP/1.1 set `Host` to the selected endpoint's authority (host, plus `:port` when the port is not the scheme default), and on HTTP/2 set the `:authority` pseudo-header to the same value - `inst-proxy-hdr-authority`
6. [x] - `p1` - Treat `Host` and `:authority` as computed values that a `set` or `add` entry cannot override, so no configuration entry can redirect the connection authority away from the selected endpoint - `inst-proxy-hdr-authority-final`
7. [x] - `p1` - Do not read `:authority` as a routing input: the HTTP/2 pseudo-header replaces `Host` for connection addressing only and never substitutes for `X-OAGW-Target-Host`, which stays the sole endpoint-selection header on both protocol versions - `inst-proxy-hdr-authority-not-routing`
8. [x] - `p1` - Adjust the well-known content headers: set `Content-Length` to the exact byte length of the forwarded body, omit it for a body-less request, and forward `Content-Type` unchanged - `inst-proxy-hdr-content`
9. [x] - `p1` - Response direction: after hop-by-hop stripping, apply `headers.response` in the fixed order `remove`, then `set`, then `add`; the response configuration has no passthrough mode, so upstream response headers that survive stripping and `remove` are forwarded - `inst-proxy-hdr-response-rules`
10. [x] - `p1` - Do not attempt complex or conditional header rewriting here: the schema's set/add/remove/passthrough vocabulary is the whole surface, and anything beyond it is a transform plugin's concern - `inst-proxy-hdr-scope`
11. [x] - `p2` - **EXTENSION POINT (no-op in this feature)** upgrade-header hook: DECOMPOSITION entry 2.6 needs `Connection` and `Upgrade` preserved for a WebSocket handshake instead of stripped, and attaches that exception here; this feature always strips them - `inst-proxy-hdr-upgrade-hook`
12. [x] - `p1` - **RETURN** the transformed header map - `inst-proxy-hdr-return`

### Forward the Request to the Selected Endpoint

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-forward-request`

**Input**: The selected endpoint, the outbound method, path, query, headers and body, and the gear configuration (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`).

**Output**: The upstream response (status, headers, body stream), or a `502`, `503` or `504` gateway error.

**Steps**:
1. [x] - `p1` - Compose the outbound target as `{scheme}://{host}[:{port}]{path}[?{query}]` from the selected endpoint and the effective path and query - `inst-proxy-fw-compose`
2. [x] - `p1` - **IF** the selected endpoint's scheme is a plaintext scheme - `inst-proxy-fw-if-plaintext`
   1. [x] - `p1` - **IF** `allow_http_upstream` is `true`, as the graded configuration sets it, open the plaintext connection and forward the request - `inst-proxy-fw-plaintext-allowed`
   2. [x] - `p1` - **ELSE** refuse the request before any socket is opened and **RETURN** `502 ProtocolError` with GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1`, which is the platform-default HTTPS-only posture of `cpt-cf-oagw-constraint-https-only` - `inst-proxy-fw-plaintext-refused`
   3. [x] - `p1` - Do not re-litigate whether the scheme was legal to declare on the upstream record: that decision was made at the management layer, and this gate governs only whether a plaintext connection is opened - `inst-proxy-fw-plaintext-layering`
3. [x] - `p1` - Leave TLS schemes unaffected by the flag - `inst-proxy-fw-tls-unaffected`
4. [x] - `p1` - Read `ssrf_policy.enabled` from gear configuration; the DNS-resolution and IP-pinning controls it would gate are out of scope for this round, so with the graded value `false` no address-policy evaluation is performed and the SSRF posture rests on the scheme gate, the path and query validation, and the header stripping already applied - `inst-proxy-fw-ssrf-policy`
5. [x] - `p1` - Negotiate the HTTP version per host: attempt HTTP/2 via ALPN on a TLS connection and fall back to HTTP/1.1, entirely delegated to `toolkit_http`'s underlying hyper-rustls connector and its own generic connection pooling (RF-011: this feature adds no bespoke per-host ALPN-outcome cache of its own, one-hour or otherwise); a plaintext connection offers no ALPN and therefore uses HTTP/1.1 - `inst-proxy-fw-version`
6. [x] - `p1` - Start a single request deadline of `proxy_timeout_secs` seconds (2 in the graded configuration) covering connection establishment, request transmission and response receipt - `inst-proxy-fw-deadline`
7. [x] - `p1` - Send the outbound request exactly once - `inst-proxy-fw-send`
8. [x] - `p2` - **EXTENSION POINT (no-op in this feature)** protocol-upgrade hook: DECOMPOSITION entry 2.6 branches here when the handshake response is `101 Switching Protocols`, and detects a `text/event-stream` response for unbuffered relay; this feature treats every response as a complete non-streaming message - `inst-proxy-fw-upgrade-hook`
9. [x] - `p1` - **IF** the deadline expires - `inst-proxy-fw-if-timeout`
   1. [x] - `p1` - Abort the connection and classify the expiry: before the connection was established, `504 ConnectionTimeout`; established but no response headers received, `504 RequestTimeout`; response headers received but the body stalled, `504 IdleTimeout` - `inst-proxy-fw-timeout-classify`
10. [x] - `p1` - **IF** the connection is refused, reset before response headers, or cannot be established - `inst-proxy-fw-if-unavailable`
    1. [x] - `p1` - **RETURN** `503 LinkUnavailable` - `inst-proxy-fw-return-unavailable`
11. [x] - `p1` - **IF** the upstream response cannot be parsed as valid HTTP - `inst-proxy-fw-if-protocol`
    1. [x] - `p1` - **RETURN** `502 ProtocolError` - `inst-proxy-fw-return-protocol`
12. [x] - `p1` - Never re-issue the client request after any failure, including a timeout or a reset: exactly one client-level attempt is made, per `cpt-cf-oagw-principle-no-retry`. Connector-level connection or endpoint failover before the request has been accepted is the only permitted retry-shaped behaviour, and it never duplicates an accepted request - `inst-proxy-fw-no-retry`
13. [x] - `p1` - Implement no circuit breaker and no upstream health tracking: both are excluded from this round, so a failing endpoint stays in the round-robin pool - `inst-proxy-fw-no-breaker`
14. [x] - `p1` - Keep this the only code path in the gear that opens a connection to an external host, per `cpt-cf-oagw-constraint-no-direct-internet` - `inst-proxy-fw-sole-egress`
15. [x] - `p1` - **RETURN** the upstream response, or the mapped gateway error - `inst-proxy-fw-return`

### Relay the Upstream Response to the Caller

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-relay-response`

**Input**: The upstream response (status, headers, body) and the effective `headers.response` configuration.

**Output**: The response delivered to the caller.

**Steps**:
1. [x] - `p1` - Carry the upstream status code through unchanged for every class, including `3xx`, `4xx` and `5xx`; OAGW does not follow redirects and does not substitute a status of its own - `inst-proxy-relay-status`
2. [x] - `p1` - Transform the response headers by calling `cpt-cf-oagw-algo-proxy-transform-headers` in the response direction. This step also carries the named **EXTENSION POINT (no-op in this feature)** post-relay response-header hook: after the configured `headers.response` rules have been applied and before the status and headers are committed to the caller, DECOMPOSITION entry 2.7 adds the `Access-Control-*` headers and `Vary: Origin` to the relayed response here; naming the hook keeps that response-phase mutation as explicit as the post-call Transform-on-response plugin hook already is, so no dependent feature has to mutate response headers at an unnamed location - `inst-proxy-relay-headers`
3. [x] - `p1` - Relay the response body byte-for-byte without inspection, re-encoding, pretty-printing or truncation, whatever its content type - `inst-proxy-relay-body`
4. [x] - `p1` - **IF** the delivered body length disagrees with the upstream's own `Content-Length` - `inst-proxy-relay-if-truncated`
   1. [x] - `p1` - **RETURN** `502 DownstreamError` with GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` rather than delivering a partial body as if it were complete - `inst-proxy-relay-return-truncated`
5. [x] - `p1` - Set `X-OAGW-Error-Source: upstream` on every response that originated at the upstream, including successful ones, and replace any same-named header the upstream supplied so the value always reflects OAGW's own classification, per `cpt-cf-oagw-principle-error-source` - `inst-proxy-relay-error-source`
6. [x] - `p1` - Never wrap an upstream response in the RFC 9457 envelope: `application/problem+json` is reserved for gateway-originated errors - `inst-proxy-relay-no-envelope`
7. [x] - `p1` - Store no part of the response: nothing is cached, persisted or logged as content, per `cpt-cf-oagw-principle-no-cache` - `inst-proxy-relay-no-store`
8. [x] - `p2` - **EXTENSION POINT (no-op in this feature)** incremental-relay hook: DECOMPOSITION entry 2.6 forwards each SSE event as received here instead of relaying a complete message, and reports an aborted stream as `StreamAborted` - `inst-proxy-relay-stream-hook`
9. [x] - `p1` - **RETURN** the relayed response - `inst-proxy-relay-return`

### Map a Gateway Failure to Its Response

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-map-error`

**Input**: The failure condition raised anywhere on this path, plus the request context available at that point (alias, upstream identifier, selected host, effective path, correlation identifier).

**Output**: An HTTP status, a GTS error `type`, and the problem-details fields handed to the shared error renderer.

**Steps**:
1. [x] - `p1` - Map an inbound-form failure (bad alias grammar, rejected path-suffix segment) to `400` with `type` `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` - `inst-proxy-err-route-error`
2. [x] - `p1` - Map a validation failure (path suffix supplied under `disabled`, non-allowlisted query parameter, malformed or mismatched `Content-Length`, unsupported `Transfer-Encoding`, `Content-Length` with `Transfer-Encoding`, CR/LF or NUL in a header) to `400` with the same validation `type` - `inst-proxy-err-validation`
3. [x] - `p1` - Map the three target-host failures to `400` with `type` `...cf.oagw.routing.missing_target_host.v1`, `...cf.oagw.routing.invalid_target_host.v1` and `...cf.oagw.routing.unknown_target_host.v1` respectively, each carrying the extension fields its documented body specifies (`alias` and `valid_hosts`; `invalid_value`; `invalid_value` and `valid_hosts`) - `inst-proxy-err-target-host`
4. [x] - `p1` - Map an unresolved alias, an unmatched route and a gRPC-protocol upstream to `404` with `type` `...cf.oagw.route.not_found.v1` - `inst-proxy-err-not-found`
5. [x] - `p1` - Map an over-limit body to `413` with `type` `...cf.oagw.payload.too_large.v1` - `inst-proxy-err-too-large`
6. [x] - `p1` - Map an upstream protocol violation, an unparseable upstream response, and a refused plaintext connection to `502` with `type` `...cf.oagw.protocol.error.v1` - `inst-proxy-err-protocol`
7. [x] - `p1` - Map a response that could not be relayed intact to `502` with `type` `...cf.oagw.downstream.error.v1` - `inst-proxy-err-downstream`
8. [x] - `p1` - Map an effective `enabled: false` upstream, a refused or reset connection, and an unestablishable connection to `503` with `type` `...cf.oagw.link.unavailable.v1` - `inst-proxy-err-unavailable`
9. [x] - `p1` - Map the three deadline expiries to `504` with `type` `...cf.oagw.timeout.connection.v1`, `...cf.oagw.timeout.request.v1` and `...cf.oagw.timeout.idle.v1` respectively - `inst-proxy-err-timeouts`
10. [x] - `p1` - Populate the RFC 9457 fields for every mapping: `type` (the GTS identifier), `title`, `status`, `detail` (occurrence-specific and free of credentials, body content, header values and query values), and `instance` set to the inbound `/oagw/v1/proxy/{alias}[/{path_suffix}]` path - `inst-proxy-err-standard-fields`
11. [x] - `p1` - Populate the extension fields that are known at failure time: `upstream_id`, `host`, `path` and `trace_id`, omitting any field whose value is not yet known (for example `upstream_id` for an unresolved alias) rather than emitting a placeholder - `inst-proxy-err-extension-fields`
12. [x] - `p1` - Emit every gateway error as `application/problem+json` with `X-OAGW-Error-Source: gateway`, using the shared renderer that gear foundation provides rather than a second envelope implementation, per `cpt-cf-oagw-principle-rfc9457` - `inst-proxy-err-render`
13. [x] - `p1` - Emit none of `401 AuthenticationFailed`, `429 RateLimitExceeded`, `500 SecretNotFound`, `502 StreamAborted`, `503 CircuitBreakerOpen`, `503 PluginNotFound` or `409 PluginInUse` from this path. `401 AuthenticationFailed` belongs to plugin execution, `429 RateLimitExceeded` to rate limiting, `502 StreamAborted` to streaming, `503 PluginNotFound` and `409 PluginInUse` to plugin management, and `503 CircuitBreakerOpen` is excluded from this round with the circuit breaker. `500 SecretNotFound` is different and must not be attributed to plugin execution: it is raised by no feature in this decomposition round at all, because plugin execution deliberately surfaces an absent or inaccessible secret as `401 AuthenticationFailed` instead of disclosing that a named secret could not be resolved. The catalog row therefore stays defined and renderable in the shared error catalog while remaining unraised - `inst-proxy-err-not-mine`
14. [x] - `p1` - **RETURN** the status, `type` and populated envelope fields - `inst-proxy-err-return`

### Observe the Proxy Request

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-observe-request`

**Input**: The request context at each lifecycle boundary and the final outcome (status and error classification).

**Output**: One structured audit record per request and the base metric updates.

**Steps**:
1. [x] - `p1` - Obtain the request correlation identifier from the correlation plumbing gear foundation provides, reusing an inbound value when the platform supplied one and otherwise taking a freshly generated one, and bind it to the request scope for the whole lifecycle - `inst-proxy-obs-correlate`
2. [x] - `p1` - Expose the same identifier as `request_id` in the audit record and as `trace_id` in every gateway problem-details body, so a client-visible error can be joined to its log line - `inst-proxy-obs-expose`
3. [x] - `p1` - Increment `oagw_requests_in_flight{host}` on entry and decrement it on completion, including on every error path - `inst-proxy-obs-in-flight`
4. [x] - `p1` - Measure the elapsed time per lifecycle phase and record it in `oagw_request_duration_seconds{host, http.route, phase}` using the documented bucket boundaries - `inst-proxy-obs-duration`
5. [x] - `p1` - Emit exactly one audit record per request on completion, with the documented fields `timestamp`, `level`, `event` (`proxy_request`), `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size` and `error_type` - `inst-proxy-obs-audit-record`
6. [x] - `p1` - Set `host` to the upstream alias or selected endpoint host and `path` to the effective upstream-side path, not to the inbound `/oagw/v1/proxy/...` path, so log and metric cardinality stays bounded - `inst-proxy-obs-host-path`
7. [x] - `p1` - Set `level` to `INFO` for a completed request and to `ERROR` for an upstream failure, timeout or gateway error, and set `error_type` to the mapped error name on failure and to null on success - `inst-proxy-obs-levels`
8. [x] - `p1` - Increment `oagw_requests_total{host, http.request.method, http.route, http.response.status_code}` on every completed request and `oagw_errors_total{host, http.route, error_type}` on every failure - `inst-proxy-obs-counters`
9. [x] - `p1` - Keep label cardinality bounded as documented: no tenant label, `http.route` as the normalized route match pattern rather than the raw request path, `http.request.method` normalized to a standard verb or `_OTHER`, and `http.response.status_code` as the numeric status - `inst-proxy-obs-cardinality`
10. [x] - `p1` - Log no request or response body, no query string, no header value outside an allowlist, and no credential material in any field, per `cpt-cf-oagw-nfr-credential-isolation` - `inst-proxy-obs-no-pii`
11. [x] - `p1` - Emit no rate-limit, circuit-breaker or upstream-health metric from this path: those depend on machinery owned elsewhere or excluded from this round - `inst-proxy-obs-not-mine`
12. [x] - `p1` - **RETURN** the completed audit record - `inst-proxy-obs-return`

## 4. States (CDSL)

### Proxy Request Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-proxy-request-lifecycle`

A genuine lifecycle exists here even though nothing is persisted: the per-request context advances through named phases that the `phase` label of `oagw_request_duration_seconds` reports, that the extension points of DECOMPOSITION entries 2.6 to 2.9 attach to, and that decide whether a caller receives an upstream response or a gateway error. No other entity in this feature has a lifecycle: upstreams and routes are read-only inputs here and their `enabled` field is evaluated, not transitioned.

**States**: Received, Resolved, Validated, Dispatched, Relaying, Completed, Failed

**Initial State**: Received

**Transitions**:
1. [ ] - `p1` - **FROM** Received **TO** Resolved **WHEN** the alias resolves to an enabled upstream, a route matches, and the effective configuration is merged - `inst-proxy-state-received-resolved`
2. [ ] - `p1` - **FROM** Received **TO** Failed **WHEN** the request form is invalid, the alias does not resolve, no route matches, or the effective enabled state is false - `inst-proxy-state-received-failed`
3. [ ] - `p1` - **FROM** Resolved **TO** Validated **WHEN** the guard rules, the body validation and the endpoint selection all pass - `inst-proxy-state-resolved-validated`
4. [ ] - `p1` - **FROM** Resolved **TO** Failed **WHEN** a guard rule rejects the request, the body is invalid or too large, or target-host selection fails - `inst-proxy-state-resolved-failed`
5. [ ] - `p1` - **FROM** Validated **TO** Dispatched **WHEN** the outbound connection is established and the request has been sent within the deadline - `inst-proxy-state-validated-dispatched`
6. [ ] - `p1` - **FROM** Validated **TO** Failed **WHEN** the plaintext gate refuses the scheme, the connection cannot be established, or the connection deadline expires - `inst-proxy-state-validated-failed`
7. [ ] - `p1` - **FROM** Dispatched **TO** Relaying **WHEN** upstream response headers are received within the deadline - `inst-proxy-state-dispatched-relaying`
8. [ ] - `p1` - **FROM** Dispatched **TO** Failed **WHEN** the request deadline expires or the upstream response cannot be parsed - `inst-proxy-state-dispatched-failed`
9. [ ] - `p1` - **FROM** Relaying **TO** Completed **WHEN** the status, headers and whole body have been relayed to the caller - `inst-proxy-state-relaying-completed`
10. [ ] - `p1` - **FROM** Relaying **TO** Failed **WHEN** the idle deadline expires or the body cannot be relayed intact - `inst-proxy-state-relaying-failed`
11. [ ] - `p1` - **FROM** Completed **TO** Completed **WHEN** the outcome is an upstream error status, which is a completed relay and not a gateway failure - `inst-proxy-state-completed-upstream-error`
12. [ ] - `p1` - Emit the audit record and the base metrics on entry to Completed or Failed, and on no other transition, so exactly one record exists per request - `inst-proxy-state-terminal-observe`
13. [ ] - `p2` - **EXTENSION POINT (no-op in this feature)** long-lived transfer state: DECOMPOSITION entry 2.6 extends Relaying into a streaming or upgraded-connection state that survives many events or frames; this feature leaves Relaying only for Completed or Failed - `inst-proxy-state-streaming-hook`

## 5. Definitions of Done

**Review-domain dispositions.** UX and accessibility — not applicable because this feature exposes no user interface: its whole surface is one machine-to-machine HTTP path whose responses are the upstream's own bytes or an `application/problem+json` document, with no rendered view, no interaction affordance and therefore no assistive-technology contract. Compliance and privacy — not applicable because OAGW originates and stores no personal or otherwise regulated data on this path: request and response payloads traverse the gateway untouched and unretained, nothing is cached beyond configuration, and no field of any payload is inspected, classified or persisted. Data privacy — not applicable for the same reason, reinforced by the observability rules that forbid logging bodies, query strings, non-allowlisted header values and credential material; the only request-derived values that leave this path in a durable form are the bounded audit-record fields and metric labels enumerated in `cpt-cf-oagw-algo-proxy-observe-request`. Performance — applicable and dispositioned rather than waived: the latency budget of `cpt-cf-oagw-nfr-low-latency` is met through the cached resolved-configuration read path, the single client-level attempt under one deadline, and the `phase` label that separates resolution cost from upstream time, all of which are stated as verifiable behaviour in `cpt-cf-oagw-dod-proxy-config-read-path` and `cpt-cf-oagw-dod-proxy-timeout-no-retry`. Extension points and resilience/recovery are likewise dispositioned rather than waived, in `cpt-cf-oagw-dod-proxy-extension-points` and in the Reliability and Rollback statements of `cpt-cf-oagw-dod-proxy-cross-cutting` respectively.

**On the two traceability fields used below.** The `Constraints` field carries only design-constraint identifiers — the hard `cpt-cf-oagw-constraint-*` boundaries it is typed for — and reads `None` where no design constraint bears on the definition of done. The design principles, non-functional requirements and design components that a definition of done answers to are cited on the adjacent `Related` line instead, so that no citation is lost while the typed field keeps the single identifier kind it expects.

### Proxy Endpoint Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-endpoint-registration`

The system **MUST** register the proxy endpoint gear-relative at `/oagw/v1/proxy/{alias}` and `/oagw/v1/proxy/{alias}/{path_suffix}` for every method the route schema admits (`GET`, `POST`, `PUT`, `DELETE`, `PATCH`), with the path-suffix segment capturing multiple path segments, dispatch it to the Data Plane, and require the `gts.cf.core.oagw.proxy.v1~:invoke` permission on the caller's bearer token. No `/api` prefix is registered by the gear; that prefix belongs to a host gateway and the graded configuration does not set it.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request-forwarded`
- `cpt-cf-oagw-algo-proxy-parse-request`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none — this path performs no writes
- Entities: Proxy request context

### Alias Resolution Through the Tenant Chain

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-alias-resolution`

The system **MUST** normalize the inbound alias to ASCII lowercase with trailing dots stripped, walk the caller's tenant chain from descendant to root using tenant-scoped reads, select the closest tenant's upstream declaring that alias as the routing target so that a descendant shadows its ancestors, retain the same-alias ancestor chain so enforced ancestor configuration survives shadowing, and return `404 RouteNotFound` when no tenant in the chain declares the alias.

**RF-009 note**: production always wires `crate::proxy::hierarchy::NoTenantHierarchy` (every tenant is its own root, so the ancestor chain this DoD describes is always length-one in the deployed gear) -- this gear has no `GearCtx`/cross-gear client-hub handle reachable from `RestApiCapability::register_rest` to a real `tenant-resolver-sdk` client this round, and adding one is explicitly out of scope. This is fail-safe (it only ever narrows which ancestor-chain shadowing/merge branches can fire, never widens). The multi-level ancestor-chain behaviour above is verified via an injected fake `TenantHierarchyProvider` in this crate's own tests, not through the production wiring.

**Implements**:
- `cpt-cf-oagw-algo-proxy-resolve-alias`
- `cpt-cf-oagw-flow-proxy-alias-unresolved`

**Constraints**: None

**Related**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- DB: none — read-only through the resolved-configuration read path
- Entities: Upstream, Resolved route match

### Route Matching Within the Resolved Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-route-matching`

The system **MUST** match a request to a route by treating the method allowlist and the `match.http.path` prefix as match keys, ordering surviving candidates by longest matching prefix, then smallest tenant distance so a descendant route outranks an inherited ancestor route, then highest numeric `priority`; and **MUST** return `404 RouteNotFound` when no candidate survives, including when the resolved upstream's protocol is the gRPC identifier.

**Implements**:
- `cpt-cf-oagw-algo-proxy-match-route`
- `cpt-cf-oagw-flow-proxy-alias-unresolved`

**Constraints**: None

**Related**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`
- DB: none — read-only through the resolved-configuration read path
- Entities: Route, Resolved route match

### Enable and Disable Enforcement at Proxy Time

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-enable-disable`

The system **MUST** compute the effective enabled state of the selected upstream as the logical AND of its own `enabled` and that of every same-alias ancestor upstream in the chain, reject the request with `503` and the link-unavailable GTS `type` when that state is false without opening any upstream connection, and exclude every route with `enabled: false` from matching rather than reporting it as an error.

**RF-009 note**: as with `cpt-cf-oagw-dod-proxy-alias-resolution` above, production's `NoTenantHierarchy` wiring means the "every same-alias ancestor upstream" AND-reduction always has exactly one term (the selected upstream's own `enabled`) in the deployed gear; the multi-level case is verified only via an injected fake `TenantHierarchyProvider` in tests, not through production wiring -- see that DoD's note for why closing this is out of scope.

**Implements**:
- `cpt-cf-oagw-flow-proxy-upstream-disabled`
- `cpt-cf-oagw-algo-proxy-resolve-alias`
- `cpt-cf-oagw-algo-proxy-match-route`

**Constraints**: None

**Related**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none — read-only through the resolved-configuration read path
- Entities: Upstream, Route

### Hierarchical Configuration Merge

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-config-merge`

The system **MUST** produce one effective configuration per request in the priority order Upstream (base) < Route < Tenant, applying the documented sharing-mode semantics per field: `auth` overridden only under `inherit` and with the override permission and forced under `enforce`; the rate-limit budget computed as the minimum of the selected, route and all enforced ancestor rates; plugin chains concatenated ancestor-then-descendant and upstream-then-route with enforced entries unremovable; `cors` origins unioned under `inherit` and forced under `enforce`; `tags` unioned additively with no sharing mode and no removal; and `headers` taken verbatim from the selected upstream because that field has neither a sharing mode nor a route-level counterpart.

**RF-009 note**: as with `cpt-cf-oagw-dod-proxy-alias-resolution` above, production's `NoTenantHierarchy` wiring means every "enforced ancestor" term in this merge (the rate-limit minimum, and every other field's ancestor-chain fold) is always vacuous in the deployed gear -- the ancestor-chain merge behaviour (`Sharing::Inherit`/`Sharing::Enforce` folding across more than one tenant level) is verified via an injected fake `TenantHierarchyProvider` in this crate's own tests, not through production wiring. `merge_auth`/`merge_cors`/`merge_rate_limit`/`merge_plugins`/`merge_tags` themselves correctly implement the multi-level fold; only the production hierarchy source is narrowed.

**Implements**:
- `cpt-cf-oagw-algo-proxy-merge-config`
- `cpt-cf-oagw-algo-proxy-read-resolved-config`

**Constraints**: None

**Related**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none — read-only through the resolved-configuration read path
- Entities: Upstream, Route, Proxy request context

### Resolved-Configuration Read Path and Invalidation

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-proxy-config-read-path`

The system **MUST** read the resolved `(effective upstream, matched route)` pair through the documented read path — Data Plane L1, then a single Control Plane resolution call that performs one hierarchy walk and one merge behind the Control Plane's own L1, optional L2 and database — populate the Data Plane L1 on a miss, and expose the explicit flush entry point a configuration write invokes, since these caches carry no TTL and correctness must not depend on expiry. It **MUST NOT** cache any upstream response.

**Implements**:
- `cpt-cf-oagw-algo-proxy-read-resolved-config`

**Constraints**: None

**Related**: `cpt-cf-oagw-principle-no-cache`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none — read-only through the Control Plane
- Entities: Proxy request context, Resolved route match

### Target-Host Behaviour Matrix

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-target-host`

The system **MUST** implement the `X-OAGW-Target-Host` matrix in full: the header is optional for a single-endpoint upstream; absent on a multi-endpoint pool whose alias is explicit or IP-based it selects by round robin over the declared endpoint order; absent on a multi-endpoint pool whose alias was derived from a registrable common suffix it is required and yields `400 MissingTargetHost`; present it is always format-validated, yielding `400 InvalidTargetHost` for a value carrying a port, path, scheme, userinfo, whitespace or control character, and `400 UnknownTargetHost` for a well-formed value matching no configured endpoint host; and present and matching it selects that endpoint and bypasses round robin. All three errors **MUST** carry their documented extension fields and **MUST NOT** advance the round-robin cursor.

**Implements**:
- `cpt-cf-oagw-algo-proxy-select-endpoint`
- `cpt-cf-oagw-flow-proxy-target-host-rejected`

**Constraints**: None

**Related**: `cpt-cf-oagw-nfr-input-validation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none — read-only
- Entities: Endpoint, Endpoint pool (round-robin state)

### Header Handling in Three Categories

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-header-handling`

The system **MUST** consume `X-OAGW-Target-Host` during endpoint selection and then strip it so it never reaches an upstream; **MUST** unconditionally strip `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding` and `Upgrade` in both directions; **MUST** apply the upstream `headers.request` passthrough mode (`none`, `allowlist`, `all`) and then `remove`, `set`, `add` in that fixed order, and `headers.response` `remove`, `set`, `add` on the way back; **MUST** replace `Host` on HTTP/1.1 and `:authority` on HTTP/2 with the selected endpoint's authority after configured rules have run, such that no configured rule can redirect the connection; **MUST NOT** treat `:authority` as an endpoint-selection input; and **MUST** recompute `Content-Length` to the forwarded body's exact length.

**Implements**:
- `cpt-cf-oagw-algo-proxy-transform-headers`
- `cpt-cf-oagw-flow-proxy-request-forwarded`

**Constraints**: None

**Related**: `cpt-cf-oagw-nfr-ssrf-protection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none — read-only
- Entities: Upstream, Proxy request context

### Body Validation Before Buffering

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-body-validation`

The system **MUST** reject a declared or accumulated body larger than the 100 MB hard limit with `413 PayloadTooLarge` before the body is buffered and before any upstream connection is opened; **MUST** reject a malformed `Content-Length`, a `Content-Length` that disagrees with the bytes actually received, a `Transfer-Encoding` naming anything other than the single token `chunked`, and the presence of both headers together, each with `400 ValidationError`; and **MUST** forward the accepted body unmodified.

**Implements**:
- `cpt-cf-oagw-algo-proxy-validate-body`
- `cpt-cf-oagw-flow-proxy-request-invalid`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Proxy request context

### Guard Rules for Path Suffix and Query

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-guard-rules`

The system **MUST** reject a supplied path suffix with `400 ValidationError` when the matched route sets `path_suffix_mode: disabled`; **MUST** append the unmatched remainder of the inbound path expression to `match.http.path` when the mode is `append`, joining with exactly one separator and degenerating correctly when the remainder is empty or `match.http.path` is `/`; and **MUST** reject with `400 ValidationError` any inbound query parameter whose name is absent from `match.http.query_allowlist`, treating an empty or absent allowlist as admitting no parameter, so that only allowlisted parameters ever reach an upstream and none is silently dropped.

**Implements**:
- `cpt-cf-oagw-algo-proxy-apply-guard-rules`
- `cpt-cf-oagw-flow-proxy-request-invalid`

**Constraints**: None

**Related**: `cpt-cf-oagw-nfr-input-validation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`
- DB: none — read-only
- Entities: Route, HTTP match

### Request Forwarding and Response Relay

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-forwarding`

The system **MUST** forward the request to the selected endpoint with the method passed through, the computed path and query, the transformed headers and the unmodified body; **MUST** negotiate the HTTP version per host with an ALPN attempt and an HTTP/1.1 fallback, delegated entirely to the underlying `toolkit_http`/hyper-rustls connector's own connection pooling (RF-011: not a bespoke per-host cache this feature implements itself); and **MUST** relay the upstream status, transformed headers and byte-for-byte body back to the caller, returning `502 DownstreamError` rather than delivering a body that disagrees with the upstream's own `Content-Length`.

**Implements**:
- `cpt-cf-oagw-algo-proxy-forward-request`
- `cpt-cf-oagw-algo-proxy-relay-response`
- `cpt-cf-oagw-flow-proxy-request-forwarded`
- `cpt-cf-oagw-flow-proxy-upstream-error-relayed`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Proxy request context, Endpoint

### Plaintext Upstream Connection Gate

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-plaintext-gate`

The system **MUST** consult `allow_http_upstream` before opening a connection to an endpoint whose scheme is plaintext: with the flag `true`, as the graded configuration sets it, the plaintext connection is opened and the request is forwarded; with the flag `false`, the platform default that `cpt-cf-oagw-constraint-https-only` describes, the attempt is refused before any socket is opened and rendered as `502` with the protocol-error GTS `type`. TLS schemes **MUST** be unaffected by the flag, and this gate **MUST NOT** re-validate whether the scheme was legal to declare on the upstream record, which the management layer already settled.

**Implements**:
- `cpt-cf-oagw-algo-proxy-forward-request`
- `cpt-cf-oagw-flow-proxy-upstream-unreachable`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Endpoint

### Request Deadline and No Retries

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-timeout-no-retry`

The system **MUST** apply a single request deadline taken from the gear configuration key `proxy_timeout_secs` (2 seconds in the graded configuration) covering connection establishment, request transmission and response receipt; **MUST** abort the upstream connection on expiry and return `504` with the connection, request or idle timeout GTS `type` according to which phase was in progress; and **MUST** make exactly one client-level attempt, never re-issuing the client request after any failure and implementing no circuit breaker.

**Implements**:
- `cpt-cf-oagw-algo-proxy-forward-request`
- `cpt-cf-oagw-flow-proxy-upstream-unreachable`

**Constraints**: None

**Related**: `cpt-cf-oagw-principle-no-retry`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Proxy request context

### Gateway and Upstream Error Split

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-error-source-split`

The system **MUST** set `X-OAGW-Error-Source: gateway` with an `application/problem+json` body on every response OAGW itself originates, using the GTS `type`, status and extension fields documented for each condition; **MUST** set `X-OAGW-Error-Source: upstream` on every response that originated at the upstream, success or error alike, replacing any same-named header the upstream supplied; and **MUST NOT** re-render an upstream response body in the problem-details envelope, whatever its status or content type.

**Implements**:
- `cpt-cf-oagw-algo-proxy-map-error`
- `cpt-cf-oagw-algo-proxy-relay-response`
- `cpt-cf-oagw-flow-proxy-upstream-error-relayed`

**Constraints**: None

**Related**: `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-rfc9457`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Problem Details error envelope, Proxy request context

### Base Observability for the Proxy Path

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-proxy-observability`

The system **MUST** assign a correlation identifier to every proxy request, expose it as `request_id` in the audit record and `trace_id` in every gateway problem-details body, emit exactly one structured JSON audit record per request with the documented field set, and update the base counters, histogram and in-flight gauge with the documented label vocabulary and bounded cardinality. It **MUST NOT** log bodies, query strings, non-allowlisted header values or credential material in any field.

**Implements**:
- `cpt-cf-oagw-algo-proxy-observe-request`
- `cpt-cf-oagw-state-proxy-request-lifecycle`

**Constraints**: None

**Related**: `cpt-cf-oagw-nfr-credential-isolation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Proxy request context

### Named Extension Points for the Layered Features

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-proxy-extension-points`

The system **MUST** leave the named extension points that the four dependent features attach to, each a declared no-op in this feature so that adding a dependent feature does not require restructuring this path: a pre-resolution classification hook carrying the CORS preflight fast path; a post-resolution policy hook for CORS origin/method validation and rate-limit evaluation; pre-call and post-call plugin hooks for the Auth, Guard and Transform chain; a post-relay response-header hook at which CORS adds its `Access-Control-*` and `Vary: Origin` headers to the relayed response; an upgrade-header exception hook, a protocol-upgrade branch and an incremental-relay hook for streaming; and a long-lived transfer state extending the Relaying state.

The system **MUST** also fix the relative order at the post-resolution policy hook rather than leaving it to the attaching features, so that a request failing more than one policy has one defined outcome: CORS origin/method validation, then rate-limit evaluation, then the plugin chain (Auth, then Guards, then Transform `on_request`), then forwarding. A request that is both CORS-invalid and over budget **MUST** therefore receive the CORS rejection without consuming rate-limit budget. The CORS preflight fast path sits outside that ordering entirely and **MUST** short-circuit before any resolution or tenant walk occurs.

No unbuffered-body or request-body-streaming hook is declared, because inbound request-body streaming is out of scope for this decomposition round: `cpt-cf-oagw-algo-proxy-validate-body` therefore always buffers the accepted body up to the limit, and the streaming feature's scope is upstream-response event streaming and WebSocket frame relay only.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request-forwarded`
- `cpt-cf-oagw-algo-proxy-validate-body`
- `cpt-cf-oagw-algo-proxy-transform-headers`
- `cpt-cf-oagw-algo-proxy-forward-request`
- `cpt-cf-oagw-algo-proxy-relay-response`
- `cpt-cf-oagw-state-proxy-request-lifecycle`

**Constraints**: None

**Related**: `cpt-cf-oagw-component-model`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Proxy request context

### Cross-Cutting Qualities of the Proxy Path

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-proxy-cross-cutting`

The system **MUST** satisfy the following cross-cutting properties on this path, and the implementation **MUST** treat each statement as verifiable rather than aspirational:

- **Security**: the tenant and principal come only from the SecurityContext, never from a request header, query parameter or body; the caller's token must carry `gts.cf.core.oagw.proxy.v1~:invoke`; all configuration reads are tenant-scoped; the SSRF posture consists of the scheme gate, path and query validation and header stripping, with DNS resolution and IP pinning out of scope for this round and `ssrf_policy.enabled` read but gating no behaviour this feature implements; header names and values containing CR, LF or NUL are rejected and a `Content-Length` plus `Transfer-Encoding` combination is refused; no credential material is handled on this path at all, since credential resolution and injection belong to plugin execution.
- **Reliability**: exactly one client-level attempt per request under one deadline; no circuit breaker; round-robin state is per-instance and non-authoritative; a failure aborts the upstream connection and discards any partial response instead of emitting it.
- **Data integrity**: the upstream status, headers and body are relayed byte-exact; `Content-Length` is recomputed to the truth on the way out and verified against the delivered bytes on the way back; the request body is never transcoded.
- **Observability**: one correlation identifier, one audit record and one set of metric updates per request, on both success and failure paths.
- **Rollback**: not applicable in the database sense — this path performs no writes, owns no migration and leaves no persistent state to reverse; the only residue of a failed request is an advanced round-robin cursor and its observability records, both of which are per-instance and self-correcting. Feature-level rollback is therefore achieved by disabling the affected upstream or route through the management API, which this path honours immediately once the configuration caches are flushed.

**Implements**:
- `cpt-cf-oagw-algo-proxy-parse-request`
- `cpt-cf-oagw-algo-proxy-forward-request`
- `cpt-cf-oagw-algo-proxy-relay-response`
- `cpt-cf-oagw-algo-proxy-observe-request`

**Constraints**: None

**Related**: `cpt-cf-oagw-nfr-multi-tenancy`, `cpt-cf-oagw-nfr-ssrf-protection`, `cpt-cf-oagw-nfr-observability`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- DB: none
- Entities: Proxy request context, Resolved route match, Endpoint pool (round-robin state)

## 6. Acceptance Criteria

Every criterion below is verifiable by an automated test against a running gear with the graded configuration (`proxy_timeout_secs: 2`, `allow_http_upstream: true`, `ssrf_policy.enabled: false`) and a controllable test upstream.

- [x] A `GET` to `/oagw/v1/proxy/{alias}/{path_suffix}` against an enabled upstream and a matching route reaches the test upstream, and the caller receives the upstream's `200` status, its response body byte-for-byte and its content type.
- [x] A `POST` with a JSON body reaches the test upstream with the body bytes unchanged and a `Content-Length` equal to the body's actual length.
- [x] With `path_suffix_mode: append`, a request to `/oagw/v1/proxy/{alias}/extra/segments` reaches the upstream at `match.http.path` joined to the unmatched remainder with exactly one separator; the degenerate cases (empty remainder, and `match.http.path` equal to `/`) produce the documented paths.
- [x] With `path_suffix_mode: disabled`, a request that supplies a path suffix is rejected with `400`, the validation-error GTS `type` and `X-OAGW-Error-Source: gateway`, and the upstream records no request.
- [x] A query parameter listed in `match.http.query_allowlist` arrives at the upstream with its name, value and position preserved, while a request carrying a parameter absent from the allowlist is rejected with `400` and never reaches the upstream; a route with an empty allowlist rejects any parameter at all.
- [x] A request whose alias matches no upstream at any tenant in the caller's chain returns `404` with GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`; the same status and `type` are returned when an upstream resolves but no enabled route matches the method or path prefix.
- [x] A descendant tenant and its ancestor both declaring the same alias resolve to the descendant's upstream, and the ancestor's `sharing: enforce` configuration is still present in the effective configuration used for the request.
- [x] An upstream with `enabled: false`, and a descendant upstream shadowing an ancestor upstream with `enabled: false`, both return `503` with the link-unavailable GTS `type`, and the test upstream records no request in either case.
- [x] A multi-endpoint upstream whose alias was derived from a registrable common suffix returns `400` with GTS `type` `...cf.oagw.routing.missing_target_host.v1` when `X-OAGW-Target-Host` is absent, and the body's `valid_hosts` lists every configured endpoint host.
- [x] A request supplying `X-OAGW-Target-Host: host.example.com:8443` returns `400` with GTS `type` `...cf.oagw.routing.invalid_target_host.v1` and an `invalid_value` field equal to the rejected value.
- [x] A request supplying a well-formed `X-OAGW-Target-Host` value that matches no configured endpoint returns `400` with GTS `type` `...cf.oagw.routing.unknown_target_host.v1` and both `invalid_value` and `valid_hosts` fields.
- [x] A multi-endpoint upstream with an explicit alias distributes successive requests across its endpoints in declaration order when `X-OAGW-Target-Host` is absent, and sends every request to the named endpoint when it is present; a rejected target-host request does not shift the distribution of the accepted ones.
- [x] A single-endpoint upstream is reached with the header absent and with the header naming its sole endpoint, and neither case changes the target.
- [x] A request to a single-endpoint upstream supplying a well-formed `X-OAGW-Target-Host` value that does not match that upstream's sole endpoint host is rejected with `400` and GTS `type` `...cf.oagw.routing.unknown_target_host.v1`, carrying both `invalid_value` and `valid_hosts`, and the test upstream records no request — confirming the validate-and-reject resolution of ADR-0001's example-versus-matrix conflict rather than the header being ignored.
- [x] A request carrying `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding` and `Upgrade` reaches the upstream with none of those header names present, and `X-OAGW-Target-Host` is likewise absent from what the upstream receives.
- [x] The upstream observes `Host` (or `:authority` on HTTP/2) equal to the selected endpoint's authority even when the caller sent a different `Host` and even when `headers.request.set` names `Host`.
- [x] With `headers.request.passthrough: none` no inbound header other than the computed ones reaches the upstream; with `allowlist` only allowlisted names reach it; with `all` the surviving inbound headers reach it; and `remove`, `set`, `add` are observably applied in that order.
- [x] A successful proxied response carries `X-OAGW-Error-Source: upstream`, and every gateway-originated error response carries `X-OAGW-Error-Source: gateway` with `Content-Type: application/problem+json` and the documented GTS `type`.
- [x] An upstream returning `418` with a non-JSON body produces `418` at the caller with that body byte-for-byte, `X-OAGW-Error-Source: upstream`, and no problem-details envelope; the same holds for an upstream `500`.
- [x] A request whose body exceeds the 100 MB limit is rejected with `413` and the payload-too-large GTS `type`, and the test upstream records no request; a malformed `Content-Length`, a `Content-Length` disagreeing with the delivered bytes, a `Transfer-Encoding` other than `chunked`, and both headers present together are each rejected with `400`.
- [x] An upstream that delays its response beyond `proxy_timeout_secs` (2 seconds) produces `504` with a timeout GTS `type` and `X-OAGW-Error-Source: gateway` within a bound close to the configured deadline, and the client request is sent to the upstream exactly once.
- [x] An endpoint that refuses the connection produces `503` with the link-unavailable GTS `type`, and no second connection attempt at the client-request level is made.
- [x] With `allow_http_upstream: true`, a request to an upstream whose endpoint scheme is `http` reaches the upstream over a plaintext connection and returns its response; with the flag set to `false`, the same request returns `502` with the protocol-error GTS `type` and the upstream records no connection.
- [x] Every proxy request, successful or failed, emits exactly one structured JSON audit record containing `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size` and `error_type`, with `error_type` null on success; no record contains a request or response body, a query string or a credential value.
- [x] The `trace_id` in a gateway problem-details body equals the `request_id` in the audit record for the same request.
- [x] The base metrics `oagw_requests_total`, `oagw_request_duration_seconds`, `oagw_requests_in_flight` and `oagw_errors_total` are updated for a proxied request with `http.route` carrying the normalized route pattern rather than the raw request path, and no metric carries a tenant label.
- [x] Updating an upstream or route through the management API and flushing the configuration caches changes the next proxy request's behaviour without a gear restart, confirming the no-TTL, explicit-invalidation read path.
- [x] The named extension points are present and inert: with no CORS, rate-limit or plugin configuration, and with a non-streaming upstream, a proxied request behaves exactly as the success criteria above describe, and no rate-limit, plugin or streaming behaviour is observable.
