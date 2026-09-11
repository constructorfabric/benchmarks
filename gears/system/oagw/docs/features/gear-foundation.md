# Feature: Gear Foundation


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Inbound Authenticated Request](#inbound-authenticated-request)
  - [Unmatched Route Produces a Structured Gateway Error](#unmatched-route-produces-a-structured-gateway-error)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Gear Registration and Startup](#gear-registration-and-startup)
  - [OagwConfig Deserialization and Defaulting](#oagwconfig-deserialization-and-defaulting)
  - [Gateway Error Construction](#gateway-error-construction)
  - [Bearer Token Authentication Gate](#bearer-token-authentication-gate)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Lifecycle State Machine](#gear-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Module Skeleton for the Control Plane / Data Plane Split](#module-skeleton-for-the-control-plane--data-plane-split)
  - [Gear Registration via Toolkit Macro and Inventory](#gear-registration-via-toolkit-macro-and-inventory)
  - [OagwConfig Deserialization with Field Defaults](#oagwconfig-deserialization-with-field-defaults)
  - [Base Router Mounted at /oagw/v1](#base-router-mounted-at-oagwv1)
  - [Shared RFC 9457 Problem Details Type](#shared-rfc-9457-problem-details-type)
  - [GTS Error-Type and HTTP Status Pairings](#gts-error-type-and-http-status-pairings)
  - [Universal X-OAGW-Error-Source Header](#universal-x-oagw-error-source-header)
  - [Inbound Bearer Token Authentication and Shared Permission Gate](#inbound-bearer-token-authentication-and-shared-permission-gate)
  - [External Dependency Handles Resolved from the Typed Client Registry](#external-dependency-handles-resolved-from-the-typed-client-registry)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-gear-foundation`
## 1. Feature Context

### 1.1 Overview

This feature builds the skeleton of the `oagw` gear: registration with the host runtime,
configuration loading, the base route mount, and the shared error model every later feature
attaches to.

### 1.2 Purpose

`gears/system/oagw/oagw/src/lib.rs` is currently empty. Before any resource CRUD or proxy
handler can exist, the gear must be discoverable by the host runtime, must load its own
configuration, must expose a mount point for routes, and must have one consistent way to
report errors. This feature builds that shared base so features 2 through 8 can attach
handlers, repositories, and plugins to it without re-deriving these mechanics.

**Requirements**: `cpt-cf-oagw-fr-error-codes`

**Principles**: `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Sends Bearer-token-authenticated requests to any route mounted under `/oagw/v1`; subject to the shared permission gate this feature establishes. |
| `cpt-cf-oagw-actor-tenant-admin` | Same inbound authentication gate applies before any tenant-scoped management call defined in later features. |
| `cpt-cf-oagw-actor-app-developer` | Calls proxy routes through the same base router and inbound authentication gate; receives an RFC 9457 (a standard, machine-readable HTTP error body format) gateway error when a call fails before reaching an upstream. |
| `cpt-cf-oagw-actor-types-registry` | Its handle is resolved and exposed from the typed client registry during this feature's startup, so later features register their own type schemas against an initialized registry, instead of against one this feature must reach. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-design-drivers` (§1.2 Architecture Drivers),
  `cpt-cf-oagw-design-layers` (§1.3 Architecture Layers: `domain/`, `infra/`, `api/rest/` split),
  `cpt-cf-oagw-tech-dependencies` (§1.3 Technology Dependencies), `cpt-cf-oagw-design-dependencies`
  (§3.4 Internal & External Dependencies), `cpt-cf-oagw-interface-api` (§3.3 API Contracts; only
  the base mount applies at this feature's gate)
- **ADRs**: [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md), [0001 Request Routing](../ADR/0001-request-routing.md)
- **Decomposition**: `cpt-cf-oagw-feature-gear-foundation`
- **Dependencies**: None

## 2. Actor Flows (CDSL)

**Use cases**: None. This feature has no dedicated PRD use case; it supplies the
authentication and error mechanics that later features' use cases depend on.

### Inbound Authenticated Request

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-inbound-auth`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request carrying a valid Bearer token and the required GTS permission is handed to its
  matched route handler.

**Error Scenarios**:
- A missing, malformed, or invalid Bearer token stops the request before any handler runs.
- A resolved principal lacking the permission required by the matched route is stopped the
  same way, using the one 401 error type DESIGN.md defines for authentication failure.

**Steps**:
1. [ ] - `p1` - Actor sends a request to a route under `/oagw/v1` carrying an `Authorization: Bearer <token>` header - `inst-auth-1`
2. [ ] - `p1` - API: `{METHOD} /oagw/v1/{path}` (request enters the gear's base router) - `inst-auth-2`
3. [ ] - `p1` - `toolkit-auth` (the platform's shared authentication library) validates the token and resolves a security principal - `inst-auth-3`
4. [ ] - `p1` - **IF** the token is missing, malformed, or invalid - `inst-auth-4`
   1. [ ] - `p1` - **RETURN** 401 `AuthenticationFailed` problem body (`application/problem+json`) with `X-OAGW-Error-Source: gateway` - `inst-auth-4a`
5. [ ] - `p1` - **ELSE** - `inst-auth-5`
   1. [ ] - `p1` - The shared permission-check mechanism evaluates the GTS permission required by the matched route against the resolved principal - `inst-auth-5a`
6. [ ] - `p1` - **IF** the resolved principal lacks the required permission - `inst-auth-6`
   1. [ ] - `p1` - **RETURN** 401 `AuthenticationFailed` problem body with `X-OAGW-Error-Source: gateway` - `inst-auth-6a`
7. [ ] - `p1` - **ELSE** - `inst-auth-7`
   1. [ ] - `p1` - **RETURN** control passed to the matched route handler, principal attached to the request context - `inst-auth-7a`

### Unmatched Route Produces a Structured Gateway Error

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-unmatched-route`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request to a path under `/oagw/v1` that no feature has registered a handler for receives
  a structured `RouteNotFound` problem body instead of a bare framework 404.
- The response still carries `X-OAGW-Error-Source: gateway` alongside the problem body.

**Error Scenarios**:
- None beyond the case above; this flow is itself the gear's fallback for unmatched paths.

**Steps**:
1. [ ] - `p1` - Actor sends a request to `/oagw/v1/{unregistered-path}` after passing Bearer token authentication - `inst-unmatched-1`
2. [ ] - `p1` - API: `{METHOD} /oagw/v1/{unregistered-path}` (no handler is registered for this path in this build) - `inst-unmatched-2`
3. [ ] - `p1` - The base router's fallback handler builds a `RouteNotFound` problem body carrying `type`, `title`, `status`, `detail`, `instance`, and `trace_id` - `inst-unmatched-3`
4. [ ] - `p1` - **RETURN** 404 response, `Content-Type: application/problem+json`, `X-OAGW-Error-Source: gateway` - `inst-unmatched-4`

## 3. Processes / Business Logic (CDSL)

### Gear Registration and Startup

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gear-foundation-registration`

**Input**: Host runtime startup event (the runtime's gear discovery pass)

**Output**: `oagw` gear instance registered, base router mounted, and the `types_registry`,
`credstore`, `authz-resolver`, and `tenant-resolver` handles resolved and exposed

**Steps**:
1. [ ] - `p1` - The host runtime iterates gears collected by `inventory` (a Rust crate that gathers `#[toolkit::gear(...)]`-tagged items at compile time for later discovery) - `inst-reg-1`
2. [ ] - `p1` - The `oagw` gear's `#[toolkit::gear(...)]` entry point is invoked with handles to `types_registry`, `cred_store`, `api_ingress`, `toolkit-db`, and `toolkit-auth` - `inst-reg-2`
3. [ ] - `p1` - **TRY** - `inst-reg-3`
   1. [ ] - `p1` - Deserialize `gears.oagw.config` into `OagwConfig` (delegates to `cpt-cf-oagw-algo-gear-foundation-config-load`) - `inst-reg-3a`
4. [ ] - `p1` - **CATCH** a config deserialization failure - `inst-reg-4`
   1. [ ] - `p1` - Abort gear startup with a descriptive error, so the host runtime fails fast rather than serving with an unknown config - `inst-reg-4a`
5. [ ] - `p1` - Mount the base router at `/oagw/v1` (Override 1: gear-relative, no `/api` prefix), giving later features a place to register resource routes - `inst-reg-5`
6. [ ] - `p1` - Register the Bearer-token authentication gate (`cpt-cf-oagw-algo-gear-foundation-bearer-auth`) on the mounted router - `inst-reg-6`
7. [ ] - `p1` - Resolve and expose the `types_registry`, `credstore`, `authz-resolver`, and `tenant-resolver` handles from the typed client registry, so later features register against an initialized dependency rather than an absent one - `inst-reg-7`
8. [ ] - `p1` - **RETURN** gear ready for request handling - `inst-reg-8`

### OagwConfig Deserialization and Defaulting

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gear-foundation-config-load`

**Input**: Raw `gears.oagw.config` YAML node, present or absent

**Output**: A fully populated `OagwConfig` value

**Steps**:
1. [ ] - `p1` - **IF** the `gears.oagw.config` block is absent from the supplied configuration file - `inst-cfgload-1`
   1. [ ] - `p1` - Use the documented default for every field: `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled` - `inst-cfgload-1a`
2. [ ] - `p1` - **ELSE** - `inst-cfgload-2`
   1. [ ] - `p1` - Deserialize the present block, applying the same per-field default to any field the block omits - `inst-cfgload-2a`
3. [ ] - `p1` - **FOR EACH** field in the resulting `OagwConfig` - `inst-cfgload-3`
   1. [ ] - `p1` - Validate the field deserializes as the type `OagwConfig` declares for it; for
      example, `proxy_timeout_secs` must deserialize as the integer type `OagwConfig` declares - `inst-cfgload-3a`
4. [ ] - `p1` - **IF** any field's value fails that type check - `inst-cfgload-4`
   1. [ ] - `p1` - **RETURN** a config deserialization error to the caller; this surfaces through
      the same channel the registration algorithm's **CATCH** (`inst-reg-4`) already handles - `inst-cfgload-4a`
5. [ ] - `p1` - **ELSE** - `inst-cfgload-5`
   1. [ ] - `p1` - **RETURN** the populated `OagwConfig` - `inst-cfgload-5a`

### Gateway Error Construction

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gear-foundation-error-construct`

**Input**: An internal error condition raised by a handler (this feature's own fallback, or a
later feature's handler)

**Output**: An RFC 9457 problem body (`application/problem+json`) carrying
`X-OAGW-Error-Source: gateway`

**Steps**:
1. [ ] - `p1` - Map the internal error condition to one of the GTS error-type identifiers and its HTTP status from DESIGN.md's error table (§3.3): `RouteError`/`ValidationError` (400), `MissingTargetHost` (400), `InvalidTargetHost` (400), `UnknownTargetHost` (400), `AuthenticationFailed` (401), `RouteNotFound` (404), `PluginInUse` (409), `PayloadTooLarge` (413), `RateLimitExceeded` (429), `SecretNotFound` (500), `ProtocolError` (502), `DownstreamError` (502), `StreamAborted` (502), `LinkUnavailable` (503), `CircuitBreakerOpen` (503), `PluginNotFound` (503), `ConnectionTimeout` (504), `RequestTimeout` (504), `IdleTimeout` (504) - `inst-errconstruct-1`
2. [ ] - `p1` - Build the Problem Details body: `type`, `title`, `status`, `detail`, `instance`, plus the OAGW extension fields `upstream_id`, `host`, `path`, `retry_after_seconds`, and `trace_id` where the error type carries them - `inst-errconstruct-2`
3. [ ] - `p1` - **IF** the error type carries retry guidance (for example, `RateLimitExceeded`) - `inst-errconstruct-3`
   1. [ ] - `p1` - Set the `Retry-After` header from `retry_after_seconds` - `inst-errconstruct-3a`
4. [ ] - `p1` - Set `Content-Type: application/problem+json` - `inst-errconstruct-4`
5. [ ] - `p1` - Set `X-OAGW-Error-Source: gateway` - `inst-errconstruct-5`
6. [ ] - `p1` - **RETURN** the HTTP response with the mapped status code - `inst-errconstruct-6`

### Bearer Token Authentication Gate

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gear-foundation-bearer-auth`

**Input**: An inbound HTTP request addressed to a route under `/oagw/v1`

**Output**: An authenticated principal attached to the request, or an early 401 gateway error

**Steps**:
1. [ ] - `p1` - Extract the `Authorization` header from the request - `inst-bearer-1`
2. [ ] - `p1` - **IF** the header is missing or not a `Bearer`-scheme value - `inst-bearer-2`
   1. [ ] - `p1` - **RETURN** 401 `AuthenticationFailed` built by `cpt-cf-oagw-algo-gear-foundation-error-construct` - `inst-bearer-2a`
3. [ ] - `p1` - **TRY** - `inst-bearer-3`
   1. [ ] - `p1` - Validate the token and resolve a security principal via `toolkit-auth` - `inst-bearer-3a`
4. [ ] - `p1` - **CATCH** a validation failure - `inst-bearer-4`
   1. [ ] - `p1` - **RETURN** 401 `AuthenticationFailed` built by `cpt-cf-oagw-algo-gear-foundation-error-construct` - `inst-bearer-4a`
5. [ ] - `p1` - Evaluate the GTS permission required by the matched route against the resolved principal - `inst-bearer-5`
6. [ ] - `p1` - **IF** the permission check fails - `inst-bearer-6`
   1. [ ] - `p1` - **RETURN** 401 `AuthenticationFailed` built by `cpt-cf-oagw-algo-gear-foundation-error-construct` - `inst-bearer-6a`
7. [ ] - `p1` - **RETURN** the resolved principal attached to the request context; the request proceeds to its matched handler - `inst-bearer-7`

## 4. States (CDSL)

### Gear Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-gear-foundation-lifecycle`

**States**: Discovered, ConfigLoaded, RouterMounted, Ready

**Initial State**: Discovered

**Transitions**:
1. [ ] - `p1` - **FROM** Discovered **TO** ConfigLoaded **WHEN** `inventory` invokes the gear's `#[toolkit::gear(...)]` entry point and `OagwConfig` deserialization succeeds - `inst-lifecycle-1`
2. [ ] - `p1` - **FROM** ConfigLoaded **TO** RouterMounted **WHEN** the base router is mounted at `/oagw/v1` with the Bearer-token authentication gate attached - `inst-lifecycle-2`
3. [ ] - `p1` - **FROM** RouterMounted **TO** Ready **WHEN** the `types_registry`, `credstore`,
   `authz-resolver`, and `tenant-resolver` handles are resolved from the typed client registry - `inst-lifecycle-3`

## 5. Definitions of Done

The `Entities` bullets below (`OagwConfig`, `ProblemDetails`) name gear-level Rust types this
feature defines, not tracked domain-model entities. DECOMPOSITION §2.1 lists Domain Model
Entities as None for this feature, and that stays unchanged.

### Module Skeleton for the Control Plane / Data Plane Split

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-module-skeleton`

The system **MUST** scaffold the three-layer module skeleton `cpt-cf-oagw-design-layers`
defines: `domain/`, `infra/`, and `api/rest/`. The `domain/` module **MUST** declare
`ControlPlaneService` and `DataPlaneService` as minimal Rust traits. This gives ADR-0001's
control-plane / data-plane dispatch rule a concrete place to attach later, with no method
bodies required at this feature's gate. The `infra/` module **MUST** exist as a placeholder
for the SeaORM repositories, Pingora bridge, and plugin registry that later features fill in.
The `api/rest/` module **MUST** hold this feature's own base router and fallback handler, so
later features register their Axum handlers in the same layer rather than inventing a new one.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-registration`

**Touches**:
- Entities: None (module and trait scaffolding, not a domain-model entity)

### Gear Registration via Toolkit Macro and Inventory

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-registration`

The system **MUST** register the `oagw` gear with the host runtime using the
`#[toolkit::gear(...)]` attribute macro together with `inventory`'s compile-time
self-registration, so the runtime discovers and initializes `oagw` automatically at startup
with no manual wiring elsewhere in the platform.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-registration`
- `cpt-cf-oagw-state-gear-foundation-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- Entities: `OagwConfig`

### OagwConfig Deserialization with Field Defaults

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-config-defaults`

The system **MUST** deserialize `OagwConfig` from the `gears.oagw.config` YAML block
(`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`) and **MUST** supply a
working default for every one of those fields, so a deployment with no `oagw.config` block
still starts and serves requests correctly.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-config-load`

**Touches**:
- Entities: `OagwConfig`

### Base Router Mounted at /oagw/v1

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-router-mount`

The system **MUST** mount the gear's base router at the gear-relative path `/oagw/v1`, with no
`/api` prefix (Override 1), so every later feature registers its resource routes underneath
this one mount point rather than choosing its own base path.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-registration`

**Touches**:
- API: base path `/oagw/v1`

### Shared RFC 9457 Problem Details Type

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-error-model`

The system **MUST** define a shared Problem Details type carrying `type`, `title`, `status`,
`detail`, `instance`, and the OAGW extension fields `upstream_id`, `host`, `path`,
`retry_after_seconds`, and `trace_id`. Every later feature builds its error responses from this
one shared type, instead of redefining the body shape per handler.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-unmatched-route`
- `cpt-cf-oagw-algo-gear-foundation-error-construct`

**Touches**:
- API: base path `/oagw/v1` (fallback handler)
- Entities: `ProblemDetails`

### GTS Error-Type and HTTP Status Pairings

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-error-type-catalog`

The system **MUST** define all nineteen GTS error-type identifiers and their HTTP status
pairings from DESIGN.md §3.3. Later features are the ones that actually trigger most of them.
Each later feature then maps its own failures onto an already-declared identifier, instead of
inventing new error-type strings.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-error-construct`

**Touches**:
- Entities: `ProblemDetails`

### Universal X-OAGW-Error-Source Header

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-error-source-header`

The system **MUST** attach `X-OAGW-Error-Source: gateway|upstream` to every response this gear
returns, success or error alike. A client can then always tell whether a given response came
from the gateway itself, or was relayed from an upstream call.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-unmatched-route`
- `cpt-cf-oagw-algo-gear-foundation-error-construct`

**Touches**:
- API: base path `/oagw/v1` (fallback handler)

### Inbound Bearer Token Authentication and Shared Permission Gate

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-bearer-auth`

The system **MUST** wire `toolkit-auth` Bearer token authentication as the shared
permission-check gate on every route mounted under `/oagw/v1`, in this feature and every later
one, denying an unauthenticated or unauthorized request with a 401 gateway error before any
handler logic runs.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-inbound-auth`
- `cpt-cf-oagw-algo-gear-foundation-bearer-auth`

**Touches**:
- API: base path `/oagw/v1` (authentication gate)

### External Dependency Handles Resolved from the Typed Client Registry

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-dependency-registration`

The system **MUST** register handles for `types_registry`, `cred_store`, `api_ingress`,
`toolkit-db`, and `toolkit-auth` at startup. This holds even where a given dependency's data
path is unused in the graded deployment, for example when no database is configured. It
**MUST** also resolve and expose the `types_registry`, `credstore`, `authz-resolver`, and
`tenant-resolver` handles from the typed client registry during init. Later features then
register their own GTS (Global Type System) type schemas against an already-initialized
dependency, instead of one this feature must reach on their behalf. Registering the entity
schemas those types describe is not this feature's job; it belongs to the features that own
those entities.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-registration`

## 6. Acceptance Criteria

- [ ] The server starts with the `oagw` gear registered, discoverable through the `inventory`
  mechanism, with no startup panic.
- [ ] A route registered inside this feature's own test suite, under `/oagw/v1`, is served by
  its handler rather than answered with a bare framework 404; production resource routes are
  not registered until later features land.
- [ ] A request to an unregistered path under `/oagw/v1` returns a `RouteNotFound` problem body
  (`application/problem+json`) with `X-OAGW-Error-Source: gateway` and HTTP status 404.
- [ ] A deliberately triggered gateway error returns `application/problem+json` carrying the
  documented GTS `type` identifier for that error and the matching HTTP status from
  DESIGN.md §3.3.
- [ ] A request to a route under `/oagw/v1` with no `Authorization` header, or an invalid
  Bearer token, is rejected with HTTP 401, `AuthenticationFailed`, and
  `X-OAGW-Error-Source: gateway`.
- [ ] `OagwConfig` loads with the documented defaults when the `gears.oagw.config` block is
  absent from the supplied configuration.
- [ ] `OagwConfig` loads `proxy_timeout_secs: 2`, `allow_http_upstream: true`, and
  `ssrf_policy.enabled: false` when started with `config/e2e-local.yaml`.
- [ ] Every response this feature itself produces, success or error, carries an
  `X-OAGW-Error-Source` header set to `gateway`.
- [ ] The `upstream` value of `X-OAGW-Error-Source` is not exercised at this feature's gate; it
  is first exercised starting with `cpt-cf-oagw-feature-proxy-data-plane-http`, once an actual
  upstream call exists to relay an error from.
