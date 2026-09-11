# Feature: HTTP Proxy Data Plane


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxied Request Succeeds](#proxied-request-succeeds)
  - [Proxied Request Rejected By A Guard](#proxied-request-rejected-by-a-guard)
  - [Proxied Request Fails Upstream](#proxied-request-fails-upstream)
  - [CORS Preflight Answered Locally](#cors-preflight-answered-locally)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Guard Evaluation](#guard-evaluation)
  - [Body Validation](#body-validation)
  - [Header Transformation](#header-transformation)
  - [Endpoint Selection](#endpoint-selection)
  - [Upstream Invocation](#upstream-invocation)
  - [Error Mapping And Source Stamping](#error-mapping-and-source-stamping)
- [4. States (CDSL)](#4-states-cdsl)
  - [Proxy Request Lifecycle State Machine](#proxy-request-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Proxy Endpoint Registration](#proxy-endpoint-registration)
  - [Disabled Upstream Rejection](#disabled-upstream-rejection)
  - [Query Allowlist Guard](#query-allowlist-guard)
  - [Path Suffix Guard](#path-suffix-guard)
  - [Body Validation And Size Limit](#body-validation-and-size-limit)
  - [Hop-By-Hop Header Stripping](#hop-by-hop-header-stripping)
  - [Header Plan Application](#header-plan-application)
  - [Target Host Selection](#target-host-selection)
  - [Plaintext Connection Policy](#plaintext-connection-policy)
  - [Proxy Timeout Enforcement](#proxy-timeout-enforcement)
  - [Error Mapping And Source Stamping](#error-mapping-and-source-stamping-1)
  - [CORS Preflight Handling](#cors-preflight-handling)
  - [CORS Request Enforcement](#cors-request-enforcement)
  - [Plugin And Rate Limit Hook Points](#plugin-and-rate-limit-hook-points)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-http-proxy-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-http-proxy`

## 1. Feature Context

### 1.1 Overview

This feature executes plain-HTTP proxy requests against an already resolved upstream, validating and transforming each request before forwarding it. It returns every upstream response or gateway failure to the caller with an explicit error-source attribution.

### 1.2 Purpose

The gear must expose one proxy surface that application gears call instead of reaching external services directly. This feature turns the effective configuration produced by alias resolution and route matching into an actual outbound call. It owns post-match guard rejection, body validation, header transformation, endpoint selection, timeouts, and error rendering. Route selection itself stays with the configuration-resolution feature, which reports a non-match as route-not-found rather than as a guard rejection.

Method conformance is guaranteed by route selection, so this feature applies no separate method guard. Configuration resolution matches the inbound method together with the longest path prefix, and this feature renders the resulting route-not-found outcome as `404`. It likewise renders the upstream-disabled outcome that resolution reports when an alias resolves only to disabled upstreams.

The proxy surface is registered gear-relative as `/oagw/v1/proxy/{alias}` and `/oagw/v1/proxy/{alias}/{path}`, never under an `/api` prefix. Plain HTTP request and response proxying is covered here; server-sent-event streaming and WebSocket upgrades extend this lifecycle in the streaming feature. The plaintext-connection policy defined here covers both the `http` and `ws` endpoint schemes, and it is a shared path that the streaming feature reuses unchanged. Plugin execution and rate-limit enforcement are not implemented here, yet this feature establishes the single lifecycle position at which both hooks are invoked.

Authentication and authorization are enforced by the platform gateway ahead of this gear, so this feature implements neither and only renders their failures. Rollout and rollback are not applicable at feature level, because this data plane ships inside the gear binary with no independent deployment toggle.

**Requirements**: `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-fr-error-codes`, `cpt-cf-oagw-nfr-ssrf-protection`, `cpt-cf-oagw-nfr-input-validation`, `cpt-cf-oagw-nfr-high-availability`, `cpt-cf-oagw-nfr-observability`

**Principles**: `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-no-cache`, `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`, `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-no-direct-internet`

**Design elements**: `cpt-cf-oagw-component-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-interface-proxy-api`, `cpt-cf-oagw-seq-proxy-flow`, `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-cors`, `cpt-cf-oagw-adr-error-source-distinction`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends proxy requests to an alias and consumes the proxied response or the gateway error |
| `cpt-cf-oagw-actor-upstream-service` | Receives the forwarded request and returns the response that this feature passes through |
| `cpt-cf-oagw-actor-platform-operator` | Configures the gear proxy timeout and the plaintext-upstream policy that this feature enforces |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **ADRs**: [0001 Request Routing](../ADR/0001-request-routing.md), [0004 CORS](../ADR/0004-cors.md), [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md)
- **Schemas**: [upstream.v1.schema.json](../schemas/upstream.v1.schema.json), [route.v1.schema.json](../schemas/route.v1.schema.json)
- **Dependencies**:
  - `cpt-cf-oagw-feature-config-resolution` supplies the merged effective configuration, the matched route, and the header transformation plan that this feature executes.
  - `cpt-cf-oagw-feature-gear-foundation` supplies the mounted gear-relative router, the proxy timeout configuration, the plaintext-upstream policy, and the problem-details error contract.
  - `cpt-cf-oagw-feature-streaming-proxy` and `cpt-cf-oagw-feature-plugin-runtime` build on the lifecycle defined here; this feature does not depend on either of them.

Out of scope here: server-sent-event and WebSocket handling, plugin chain execution, rate-limit enforcement, circuit-breaker logic, distributed rate-limit synchronization, the Redis second-level cache, Starlark plugin execution, runtime gRPC proxying, and DNS or IP-pinning implementation details. Control-plane persistence is in-process, so no database is read or written on the proxy path.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-oagw-usecase-proxy-request`

### Proxied Request Succeeds

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-request-success`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A `GET` on `/oagw/v1/proxy/{alias}/{path}` returns the upstream status, headers, and body unchanged except for the applied response header plan.
- A `POST` with a JSON body below the size limit forwards method, path, allowed query parameters, and body to the selected endpoint.
- A request whose upstream declares several endpoints reaches one pool member chosen by round-robin selection or by an explicit target-host header.

**Error Scenarios**:
- The alias resolves to nothing, so the caller receives `404` with error type `RouteNotFound`.
- No candidate route lists the inbound method, so selection fails and the caller receives `404` with error type `RouteNotFound`.
- The alias resolves only to disabled upstreams, so the caller receives `503` with error type `LinkUnavailable`.
- The upstream connection cannot be opened because the endpoint scheme is `http` and plaintext connections are disabled.

**Steps**:
1. [ ] - `p1` - Application developer sends `{METHOD} /oagw/v1/proxy/{alias}/{path}` with headers, optional query parameters, and optional body - `inst-flow-success-01`
2. [ ] - `p1` - {API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (inbound request accepted, security context extracted, request identifier assigned)} - `inst-flow-success-02`
3. [ ] - `p1` - **IF** the request is a CORS preflight - `inst-flow-success-03`
   1. [ ] - `p1` - Delegate to `cpt-cf-oagw-flow-cors-preflight` and stop this flow - `inst-flow-success-04`
4. [ ] - `p1` - Request the effective configuration and matched route for the alias, path, and method from configuration resolution - `inst-flow-success-05`
5. [ ] - `p1` - **IF** resolution reports an unknown alias, a disabled upstream, or no matching route - `inst-flow-success-06`
   1. [ ] - `p1` - Render the reported gateway error through `cpt-cf-oagw-algo-error-mapping` and **RETURN** it - `inst-flow-success-07`
6. [ ] - `p1` - Enforce the effective CORS configuration on the actual request through `cpt-cf-oagw-algo-guard-evaluation` - `inst-flow-success-08`
7. [ ] - `p1` - Evaluate the query-parameter and path-suffix guards through `cpt-cf-oagw-algo-guard-evaluation` - `inst-flow-success-09`
8. [ ] - `p1` - Validate the request body through `cpt-cf-oagw-algo-body-validation` - `inst-flow-success-10`
9. [ ] - `p1` - Invoke the auth, guard, and request-transform plugin hooks and the rate-limit hook at their fixed lifecycle position - `inst-flow-success-11`
10. [ ] - `p1` - Build the outbound request through `cpt-cf-oagw-algo-header-transformation` - `inst-flow-success-12`
11. [ ] - `p1` - Select the target endpoint through `cpt-cf-oagw-algo-endpoint-selection` - `inst-flow-success-13`
12. [ ] - `p1` - Forward the request and read the response through `cpt-cf-oagw-algo-upstream-invocation` - `inst-flow-success-14`
13. [ ] - `p1` - Apply the response header plan and the CORS response headers, then invoke the response-transform hook - `inst-flow-success-15`
14. [ ] - `p1` - Stamp `X-OAGW-Error-Source: upstream` on the response, because its status and body originate at the upstream - `inst-flow-success-16`
15. [ ] - `p1` - **RETURN** the upstream status, the transformed headers, and the unmodified upstream body - `inst-flow-success-17`

### Proxied Request Rejected By A Guard

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-guard-rejection`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request carrying a query parameter outside `query_allowlist` is refused, and the parameter name appears in the problem detail.
- A request supplying a path suffix while `path_suffix_mode` is `disabled` is refused with a validation error.
- A request whose declared body size exceeds the hard limit is refused before the body is buffered.

**Error Scenarios**:
- A missing, malformed, or unknown `X-OAGW-Target-Host` value prevents endpoint selection and yields a `400` gateway error.
- A disallowed origin or method under an enabled CORS configuration yields a `403` gateway error.

**Steps**:
1. [ ] - `p1` - Application developer sends a proxy request that violates one guard or body rule - `inst-flow-guard-01`
2. [ ] - `p1` - {API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (request accepted, effective configuration and matched route resolved)} - `inst-flow-guard-02`
3. [ ] - `p1` - Evaluate guards and body rules in their fixed order through `cpt-cf-oagw-algo-guard-evaluation` and `cpt-cf-oagw-algo-body-validation` - `inst-flow-guard-03`
4. [ ] - `p1` - **IF** any guard or body rule rejects the request - `inst-flow-guard-04`
   1. [ ] - `p1` - Abandon the request without opening any upstream connection and without buffering the remaining body - `inst-flow-guard-05`
   2. [ ] - `p1` - Render the rejection through `cpt-cf-oagw-algo-error-mapping` with `X-OAGW-Error-Source: gateway` - `inst-flow-guard-06`
   3. [ ] - `p1` - **RETURN** the `application/problem+json` body carrying `type`, `title`, `status`, `detail`, and `instance` - `inst-flow-guard-07`
5. [ ] - `p1` - **ELSE** - `inst-flow-guard-08`
   1. [ ] - `p1` - Continue the successful path described by `cpt-cf-oagw-flow-proxy-request-success` - `inst-flow-guard-09`
6. [ ] - `p1` - **RETURN** the gateway rejection response to the caller - `inst-flow-guard-10`

### Proxied Request Fails Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-upstream-failure`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- An upstream `4xx` or `5xx` response is passed through with its status, headers, and body unchanged and marked as upstream.
- A connection that cannot be established within the configured proxy timeout yields `504` with error type `ConnectionTimeout`.
- A response that does not complete within the configured proxy timeout yields `504` with error type `RequestTimeout`.

**Error Scenarios**:
- A transport or protocol failure after connection yields `502` with error type `DownstreamError` or `ProtocolError`.
- A plaintext endpoint refused by the plaintext-connection policy yields `503` with error type `LinkUnavailable`.

**Steps**:
1. [ ] - `p1` - Upstream service answers with an error status, closes the connection, or fails to answer in time - `inst-flow-failure-01`
2. [ ] - `p1` - Observe the outcome of `cpt-cf-oagw-algo-upstream-invocation` for the forwarded request - `inst-flow-failure-02`
3. [ ] - `p1` - **IF** the upstream produced a complete HTTP response - `inst-flow-failure-03`
   1. [ ] - `p1` - Pass the status, headers, and body through unmodified, apply only the configured response header plan - `inst-flow-failure-04`
   2. [ ] - `p1` - Stamp `X-OAGW-Error-Source: upstream` and never replace the body with problem details - `inst-flow-failure-05`
4. [ ] - `p1` - **ELSE** - `inst-flow-failure-06`
   1. [ ] - `p1` - Classify the transport failure as a timeout, a link failure, a protocol failure, or a downstream failure - `inst-flow-failure-07`
   2. [ ] - `p1` - Render it through `cpt-cf-oagw-algo-error-mapping` with `X-OAGW-Error-Source: gateway` - `inst-flow-failure-08`
5. [ ] - `p1` - Never re-issue the original client request, because the gateway performs no automatic full-request retries - `inst-flow-failure-09`
6. [ ] - `p1` - **RETURN** the passthrough response or the gateway problem-details response - `inst-flow-failure-10`

### CORS Preflight Answered Locally

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-cors-preflight`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An `OPTIONS` request carrying `Origin` and `Access-Control-Request-Method` receives `204 No Content` without any upstream resolution.
- The response echoes the requested origin, method, and headers and advertises a preflight cache lifetime.
- The response varies on origin and on both preflight request headers, which prevents cache poisoning across origins.

**Error Scenarios**:
- An `OPTIONS` request without `Origin` or without `Access-Control-Request-Method` is not a preflight and follows the ordinary proxy path.

**Steps**:
1. [ ] - `p1` - Browser client sends `OPTIONS /oagw/v1/proxy/{alias}/{path}` with `Origin` and `Access-Control-Request-Method` - `inst-flow-preflight-01`
2. [ ] - `p1` - {API: `OPTIONS /oagw/v1/proxy/{alias}/{path}` (preflight detected at the handler, no tenant context required)} - `inst-flow-preflight-02`
3. [ ] - `p1` - **IF** `Origin` and `Access-Control-Request-Method` are both present - `inst-flow-preflight-03`
   1. [ ] - `p1` - Skip upstream resolution, per-request authorization, guard evaluation, and plugin invocation entirely - `inst-flow-preflight-04`
   2. [ ] - `p1` - Set `Access-Control-Allow-Origin` to the received origin and `Access-Control-Allow-Methods` to the requested method - `inst-flow-preflight-05`
   3. [ ] - `p1` - Echo `Access-Control-Request-Headers` into `Access-Control-Allow-Headers` when the client sent that header - `inst-flow-preflight-06`
   4. [ ] - `p1` - Set `Access-Control-Max-Age: 86400` and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` - `inst-flow-preflight-07`
   5. [ ] - `p1` - **RETURN** `204 No Content` with an empty body - `inst-flow-preflight-08`
4. [ ] - `p1` - **ELSE** - `inst-flow-preflight-09`
   1. [ ] - `p1` - Treat the request as an ordinary proxy call handled by `cpt-cf-oagw-flow-proxy-request-success` - `inst-flow-preflight-10`
5. [ ] - `p1` - **RETURN** the permissive preflight response or the ordinary proxy response - `inst-flow-preflight-11`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. These are reusable building blocks called by the actor flows above.

### Guard Evaluation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-guard-evaluation`

**Input**: Inbound method, path suffix, query parameters, `Origin` header, and the matched route plus effective CORS configuration.

**Output**: An accepted request, or a rejection carrying a gateway error type and an HTTP status.

Guards run after route selection and never influence it. A non-matching route is reported by configuration resolution as `RouteNotFound` with status `404`, whereas a guard rejection means the route matched but the request is not permitted. The table below lists the guards in the same order as the steps evaluate them.

| Guard | Rule | Outcome when violated |
|---|---|---|
| CORS origin | `Origin` must match `allowed_origins` exactly when CORS is enabled, on actual requests only | `403` CORS origin not allowed |
| CORS method | Method must appear in `allowed_methods` when CORS is enabled, on actual requests only | `403` CORS method not allowed |
| Query parameters | Every supplied parameter name must appear in `match.http.query_allowlist`; an empty allowlist permits none | `400` `ValidationError` |
| Path suffix | A path suffix must not be supplied when `path_suffix_mode` is `disabled` | `400` `ValidationError` |

Conformance to the matched route's `match.http.methods` is not a guard, because the inbound method is a selection key that configuration resolution already applied. A method listed by no candidate route therefore produces `404` with error type `RouteNotFound`, never a `400` rejection here.

Origin matching is exact, port-sensitive, and scheme-sensitive, and no pattern matching is applied. A wildcard origin entry matches any origin. CORS enforcement is skipped entirely when the effective CORS configuration is disabled.

**Steps**:
1. [ ] - `p1` - Normalize the inbound method, the decoded path suffix, and the parsed query parameter names - `inst-algo-guard-01`
2. [ ] - `p1` - **IF** the effective CORS configuration is enabled and the request carries an `Origin` header - `inst-algo-guard-02`
   1. [ ] - `p1` - Reject with `403` when the origin does not match `allowed_origins` exactly - `inst-algo-guard-03`
   2. [ ] - `p1` - Reject with `403` when the method is absent from `allowed_methods` - `inst-algo-guard-04`
3. [ ] - `p1` - **FOR EACH** query parameter name in the inbound query string - `inst-algo-guard-06`
   1. [ ] - `p1` - Reject with `400` and type `ValidationError` when the name is absent from `query_allowlist` - `inst-algo-guard-07`
4. [ ] - `p1` - **IF** `path_suffix_mode` is `disabled` and a non-empty path suffix was supplied - `inst-algo-guard-08`
   1. [ ] - `p1` - Reject with `400` and type `ValidationError` naming the rejected suffix - `inst-algo-guard-09`
5. [ ] - `p1` - **RETURN** acceptance, or the first rejection with its error type and status - `inst-algo-guard-10`

### Body Validation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-body-validation`

**Input**: The inbound `Content-Length` header, the `Transfer-Encoding` header, and the request body stream.

**Output**: A validated body ready for forwarding, or a rejection with status `400` or `413`.

| Check | Rule | Outcome when violated |
|---|---|---|
| `Content-Length` well-formed | Must parse as a non-negative integer when present | `400` `ValidationError` |
| `Content-Length` consistent | Must equal the number of body bytes actually received | `400` `ValidationError` |
| Header combination | `Content-Length` and `Transfer-Encoding` must not both be present | `400` `ValidationError` |
| `Transfer-Encoding` value | Only `chunked` is supported; any other coding is refused | `400` `ValidationError` |
| Hard size limit | Body must not exceed 104857600 bytes, which is the 100MB limit | `413` `PayloadTooLarge` |

The size limit is enforced before buffering. A declared `Content-Length` above the limit is refused immediately, and a chunked body is refused as soon as the accumulated byte count crosses the limit. Deeper body checks such as schema validation and content-type rules belong to guard plugins, not to this feature.

**Steps**:
1. [ ] - `p1` - Parse `Content-Length` and reject with `400` when the value is not a non-negative integer - `inst-algo-body-01`
2. [ ] - `p1` - Reject with `400` when both `Content-Length` and `Transfer-Encoding` are present on the same request - `inst-algo-body-02`
3. [ ] - `p1` - Reject with `400` when `Transfer-Encoding` names any coding other than `chunked` - `inst-algo-body-03`
4. [ ] - `p1` - **IF** a parsed `Content-Length` exceeds 104857600 bytes - `inst-algo-body-04`
   1. [ ] - `p1` - Reject with `413` and type `PayloadTooLarge` before reading any body byte - `inst-algo-body-05`
5. [ ] - `p1` - **FOR EACH** body chunk read from the inbound stream - `inst-algo-body-06`
   1. [ ] - `p1` - Increment the received byte count and reject with `413` once it exceeds the limit - `inst-algo-body-07`
6. [ ] - `p1` - Reject with `400` when the final received byte count differs from a declared `Content-Length` - `inst-algo-body-08`
7. [ ] - `p1` - **RETURN** the validated body together with its exact byte length - `inst-algo-body-09`

### Header Transformation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-header-transformation`

**Input**: Inbound request headers, the selected endpoint host and port, and the effective request and response header plans.

**Output**: The outbound header set for the upstream, and the header plan to apply to the response.

Three header categories are processed. Routing headers are consumed by the gateway and never forwarded. Hop-by-hop headers are always stripped. Remaining headers are forwarded according to the plan's passthrough mode.

| Inbound header | Rule |
|---|---|
| `X-OAGW-Target-Host` | Read during endpoint selection, then stripped |
| `Host` | Replaced by the selected upstream host |
| `Connection` | Stripped |
| `Keep-Alive` | Stripped |
| `Proxy-Authenticate` | Stripped |
| `Proxy-Authorization` | Stripped |
| `TE` | Stripped |
| `Trailer` | Stripped |
| `Transfer-Encoding` | Stripped |
| `Upgrade` | Stripped |

The passthrough mode `none` forwards no inbound header, `allowlist` forwards only names in `passthrough_allowlist`, and `all` forwards every surviving header. The plan then applies `remove`, `set`, and `add` operations, where `set` overwrites and `add` appends an additional value. On HTTP/2 the `:authority` pseudo-header is replaced with the selected upstream authority, and it never substitutes for `X-OAGW-Target-Host`. Well-known headers such as `Content-Length` and `Content-Type` are adjusted to match the outbound body, and a header carrying a carriage return or line feed is refused with status `400`.

**Steps**:
1. [ ] - `p1` - Copy the inbound headers and drop every routing and hop-by-hop header from the working set - `inst-algo-header-01`
2. [ ] - `p1` - Apply the plan's passthrough mode to decide which surviving inbound headers are forwarded - `inst-algo-header-02`
3. [ ] - `p1` - **FOR EACH** name listed in the request plan's `remove` list - `inst-algo-header-03`
   1. [ ] - `p1` - Delete every value of that name from the outbound set - `inst-algo-header-04`
4. [ ] - `p1` - Apply the request plan's `set` entries as overwrites, then its `add` entries as appended values - `inst-algo-header-05`
5. [ ] - `p1` - Set `Host` to the selected endpoint host, or `:authority` to the selected authority on HTTP/2 - `inst-algo-header-06`
6. [ ] - `p1` - Adjust `Content-Length` and `Content-Type` to describe the outbound body exactly - `inst-algo-header-07`
7. [ ] - `p1` - **RETURN** the outbound header set and the response plan to apply after the upstream answers - `inst-algo-header-08`

### Endpoint Selection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-endpoint-selection`

**Input**: The effective upstream endpoint pool, the alias type, and the optional `X-OAGW-Target-Host` header value.

**Output**: One selected endpoint, or a rejection with status `400`.

All endpoints in a pool share the same protocol, scheme, and port, so selection chooses only the host. The header is consumed for selection and then stripped from the outbound request.

| Endpoints | Alias type | Header present | Behaviour |
|---|---|---|---|
| 1 | Any | No | Route to the single endpoint |
| 1 | Any | Yes | Validate the value, then route to the single endpoint |
| 2 or more | Explicit, no common suffix | No | Distribute across the pool by round-robin |
| 2 or more | Explicit, no common suffix | Yes | Route to the named endpoint and bypass round-robin |
| 2 or more | Common suffix | No | Reject with `400` and type `MissingTargetHost` |
| 2 or more | Common suffix | Yes | Route to the named endpoint |

A header value that is not a bare hostname or IP address, for example one carrying a port, a path, or a special character, is rejected with type `InvalidTargetHost`. A syntactically valid value that matches no configured endpoint host is rejected with type `UnknownTargetHost`. Both problem bodies name the offending value, and the missing-header and unknown-host bodies additionally list the valid hosts.

**Steps**:
1. [ ] - `p1` - Read `X-OAGW-Target-Host` and record whether the pool holds one endpoint or several - `inst-algo-endpoint-01`
2. [ ] - `p1` - **IF** the header is present - `inst-algo-endpoint-02`
   1. [ ] - `p1` - Reject with `400` and type `InvalidTargetHost` when the value is not a bare hostname or IP address - `inst-algo-endpoint-03`
   2. [ ] - `p1` - Reject with `400` and type `UnknownTargetHost` when no configured endpoint host equals the value - `inst-algo-endpoint-04`
   3. [ ] - `p1` - Select the matching endpoint and bypass round-robin distribution - `inst-algo-endpoint-05`
3. [ ] - `p1` - **ELSE** - `inst-algo-endpoint-06`
   1. [ ] - `p1` - Reject with `400` and type `MissingTargetHost` when the pool holds several endpoints behind a common-suffix alias - `inst-algo-endpoint-07`
   2. [ ] - `p1` - Select the single endpoint, or the next pool member by round-robin for an explicit alias - `inst-algo-endpoint-08`
4. [ ] - `p1` - Strip `X-OAGW-Target-Host` so the upstream never receives the routing header - `inst-algo-endpoint-09`
5. [ ] - `p1` - **RETURN** the selected endpoint scheme, host, and port - `inst-algo-endpoint-10`

### Upstream Invocation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-invocation`

**Input**: The selected endpoint, the outbound method, path, query, headers, and body, and the configured proxy timeout.

**Output**: A complete upstream response, or a classified transport failure.

The gear configuration key `proxy_timeout_secs` bounds connection establishment, response completion, and idle periods on the proxied exchange. The graded deployment sets it to two seconds. A connect-phase expiry maps to `ConnectionTimeout`, a response that never completes maps to `RequestTimeout`, and a stalled data flow maps to `IdleTimeout`; each answers with status `504`.

The plaintext-connection policy is evaluated immediately before the connection is opened. When the endpoint scheme is `http` or `ws` and `allow_http_upstream` is disabled, the connection is refused with status `503` and error type `LinkUnavailable`, and no socket is opened. The refusal is uniform across both schemes, and this single path is reused unchanged by the streaming feature when it prepares a `ws` upgrade. Accepting these schemes at create time is the management surface's concern, not this feature's. In the graded configuration the flag is enabled, so plaintext endpoints are reachable.

Status `502` with error type `ProtocolError` is reserved for a different situation: an upgrade handshake answering with any status other than `101`, or an endpoint scheme that cannot serve the requested transport. A plaintext refusal never surfaces as a protocol error, and a protocol error never surfaces as a link-unavailable response.

The outbound HTTP version is negotiated per host. The first call attempts HTTP/2 through ALPN during the TLS handshake, falls back to HTTP/1.1 on failure, and caches the result for that host for one hour. Later calls reuse the cached version. Connection or endpoint-level failover inside the connector is permitted, but the original client request is never re-issued as a whole.

**Steps**:
1. [ ] - `p1` - **IF** the endpoint scheme is `http` or `ws` and the plaintext-upstream policy is disabled - `inst-algo-invoke-01`
   1. [ ] - `p1` - Refuse at connection time with status `503` and type `LinkUnavailable` - `inst-algo-invoke-02`
2. [ ] - `p1` - Resolve the negotiated HTTP version for the host from the one-hour capability cache - `inst-algo-invoke-03`
3. [ ] - `p1` - **TRY** - `inst-algo-invoke-04`
   1. [ ] - `p1` - API: outbound call to the selected endpoint scheme, host, port, and path within the configured proxy timeout - `inst-algo-invoke-05`
   2. [ ] - `p1` - Stream the response status, headers, and body back without caching any part of it - `inst-algo-invoke-06`
4. [ ] - `p1` - **CATCH** timeout, connection, or protocol failure - `inst-algo-invoke-07`
   1. [ ] - `p1` - Classify it as a connection, request, or idle timeout, a link failure, or a protocol failure - `inst-algo-invoke-08`
   2. [ ] - `p1` - Hand the classification to `cpt-cf-oagw-algo-error-mapping` without retrying the client request - `inst-algo-invoke-09`
5. [ ] - `p1` - Record request count, duration, and error type for the call to satisfy the observability requirement - `inst-algo-invoke-10`
6. [ ] - `p1` - **RETURN** the upstream response or the classified failure - `inst-algo-invoke-11`

### Error Mapping And Source Stamping

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-error-mapping`

**Input**: An error type produced anywhere in the proxy lifecycle, or a complete upstream response.

**Output**: The client-facing status, headers, and body, always carrying the `X-OAGW-Error-Source` header.

A gateway-originated failure is rendered as RFC 9457 problem details with media type `application/problem+json` and header `X-OAGW-Error-Source: gateway`. An upstream response is passed through unmodified, including its `4xx` and `5xx` statuses and its original body, and carries `X-OAGW-Error-Source: upstream`. The gateway never rewrites an upstream body into problem details.

| Error type | Status | GTS type identifier |
|---|---|---|
| RouteError | 400 | `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` |
| ValidationError | 400 | `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` |
| MissingTargetHost | 400 | `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` |
| InvalidTargetHost | 400 | `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1` |
| UnknownTargetHost | 400 | `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1` |
| AuthenticationFailed | 401 | `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` |
| RouteNotFound | 404 | `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` |
| PluginInUse | 409 | `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` |
| PayloadTooLarge | 413 | `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` |
| RateLimitExceeded | 429 | `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` |
| SecretNotFound | 500 | `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` |
| ProtocolError | 502 | `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1` |
| DownstreamError | 502 | `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` |
| StreamAborted | 502 | `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` |
| LinkUnavailable | 503 | `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` |
| CircuitBreakerOpen | 503 | `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` |
| PluginNotFound | 503 | `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1` |
| ConnectionTimeout | 504 | `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1` |
| RequestTimeout | 504 | `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` |
| IdleTimeout | 504 | `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` |
| CORS origin not allowed | 403 | `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` |
| CORS method not allowed | 403 | `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` |

`PluginInUse`, `RateLimitExceeded`, `SecretNotFound`, `PluginNotFound`, `StreamAborted`, `AuthenticationFailed`, and `CircuitBreakerOpen` are rendered by this mapper when raised elsewhere. The mapper owns the rendering, while raising those conditions belongs to the management, plugin-runtime, streaming, and future circuit-breaker work. `AuthenticationFailed` originates at the platform gateway that enforces authentication and authorization ahead of this gear, which is why neither is implemented here. The table mirrors the gear-wide error contract, so it also lists `RouteError`, which no path in this feature raises.

A response-phase guard rejection deliberately reuses the `DownstreamError` identifier that also covers transport-level upstream failure, and the plugin-runtime feature is the raiser of that response-phase case.

Two resolution outcomes reach this mapper before any guard runs, and each renders as a fixed status and error type.

| Resolution outcome | Status | Error type |
|---|---|---|
| No upstream carries the alias, or no candidate route lists the inbound method and path | 404 | RouteNotFound |
| Every hierarchy tier carrying the alias holds only disabled upstreams | 503 | LinkUnavailable |

**Steps**:
1. [ ] - `p1` - **IF** the outcome is a complete upstream response - `inst-algo-error-01`
   1. [ ] - `p1` - Keep the status, body bytes, and content type exactly as received from the upstream - `inst-algo-error-02`
   2. [ ] - `p1` - Set `X-OAGW-Error-Source: upstream` and **RETURN** the passthrough response - `inst-algo-error-03`
2. [ ] - `p1` - Look the gateway error type up in the mapping table to obtain its status and GTS identifier - `inst-algo-error-04`
3. [ ] - `p1` - Build the problem body with `type`, `title`, `status`, `detail`, and `instance` set to the request path - `inst-algo-error-05`
4. [ ] - `p1` - Add the context extensions `upstream_id`, `host`, `path`, and `trace_id` when they are known - `inst-algo-error-06`
5. [ ] - `p1` - Add `retry_after_seconds` and the matching `Retry-After` header for retriable throttling outcomes - `inst-algo-error-07`
6. [ ] - `p1` - Set `Content-Type: application/problem+json` and `X-OAGW-Error-Source: gateway`, and exclude every credential value - `inst-algo-error-08`
7. [ ] - `p1` - **RETURN** the rendered gateway error response - `inst-algo-error-09`

## 4. States (CDSL)

### Proxy Request Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-proxy-request-lifecycle`

**States**: Received, PreflightAnswered, Resolved, Guarded, Prepared, Dispatched, Completed, Rejected, Failed

**Initial State**: Received

**Transitions**:
1. [ ] - `p1` - **FROM** Received **TO** PreflightAnswered **WHEN** the request is an `OPTIONS` preflight carrying origin and requested method - `inst-state-01`
2. [ ] - `p1` - **FROM** Received **TO** Resolved **WHEN** configuration resolution returns an effective configuration and a matched route - `inst-state-02`
3. [ ] - `p1` - **FROM** Received **TO** Rejected **WHEN** resolution reports an unknown alias, a disabled upstream, or no matching route - `inst-state-03`
4. [ ] - `p1` - **FROM** Resolved **TO** Guarded **WHEN** the CORS, query, path-suffix, and body checks all pass - `inst-state-04`
5. [ ] - `p1` - **FROM** Resolved **TO** Rejected **WHEN** any guard or body check refuses the request - `inst-state-05`
6. [ ] - `p1` - **FROM** Guarded **TO** Prepared **WHEN** the header plan is applied and one endpoint is selected - `inst-state-06`
7. [ ] - `p1` - **FROM** Guarded **TO** Rejected **WHEN** the target-host header is missing, malformed, or unknown - `inst-state-07`
8. [ ] - `p1` - **FROM** Prepared **TO** Dispatched **WHEN** the connection is opened and the outbound request is sent - `inst-state-08`
9. [ ] - `p1` - **FROM** Prepared **TO** Failed **WHEN** the plaintext-connection policy refuses the endpoint scheme - `inst-state-09`
10. [ ] - `p1` - **FROM** Dispatched **TO** Completed **WHEN** the upstream returns a complete response of any status - `inst-state-10`
11. [ ] - `p1` - **FROM** Dispatched **TO** Failed **WHEN** the configured proxy timeout expires or the transport fails - `inst-state-11`
12. [ ] - `p1` - **FROM** Rejected **TO** Completed **WHEN** the gateway problem-details response is written to the caller - `inst-state-12`
13. [ ] - `p1` - **FROM** Failed **TO** Completed **WHEN** the gateway problem-details response is written to the caller - `inst-state-13`
14. [ ] - `p1` - **FROM** PreflightAnswered **TO** Completed **WHEN** the permissive `204` response is written to the caller - `inst-state-14`

## 5. Definitions of Done

Specific implementation tasks derived from the flows and processes above.

### Proxy Endpoint Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-endpoint-registration`

The system **MUST** register the proxy data-plane endpoints gear-relative, accepting every HTTP method on both `/oagw/v1/proxy/{alias}` and `/oagw/v1/proxy/{alias}/{path}`, and it **MUST NOT** register any `/api` prefix itself.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request-success`

**Constraints**: `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}`
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Proxy request context

### Disabled Upstream Rejection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-disabled-upstream-rejection`

The system **MUST** render the upstream-disabled outcome reported by configuration resolution as status `503` with error type `LinkUnavailable`, before any guard runs and without opening a connection.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request-success`
- `cpt-cf-oagw-algo-error-mapping`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Problem details error response

### Query Allowlist Guard

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-query-allowlist-guard`

The system **MUST** reject a request carrying any query parameter absent from `match.http.query_allowlist` with status `400` and error type `ValidationError`, treating an empty allowlist as permitting no parameter.

**Implements**:
- `cpt-cf-oagw-flow-proxy-guard-rejection`
- `cpt-cf-oagw-algo-guard-evaluation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Guard rule set

### Path Suffix Guard

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-path-suffix-guard`

The system **MUST** reject a request that supplies a path suffix while the matched route sets `path_suffix_mode` to `disabled`, answering with status `400` and error type `ValidationError`.

**Implements**:
- `cpt-cf-oagw-flow-proxy-guard-rejection`
- `cpt-cf-oagw-algo-guard-evaluation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Guard rule set

### Body Validation And Size Limit

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-body-validation`

The system **MUST** validate that `Content-Length` parses as a non-negative integer and matches the received byte count, **MUST** refuse any `Transfer-Encoding` other than `chunked`, and **MUST** refuse a body above 104857600 bytes with status `413` before buffering it.

**Implements**:
- `cpt-cf-oagw-algo-body-validation`
- `cpt-cf-oagw-flow-proxy-guard-rejection`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Body validation outcome

### Hop-By-Hop Header Stripping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-hop-by-hop-stripping`

The system **MUST** strip `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, and `Upgrade` from every outbound request, and **MUST** refuse a header value containing a carriage return or line feed.

**Implements**:
- `cpt-cf-oagw-algo-header-transformation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Proxy request context

### Header Plan Application

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-header-plan-application`

The system **MUST** apply the effective request header plan in the order remove, set, add after the configured passthrough mode, **MUST** rewrite `Host` to the selected upstream host and `:authority` on HTTP/2, and **MUST** apply the response header plan before answering the caller.

**Implements**:
- `cpt-cf-oagw-algo-header-transformation`
- `cpt-cf-oagw-flow-proxy-request-success`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Proxy request context, Proxy response context

### Target Host Selection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-target-host-selection`

The system **MUST** consume `X-OAGW-Target-Host` for endpoint selection and then strip it, **MUST** distribute multi-endpoint pools by round-robin when no header is supplied and the alias is explicit, and **MUST** answer `400` with `MissingTargetHost`, `InvalidTargetHost`, or `UnknownTargetHost` per the documented behaviour matrix.

**Implements**:
- `cpt-cf-oagw-algo-endpoint-selection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Proxy request context

### Plaintext Connection Policy

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plaintext-connection-policy`

The system **MUST** consult `allow_http_upstream` immediately before opening a connection to an `http` or `ws` endpoint, **MUST** refuse the connection with status `503` and error type `LinkUnavailable` when the flag is disabled, and **MUST** open the connection normally when it is enabled.

The system **MUST** expose this single policy path for both schemes, so the streaming feature reuses it unchanged for a `ws` endpoint instead of defining a second plaintext rule.

**Implements**:
- `cpt-cf-oagw-algo-upstream-invocation`
- `cpt-cf-oagw-flow-proxy-upstream-failure`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Proxy request context

### Proxy Timeout Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-timeout`

The system **MUST** bound connection establishment, response completion, and idle periods by the configured `proxy_timeout_secs` value, and **MUST** answer `504` with error type `ConnectionTimeout`, `RequestTimeout`, or `IdleTimeout` according to the phase that expired.

**Implements**:
- `cpt-cf-oagw-algo-upstream-invocation`
- `cpt-cf-oagw-flow-proxy-upstream-failure`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Proxy request context

### Error Mapping And Source Stamping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-mapping`

The system **MUST** render every gateway-originated failure as `application/problem+json` with `X-OAGW-Error-Source: gateway` and the documented status for its error type, and **MUST** pass upstream responses through unmodified with `X-OAGW-Error-Source: upstream`, including upstream `4xx` and `5xx` statuses.

**Implements**:
- `cpt-cf-oagw-algo-error-mapping`
- `cpt-cf-oagw-flow-proxy-upstream-failure`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Problem details error response

### CORS Preflight Handling

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-preflight`

The system **MUST** answer an `OPTIONS` request carrying `Origin` and `Access-Control-Request-Method` with `204 No Content` before upstream resolution, echoing origin, method, and requested headers, and setting `Access-Control-Max-Age: 86400` plus the documented `Vary` header.

**Implements**:
- `cpt-cf-oagw-flow-cors-preflight`

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}/{path}`
- Entities: CORS configuration

### CORS Request Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-request-enforcement`

The system **MUST** validate an actual cross-origin request against the effective CORS configuration after resolution, **MUST** answer `403` when the origin or the method is not allowed, and **MUST** add the allow-origin, expose-headers, allow-credentials, and `Vary: Origin` headers on permitted responses.

**Implements**:
- `cpt-cf-oagw-algo-guard-evaluation`
- `cpt-cf-oagw-flow-proxy-guard-rejection`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: CORS configuration

### Plugin And Rate Limit Hook Points

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-hook-points`

The system **MUST** expose exactly one lifecycle position for the auth, guard, request-transform, rate-limit, and response-transform hooks, invoked after resolution and body validation and around the upstream call, so plugin and rate-limit behaviour attaches without changing this lifecycle.

**Implements**:
- `cpt-cf-oagw-flow-proxy-request-success`
- `cpt-cf-oagw-algo-upstream-invocation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: Proxy request context

## 6. Acceptance Criteria

- [ ] `GET /oagw/v1/proxy/api.example.com/v1/items` against a healthy upstream returns the upstream status `200`, the upstream body byte-for-byte, and header `X-OAGW-Error-Source: upstream`.
- [ ] `POST /oagw/v1/proxy/api.example.com/v1/items` with `Content-Type: application/json` and a 32-byte body forwards the same method, path, and body bytes to the upstream and returns its `201` response.
- [ ] An upstream answering `500` with body `{"err":"boom"}` produces client status `500`, that exact body, and header `X-OAGW-Error-Source: upstream`, with no problem-details substitution.
- [ ] An upstream answering `404` produces client status `404` with header `X-OAGW-Error-Source: upstream`, distinguishing it from the gateway `RouteNotFound` response.
- [ ] Any gateway-originated failure returns `Content-Type: application/problem+json`, header `X-OAGW-Error-Source: gateway`, and a body containing the fields `type`, `title`, `status`, and `instance`.
- [ ] `GET /oagw/v1/proxy/does-not-exist/v1/items` returns `404` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` and header `X-OAGW-Error-Source: gateway`.
- [ ] `DELETE /oagw/v1/proxy/api.example.com/v1/items` against a route whose methods are `GET` and `POST` returns `404` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`, and the upstream receives no request.
- [ ] `GET /oagw/v1/proxy/api.example.com/v1/items` whose alias resolves only to upstreams with `enabled` set to `false` returns `503` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`.
- [ ] That disabled-upstream response carries `X-OAGW-Error-Source: gateway`, and no socket is opened toward any endpoint of the disabled upstream.
- [ ] `GET /oagw/v1/proxy/api.example.com/v1/items?debug=1` against a route whose `query_allowlist` omits `debug` returns `400` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`.
- [ ] `GET /oagw/v1/proxy/api.example.com/v1/items/extra` against a route with `path_suffix_mode` set to `disabled` returns `400` and the upstream receives no request.
- [ ] A `POST` declaring `Content-Length: 104857601` returns `413` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` and no body byte is buffered.
- [ ] A chunked `POST` whose streamed body crosses 104857600 bytes returns `413` and the upstream connection is closed without forwarding the remainder.
- [ ] A `POST` sending both `Content-Length: 10` and `Transfer-Encoding: chunked` returns `400` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`.
- [ ] A `POST` sending `Transfer-Encoding: gzip` returns `400`, while `Transfer-Encoding: chunked` is accepted and forwarded.
- [ ] A request carrying `Connection: keep-alive`, `TE: trailers`, `Trailer: X-T`, `Upgrade: h2c`, `Keep-Alive: timeout=5`, `Proxy-Authenticate: Basic`, `Proxy-Authorization: Basic abc`, and `Transfer-Encoding: chunked` reaches the upstream with none of those eight header names present.
- [ ] A request to an upstream endpoint host `origin.example.com` reaches the upstream with `Host: origin.example.com`, regardless of the inbound `Host` value.
- [ ] A request header plan with `remove: [X-Drop]`, `set: {X-Set: a}`, and `add: {X-Add: b}` yields an upstream request without `X-Drop`, with `X-Set: a`, and with `X-Add: b` appended.
- [ ] A response header plan removing `Server` and setting `X-Gw: 1` yields a client response without `Server` and with `X-Gw: 1`, while the upstream body stays unchanged.
- [ ] A request carrying `X-OAGW-Target-Host: us.vendor.com` to a two-endpoint pool reaches `us.vendor.com`, and the upstream never receives the `X-OAGW-Target-Host` header.
- [ ] A request to a common-suffix alias `vendor.com` with endpoints `us.vendor.com` and `eu.vendor.com` and no target-host header returns `400` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1`.
- [ ] A request carrying `X-OAGW-Target-Host: us.vendor.com:8443` returns `400` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1`.
- [ ] A request carrying `X-OAGW-Target-Host: apac.vendor.com` against endpoints `us.vendor.com` and `eu.vendor.com` returns `400` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1`.
- [ ] Ten sequential requests to a two-endpoint explicit-alias pool with no target-host header reach each endpoint five times, confirming round-robin distribution.
- [ ] An upstream that never answers within the configured `proxy_timeout_secs` of two seconds returns `504` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` and header `X-OAGW-Error-Source: gateway`.
- [ ] An endpoint that refuses connections returns `502` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` and the client request is never re-issued.
- [ ] With `allow_http_upstream` disabled, a request to an `http` endpoint returns `503` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` and no socket is opened.
- [ ] With `allow_http_upstream` enabled, the same request to the `http` endpoint is forwarded and the upstream response is returned unchanged.
- [ ] With `allow_http_upstream` disabled, a request to a `ws` endpoint returns `503` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`, matching the `http` outcome exactly.
- [ ] `OPTIONS /oagw/v1/proxy/api.example.com/v1/items` with `Origin: https://app.example.com`, `Access-Control-Request-Method: POST`, and `Access-Control-Request-Headers: Content-Type, Authorization` returns `204` with an empty body.
- [ ] That preflight response carries `Access-Control-Allow-Origin: https://app.example.com`, `Access-Control-Allow-Methods: POST`, `Access-Control-Allow-Headers: Content-Type, Authorization`, `Access-Control-Max-Age: 86400`, and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`.
- [ ] The preflight succeeds without any upstream resolution, so it also returns `204` for an alias that no upstream defines.
- [ ] A `POST` with `Origin: https://evil.com` against a CORS configuration allowing only `https://app.example.com` returns `403` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and the upstream receives no request.
- [ ] A `DELETE` with `Origin: https://app.example.com` against `allowed_methods` of `GET` and `POST` returns `403` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`.
- [ ] A permitted cross-origin `GET` returns `Access-Control-Allow-Origin: https://app.example.com`, `Vary: Origin`, and `Access-Control-Expose-Headers: X-Request-ID` when those headers are configured for exposure.
- [ ] A request whose upstream has CORS disabled receives no `Access-Control-Allow-Origin` header and is forwarded without origin enforcement.
- [ ] Every proxy response, successful or failed, carries the `X-OAGW-Error-Source` header, and no problem-details body contains any credential value.
