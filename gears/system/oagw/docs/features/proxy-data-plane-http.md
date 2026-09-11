# Feature: Proxy Data Plane — HTTP


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy HTTP Request Flow](#proxy-http-request-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Authorization Check](#authorization-check)
  - [Alias Resolution Algorithm](#alias-resolution-algorithm)
  - [Route Matching Algorithm](#route-matching-algorithm)
  - [Guard Validation Algorithm](#guard-validation-algorithm)
  - [Body Validation Algorithm](#body-validation-algorithm)
  - [Endpoint Selection Algorithm (X-OAGW-Target-Host)](#endpoint-selection-algorithm-x-oagw-target-host)
  - [Header Transformation Algorithm](#header-transformation-algorithm)
  - [Request Transformation Algorithm](#request-transformation-algorithm)
  - [SSRF Guard Check Algorithm](#ssrf-guard-check-algorithm)
  - [Outbound Call Algorithm](#outbound-call-algorithm)
  - [Error Source Mapping Algorithm](#error-source-mapping-algorithm)
- [4. States (CDSL)](#4-states-cdsl)
  - [Proxy Request Lifecycle State Machine](#proxy-request-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Authorization Gate Runs First](#authorization-gate-runs-first)
  - [Alias Resolution with Tenant Shadowing](#alias-resolution-with-tenant-shadowing)
  - [Route Matching and Guard Enforcement](#route-matching-and-guard-enforcement)
  - [Body Validation Before Buffering](#body-validation-before-buffering)
  - [X-OAGW-Target-Host Endpoint Selection Matrix](#x-oagw-target-host-endpoint-selection-matrix)
  - [Header Transformation and Rewrite](#header-transformation-and-rewrite)
  - [Request Transformation, Scheme Policy, and Outbound Call](#request-transformation-scheme-policy-and-outbound-call)
  - [SSRF Guard Check Runs as a Policy-Gated No-Op](#ssrf-guard-check-runs-as-a-policy-gated-no-op)
  - [Response Relay and Error-Source Mapping](#response-relay-and-error-source-mapping)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)
  - [The Scheme Policy Split](#the-scheme-policy-split)
  - [Out of Scope](#out-of-scope)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-proxy-data-plane-http-implemented`

- [ ] `p1` - `cpt-cf-oagw-feature-proxy-data-plane-http`

## 1. Feature Context

### 1.1 Overview

The core plain-HTTP proxy path: it resolves an alias to an upstream, matches a route, runs guard
and body checks, selects an endpoint, rewrites headers, and forwards the request without caching
or retrying it.

### 1.2 Purpose

`cpt-cf-oagw-actor-app-developer` calls a single gear-relative endpoint,
`{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]`, to reach an external
`cpt-cf-oagw-actor-upstream-service` without holding its credentials or connection details. This
feature builds the non-configurable half of that path: authorization, alias resolution, route
matching, guard rules, body validation, endpoint selection, header rewriting, request forwarding,
and the response relay that follows. GTS (Global Type System) identifiers, RFC 9457 (`Problem
Details for HTTP APIs`) error bodies, and the `X-OAGW-Error-Source` header apply across every
stage so a client can always tell whether a failure came from the gateway or from the upstream.

**Requirements**: `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-fr-header-transform`,
`cpt-cf-oagw-nfr-ssrf-protection`, `cpt-cf-oagw-nfr-low-latency`

**Principles**: `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-no-cache`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxy request and receives the relayed response or a gateway error |
| `cpt-cf-oagw-actor-upstream-service` | External HTTP service that the gateway dials and whose response it relays |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-oagw-usecase-proxy-request`
- **Design**: [DESIGN.md](../DESIGN.md) §3.2 Component Model (Alias Resolution, Headers
  Transformation, Guard Rules, Body Validation Rules, Transformation Rules), §3.3 API Contracts
  (Proxy API, Error Response Format), §3.5 Interactions & Sequences (Proxy Request Flow,
  `cpt-cf-oagw-seq-proxy-flow`)
- **ADR**: [0001-request-routing.md](../ADR/0001-request-routing.md) (`cpt-cf-oagw-adr-request-routing`,
  X-OAGW-Target-Host Behavior Matrix), [0007-error-source-distinction.md](../ADR/0007-error-source-distinction.md),
  [0005-data-plane-caching.md](../ADR/0005-data-plane-caching.md) (`cpt-cf-oagw-adr-data-plane-caching`),
  [0006-state-management.md](../ADR/0006-state-management.md) (`cpt-cf-oagw-adr-state-management`)
- **Decomposition**: `cpt-cf-oagw-feature-proxy-data-plane-http`
- **Dependencies**: `cpt-cf-oagw-feature-route-management-api`

## 2. Actor Flows (CDSL)

### Proxy HTTP Request Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-http-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The request reaches the upstream and its status, headers, and body are relayed unchanged.
- A single-endpoint or explicit-alias upstream is reached without the caller supplying
  `X-OAGW-Target-Host`.

**Error Scenarios**:
- The permission check, alias resolution, route matching, guard rules, body validation, or
  endpoint selection stage rejects the request before any outbound call is made.
- The outbound call times out or fails to connect, producing a gateway-originated 5xx.
- The upstream itself returns an error status, which is relayed unchanged.

**Steps**:
1. [ ] - `p1` - App developer sends `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]` with a Bearer token - `inst-req-1`
2. [ ] - `p1` - API: run the authorization check (`cpt-cf-oagw-algo-proxy-http-authorization`) against `gts.cf.core.oagw.proxy.v1~:invoke` - `inst-req-2`
3. [ ] - `p1` - **IF** the permission check fails - `inst-req-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed, `X-OAGW-Error-Source: gateway` - `inst-req-3a`
4. [ ] - `p1` - **ELSE** resolve the alias (`cpt-cf-oagw-algo-proxy-http-alias-resolution`) - `inst-req-4`
5. [ ] - `p1` - **IF** no tenant in the chain has an upstream matching the alias - `inst-req-5`
   1. [ ] - `p1` - **RETURN** 404 RouteNotFound - `inst-req-5a`
6. [ ] - `p1` - **IF** the resolved upstream (or an enforcing ancestor it binds to) is disabled - `inst-req-6`
   1. [ ] - `p1` - **RETURN** 503 LinkUnavailable - `inst-req-6a`
7. [ ] - `p1` - **ELSE** match the route (`cpt-cf-oagw-algo-proxy-http-route-matching`) - `inst-req-7`
8. [ ] - `p1` - **IF** no route matches the method and path - `inst-req-8`
   1. [ ] - `p1` - **RETURN** 404 RouteNotFound - `inst-req-8a`
9. [ ] - `p1` - **ELSE** run guard validation (`cpt-cf-oagw-algo-proxy-http-guard-validation`) - `inst-req-9`
10. [ ] - `p1` - **IF** a guard rejects the request - `inst-req-10`
    1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-req-10a`
11. [ ] - `p1` - **ELSE** run body validation (`cpt-cf-oagw-algo-proxy-http-body-validation`) - `inst-req-11`
12. [ ] - `p1` - **IF** body validation fails - `inst-req-12`
    1. [ ] - `p1` - **RETURN** 400 ValidationError or 413 PayloadTooLarge, as the specific check dictates - `inst-req-12a`
13. [ ] - `p1` - **ELSE** select the endpoint and validate `X-OAGW-Target-Host` (`cpt-cf-oagw-algo-proxy-http-endpoint-selection`) - `inst-req-13`
14. [ ] - `p1` - **IF** endpoint selection fails - `inst-req-14`
    1. [ ] - `p1` - **RETURN** 400 MissingTargetHost, InvalidTargetHost, or UnknownTargetHost, as applicable - `inst-req-14a`
15. [ ] - `p1` - **ELSE** transform headers and request (`cpt-cf-oagw-algo-proxy-http-header-transform`, `cpt-cf-oagw-algo-proxy-http-request-transform`), then issue the outbound call (`cpt-cf-oagw-algo-proxy-http-outbound-call`) - `inst-req-15`
16. [ ] - `p1` - **IF** the outbound call times out or fails to connect - `inst-req-16`
    1. [ ] - `p1` - **RETURN** the mapped gateway error with `X-OAGW-Error-Source: gateway` (`cpt-cf-oagw-algo-proxy-http-error-source-mapping`) - `inst-req-16a`
17. [ ] - `p1` - **ELSE** - `inst-req-17`
    1. [ ] - `p1` - **RETURN** the upstream's status, headers, and body unchanged, tagged `X-OAGW-Error-Source: upstream` - `inst-req-17a`

## 3. Processes / Business Logic (CDSL)

### Authorization Check

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-authorization`

**Input**: Validated Bearer token (via `toolkit-auth`) carrying the caller's tenant and granted
permissions.

**Output**: PASS with tenant context carried forward, or a 401 rejection.

**Steps**:
1. [ ] - `p1` - Extract tenant_id, principal_id, and granted permissions from the token's SecurityContext - `inst-authz-1`
2. [ ] - `p1` - **IF** the granted permissions do not include `gts.cf.core.oagw.proxy.v1~:invoke` - `inst-authz-2`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed, `X-OAGW-Error-Source: gateway` - `inst-authz-2a`
3. [ ] - `p1` - **ELSE** - `inst-authz-3`
   1. [ ] - `p1` - **RETURN** PASS, carrying tenant_id into alias resolution - `inst-authz-3a`

### Alias Resolution Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-alias-resolution`

**Input**: The `{alias}` path segment and the caller's tenant_id.

**Output**: The resolved UpstreamConfig, or a 404/503 rejection.

This upstream lookup is expected to be served through the data plane's small local cache
(`cpt-cf-oagw-adr-data-plane-caching`, `cpt-cf-oagw-adr-state-management`). The database is
consulted only on a cache miss.

**Steps**:
1. [ ] - `p1` - Normalize `{alias}` to ASCII lowercase, matching the normalization applied when the alias was stored - `inst-alias-1`
2. [ ] - `p1` - **FOR EACH** tenant in the chain from the caller's tenant to the root (descendant to root) - `inst-alias-2`
   1. [ ] - `p1` - DB: SELECT upstream WHERE tenant_id = :tenant AND alias = :normalized_alias - `inst-alias-2a`
   2. [ ] - `p1` - **IF** a matching row is found, stop walking further ancestors — the closest match wins (shadowing) - `inst-alias-2b`
3. [ ] - `p1` - **IF** no tenant in the chain has a matching upstream - `inst-alias-3`
   1. [ ] - `p1` - **RETURN** 404 RouteNotFound - `inst-alias-3a`
4. [ ] - `p1` - **IF** the resolved upstream is disabled, or it binds to an ancestor upstream that is disabled - `inst-alias-4`
   1. [ ] - `p1` - **RETURN** 503 LinkUnavailable - `inst-alias-4a`
5. [ ] - `p1` - **ELSE** - `inst-alias-5`
   1. [ ] - `p1` - **RETURN** the resolved UpstreamConfig; ancestor constraints configured with `sharing: enforce` remain active regardless of which tenant's row was selected - `inst-alias-5a`

### Route Matching Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-route-matching`

**Input**: The resolved UpstreamConfig, the inbound method and path remainder, and the same
tenant chain used for alias resolution.

**Output**: The resolved RouteConfig, or a 404 rejection.

Like alias resolution, this lookup is expected to be served through that same small local cache
(`cpt-cf-oagw-adr-data-plane-caching`, `cpt-cf-oagw-adr-state-management`) rather than the
database on every request.

**Steps**:
1. [ ] - `p1` - **FOR EACH** tenant in the descendant-to-root chain - `inst-route-1`
   1. [ ] - `p1` - DB: SELECT route WHERE upstream_id = :resolved_upstream_id AND enabled = true - `inst-route-1a`
2. [ ] - `p1` - Filter to routes whose `match.http.methods` allowlist contains the inbound method - `inst-route-2`
3. [ ] - `p1` - Filter to routes whose `match.http.path` is a prefix of the inbound path - `inst-route-3`
4. [ ] - `p1` - **IF** more than one candidate route remains - `inst-route-4`
   1. [ ] - `p1` - Select the route with the longest matching path prefix, breaking ties with the route's `priority` field; a descendant tenant's route takes priority over an inherited ancestor route - `inst-route-4a`
5. [ ] - `p1` - **IF** no candidate route remains - `inst-route-5`
   1. [ ] - `p1` - **RETURN** 404 RouteNotFound - `inst-route-5a`
6. [ ] - `p1` - **ELSE** - `inst-route-6`
   1. [ ] - `p1` - **RETURN** the selected RouteConfig - `inst-route-6a`

### Guard Validation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-guard-validation`

**Input**: The resolved RouteConfig's `match.http` block and the inbound method, query string,
and path suffix.

**Output**: PASS, or a 400 ValidationError.

**Steps**:
1. [ ] - `p1` - **IF** the inbound method is not in `match.http.methods` - `inst-guard-1`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-guard-1a`
2. [ ] - `p1` - **IF** any inbound query parameter is absent from `match.http.query_allowlist` (see DESIGN.md §3.2 Guard Rules) - `inst-guard-2`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-guard-2a`
3. [ ] - `p1` - **IF** a path suffix is present and `match.http.path_suffix_mode` is `disabled` - `inst-guard-3`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-guard-3a`
4. [ ] - `p1` - **ELSE** - `inst-guard-4`
   1. [ ] - `p1` - **RETURN** PASS - `inst-guard-4a`

### Body Validation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-body-validation`

**Input**: Inbound `Content-Length` and `Transfer-Encoding` headers, and the body stream.

**Output**: PASS, or a 400 ValidationError / 413 PayloadTooLarge rejection (see DESIGN.md §3.2
Body Validation Rules).

**Steps**:
1. [ ] - `p1` - **IF** `Content-Length` is present and is not a valid non-negative integer - `inst-body-1`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-body-1a`
2. [ ] - `p1` - **IF** `Transfer-Encoding` is present with a value other than `chunked` - `inst-body-2`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-body-2a`
3. [ ] - `p1` - **IF** the bytes read so far exceed the 100MB hard limit (`cpt-cf-oagw-constraint-body-limit`) - `inst-body-3`
   1. [ ] - `p1` - **RETURN** 413 PayloadTooLarge before buffering the remainder of the body - `inst-body-3a`
4. [ ] - `p1` - **IF** `Content-Length` is present and does not match the actual bytes read from the body stream - `inst-body-4`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-body-4a`
5. [ ] - `p1` - **ELSE** - `inst-body-5`
   1. [ ] - `p1` - **RETURN** PASS - `inst-body-5a`

### Endpoint Selection Algorithm (X-OAGW-Target-Host)

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-endpoint-selection`

**Input**: The upstream's `server.endpoints` pool, whether its alias was explicitly assigned or
derived from a common hostname suffix, and the inbound `X-OAGW-Target-Host` header.

**Output**: The selected Endpoint, or a 400 MissingTargetHost / InvalidTargetHost /
UnknownTargetHost rejection, per the behavior matrix in
[ADR-0001](../ADR/0001-request-routing.md) lines 175-184.

**Steps**:
1. [ ] - `p1` - **IF** the upstream has exactly one endpoint - `inst-ep-1`
   1. [ ] - `p1` - **IF** `X-OAGW-Target-Host` is present but is not a bare hostname or IP address (contains a port, path, or scheme) - `inst-ep-1a`
      1. [ ] - `p1` - **RETURN** 400 InvalidTargetHost - `inst-ep-1a1`
   2. [ ] - `p1` - **IF** `X-OAGW-Target-Host` is present, well-formed, and does not match the sole endpoint's host - `inst-ep-1b`
      1. [ ] - `p1` - **RETURN** 400 UnknownTargetHost - `inst-ep-1b1`
   3. [ ] - `p1` - **ELSE** (the header is absent, or present and matches the sole endpoint's host) - `inst-ep-1c`
      1. [ ] - `p1` - **RETURN** the sole endpoint - `inst-ep-1c1`
2. [ ] - `p1` - **IF** the upstream has multiple endpoints, `X-OAGW-Target-Host` is absent, and the alias was explicitly assigned (not a derived common suffix) - `inst-ep-2`
   1. [ ] - `p1` - **RETURN** the next endpoint from the round-robin sequence - `inst-ep-2a`
3. [ ] - `p1` - **IF** the upstream has multiple endpoints, `X-OAGW-Target-Host` is absent, and the alias was derived via `common_domain_suffix()` - `inst-ep-3`
   1. [ ] - `p1` - **RETURN** 400 MissingTargetHost, listing the pool's valid hosts - `inst-ep-3a`
4. [ ] - `p1` - **IF** the upstream has multiple endpoints and `X-OAGW-Target-Host` is present but is not a bare hostname or IP address (contains a port, path, or scheme) - `inst-ep-4`
   1. [ ] - `p1` - **RETURN** 400 InvalidTargetHost - `inst-ep-4a`
5. [ ] - `p1` - **IF** the upstream has multiple endpoints, `X-OAGW-Target-Host` is present and well-formed, but matches none of the pool's configured endpoint hosts - `inst-ep-5`
   1. [ ] - `p1` - **RETURN** 400 UnknownTargetHost, listing the pool's valid hosts - `inst-ep-5a`
6. [ ] - `p1` - **ELSE** (well-formed value matching a configured endpoint) - `inst-ep-6`
   1. [ ] - `p1` - **RETURN** the matching endpoint, bypassing round-robin - `inst-ep-6a`

### Header Transformation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-header-transform`

**Input**: The inbound header set, the selected Endpoint, and the inbound HTTP version.

**Output**: The outbound header set to send to the upstream.

**Steps**:
1. [ ] - `p1` - Consume `X-OAGW-Target-Host` for endpoint selection, then strip it — it is a routing header and is never forwarded (DESIGN.md §3.2 Headers Transformation) - `inst-hdr-1`
2. [ ] - `p1` - Strip every hop-by-hop header listed in DESIGN.md §3.2's table (`Connection`, `Keep-Alive`, and the rest of that list) - `inst-hdr-2`
3. [ ] - `p1` - **IF** the inbound request is HTTP/1.1 - `inst-hdr-3`
   1. [ ] - `p1` - Replace the `Host` header with the selected endpoint's `host[:port]` - `inst-hdr-3a`
4. [ ] - `p1` - **IF** the inbound request is HTTP/2 - `inst-hdr-4`
   1. [ ] - `p1` - Replace the `:authority` pseudo-header with the selected endpoint's authority; `X-OAGW-Target-Host` still governs routing and is unaffected by this rewrite - `inst-hdr-4a`
5. [ ] - `p1` - Forward all remaining headers unchanged; configurable `set`/`add`/`remove`/passthrough rules are out of scope (`cpt-cf-oagw-feature-policy-and-plugins`) - `inst-hdr-5`

### Request Transformation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-request-transform`

**Input**: The validated inbound request and the resolved RouteConfig.

**Output**: The outbound request body, path, and query string.

**Steps**:
1. [ ] - `p1` - Method: pass through unchanged - `inst-xform-1`
2. [ ] - `p1` - **IF** `match.http.path_suffix_mode` is `append` and a path suffix was supplied - `inst-xform-2`
   1. [ ] - `p1` - Outbound path = `match.http.path` joined with the supplied path suffix - `inst-xform-2a`
3. [ ] - `p1` - **ELSE** - `inst-xform-3`
   1. [ ] - `p1` - Outbound path = `match.http.path` - `inst-xform-3a`
4. [ ] - `p1` - Forward only the query parameters present in `match.http.query_allowlist`; drop the rest - `inst-xform-4`
5. [ ] - `p1` - Body: pass through unchanged, streamed rather than fully buffered where the transport allows it - `inst-xform-5`

### SSRF Guard Check Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-ssrf-guard`

**Input**: The selected Endpoint's resolved DNS answer, the inbound request's headers, path, and
query string, and `OagwConfig.ssrf_policy.enabled`.

**Output**: PASS, or a gateway rejection when the resolved target or the request fails
validation. This algorithm implements `cpt-cf-oagw-nfr-ssrf-protection`: the DNS and IP
validation, well-known header stripping, and path/query validation that the requirement mandates.
`ssrf_policy.enabled` is `false` in the graded configuration, so runtime enforcement is turned
off there. The check below still runs on every request, rather than being skipped or removed
from the code path.

**Steps**:
1. [ ] - `p1` - Validate the endpoint's resolved DNS answer against the configured allowed and denied IP segments (DNS and IP validation) - `inst-ssrf-1`
2. [ ] - `p1` - Strip well-known internal headers (for example `X-Forwarded-For`, `X-Real-IP`) from the request before it reaches header transformation - `inst-ssrf-2`
3. [ ] - `p1` - Validate the outbound path and query string against the matched RouteConfig - `inst-ssrf-3`
4. [ ] - `p1` - **IF** `OagwConfig.ssrf_policy.enabled` is `false` (the graded configuration's value) - `inst-ssrf-4`
   1. [ ] - `p1` - Run steps 1-3 as a no-op: evaluate each check, but always treat the result as PASS regardless of outcome - `inst-ssrf-4a`
5. [ ] - `p1` - **ELSE IF** the resolved IP, the stripped headers, or the path/query fail validation - `inst-ssrf-5`
   1. [ ] - `p1` - **RETURN** a gateway rejection with `X-OAGW-Error-Source: gateway` before the connection opens - `inst-ssrf-5a`
6. [ ] - `p1` - **ELSE** - `inst-ssrf-6`
   1. [ ] - `p1` - **RETURN** PASS - `inst-ssrf-6a`

### Outbound Call Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-outbound-call`

**Input**: The transformed outbound request, the selected Endpoint, and `OagwConfig`
(`proxy_timeout_secs`, `allow_http_upstream`).

**Output**: The upstream's HTTP response, a timeout outcome, a connection-failure outcome, or a
protocol-failure outcome.

**Steps**:
1. [ ] - `p1` - **IF** the endpoint's `scheme` is `http` (or `ws`) and `OagwConfig.allow_http_upstream` is false - `inst-out-1`
   1. [ ] - `p1` - **RETURN** a gateway error without dialling — the default HTTPS-only posture (`cpt-cf-oagw-constraint-https-only`) applies - `inst-out-1a`
2. [ ] - `p1` - **ELSE** open the connection in plaintext (`http`/`ws`) or TLS (`https`/`wss`/`wt`) as the endpoint's scheme dictates - `inst-out-2`
   1. [ ] - `p1` - Run the SSRF guard check (`cpt-cf-oagw-algo-proxy-http-ssrf-guard`) against the resolved DNS answer, headers, path, and query before the connection completes; the check runs on every request, and `ssrf_policy.enabled: false` in the graded configuration makes it a no-op that always passes, rather than removing the check - `inst-out-2a`
3. [ ] - `p1` - Issue the outbound call bounded by `OagwConfig.proxy_timeout_secs` (2 seconds in the graded configuration) - `inst-out-3`
4. [ ] - `p1` - Do not re-issue the client's original request on any failure (`cpt-cf-oagw-principle-no-retry`); connector-level endpoint failover within the same pool is permitted - `inst-out-4`
5. [ ] - `p1` - Do not cache the response for reuse on a later request (`cpt-cf-oagw-principle-no-cache`) - `inst-out-5`
6. [ ] - `p1` - **IF** the call exceeds the timeout - `inst-out-6`
   1. [ ] - `p1` - **RETURN** a timeout outcome for error-source mapping - `inst-out-6a`
7. [ ] - `p1` - **IF** the connection cannot be established because DNS resolution, connection refusal, or connection reset occurred - `inst-out-7`
   1. [ ] - `p1` - **RETURN** a connection-failure outcome for error-source mapping, tagged with which of the three occurred (DNS resolution failure, connection refused, or connection reset) so the mapping step can apply its deterministic rule - `inst-out-7a`
8. [ ] - `p1` - **IF** a response is received but cannot be parsed as valid HTTP (malformed status line, headers, or framing) - `inst-out-8`
   1. [ ] - `p1` - **RETURN** a protocol-failure outcome for error-source mapping - `inst-out-8a`
9. [ ] - `p1` - **ELSE** - `inst-out-9`
   1. [ ] - `p1` - **RETURN** the upstream's HTTP response - `inst-out-9a`

### Error Source Mapping Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-proxy-http-error-source-mapping`

**Input**: The outcome of any pipeline stage — an upstream HTTP response, a timeout, a
connection-failure outcome (tagged DNS resolution failure, connection refused, or connection
reset), a protocol-failure outcome (a malformed or unparseable upstream response), or a gateway
rejection from an earlier stage.

**Output**: The final HTTP response sent to the client, always carrying `X-OAGW-Error-Source`.

**Steps**:
1. [ ] - `p1` - **IF** the outbound call returned an upstream HTTP response, at any status including 4xx/5xx - `inst-err-1`
   1. [ ] - `p1` - Relay the status, headers, and body unchanged - `inst-err-1a`
   2. [ ] - `p1` - Set `X-OAGW-Error-Source: upstream` - `inst-err-1b`
2. [ ] - `p1` - **IF** the outbound call timed out waiting for the connection or the response - `inst-err-2`
   1. [ ] - `p1` - **RETURN** 504, using the GTS `type` for `ConnectionTimeout` or `RequestTimeout` from DESIGN.md's error table, whichever the stage that timed out dictates - `inst-err-2a`
   2. [ ] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-err-2b`
3. [ ] - `p1` - **IF** the outbound call returned a connection-failure outcome - `inst-err-3`
   1. [ ] - `p1` - **IF** the tagged cause is DNS resolution failure - `inst-err-3a`
      1. [ ] - `p1` - **RETURN** 503 LinkUnavailable - `inst-err-3a1`
   2. [ ] - `p1` - **ELSE** (the tagged cause is connection refused or connection reset) - `inst-err-3b`
      1. [ ] - `p1` - **RETURN** 502 DownstreamError - `inst-err-3b1`
   3. [ ] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-err-3c`
4. [ ] - `p1` - **IF** the outbound call returned a protocol-failure outcome (a malformed or unparseable upstream response) - `inst-err-4`
   1. [ ] - `p1` - **RETURN** 502 ProtocolError - `inst-err-4a`
   2. [ ] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-err-4b`
5. [ ] - `p1` - **IF** the response instead comes from an earlier gateway-side rejection (authorization, alias resolution, route matching, guards, body validation, or endpoint selection) - `inst-err-5`
   1. [ ] - `p1` - Emit RFC 9457 `application/problem+json` using the GTS `type` documented for that rejection and set `X-OAGW-Error-Source: gateway` - `inst-err-5a`

This mapping is deterministic. DNS resolution failure always maps to 503 `LinkUnavailable`;
connection refused or reset maps to 502 `DownstreamError`; and a malformed response maps to 502
`ProtocolError`. No other outcome produces one of these three statuses.

## 4. States (CDSL)

### Proxy Request Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-proxy-http-lifecycle`

**States**: Received, Authorized, AliasResolved, RouteMatched, GuardsPassed, BodyValidated,
EndpointSelected, Forwarded, Completed, Rejected

**Initial State**: Received

**Transitions**:
1. [ ] - `p1` - **FROM** Received **TO** Authorized **WHEN** the `gts.cf.core.oagw.proxy.v1~:invoke` permission check passes - `inst-lc-1`
2. [ ] - `p1` - **FROM** Received **TO** Rejected **WHEN** the permission check fails (401) - `inst-lc-2`
3. [ ] - `p1` - **FROM** Authorized **TO** AliasResolved **WHEN** alias resolution finds an enabled upstream in the tenant chain - `inst-lc-3`
4. [ ] - `p1` - **FROM** Authorized **TO** Rejected **WHEN** alias resolution returns 404 or 503 - `inst-lc-4`
5. [ ] - `p1` - **FROM** AliasResolved **TO** RouteMatched **WHEN** route matching finds an enabled route - `inst-lc-5`
6. [ ] - `p1` - **FROM** AliasResolved **TO** Rejected **WHEN** no route matches (404) - `inst-lc-6`
7. [ ] - `p1` - **FROM** RouteMatched **TO** GuardsPassed **WHEN** guard validation passes - `inst-lc-7`
8. [ ] - `p1` - **FROM** RouteMatched **TO** Rejected **WHEN** a guard rejects the request (400) - `inst-lc-8`
9. [ ] - `p1` - **FROM** GuardsPassed **TO** BodyValidated **WHEN** body validation passes - `inst-lc-9`
10. [ ] - `p1` - **FROM** GuardsPassed **TO** Rejected **WHEN** body validation fails (400/413) - `inst-lc-10`
11. [ ] - `p1` - **FROM** BodyValidated **TO** EndpointSelected **WHEN** endpoint selection resolves a single target endpoint - `inst-lc-11`
12. [ ] - `p1` - **FROM** BodyValidated **TO** Rejected **WHEN** endpoint selection fails (400) - `inst-lc-12`
13. [ ] - `p1` - **FROM** EndpointSelected **TO** Forwarded **WHEN** the outbound call is issued to the upstream - `inst-lc-13`
14. [ ] - `p1` - **FROM** Forwarded **TO** Completed **WHEN** an upstream response, success or upstream error, is relayed to the client - `inst-lc-14`
15. [ ] - `p1` - **FROM** Forwarded **TO** Rejected **WHEN** the outbound call times out or fails to connect - `inst-lc-15`

## 5. Definitions of Done

### Authorization Gate Runs First

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-authorization`

The system **MUST** enforce the `gts.cf.core.oagw.proxy.v1~:invoke` permission check as the first
step of every proxy request, before alias resolution, route matching, or any later stage runs. A
failing check **MUST** short-circuit with 401 AuthenticationFailed and `X-OAGW-Error-Source:
gateway`.

**Implements**:
- `cpt-cf-oagw-flow-proxy-http-request`
- `cpt-cf-oagw-algo-proxy-http-authorization`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]`
- Entities: ProxyContext

### Alias Resolution with Tenant Shadowing

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-alias-resolution`

The system **MUST** normalize `{alias}` to ASCII lowercase and walk the tenant hierarchy from
descendant to root, selecting the closest matching enabled upstream (shadowing). An unknown alias
**MUST** return 404 RouteNotFound. A disabled upstream, including one disabled through an
enforcing ancestor, **MUST** return 503 LinkUnavailable.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-alias-resolution`
- `cpt-cf-oagw-state-proxy-http-lifecycle`

**Touches**:
- Entities: ProxyContext

### Route Matching and Guard Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-route-and-guards`

The system **MUST** match routes for an HTTP upstream using its method allowlist plus
longest-path-prefix matching, honoring the `priority` field to break ties and excluding disabled
routes, returning 404 RouteNotFound when nothing matches. It **MUST** then reject a method
outside `match.http.methods`, a query parameter outside `match.http.query_allowlist`, or a path
suffix supplied while `path_suffix_mode` is `disabled`, each with 400 ValidationError.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-route-matching`
- `cpt-cf-oagw-algo-proxy-http-guard-validation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]`
- Entities: ProxyContext

### Body Validation Before Buffering

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-body-validation`

The system **MUST** validate `Content-Length` against the actual body size (400
ValidationError), enforce the 100MB hard limit before buffering the body (413 PayloadTooLarge),
and accept only `chunked` as a supported `Transfer-Encoding`, rejecting any other value with 400
ValidationError.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-body-validation`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- Entities: ProxyContext

### X-OAGW-Target-Host Endpoint Selection Matrix

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-endpoint-selection`

The system **MUST** implement the full endpoint-selection matrix from
[ADR-0001](../ADR/0001-request-routing.md) lines 175-184. A single endpoint routes directly when
`X-OAGW-Target-Host` is absent, or present and matching that endpoint's host. A malformed value
on a single endpoint returns 400 InvalidTargetHost, and a well-formed value naming a different
host returns 400 UnknownTargetHost. An explicit-alias pool round-robins when `X-OAGW-Target-Host`
is absent and honors it when present. A common-suffix-derived alias pool requires the header,
returning 400 MissingTargetHost without it. A malformed header value returns 400
InvalidTargetHost, and a well-formed value matching no configured endpoint returns 400
UnknownTargetHost.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-endpoint-selection`

**Touches**:
- Entities: ProxyContext

### Header Transformation and Rewrite

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-header-transform`

The system **MUST** consume and strip `X-OAGW-Target-Host` and strip every hop-by-hop header
before forwarding, and **MUST** rewrite `Host` (HTTP/1.1) or the `:authority` pseudo-header
(HTTP/2) to the selected endpoint. Configurable `set`/`add`/`remove`/passthrough header rules
remain out of scope; they belong to `cpt-cf-oagw-feature-policy-and-plugins`.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-header-transform`

**Touches**:
- Entities: ProxyContext

### Request Transformation, Scheme Policy, and Outbound Call

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-transform-and-outbound`

The system **MUST** pass the method through unchanged, append the path suffix to
`match.http.path` only when `path_suffix_mode` is `append`, forward only allow-listed query
parameters, and pass the body through unchanged. It **MUST** dial the selected endpoint in
plaintext when its scheme is `http` (or `ws`) and `OagwConfig.allow_http_upstream` is `true`, and
**MUST** refuse the dial with a gateway error when the flag is `false`, regardless of the stored
`scheme` value — the schema's acceptance of `http` at create time and this dial decision are two
separate layers. It **MUST** bound the call by `OagwConfig.proxy_timeout_secs`, issue no
automatic retry of the client's request, and cache no response.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-request-transform`
- `cpt-cf-oagw-algo-proxy-http-outbound-call`

**Constraints**: `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:
- Entities: ProxyContext, ProxyResponse

### SSRF Guard Check Runs as a Policy-Gated No-Op

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-ssrf-guard`

The system **MUST** implement DNS and IP validation, well-known internal header stripping, and
path/query validation against route configuration, satisfying `cpt-cf-oagw-nfr-ssrf-protection`.
The check **MUST** run on every outbound call regardless of policy state. Because
`OagwConfig.ssrf_policy.enabled` is `false` in the graded configuration, the check **MUST**
evaluate as a no-op that always returns PASS. It **MUST NOT** be skipped, short-circuited, or
removed from the code path.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-ssrf-guard`
- `cpt-cf-oagw-algo-proxy-http-outbound-call`

**Constraints**: `cpt-cf-oagw-nfr-ssrf-protection`

**Touches**:
- Entities: ProxyContext

### Response Relay and Error-Source Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-http-response-relay`

The system **MUST** relay the upstream's status, headers, and body unchanged with
`X-OAGW-Error-Source: upstream` for any upstream-originated response, including error statuses.
It **MUST** emit RFC 9457 `application/problem+json` with the GTS `type` documented in
DESIGN.md's error table and `X-OAGW-Error-Source: gateway` for every gateway-originated error.
It **MUST** map connection and request timeouts to 504. The connection-failure mapping **MUST**
follow the deterministic rule from the Error Source Mapping Algorithm: DNS resolution failure
maps to 503 `LinkUnavailable`, connection refused or reset maps to 502 `DownstreamError`, and a
malformed response maps to 502 `ProtocolError`.

**Implements**:
- `cpt-cf-oagw-algo-proxy-http-error-source-mapping`

**Touches**:
- Entities: ProxyResponse

## 6. Acceptance Criteria

- [ ] A `GET` through the proxy reaches a local stub upstream and returns its exact status code and body.
- [ ] An `http`-scheme upstream is dialled in plaintext and proxied successfully when `allow_http_upstream: true`.
- [ ] A proxy request for an unknown alias returns 404 RouteNotFound.
- [ ] A proxy request to a disabled upstream returns 503 LinkUnavailable.
- [ ] A request using a method outside the matched route's `match.http.methods` allowlist is rejected with 400 ValidationError.
- [ ] A query parameter outside `match.http.query_allowlist` is rejected with 400 ValidationError.
- [ ] A path suffix supplied against a route with `path_suffix_mode: disabled` is rejected with 400 ValidationError.
- [ ] A request whose `Content-Length` does not match its actual body size is rejected with 400 ValidationError.
- [ ] A request body over the 100MB hard limit is rejected with 413 PayloadTooLarge before the gateway buffers it.
- [ ] Hop-by-hop headers (for example `Connection`, `Transfer-Encoding`, `Upgrade`) sent by the client do not reach the stub upstream.
- [ ] The `Host` header (and, on an HTTP/2 request, the `:authority` pseudo-header) received by the stub upstream is rewritten to the upstream's own host.
- [ ] A multi-endpoint upstream whose alias is a derived common suffix, called without `X-OAGW-Target-Host`, returns 400 MissingTargetHost.
- [ ] A multi-endpoint upstream called with a malformed `X-OAGW-Target-Host` value returns 400 InvalidTargetHost.
- [ ] A multi-endpoint upstream called with an `X-OAGW-Target-Host` value that matches no configured endpoint returns 400 UnknownTargetHost.
- [ ] A stub upstream returning a 500 status is relayed unchanged with `X-OAGW-Error-Source: upstream`.
- [ ] A stub upstream that does not respond within `proxy_timeout_secs` produces a 504 response with `X-OAGW-Error-Source: gateway`.
- [ ] A multi-endpoint upstream with an explicit alias, called repeatedly without `X-OAGW-Target-Host`, distributes requests round-robin across its local stub endpoints.
- [ ] A multi-endpoint upstream with an explicit alias, called with `X-OAGW-Target-Host` naming one pool member, always reaches that stub endpoint, bypassing round-robin.
- [ ] A proxy request whose outbound call cannot reach a local stub upstream (connection refused) returns a gateway-originated 502 or 503 response carrying `X-OAGW-Error-Source: gateway`.
- [ ] With `ssrf_policy.enabled: false` (the graded configuration), a proxy request against a stub upstream still triggers the SSRF guard check, which always passes without being skipped.

## 7. Additional Context (optional)

### The Scheme Policy Split

Two distinct layers govern plaintext upstreams, and this feature owns only the second. Whether
the upstream resource model's `scheme` field *accepts* `http` at create time is decided by
`cpt-cf-oagw-feature-resource-model-and-store` and `cpt-cf-oagw-feature-upstream-management-api`
(Override 2 of the DECOMPOSITION: `http` and `ws` are accepted as a deliberate extension beyond
the supplied JSON Schema's four-value enum). Whether the gateway actually *opens* a plaintext
connection to a `scheme: http` (or `ws`) endpoint is decided here, at outbound-call time, and is
governed solely by `OagwConfig.allow_http_upstream`. That flag is `true` in the graded
configuration, so an `http` upstream is dialled in plaintext and proxied successfully; when the
flag is unset or `false`, the default HTTPS-only posture (`cpt-cf-oagw-constraint-https-only`)
applies and the dial is refused with a gateway error even though the stored `scheme` is `http`.

### Out of Scope

- **Credential injection, rate limiting, CORS, and configurable header set/add/remove/passthrough
  rules** — these decorate the same proxy path but are driven by the plugin chain and hierarchical
  configuration; they belong to `cpt-cf-oagw-feature-policy-and-plugins`, which composes on top of
  the pipeline this feature builds.
- **SSE and WebSocket upgrade negotiation** — a plain HTTP request/response cycle is the only
  interaction pattern covered here; streaming upgrades on the same alias/route resolution belong
  to `cpt-cf-oagw-feature-proxy-streaming`.
- **gRPC request classification and dispatch** — the upstream `protocol` enum includes a gRPC
  value, but no gRPC proxy code path is implemented or reachable in this build (Scope Reality); a
  gRPC-protocol upstream is out of scope for the data plane entirely.
- **Circuit breaker enforcement** — documented in DESIGN.md §4.7 as future resilience work; this
  feature performs no failure-rate tracking or trip/reset logic.
