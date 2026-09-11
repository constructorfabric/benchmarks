# Feature: HTTP Request Proxying


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Non-Applicability and Deferrals](#15-non-applicability-and-deferrals)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy HTTP Request](#proxy-http-request)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Alias Resolution](#alias-resolution)
  - [Route Matching](#route-matching)
  - [Endpoint Selection and Target Host Handling](#endpoint-selection-and-target-host-handling)
  - [Header Transformation](#header-transformation)
  - [Body and Timeout Enforcement](#body-and-timeout-enforcement)
  - [Outbound Connection and Error-Source Labeling](#outbound-connection-and-error-source-labeling)
- [4. Definitions of Done](#4-definitions-of-done)
  - [Milestone 1 — Request Routing and Resolution](#milestone-1--request-routing-and-resolution)
  - [Milestone 2 — Request Execution and Error Handling](#milestone-2--request-execution-and-error-handling)
- [5. Acceptance Criteria](#5-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-ph-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-proxy-http`
## 1. Feature Context

### 1.1 Overview

This feature executes the data-plane path for a plain HTTP proxied request made to `{METHOD} /oagw/v1/proxy/{alias}` or `/oagw/v1/proxy/{alias}/{*path}`: it resolves the alias to an upstream, matches a route, selects an endpoint, enforces the configured scheme, body-size, and timeout limits, transforms headers in both directions, forwards the request, and labels every response it returns with its origin.

### 1.2 Purpose

This feature is the sole owner of the proxy data-plane request path described in the gear decomposition: it consumes the read-only Upstream and Route configuration resolved by upstream and route management and turns it into an actual outbound call, satisfying the gateway's core unified-proxy value proposition and its error-attribution contract toward callers.

`cpt-cf-oagw-nfr-low-latency` is satisfied by the streaming, non-buffering request/response path this feature already describes (section 3's body-and-timeout process forwards bytes as they arrive rather than buffering to completion on either leg); this configuration asserts no numeric latency budget beyond that design property — no p95 measurement or enforcement mechanism is defined here, and that omission is deliberate rather than silent.

**Requirements**: `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-nfr-low-latency`, `cpt-cf-oagw-nfr-input-validation`, `cpt-cf-oagw-nfr-ssrf-protection`, `cpt-cf-oagw-nfr-high-availability`, `cpt-cf-oagw-nfr-observability`

**Principles**: `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-no-cache`, `cpt-cf-oagw-principle-error-source`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the proxied request to `/oagw/v1/proxy/{alias}[/{path}]` and receives either the relayed upstream response or a gateway-produced error. |
| `cpt-cf-oagw-actor-upstream-service` | The external HTTP endpoint the request is forwarded to; its status code and body are relayed to the caller unchanged when it is reached. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-upstream-management` (supplies the resolved Upstream configuration this feature reads), `cpt-cf-oagw-feature-route-management` (supplies the resolved Route configuration this feature reads)

### 1.5 Non-Applicability and Deferrals

- **No user interface**: this feature is a data-plane HTTP path with no rendered surface, so UX and accessibility requirements do not apply.
- **No regulated or personal data**: this feature relays request and response bytes it does not interpret as a data controller or processor; it handles no regulated or personal data of its own beyond what an upstream integration chooses to send through it.
- **Circuit breaking deferred (`cpt-cf-oagw-nfr-high-availability`)**: the `CircuitBreakerOpen` error code exists in the gateway's error-code taxonomy (`cpt-cf-oagw-fr-error-codes`) for this purpose, but circuit-breaking itself is deliberately deferred in this configuration — the graded deployment runs a single gear instance with no failure-threshold tracking or health-aware pool to trip a breaker against, and per-upstream breaker state design is out of scope here. The requirement ID is retained and cited rather than dropped.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-proxy-request`

### Proxy HTTP Request

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-ph-proxy-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request to a single-endpoint upstream is forwarded to that endpoint and the upstream's response is relayed unchanged with `X-OAGW-Error-Source: upstream`.
- A request to a multi-endpoint upstream that names a specific pool member via `X-OAGW-Target-Host` is forwarded to that member instead of the round-robin choice.

**Error Scenarios**:
- The alias segment of the path does not resolve to any upstream in the caller's tenant hierarchy.
- The resolved upstream is disabled.
- No enabled route on the resolved upstream matches the request's method and path.
- A multi-endpoint pool requires disambiguation and `X-OAGW-Target-Host` is missing, malformed, or does not name a pool member.
- The selected endpoint's scheme is plaintext and plaintext upstream connections are not currently allowed.
- The request or response body exceeds the configured size limit.
- The upstream does not complete the exchange within the configured timeout.

**Steps**:
1. [ ] - `p1` - App developer sends `{METHOD} /oagw/v1/proxy/{alias}` or `{METHOD} /oagw/v1/proxy/{alias}/{path}`, optionally with a query string and an `X-OAGW-Target-Host` header - `inst-ph-flow-send`
2. [ ] - `p1` - **API**: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (forwards the transformed request to the resolved endpoint, or relays the upstream's response back to the caller) - `inst-ph-flow-api`
3. [ ] - `p1` - Resolve the alias to an enabled-or-disabled upstream within the caller's tenant hierarchy using the alias resolution process - `inst-ph-flow-resolve-alias`
4. [ ] - `p1` - **IF** no upstream in the hierarchy matches the alias - `inst-ph-flow-if-unknown`
   1. [ ] - `p1` - **RETURN** 404, `X-OAGW-Error-Source: gateway` - `inst-ph-flow-404`
5. [ ] - `p1` - **IF** the resolved upstream is disabled - `inst-ph-flow-if-disabled`
   1. [ ] - `p1` - **RETURN** 503, `X-OAGW-Error-Source: gateway` - `inst-ph-flow-503`
6. [ ] - `p1` - Match the request's method and path against the resolved upstream's enabled routes using the route matching process - `inst-ph-flow-match-route`
7. [ ] - `p1` - **IF** no enabled route matches - `inst-ph-flow-if-no-route`
   1. [ ] - `p1` - **RETURN** 404 (no matching route), `X-OAGW-Error-Source: gateway` - `inst-ph-flow-404-route`
8. [ ] - `p1` - Select an endpoint from the matched route's upstream pool using the endpoint selection process, honoring `X-OAGW-Target-Host` when present, then strip that header - `inst-ph-flow-select-endpoint`
9. [ ] - `p1` - **IF** endpoint selection reports a missing, invalid, or unknown target host - `inst-ph-flow-if-bad-target-host`
   1. [ ] - `p1` - **RETURN** the corresponding 400, `X-OAGW-Error-Source: gateway` - `inst-ph-flow-400-target-host`
10. [ ] - `p1` - **IF** the selected endpoint's scheme is plaintext and plaintext upstream connections are not allowed - `inst-ph-flow-if-scheme-blocked`
    1. [ ] - `p1` - **RETURN** 502, `X-OAGW-Error-Source: gateway`, without attempting to connect, per the `DownstreamError` mapping in `cpt-cf-oagw-fr-error-codes` - `inst-ph-flow-scheme-blocked`
11. [ ] - `p1` - Transform the inbound headers (strip routing and hop-by-hop headers, replace `Host`, apply configured passthrough/set/add/remove rules) and validate the declared body length against the size limit - `inst-ph-flow-transform-request`
12. [ ] - `p1` - **IF** the declared or observed body length exceeds the size limit - `inst-ph-flow-if-body-too-large`
    1. [ ] - `p1` - **RETURN** 413, `X-OAGW-Error-Source: gateway`, before buffering the body - `inst-ph-flow-413`
13. [ ] - `p1` - Stream the transformed request to the selected endpoint's authority, bounded by the configured request timeout - `inst-ph-flow-forward`
14. [ ] - `p1` - **IF** the timeout elapses before the upstream completes the exchange - `inst-ph-flow-if-timeout`
    1. [ ] - `p1` - **RETURN** 504, `X-OAGW-Error-Source: gateway`, and abort the outbound connection, per the `Timeout` mapping in `cpt-cf-oagw-fr-error-codes` - `inst-ph-flow-timeout`
15. [ ] - `p1` - Transform the upstream's response headers (strip hop-by-hop headers, apply configured response rules) - `inst-ph-flow-transform-response`
16. [ ] - `p1` - **RETURN** the upstream's response with its original status code unchanged and `X-OAGW-Error-Source: upstream` - `inst-ph-flow-relay`

## 3. Processes / Business Logic (CDSL)

Reusable internal steps invoked by the actor flow above, grouped by the entry's two milestones: request routing and resolution, then request execution and error handling.

### Alias Resolution

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ph-alias-resolution`

**Input**: the raw path segment following `/oagw/v1/proxy/` and the caller's tenant identifier

**Output**: a resolved upstream and the remaining path suffix, or an unknown-alias / disabled-upstream signal

**Steps**:
1. [ ] - `p1` - Extract the first path segment after `/oagw/v1/proxy/` as the alias; treat any remaining path as the path suffix carried into route matching - `inst-ph-alias-extract`
2. [ ] - `p1` - Search the caller's tenant hierarchy for an upstream with that alias, starting at the caller's own tenant and walking upward toward the root - `inst-ph-alias-search`
3. [ ] - `p1` - **IF** the caller's own tenant (or the closest ancestor tried so far) defines an upstream with that alias - `inst-ph-alias-if-found`
   1. [ ] - `p1` - That upstream is the resolved match; it shadows any ancestor upstream sharing the same alias, though limits enforced by an ancestor still apply to the resolved upstream - `inst-ph-alias-shadow`
4. [ ] - `p1` - **ELSE** continue the walk to each ancestor tenant in turn until the alias is found or the root tenant has been searched - `inst-ph-alias-continue`
5. [ ] - `p1` - **IF** no tenant in the hierarchy defines an upstream with that alias - `inst-ph-alias-if-unknown`
   1. [ ] - `p1` - **RETURN** unknown-alias (404) - `inst-ph-alias-unknown`
6. [ ] - `p1` - **IF** the resolved upstream's enabled flag is false - `inst-ph-alias-if-disabled`
   1. [ ] - `p1` - **RETURN** upstream-disabled (503) - `inst-ph-alias-disabled`
7. [ ] - `p1` - **RETURN** the resolved upstream and the remaining path suffix - `inst-ph-alias-return`

### Route Matching

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ph-route-matching`

**Input**: the resolved upstream's enabled routes, the request method, the path suffix from alias resolution, and the inbound query string

**Output**: a matched route plus the effective forwarded path and query, or a no-match signal

**Steps**:
1. [ ] - `p1` - Filter the resolved upstream's enabled routes to those whose method allowlist includes the request's method - `inst-ph-route-filter-method`
2. [ ] - `p1` - Among the remaining routes, select the one whose configured path is the longest prefix match of the path suffix - `inst-ph-route-longest-prefix`
3. [ ] - `p1` - **IF** no route satisfies both the method allowlist and the prefix match - `inst-ph-route-if-none`
   1. [ ] - `p1` - **RETURN** no-matching-route (404) - `inst-ph-route-404`
4. [ ] - `p1` - **IF** the matched route's `path_suffix_mode` is `append` - `inst-ph-route-if-append`
   1. [ ] - `p1` - Append the portion of the path suffix beyond the matched route's configured path to the upstream's target path - `inst-ph-route-append`
5. [ ] - `p1` - **ELSE** (`path_suffix_mode` is `disabled`) - `inst-ph-route-else-disabled-suffix`
   1. [ ] - `p1` - Reject any remaining path suffix beyond the matched route's configured path rather than forwarding it - `inst-ph-route-reject-suffix`
6. [ ] - `p1` - **IF** the matched route's `query_allowlist` is non-empty - `inst-ph-route-if-allowlist`
   1. [ ] - `p1` - Forward only the inbound query parameters named in the allowlist; drop the rest - `inst-ph-route-filter-query`
7. [ ] - `p1` - **RETURN** the matched route, the effective forwarded path, and the effective forwarded query - `inst-ph-route-return`

### Endpoint Selection and Target Host Handling

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ph-endpoint-selection`

**Input**: the matched route's upstream endpoint pool and the inbound `X-OAGW-Target-Host` header, if present

**Output**: a selected endpoint (scheme, host, port), or a missing/invalid/unknown target-host signal

**Decision matrix** (adapted from the request-routing decision record's target-host behavior matrix):

| Pool size | Alias origin | Header present | Header well-formed | Behavior |
|---|---|---|---|---|
| Any | Any | Yes | No (not a bare hostname or IP address) | Invalid-target-host (400), regardless of pool size or alias origin |
| Single endpoint | Any | No | — | Route to the sole endpoint; no disambiguation needed |
| Single endpoint | Any | Yes | Yes | Route to the sole endpoint if it matches, otherwise unknown-target-host (400) |
| Multiple endpoints | Not derived from a shared endpoint-hostname suffix | No | — | Round-robin across the pool |
| Multiple endpoints | Not derived from a shared endpoint-hostname suffix | Yes | Yes | Route to the named endpoint, bypassing round-robin |
| Multiple endpoints | Derived from a shared endpoint-hostname suffix | No | — | Missing-target-host (400) |
| Multiple endpoints | Derived from a shared endpoint-hostname suffix | Yes | Yes | Route to the named endpoint if it is a pool member, otherwise unknown-target-host (400) |

**Steps**:
1. [ ] - `p1` - **IF** `X-OAGW-Target-Host` is present on the inbound request - `inst-ph-endpoint-if-header-present`
   1. [ ] - `p1` - **IF** the value is not a bare hostname or IP address (no scheme, port, path, or other characters) - `inst-ph-endpoint-if-malformed`
      1. [ ] - `p1` - **RETURN** invalid-target-host (400), echoing the offending value - `inst-ph-endpoint-invalid`
2. [ ] - `p1` - **IF** the pool has exactly one endpoint - `inst-ph-endpoint-if-single`
   1. [ ] - `p1` - **IF** the header is absent, or present and matches the sole endpoint's host - `inst-ph-endpoint-single-match`
      1. [ ] - `p1` - **RETURN** the sole endpoint - `inst-ph-endpoint-single-return`
   2. [ ] - `p1` - **ELSE** (header present but does not match) - `inst-ph-endpoint-single-mismatch`
      1. [ ] - `p1` - **RETURN** unknown-target-host (400), listing the one valid host - `inst-ph-endpoint-single-unknown`
3. [ ] - `p1` - **ELSE** (the pool has two or more endpoints) - `inst-ph-endpoint-else-multi`
   1. [ ] - `p1` - **IF** the header is present - `inst-ph-endpoint-multi-if-header`
      1. [ ] - `p1` - **IF** its value matches a pool member's host - `inst-ph-endpoint-multi-if-match`
         1. [ ] - `p1` - **RETURN** the named endpoint, bypassing round-robin - `inst-ph-endpoint-multi-named`
      2. [ ] - `p1` - **ELSE** - `inst-ph-endpoint-multi-else-nomatch`
         1. [ ] - `p1` - **RETURN** unknown-target-host (400), echoing the value and listing valid hosts - `inst-ph-endpoint-multi-unknown`
   2. [ ] - `p1` - **ELSE** (header absent) - `inst-ph-endpoint-multi-else-noheader`
      1. [ ] - `p1` - **IF** the pool's alias is derived from a shared endpoint-hostname suffix - `inst-ph-endpoint-multi-if-suffix`
         1. [ ] - `p1` - **RETURN** missing-target-host (400), listing valid hosts - `inst-ph-endpoint-multi-missing`
      2. [ ] - `p1` - **ELSE** - `inst-ph-endpoint-multi-else-explicit`
         1. [ ] - `p1` - **RETURN** the next endpoint in round-robin order - `inst-ph-endpoint-multi-roundrobin`
4. [ ] - `p1` - Strip `X-OAGW-Target-Host` from the headers before forwarding, whether or not it was present, since it is a routing header the gateway consumes - `inst-ph-endpoint-strip-header`

### Header Transformation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ph-header-transform`

**Input**: the inbound request headers, the selected endpoint's authority, the upstream's configured request/response header rules, and the upstream's response headers (on the return trip)

**Output**: the outbound request headers sent to the endpoint, and the outbound response headers returned to the caller

**Steps**:
1. [ ] - `p1` - Remove routing headers the gateway consumes (`X-OAGW-Target-Host`) from the inbound headers - `inst-ph-header-strip-routing`
2. [ ] - `p1` - Remove the hop-by-hop header set (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`) from the inbound headers - `inst-ph-header-strip-hop-request`
3. [ ] - `p1` - Apply the upstream's configured request passthrough mode: `none` forwards no remaining inbound header other than the framing-header exception below, `allowlist` forwards only headers named in the passthrough allowlist, `all` forwards every remaining inbound header - `inst-ph-header-passthrough`
4. [ ] - `p1` - Apply the upstream's configured request header rules in order: remove the named headers, then set (overwrite) the named headers, then add (append, duplicates allowed) the named headers - `inst-ph-header-set-add-remove-request`
5. [ ] - `p1` - Replace the request's `Host` (or equivalent authority) with the selected endpoint's authority - `inst-ph-header-replace-host`
6. [ ] - `p1` - **RETURN** the outbound request headers - `inst-ph-header-return-request`
7. [ ] - `p1` - Remove the hop-by-hop header set from the upstream's response headers - `inst-ph-header-strip-hop-response`
8. [ ] - `p1` - Apply the upstream's configured response header rules in order: remove the named headers, then set (overwrite), then add (append) - `inst-ph-header-set-add-remove-response`
9. [ ] - `p1` - **RETURN** the outbound response headers - `inst-ph-header-return-response`

**Framing-header exception under `passthrough: none`**: the implementation forwards `content-type`, `content-length`, `accept`, and `accept-encoding` even when the upstream's configured request passthrough mode is `none`, because a body cannot be interpreted without its framing headers. This is a documented, deliberate exception to step 3 above, in the same spirit as the `Upgrade`/`Connection` exception `cpt-cf-oagw-feature-proxy-streaming` documents for upgrade requests (`cpt-cf-oagw-dod-ps-upgrade-header-exception`): `none` forwards no remaining inbound header other than these four.

### Body and Timeout Enforcement

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ph-body-timeout`

**Input**: the outbound request headers and body stream, the configured body size cap, and the configured `proxy_timeout_secs`

**Output**: the streamed upstream exchange, a gateway body-too-large (413) / timeout (504) signal, or — for an undeclared response body that exceeds the cap mid-stream, after relaying has begun — an aborted connection with no status substituted

**Steps**:
1. [ ] - `p1` - **IF** the request declares a body length exceeding the 100 MB cap - `inst-ph-body-if-declared-too-large`
   1. [ ] - `p1` - **RETURN** body-too-large (413) before any body bytes are buffered - `inst-ph-body-413-declared`
2. [ ] - `p1` - **ELSE** stream the request body to the endpoint without fully buffering it, counting bytes as they pass - `inst-ph-body-stream-request`
3. [ ] - `p1` - **IF** the streamed byte count exceeds the 100 MB cap before the body completes - `inst-ph-body-if-observed-too-large`
   1. [ ] - `p1` - Abort the request and **RETURN** body-too-large (413) - `inst-ph-body-413-observed`
4. [ ] - `p1` - Start a timeout bounded by the configured `proxy_timeout_secs` when the outbound connection begins - `inst-ph-body-start-timeout`
5. [ ] - `p1` - **IF** the upstream does not complete its response within the timeout - `inst-ph-body-if-timeout`
   1. [ ] - `p1` - Abort the outbound connection and **RETURN** 504, `X-OAGW-Error-Source: gateway`, per the `Timeout` mapping in `cpt-cf-oagw-fr-error-codes` - `inst-ph-body-timeout-return`
6. [ ] - `p1` - **ELSE IF** the upstream's response declares a `Content-Length` exceeding the 100 MB cap and no response status or body bytes have yet been relayed to the caller - `inst-ph-body-if-response-declared-too-large`
   1. [ ] - `p1` - **RETURN** body-too-large (413), `X-OAGW-Error-Source: gateway`, without relaying the upstream's status line or any body bytes - `inst-ph-body-413-response-declared`
7. [ ] - `p1` - **ELSE** relay the upstream's status and headers to the caller, then stream the response body back incrementally, counting bytes as they pass - `inst-ph-body-stream-response`
8. [ ] - `p1` - **IF** the response's length is undeclared (chunked or otherwise unknown) and the streamed byte count exceeds the 100 MB cap after relaying has begun - `inst-ph-body-if-response-observed-too-large`
   1. [ ] - `p1` - Stop relaying and abort the client connection without a clean close, since the status and headers already sent cannot be replaced with a 413 - `inst-ph-body-abort-response-observed`
9. [ ] - `p1` - **RETURN** the streamed exchange, or the aborted-connection outcome when the undeclared response body exceeded the cap mid-stream - `inst-ph-body-return`

### Outbound Connection and Error-Source Labeling

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ph-outbound-connection`

**Input**: the selected endpoint (scheme, host, port), the transformed outbound request, the `allow_http_upstream` flag, and the `ssrf_policy.enabled` flag

**Output**: a response labeled with the correct `X-OAGW-Error-Source` value

**Steps**:
1. [ ] - `p1` - **IF** `ssrf_policy.enabled` is true - `inst-ph-outbound-if-ssrf-enabled`
   1. [ ] - `p1` - Check the resolved outbound target against the SSRF policy before connecting - `inst-ph-outbound-ssrf-check`
   2. [ ] - `p1` - **IF** the policy blocks the resolved target - `inst-ph-outbound-if-ssrf-blocked`
      1. [ ] - `p1` - **RETURN** 502, `X-OAGW-Error-Source: gateway`, without connecting - `inst-ph-outbound-ssrf-blocked-return`
2. [ ] - `p1` - **ELSE** (`ssrf_policy.enabled` is false, as in the graded configuration) the gate is a pass-through and the resolved target proceeds unchecked - `inst-ph-outbound-ssrf-passthrough`
3. [ ] - `p1` - **IF** the selected endpoint's scheme is plaintext and `allow_http_upstream` is false - `inst-ph-outbound-if-scheme-blocked`
   1. [ ] - `p1` - **RETURN** 502, `X-OAGW-Error-Source: gateway`, without opening a socket to the endpoint, per the `DownstreamError` mapping in `cpt-cf-oagw-fr-error-codes` - `inst-ph-outbound-scheme-blocked`
4. [ ] - `p1` - **ELSE** make exactly one outbound connection attempt to the selected endpoint for this request - `inst-ph-outbound-attempt`
5. [ ] - `p1` - **IF** the connection attempt or request send fails before any upstream response is received - `inst-ph-outbound-if-conn-fail`
   1. [ ] - `p1` - **RETURN** 502, `X-OAGW-Error-Source: gateway`, per the `DownstreamError` mapping in `cpt-cf-oagw-fr-error-codes`; do not automatically retry the client's request - `inst-ph-outbound-conn-fail-return`
6. [ ] - `p1` - **ELSE** (the upstream returns a response, including a 4xx or 5xx status) - `inst-ph-outbound-else-response`
   1. [ ] - `p1` - Relay the response with its original status code unchanged and `X-OAGW-Error-Source: upstream`; do not store the response for reuse on a later request - `inst-ph-outbound-relay`
7. [ ] - `p1` - **RETURN** the labeled response - `inst-ph-outbound-return`

## 4. Definitions of Done

### Milestone 1 — Request Routing and Resolution

#### Alias Resolution, Unknown Alias, and Disabled Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ph-alias-resolution`

The system **MUST** extract the alias segment from the proxy request path and resolve it to an upstream within the calling tenant. The system **MUST** respond 404 when no upstream owned by the calling tenant defines that alias, and **MUST** respond 503 when the resolved upstream's enabled flag is false, in both cases with `X-OAGW-Error-Source: gateway`. Walking the tenant hierarchy from the caller's own tenant toward the root, and shadowing an ancestor's same-alias upstream by a closer tenant, are not served in this configuration — the gear has no access to a tenant-hierarchy source, per the deferral recorded in `cpt-cf-oagw-feature-upstream-management`'s §1.5 — so alias resolution operates within the calling tenant only.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-alias-resolution`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Upstream`

#### Route Matching, Path Suffix, and Query Allowlist

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ph-route-matching`

The system **MUST** select among the resolved upstream's enabled routes by filtering on the request method and then choosing the longest path-prefix match against the remaining path. The system **MUST** apply the matched route's `path_suffix_mode` to decide whether the path beyond the matched prefix is appended to the upstream's target path or rejected, and **MUST** filter the forwarded query string to only the parameters named in the matched route's `query_allowlist` when that allowlist is non-empty. The system **MUST** respond 404 with `X-OAGW-Error-Source: gateway` when no enabled route satisfies both the method and prefix conditions.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-route-matching`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Route`

#### Endpoint Selection and the X-OAGW-Target-Host Decision Matrix

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ph-endpoint-selection`

The system **MUST** select an endpoint from the matched route's upstream pool without requiring any header when the pool has a single endpoint, and **MUST** select by round-robin among a multi-endpoint pool unless `X-OAGW-Target-Host` names a specific pool member, in which case that member is selected and round-robin is bypassed. The system **MUST** read `X-OAGW-Target-Host` for routing and then strip it before forwarding, in both the HTTP/1.1 and HTTP/2 request forms. The system **MUST** produce exactly three distinct 400 outcomes, each with `X-OAGW-Error-Source: gateway`: a missing-target-host response, produced when a multi-endpoint pool whose alias is derived from a shared endpoint-hostname suffix receives no header, listing the pool's valid hosts; an invalid-target-host response, produced when the header value is not a bare hostname or IP address, echoing the offending value; and an unknown-target-host response, produced when a well-formed header value does not match any pool member, echoing the offending value and listing the pool's valid hosts.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-endpoint-selection`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Upstream`, `Endpoint`

### Milestone 2 — Request Execution and Error Handling

#### Scheme Enforcement for Outbound Connections

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ph-scheme-enforcement`

The system **MUST** check the selected endpoint's configured scheme immediately before opening the outbound connection: when that scheme is plaintext, the system **MUST** connect only if `allow_http_upstream` is true, and **MUST** refuse the request with 502 (`X-OAGW-Error-Source: gateway`, the `DownstreamError` mapping in `cpt-cf-oagw-fr-error-codes`), without attempting the connection, when `allow_http_upstream` is false. This is the only point in the request path where `allow_http_upstream` has any effect — it does not influence whether an upstream configured with a plaintext scheme can be created and stored, which is a separate, earlier concern. In the graded configuration `allow_http_upstream` is true, so a plaintext endpoint is connected to normally.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-outbound-connection`

**Constraints**: `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Endpoint`

#### SSRF Policy Gate on the Connection Path

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ph-ssrf-gate`

The system **MUST** check the resolved outbound target against the SSRF policy immediately before connecting when `ssrf_policy.enabled` is true, and **MUST** refuse a blocked target with 502, `X-OAGW-Error-Source: gateway`, without connecting. In the graded configuration `ssrf_policy.enabled` is false, so this gate is a pass-through and every resolved target proceeds to connection unchecked; `cpt-cf-oagw-nfr-ssrf-protection` is satisfied by the gate's existence and behavior when enabled, not by its being active in this configuration.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-outbound-connection`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Endpoint`

#### Header Transformation in Both Directions

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ph-header-transform`

The system **MUST** consume routing headers (`X-OAGW-Target-Host`) and never forward them, **MUST** strip the hop-by-hop header set (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`) from both the outbound request and the returned response, **MUST** replace the request's host authority with the selected endpoint's authority, and **MUST** apply the upstream's configured request and response header set/add/remove rules together with the configured passthrough mode (`none`, `allowlist`, or `all`) for inbound header forwarding. Under `passthrough: none`, the implementation forwards `content-type`, `content-length`, `accept`, and `accept-encoding` regardless, since a body cannot be interpreted without its framing headers; this is a documented, deliberate exception, in the same spirit as the `Upgrade`/`Connection` exception `cpt-cf-oagw-dod-ps-upgrade-header-exception` documents for upgrade requests.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-header-transform`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Upstream`

#### Body Size Cap Enforced Before Buffering

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ph-body-limit`

The system **MUST** stream request and response bodies to their destination rather than fully buffering them, and **MUST** cap both directions at 100 MB. On the request side, and on the response side when the upstream declares a `Content-Length` over the cap before any response bytes are relayed, the system **MUST** reject with 413 before any body bytes are buffered or relayed. On the response side, when the upstream's declared length is unknown (chunked or otherwise undeclared) and the cap is exceeded after the status and headers have already been relayed, the system **MUST** stop relaying and abort the connection without a clean close, since no status can be substituted once relaying has begun.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-body-timeout`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`

#### Timeout Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ph-timeout`

The system **MUST** bound each outbound proxy exchange by the configured `proxy_timeout_secs` (2 seconds in the graded configuration) and **MUST** respond with 504, `X-OAGW-Error-Source: gateway`, per the `Timeout` mapping in `cpt-cf-oagw-fr-error-codes`, when the upstream does not complete the exchange within that bound.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-body-timeout`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`

#### Error-Source Distinction on Every Proxy Response

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ph-error-source`

The system **MUST** attach `X-OAGW-Error-Source: gateway` to every response the gateway itself produces on the proxy path, and **MUST** attach `X-OAGW-Error-Source: upstream` to every response relayed from the upstream, including when the upstream itself returned a 4xx or 5xx status. The system **MUST** relay the upstream's own status code unchanged and **MUST NOT** rewrite it.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-outbound-connection`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`

#### Correlation Identifier and Outcome Recording

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ph-observability`

The system **MUST** assign a correlation identifier to every proxied request and **MUST** record that request's outcome (its resolved status code and its `X-OAGW-Error-Source` label) against that identifier, giving `cpt-cf-oagw-nfr-observability`'s per-request logging requirement real behavior on the proxy data-plane path. In this configuration the correlation identifier is the `x-request-id` the host runtime already attaches to every response, not an identifier this feature mints itself, and the outcome is recorded through the platform's tracing facility (connection and relay outcomes recorded as trace events) rather than through a metrics pipeline this feature owns; a dedicated metrics exporter is deferred, and this DoD is satisfied by the correlation identifier and the traced outcome record, not by exported metrics.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`

#### No Automatic Whole-Request Retries and No Response Caching

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ph-no-retry-no-cache`

The system **MUST** make exactly one outbound attempt per client request and **MUST NOT** automatically re-issue the client's request as a whole on failure, and **MUST NOT** cache an upstream response for reuse on a subsequent request.

**Implements**:
- `cpt-cf-oagw-flow-ph-proxy-request`
- `cpt-cf-oagw-algo-ph-outbound-connection`

**Constraints**: None

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`

## 5. Acceptance Criteria

- [ ] A request that resolves to a known, enabled upstream with a matching route and reachable endpoint returns the upstream's original status code and body, with `X-OAGW-Error-Source: upstream` present, including for a 200 response.
- [ ] A request whose upstream returns 500 is relayed to the caller as 500, with the upstream's original body unchanged and `X-OAGW-Error-Source: upstream`.
- [ ] A request naming an alias that does not resolve to any upstream in the caller's tenant hierarchy returns 404 with `X-OAGW-Error-Source: gateway`.
- [ ] A request resolving to a disabled upstream returns 503 with `X-OAGW-Error-Source: gateway`.
- [ ] A request whose method or path does not match any enabled route on the resolved upstream returns 404 with `X-OAGW-Error-Source: gateway`.
- [ ] A multi-endpoint pool whose alias is derived from a shared endpoint-hostname suffix, called without `X-OAGW-Target-Host`, returns 400 listing the pool's valid hosts, with `X-OAGW-Error-Source: gateway`.
- [ ] The same pool, called with an `X-OAGW-Target-Host` value that is not a bare hostname or IP address, returns 400 echoing that value as the invalid value, with `X-OAGW-Error-Source: gateway`.
- [ ] The same pool, called with a well-formed but unrecognized `X-OAGW-Target-Host` value, returns 400 echoing that value and listing the pool's valid hosts, with `X-OAGW-Error-Source: gateway`.
- [ ] The same pool, called with an `X-OAGW-Target-Host` value matching a pool member, is forwarded to that specific member rather than to the round-robin choice.
- [ ] A single-endpoint upstream accepts a request with no `X-OAGW-Target-Host` and forwards it to its one endpoint.
- [ ] The request the upstream receives contains none of the hop-by-hop headers (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`), and its host header names the upstream's authority rather than the caller's.
- [ ] The request the upstream receives does not contain `X-OAGW-Target-Host`, even when the caller sent it.
- [ ] When the matched route's `query_allowlist` is non-empty, only the listed query parameters reach the upstream; parameters not on the list are dropped.
- [ ] When the matched route's `path_suffix_mode` is `append`, the path beyond the matched route's configured path is appended to the upstream's request path; when it is `disabled`, sending such a suffix is rejected.
- [ ] A request declaring a body length larger than 100 MB is rejected with 413 before any body bytes are buffered.
- [ ] A request to an upstream whose endpoint scheme is plaintext succeeds when `allow_http_upstream` is true, as in the graded configuration.
- [ ] A request that does not receive a complete upstream response within the configured `proxy_timeout_secs` (2 seconds in the graded configuration) returns 504 with `X-OAGW-Error-Source: gateway`.
- [ ] An upstream response that declares a `Content-Length` over 100 MB is rejected with 413, `X-OAGW-Error-Source: gateway`, before the status line or any body bytes are relayed to the caller.
- [ ] An upstream response with no declared length (chunked) whose relayed body exceeds 100 MB after relaying has begun is not completed with a substituted status; the gateway stops relaying and aborts the connection instead.
- [ ] Exactly one outbound connection attempt is made per client request; the gateway never automatically re-issues the same client request as a whole after a failed attempt.
- [ ] Two consecutive, identical requests to the same upstream each produce a fresh outbound call; no response is served from a cache for the second request.
- [ ] With `ssrf_policy.enabled` false, as in the graded configuration, a request to a resolved target that a policy-enabled deployment would block still connects normally, demonstrating the gate is a pass-through here.
- [ ] Every proxied request's log record carries a correlation identifier and the request's resolved outcome (status code and `X-OAGW-Error-Source` label).
