# Feature: Gear Foundation, Configuration and Error Model


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Operator Verifies the Mounted Gear Surface Is Live](#operator-verifies-the-mounted-gear-surface-is-live)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Gear Self-Registration and Type Registration](#gear-self-registration-and-type-registration)
  - [Gear Configuration Resolution](#gear-configuration-resolution)
  - [Gear Router Mounting](#gear-router-mounting)
  - [Control-Plane State Initialization](#control-plane-state-initialization)
  - [Error-to-Problem-Document Mapping](#error-to-problem-document-mapping)
  - [Error-Source Header Stamping](#error-source-header-stamping)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Lifecycle State Machine](#gear-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Gear Registers With the Host Runtime](#gear-registers-with-the-host-runtime)
  - [Gear Configuration Resolves Once at Startup](#gear-configuration-resolves-once-at-startup)
  - [Gear Router Mounts Gear-Relative](#gear-router-mounts-gear-relative)
  - [Unmatched Requests Under the Mount Return a Well-Formed Error](#unmatched-requests-under-the-mount-return-a-well-formed-error)
  - [In-Process Control-Plane State Initializes at Startup](#in-process-control-plane-state-initializes-at-startup)
  - [Every Gateway Error Is Shaped as an RFC 9457 Problem Document](#every-gateway-error-is-shaped-as-an-rfc-9457-problem-document)
  - [Error Types Map to a Fixed, Documented HTTP Status Table](#error-types-map-to-a-fixed-documented-http-status-table)
  - [Every Response Carries the Error-Source Header](#every-response-carries-the-error-source-header)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-gear-foundation`
## 1. Feature Context

### 1.1 Overview

This feature establishes OAGW as a loadable gear: it registers with the host runtime, resolves its
configuration once at startup, mounts its router gear-relative, and defines the shared error contract
every later feature builds on.

### 1.2 Purpose

Later features add concrete upstream, route, plugin, and proxy endpoints. None of them can exist until
the gear can be loaded and stopped by the platform, its configuration is resolved, its router is mounted
at a stable gear-relative path, and every response — success or failure — carries a consistent error
shape and source attribution. This feature fixes that foundation once, so downstream features only add
routes and behavior, never re-derive startup, configuration, or error handling.

**Requirements**: `cpt-cf-oagw-fr-error-codes`, `cpt-cf-oagw-contract-types-registry`

**Principles**: `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-tenant-scope`

Inbound authentication and authorization for every request reaching this gear are enforced by the
platform's API gateway ahead of this gear's mounted router; this feature performs no credential or
permission check of its own. The gear renders `AuthenticationFailed` as a `401` problem document only
when that error is raised elsewhere in the request path, never as a verification step this feature
carries out itself.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Deploys the gear, observes that it registered with the host runtime, and confirms the mounted surface responds correctly before any downstream feature adds routes beneath it. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: None. This feature has no upstream feature dependency; `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-plugin-management` both depend on it, per DECOMPOSITION.md §3, because they need its mounted router, gear configuration, and error model before exposing their own endpoints.

## 2. Actor Flows (CDSL)

### Operator Verifies the Mounted Gear Surface Is Live

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-mounted-surface-check`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Before any downstream feature has registered a route, the operator's request to a path under the
  gear-relative mount still returns a well-formed problem-details response, confirming the gear
  registered with the host runtime and its router is mounted and reachable.

**Error Scenarios**:
- The gear failed to register or its configuration failed to resolve at startup; the host runtime never
  starts the gear, so no response is produced by the gear at all. Diagnosing that startup failure is a
  host-runtime concern, out of scope for this feature.

**Steps**:
1. [ ] - `p1` - Operator sends an HTTP request to a path prefixed with `/oagw/v1/` for which no
   downstream feature has yet registered a handler - `inst-mount-check-request`
2. [ ] - `p1` - System receives the request on the mounted gear router - `inst-mount-check-receive`
3. [ ] - `p1` - **IF** no registered handler matches the request path - `inst-mount-check-if-unmatched`
   1. [ ] - `p1` - System selects the RouteNotFound error type from the documented error-type-to-status
      mapping - `inst-mount-check-select-error`
4. [ ] - `p1` - System builds the RFC 9457 `application/problem+json` body for the selected error type
   - `inst-mount-check-build-problem`
5. [ ] - `p1` - System stamps `X-OAGW-Error-Source: gateway` on the response - `inst-mount-check-stamp-header`
6. [ ] - `p1` - **RETURN** the `404` response with the `application/problem+json` body and the stamped
   header, confirming the gear is registered, its router is mounted, and its error model is active -
   `inst-mount-check-return`

## 3. Processes / Business Logic (CDSL)

### Gear Self-Registration and Type Registration

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-gear-registration`

**Input**: The gear, as loaded by the host runtime at platform startup.

**Output**: A registered gear instance the host runtime can start and stop, with its schemas known to
the types registry.

**Steps**:
1. [ ] - `p1` - Parse the gear's declared schemas that must be known to the types registry -
   `inst-gear-reg-parse-schemas`
2. [ ] - `p1` - Register those schemas with the types registry so upstream, route, and plugin instances
   can later be validated against them - `inst-gear-reg-register-schemas`
3. [ ] - `p1` - **TRY** registering the gear's start and stop lifecycle hooks with the host runtime -
   `inst-gear-reg-try-hooks`
4. [ ] - `p1` - **CATCH** a registration or schema-registration failure - `inst-gear-reg-catch`
   1. [ ] - `p1` - Fail gear startup and surface the failure to the host runtime; the gear does not
      reach the started state - `inst-gear-reg-fail-startup`
5. [ ] - `p1` - **RETURN** the registered gear, ready for configuration resolution -
   `inst-gear-reg-return`

### Gear Configuration Resolution

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-config-resolution`

**Input**: The gear's configuration section as supplied at process startup, including
`proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled`.

**Output**: A resolved, immutable configuration snapshot held for the life of the process.

**Steps**:
1. [ ] - `p1` - Parse the `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled` keys
   from the gear configuration section - `inst-config-res-parse`
2. [ ] - `p1` - **TRY** validating each key against its expected type and bounds -
   `inst-config-res-try-validate`
3. [ ] - `p1` - **CATCH** an invalid or missing required value - `inst-config-res-catch`
   1. [ ] - `p1` - Fail gear startup with a validation error naming the offending key -
      `inst-config-res-fail`
4. [ ] - `p1` - **RETURN** the resolved configuration snapshot, computed once and reused for every
   later request instead of being re-parsed - `inst-config-res-return`

### Gear Router Mounting

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-router-mount`

**Input**: The platform's REST capability wiring, available once the gear is registered.

**Output**: The gear's router, mounted gear-relative at `/oagw/v1/...`, ready for downstream features to
register routes beneath it.

**Steps**:
1. [ ] - `p1` - Parse the gear-relative mount path `/oagw/v1` supplied to the gear's REST capability
   wiring - `inst-router-mount-parse-path`
2. [ ] - `p1` - Mount the gear's router beneath that path, independent of any prefix path applied by an
   operator-facing gateway in front of this gear - `inst-router-mount-attach`
3. [ ] - `p1` - **FOR EACH** request that reaches the mounted router with no matching downstream-registered
   route - `inst-router-mount-for-each-unmatched`
   1. [ ] - `p1` - Apply the RouteNotFound error-to-problem-document mapping as the fallback response -
      `inst-router-mount-apply-fallback`
4. [ ] - `p1` - **RETURN** the mounted router reference so downstream features can register their own
   routes beneath it - `inst-router-mount-return`

### Control-Plane State Initialization

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-control-plane-init`

**Input**: The started gear instance, after its router is mounted.

**Output**: An empty, in-process control-plane state structure, ready to hold upstream, route, and
plugin configuration for the life of the process.

**Steps**:
1. [ ] - `p1` - Parse the shape of the initial, empty control-plane state structure for upstreams,
   routes, and plugins - `inst-cp-init-parse-shape`
2. [ ] - `p1` - Initialize that structure in-process; no database connection is opened, because
   control-plane persistence is in-process for this deployment - `inst-cp-init-initialize`
3. [ ] - `p1` - **RETURN** the initialized control-plane state handle for later features to read and
   write - `inst-cp-init-return`

### Error-to-Problem-Document Mapping

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-error-to-problem-mapping`

**Input**: An internal error raised by any part of the gear, management or data plane.

**Output**: An RFC 9457 `application/problem+json` body paired with its mapped HTTP status.

**Steps**:
1. [ ] - `p1` - Parse the raised error into one of the documented error types (for example
   `ValidationError`, `AuthenticationFailed`, `RouteNotFound`, `PayloadTooLarge`, `RateLimitExceeded`,
   `SecretNotFound`, `DownstreamError`, `CircuitBreakerOpen`, `ConnectionTimeout`, `RequestTimeout`,
   `IdleTimeout`) - `inst-err-map-parse-type`
2. [ ] - `p1` - Look up the HTTP status and GTS `type` identifier for that error type in the documented
   error-type-to-status mapping - `inst-err-map-lookup`
3. [ ] - `p1` - **FOR EACH** applicable extension field (`upstream_id`, `host`, `path`,
   `retry_after_seconds`, `trace_id`) present in the error's context - `inst-err-map-for-each-extension`
   1. [ ] - `p1` - Attach the extension field to the problem body - `inst-err-map-attach-extension`
4. [ ] - `p1` - **TRY** serializing the `type`, `title`, `status`, `detail`, and `instance` fields into
   the problem body - `inst-err-map-try-serialize`
5. [ ] - `p1` - **CATCH** a serialization failure - `inst-err-map-catch`
   1. [ ] - `p1` - Fall back to the `Internal` error type mapped to status `500` -
      `inst-err-map-fallback-internal`
6. [ ] - `p1` - **RETURN** the problem body together with its mapped HTTP status -
   `inst-err-map-return`

### Error-Source Header Stamping

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-error-source-stamping`

**Input**: An outgoing response for a request, classified as either gateway-originated or an upstream
passthrough.

**Output**: The same response with `X-OAGW-Error-Source` set.

**Steps**:
1. [ ] - `p1` - Parse the origin classification of the outgoing response - `inst-src-stamp-parse-origin`
2. [ ] - `p1` - **IF** the response body and status were produced by gateway logic (validation, the
   mounted-router fallback, or gear-level error handling) - `inst-src-stamp-if-gateway`
   1. [ ] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-src-stamp-set-gateway`
3. [ ] - `p1` - **ELSE** the response is a passthrough of an upstream call established by later
   data-plane features - `inst-src-stamp-else-upstream`
   1. [ ] - `p1` - Set `X-OAGW-Error-Source: upstream` - `inst-src-stamp-set-upstream`
4. [ ] - `p1` - **RETURN** the response with the header attached, on every response the gear returns,
   success or failure - `inst-src-stamp-return`

## 4. States (CDSL)

### Gear Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-gear-lifecycle`

**States**: Unloaded, Registered, ConfigResolved, Started, Stopped, Failed

**Initial State**: Unloaded

**Transitions**:
1. [ ] - `p1` - **FROM** Unloaded **TO** Registered **WHEN** the host runtime loads the gear and its
   schemas register successfully with the types registry - `inst-lifecycle-unloaded-to-registered`
2. [ ] - `p1` - **FROM** Registered **TO** Failed **WHEN** gear or type registration fails -
   `inst-lifecycle-registered-to-failed`
3. [ ] - `p1` - **FROM** Registered **TO** ConfigResolved **WHEN** the gear configuration section
   parses and validates successfully - `inst-lifecycle-registered-to-config-resolved`
4. [ ] - `p1` - **FROM** Registered **TO** Failed **WHEN** configuration resolution fails -
   `inst-lifecycle-registered-config-fail`
5. [ ] - `p1` - **FROM** ConfigResolved **TO** Started **WHEN** the gear's router is mounted
   gear-relative and its in-process control-plane state is initialized -
   `inst-lifecycle-config-resolved-to-started`
6. [ ] - `p1` - **FROM** Started **TO** Stopped **WHEN** the host runtime stops the gear -
   `inst-lifecycle-started-to-stopped`

## 5. Definitions of Done

### Gear Registers With the Host Runtime

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-registration`

The system **MUST** register the gear with the host runtime at startup, including GTS type
registration for the gear's schemas, so the platform can load, start, and stop it.

**Implements**:
- `cpt-cf-oagw-algo-gear-registration`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- Entities: Gear configuration section (proxy timeout, plaintext-upstream policy, SSRF-guard toggling)

### Gear Configuration Resolves Once at Startup

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-config-resolution`

The system **MUST** resolve `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled` from
the gear configuration section exactly once at startup, and hold the resolved values for the life of
the process instead of re-parsing them per request.

**Implements**:
- `cpt-cf-oagw-algo-config-resolution`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- Entities: Gear configuration section (proxy timeout, plaintext-upstream policy, SSRF-guard toggling)

### Gear Router Mounts Gear-Relative

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-router-mount`

The system **MUST** mount the gear's router gear-relative at `/oagw/v1/...`, never under
`/api/oagw/v1/...`, so later features can register their own routes beneath it. Inbound authentication
and authorization for every request reaching the mounted router are enforced by the platform's API
gateway ahead of this router; the gear itself checks no credential or permission on any request.

**Implements**:
- `cpt-cf-oagw-algo-router-mount`
- `cpt-cf-oagw-flow-mounted-surface-check`

### Unmatched Requests Under the Mount Return a Well-Formed Error

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-unmounted-path-fallback`

The system **MUST** return the standard `RouteNotFound` problem-details response, with the
`X-OAGW-Error-Source` header stamped, for any request under `/oagw/v1/...` that matches no
downstream-registered route.

**Implements**:
- `cpt-cf-oagw-flow-mounted-surface-check`
- `cpt-cf-oagw-algo-error-to-problem-mapping`

### In-Process Control-Plane State Initializes at Startup

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-state`

The system **MUST** initialize an in-process control-plane state structure for upstream, route, and
plugin configuration at startup, held for the life of the process, with no database connection opened.

**Implements**:
- `cpt-cf-oagw-algo-control-plane-init`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

### Every Gateway Error Is Shaped as an RFC 9457 Problem Document

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-problem-details-shape`

The system **MUST** shape every gateway-originated error response as `application/problem+json`
carrying `type`, `title`, `status`, `detail`, and `instance`, plus applicable extension fields
(`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`).

**Implements**:
- `cpt-cf-oagw-algo-error-to-problem-mapping`

### Error Types Map to a Fixed, Documented HTTP Status Table

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-status-mapping`

The system **MUST** apply the documented error-type-to-HTTP-status table consistently, including at
minimum `ValidationError`:400, `AuthenticationFailed`:401, `RouteNotFound`:404, `PayloadTooLarge`:413,
`RateLimitExceeded`:429, `SecretNotFound`:500, `DownstreamError`:502, `CircuitBreakerOpen`:503,
`ConnectionTimeout`:504, `RequestTimeout`:504, and `IdleTimeout`:504, so every later feature reuses the
same mapping rather than defining its own.

**Implements**:
- `cpt-cf-oagw-algo-error-to-problem-mapping`

### Every Response Carries the Error-Source Header

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-source-header`

The system **MUST** stamp `X-OAGW-Error-Source: gateway` on every gateway-originated response and
`X-OAGW-Error-Source: upstream` on every upstream-passthrough response, on every response the gear
returns, not only on failures.

**Implements**:
- `cpt-cf-oagw-algo-error-source-stamping`

**Touches**:
- Entities: Problem Details error response

## 6. Acceptance Criteria

- [ ] At startup, the gear registers with the host runtime and its schemas register with the types
  registry without error; the host runtime can subsequently start and stop the gear.
- [ ] At startup, the gear resolves `proxy_timeout_secs`, `allow_http_upstream`, and
  `ssrf_policy.enabled` from its configuration section exactly once; every later read of these values
  returns the same resolved snapshot without re-parsing the raw configuration.
- [ ] Startup fails, and the gear does not start, if any of `proxy_timeout_secs`, `allow_http_upstream`,
  or `ssrf_policy.enabled` is present with an invalid type or out-of-bounds value.
- [ ] The gear's router is reachable at paths prefixed with `/oagw/v1/` and is never reachable at any
  path prefixed with `/api/oagw/v1/`.
- [ ] A request to a path under `/oagw/v1/` that matches no route registered by any downstream feature
  returns HTTP `404`, with `Content-Type: application/problem+json`, and body field `type` equal to
  `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`.
- [ ] Every gateway-originated `application/problem+json` response body includes non-empty `type`,
  `title`, `status`, `detail`, and `instance` fields.
- [ ] Every response returned by the gear, success or failure, includes an `X-OAGW-Error-Source`
  response header whose value is exactly `gateway` or `upstream`.
- [ ] A response whose body and status originate from gateway logic (not a passthrough of an upstream
  call) always carries `X-OAGW-Error-Source: gateway`.
- [ ] The documented error-type-to-status table returns `400` for `ValidationError`, `401` for
  `AuthenticationFailed`, `404` for `RouteNotFound`, `413` for `PayloadTooLarge`, `429` for
  `RateLimitExceeded`, `500` for `SecretNotFound`, `502` for `DownstreamError`, `503` for
  `CircuitBreakerOpen`, and `504` for each of `ConnectionTimeout`, `RequestTimeout`, and `IdleTimeout`.
- [ ] At startup, the gear opens no database connection and attempts no schema migration; control-plane
  state for upstreams, routes, and plugins is held entirely in-process for the life of the gear.
- [ ] This feature registers no concrete management or proxy resource endpoint; the mounted router
  responds only with the fallback `RouteNotFound` problem document until a downstream feature registers
  a route.
