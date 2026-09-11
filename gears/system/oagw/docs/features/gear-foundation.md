# Feature: Gear Foundation and Configuration


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Non-Applicability Dispositions](#15-non-applicability-dispositions)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Platform Operator Deploys and Starts the Gear](#platform-operator-deploys-and-starts-the-gear)
  - [Application Developer Receives a Canonically Mapped Error](#application-developer-receives-a-canonically-mapped-error)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Load Typed Gear Configuration](#load-typed-gear-configuration)
  - [Resolve the Calling Tenant and Subject from the Security Context](#resolve-the-calling-tenant-and-subject-from-the-security-context)
  - [Create the Shared In-Memory Control-Plane Store](#create-the-shared-in-memory-control-plane-store)
  - [Map a Domain Error to a Problem+JSON Response](#map-a-domain-error-to-a-problemjson-response)
  - [Report Gear Readiness](#report-gear-readiness)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Initialization Lifecycle State Machine](#gear-initialization-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Gear Registers With the Host Runtime and Mounts Its Routes](#gear-registers-with-the-host-runtime-and-mounts-its-routes)
  - [Typed Configuration Surface With Safe Defaults](#typed-configuration-surface-with-safe-defaults)
  - [Tenant Identity Extraction From the Security Context](#tenant-identity-extraction-from-the-security-context)
  - [Shared In-Memory Control-Plane Store](#shared-in-memory-control-plane-store)
  - [Canonical Domain-Error-to-Problem+JSON Mapping](#canonical-domain-error-to-problemjson-mapping)
  - [Gear Readiness Reporting](#gear-readiness-reporting)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-gf-implemented`

- [ ] `p1` - `cpt-cf-oagw-feature-gear-foundation`

## 1. Feature Context

### 1.1 Overview

Establishes OAGW as a registered ToolKit gear: it participates in the host runtime's REST phase to mount its management and proxy routes, exposes a typed configuration surface with safe defaults, creates the single in-memory control-plane store every other feature shares, and defines the canonical mapping from domain errors to RFC 9457 `application/problem+json` responses that every other feature reuses.

### 1.2 Purpose

Every one of the other six OAGW features executes inside the wiring this feature establishes: none of them can register a route, read a configuration value, hold control-plane state, or return a well-formed error without it. This feature realizes the gear-wiring portion of `cpt-cf-oagw-component-model` and gives the whole gear a single, consistent posture toward the host runtime and toward its callers, so that a client of any management or proxy endpoint sees the same error envelope regardless of which internal feature rejected the request.

**Requirements**: `cpt-cf-oagw-fr-error-codes`

**Principles**: `cpt-cf-oagw-principle-rfc9457`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Supplies (or omits) the gear's deployment configuration and observes whether the gear starts and reports ready. |
| `cpt-cf-oagw-actor-app-developer` | Receives the RFC 9457 `application/problem+json` error responses whose status-code vocabulary this feature defines, regardless of which feature's logic raised the underlying domain error. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: None — this feature has no dependency on any other FEATURE; every other FEATURE depends on it.

### 1.5 Non-Applicability Dispositions

- **Inbound authentication and authorization**: performed by the host runtime before a request reaches this gear. This feature does not authenticate callers or evaluate coarse permissions itself; it only maps the failures the host runtime (or a feature's own OAGW-specific permission check) raises to HTTP statuses, per the `unauthenticated` and `permission denied` categories this feature adds to its canonical error mapping (see Section 3).
- **No feature-owned permission decisions in this gear**: this gear performs no OAGW-specific permission checks of its own, in this configuration, beyond the `unauthenticated` (401) mapping described above. The `:create` and bind permission preconditions the PRD's use cases name (Route Management's `gts.cf.core.oagw.route.v1~:create` check, Upstream Management's `oagw:upstream:bind` check) are described in those features' own flows and §1.5 sections, but are not separately enforced by feature-specific logic in this configuration; inbound authentication and coarse authorization performed by the host runtime are the only gate those requests pass through before reaching feature logic. The `permission denied` (403) category exists in this feature's mapping (Section 3) and is exercised by this feature's own unit tests, but this feature only maps whichever category a calling feature's logic raises — it never itself decides when a `403` applies, and no feature currently raises one in this configuration.
- **User interface**: this feature exposes no user interface, so accessibility and UX checklist domains are not applicable.
- **Regulated or personal data**: this feature stores no regulated or personal data; it holds gear configuration values and gear-lifecycle state only.

## 2. Actor Flows (CDSL)

**Use cases**: None — this feature underlies every use case in PRD.md rather than owning one of its own.

### Platform Operator Deploys and Starts the Gear

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-gf-startup`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The deployment configuration includes a `gears.oagw.config` sub-section with one or more recognized keys; the gear starts with the supplied values (falling back to documented defaults for any recognized key left unset) in effect.
- The deployment configuration omits the `gears.oagw.config` sub-section entirely; the gear starts with every configuration key at its documented default.
- The `gears.oagw.config` sub-section includes one or more keys the gear does not recognize; the gear starts normally and ignores them.

**Error Scenarios**:
- A recognized configuration key holds a value of the wrong type; the gear fails to initialize and reports which key is malformed, before any shared state or route is created.
- No REST host is present in the deployment to mount routes onto; the gear's REST-phase registration fails and the gear does not report ready.

**Steps**:
1. [ ] - `p1` - Platform Operator supplies, partially supplies, or omits the `gears.oagw.config` sub-section of the host deployment configuration - `inst-gf-startup-01`
2. [ ] - `p1` - The host runtime reaches the init lifecycle phase and this gear resolves its typed configuration (`cpt-cf-oagw-algo-gf-load-config`) from that sub-section - `inst-gf-startup-02`
3. [ ] - `p1` - **IF** configuration resolution fails because a recognized key holds a value of the wrong type - `inst-gf-startup-03`
   1. [ ] - `p1` - The gear reports an initialization error identifying the offending key and does not proceed to create shared state or mount routes - `inst-gf-startup-04`
4. [ ] - `p1` - **ELSE** - `inst-gf-startup-05`
   1. [ ] - `p1` - The gear creates the shared in-memory control-plane store exactly once (`cpt-cf-oagw-algo-gf-init-state`) and retains it for the lifetime of the process - `inst-gf-startup-06`
5. [ ] - `p1` - The host runtime reaches the REST phase and this gear registers its management and proxy routes under the gear-relative `/oagw/v1` prefix onto the single shared router the host composes, with no `/api` segment added by this gear - `inst-gf-startup-07`
6. [ ] - `p1` - **IF** no REST host is present in the deployment to compose that shared router - `inst-gf-startup-08`
   1. [ ] - `p1` - Route registration fails and the gear does not report ready - `inst-gf-startup-09`
7. [ ] - `p1` - **ELSE** - `inst-gf-startup-10`
   1. [ ] - `p1` - The gear registers a named readiness check with the host runtime's readiness aggregator (`cpt-cf-oagw-algo-gf-readiness`) - `inst-gf-startup-11`
8. [ ] - `p1` - **RETURN** the gear reports ready once configuration resolution, shared-state creation, and route registration have all completed without error - `inst-gf-startup-12`

### Application Developer Receives a Canonically Mapped Error

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-gf-error-response`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A management or proxy request fails inside some other feature's logic with a domain error that carries one of the named entries in DESIGN.md's error-code taxonomy; the caller receives the HTTP status the taxonomy documents for that entry.
- A management request fails with a domain error that carries only a generic canonical category (unauthenticated, permission denied, invalid argument, not found, already exists, failed precondition, unavailable, internal); the caller receives the status this feature's category mapping specifies.

**Error Scenarios**:
- A domain error is raised that matches neither a named taxonomy entry nor a canonical category; the caller still receives a well-formed `application/problem+json` response rather than an unhandled failure.
- A request reaches a feature's own OAGW-specific authorization check (for example Upstream Management's bind-permission decision) and that check fails; the caller receives `403` via the `permission denied` category rather than falling through to a `500`.

**Steps**:
1. [ ] - `p1` - Application Developer issues a request against a management or proxy path under `/oagw/v1` - `inst-gf-error-01`
2. [ ] - `p1` - **API**: `{METHOD} /oagw/v1/{path}` (the specific path and method belong to the feature that owns the endpoint; this flow starts once that feature's logic raises a domain error while handling the request) - `inst-gf-error-02`
3. [ ] - `p1` - This feature's canonical error-mapping process (`cpt-cf-oagw-algo-gf-error-mapping`) classifies the domain error and resolves the HTTP status and problem body for it - `inst-gf-error-03`
4. [ ] - `p1` - **RETURN** an `application/problem+json` response carrying `type`, `title`, `status`, and `detail`, at the HTTP status the mapping resolved - `inst-gf-error-04`

## 3. Processes / Business Logic (CDSL)

### Load Typed Gear Configuration

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gf-load-config`

**Input**: The `gears.oagw.config` sub-section of the host deployment configuration, or its absence.

**Output**: A fully-resolved typed configuration exposing `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`, `token_cache_ttl_secs`, and `token_cache_capacity`.

**Steps**:
1. [ ] - `p1` - Treat a missing `gears.oagw.config` sub-section as an empty mapping rather than as an error - `inst-gf-config-01`
2. [ ] - `p1` - **FOR EACH** recognized key (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`, `token_cache_ttl_secs`, `token_cache_capacity`) - `inst-gf-config-02`
   1. [ ] - `p1` - **IF** the key is present in the mapping, adopt its supplied value - `inst-gf-config-03`
   2. [ ] - `p1` - **ELSE** adopt this feature's documented default: `proxy_timeout_secs` = 30 seconds, `allow_http_upstream` = `false`, `ssrf_policy.enabled` = `true`, `token_cache_ttl_secs` = 300 seconds, `token_cache_capacity` = 10,000 entries - `inst-gf-config-04`
3. [ ] - `p1` - Ignore every key present in the mapping that is not one of the five recognized keys, without treating it as an error - `inst-gf-config-05`
4. [ ] - `p1` - **TRY** - `inst-gf-config-06`
   1. [ ] - `p1` - Validate that each recognized key's supplied value matches its declared type (a duration in seconds, a boolean, or an entry count) - `inst-gf-config-07`
5. [ ] - `p1` - **CATCH** a recognized key holding a value of the wrong type - `inst-gf-config-08`
   1. [ ] - `p1` - Fail gear initialization with an error that identifies the offending key, before shared state or routes are created - `inst-gf-config-09`
6. [ ] - `p1` - **RETURN** the resolved typed configuration, shared with the features that consume individual keys - `inst-gf-config-10`

### Resolve the Calling Tenant and Subject from the Security Context

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gf-tenant-context`

**Input**: The inbound request reaching a management or proxy handler under `/oagw/v1`.

**Output**: The calling subject and tenant scope used to scope every control-plane read and write, or an empty (no-tenant) scope.

**Steps**:
1. [ ] - `p1` - Determine whether the host runtime has attached a security context to the inbound request - `inst-gf-tenant-01`
2. [ ] - `p1` - **IF** a security context is attached - `inst-gf-tenant-02`
   1. [ ] - `p1` - Obtain the calling subject and the calling tenant from that security context - `inst-gf-tenant-03`
   2. [ ] - `p1` - Scope every control-plane read and write this request performs to that tenant, exactly as "the calling tenant" is referenced throughout every other feature's flows - `inst-gf-tenant-04`
3. [ ] - `p1` - **ELSE** (no security context is attached) - `inst-gf-tenant-05`
   1. [ ] - `p1` - Treat the request as having no tenant scope (no subject, no tenant) rather than guessing or defaulting to a tenant - `inst-gf-tenant-06`
   2. [ ] - `p1` - The request's subsequent authorization step raises the "unauthenticated" canonical category (`cpt-cf-oagw-algo-gf-error-mapping`), mapped to `401` - `inst-gf-tenant-07`
4. [ ] - `p1` - **RETURN** the resolved subject/tenant scope (or its absence) for use by the handling feature's own tenant-scoping and, where a feature defines one, its OAGW-specific permission check - `inst-gf-tenant-08`

### Create the Shared In-Memory Control-Plane Store

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gf-init-state`

**Input**: None — this process runs unconditionally, once, during the init lifecycle phase, after configuration has resolved.

**Output**: A single shared store instance handed to both control-plane and data-plane internals for the remaining lifetime of the process.

**Steps**:
1. [ ] - `p1` - Allocate one empty in-memory store, since the graded configuration runs without a database - `inst-gf-state-01`
2. [ ] - `p1` - **DB**: this store carries the same entities, cascade-delete relationships, and per-tenant uniqueness invariants that the persisted-deployment schema (`cpt-cf-oagw-db-schema`) documents — for example `UNIQUE(tenant_id, alias)` on upstreams — realized as in-memory constraints instead of database constraints - `inst-gf-state-02`
3. [ ] - `p1` - Share this same store instance with both the control-plane CRUD logic and the data-plane request-resolution logic; no second copy is created - `inst-gf-state-03`
4. [ ] - `p1` - **RETURN** the shared store instance for use by every other feature's control-plane and data-plane logic - `inst-gf-state-04`

### Map a Domain Error to a Problem+JSON Response

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gf-error-mapping`

**Input**: A domain error raised by any control-plane or data-plane operation in the gear, carrying either one of the named entries in DESIGN.md's error-code taxonomy or a generic canonical category (unauthenticated, permission denied, invalid argument, not found, already exists, failed precondition, unavailable, internal), plus a human-readable detail message.

**Output**: An HTTP response with media type `application/problem+json` containing `type`, `title`, `status`, and `detail` members, per the RFC 9457 principle (`cpt-cf-oagw-principle-rfc9457`).

**Steps**:
1. [ ] - `p1` - Determine whether the domain error carries one of the named error-code-taxonomy entries (for example `RouteNotFound`, `PluginInUse`, `PayloadTooLarge`, `RateLimitExceeded`, `CircuitBreakerOpen`) or only a generic canonical category - `inst-gf-errmap-01`
2. [ ] - `p1` - **IF** the error carries a named taxonomy entry - `inst-gf-errmap-02`
   1. [ ] - `p1` - Use the HTTP status the taxonomy documents for that entry as a fixed, per-entry mapping - `inst-gf-errmap-03`
3. [ ] - `p1` - **ELSE** (a generic canonical category, used chiefly by control-plane CRUD errors that do not need a more specific named code) - `inst-gf-errmap-04`
   1. [ ] - `p1` - **IF** the category is "unauthenticated" (no security context is attached to the request, per `cpt-cf-oagw-algo-gf-tenant-context`) - `inst-gf-errmap-unauth-01`
      1. [ ] - `p1` - Use status 401 - `inst-gf-errmap-unauth-02`
   2. [ ] - `p1` - **IF** the category is "permission denied" (a feature's own OAGW-specific authorization check, for example Upstream Management's bind-permission decision, rejects the caller) - `inst-gf-errmap-permdenied-01`
      1. [ ] - `p1` - Use status 403 - `inst-gf-errmap-permdenied-02`
   3. [ ] - `p1` - **IF** the category is "invalid argument" - `inst-gf-errmap-05`
      1. [ ] - `p1` - Use status 400 - `inst-gf-errmap-06`
   4. [ ] - `p1` - **IF** the category is "not found" - `inst-gf-errmap-07`
      1. [ ] - `p1` - Use status 404 - `inst-gf-errmap-08`
   5. [ ] - `p1` - **IF** the category is "already exists" - `inst-gf-errmap-09`
      1. [ ] - `p1` - Use status 409 - `inst-gf-errmap-10`
   6. [ ] - `p1` - **IF** the category is "failed precondition" - `inst-gf-errmap-11`
      1. [ ] - `p1` - Use status 409, matching DESIGN.md's own examples of this category (for example a plugin blocked from deletion while still referenced) - `inst-gf-errmap-12`
   7. [ ] - `p1` - **IF** the category is "unavailable" - `inst-gf-errmap-13`
      1. [ ] - `p1` - Use status 503 - `inst-gf-errmap-14`
   8. [ ] - `p1` - **IF** the category is "internal" - `inst-gf-errmap-15`
      1. [ ] - `p1` - Use status 500 - `inst-gf-errmap-16`
4. [ ] - `p1` - **IF** the domain error matches neither a named taxonomy entry nor a recognized canonical category (an error type introduced without an explicit mapping) - `inst-gf-errmap-17`
   1. [ ] - `p1` - Default to status 500 under the "internal" category rather than leaking an unmapped error to the caller - `inst-gf-errmap-18`
5. [ ] - `p1` - Build the response body with a `type` member identifying the error, a `title` summarizing the category, the resolved `status`, and a `detail` describing the specific failure - `inst-gf-errmap-19`
6. [ ] - `p1` - Set the response media type to `application/problem+json` - `inst-gf-errmap-20`
7. [ ] - `p1` - **RETURN** the assembled problem body paired with the resolved HTTP status code - `inst-gf-errmap-21`

### Report Gear Readiness

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-gf-readiness`

**Input**: A readiness probe forwarded by the host runtime's readiness aggregator, or a liveness probe that never reaches gear-specific logic.

**Output**: A healthy/unhealthy readiness result for this gear, folded into the host's aggregate readiness report.

**Steps**:
1. [ ] - `p1` - Determine whether configuration resolution, shared-state creation, and route registration have all completed since this gear's initialization began - `inst-gf-ready-01`
2. [ ] - `p1` - **IF** all three have completed without error - `inst-gf-ready-02`
   1. [ ] - `p1` - Report this gear healthy under its registered readiness-check name - `inst-gf-ready-03`
3. [ ] - `p1` - **ELSE** - `inst-gf-ready-04`
   1. [ ] - `p1` - Report this gear unhealthy, naming the stage that has not completed - `inst-gf-ready-05`
4. [ ] - `p1` - **RETURN** the readiness result to the host's readiness aggregator - `inst-gf-ready-06`

## 4. States (CDSL)

### Gear Initialization Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-gf-lifecycle`

**States**: Uninitialized, ConfigResolved, StateCreated, RoutesMounted, Ready, Unhealthy

**Initial State**: Uninitialized

**Transitions**:
1. [ ] - `p1` - **FROM** Uninitialized **TO** ConfigResolved **WHEN** the typed configuration surface has resolved (documented defaults applied for any absent key or absent section, unrecognized keys ignored) - `inst-gf-lifecycle-01`
2. [ ] - `p1` - **FROM** ConfigResolved **TO** StateCreated **WHEN** the shared in-memory control-plane store has been created - `inst-gf-lifecycle-02`
3. [ ] - `p1` - **FROM** StateCreated **TO** RoutesMounted **WHEN** the host runtime's REST phase has mounted this gear's routes under `/oagw/v1` - `inst-gf-lifecycle-03`
4. [ ] - `p1` - **FROM** RoutesMounted **TO** Ready **WHEN** this gear's readiness check has registered with the host and reports healthy - `inst-gf-lifecycle-04`
5. [ ] - `p1` - **FROM** Uninitialized **TO** Unhealthy **WHEN** configuration resolution fails (a recognized key holds a value of the wrong type) - `inst-gf-lifecycle-05`
6. [ ] - `p1` - **FROM** ConfigResolved **TO** Unhealthy **WHEN** shared-state creation fails - `inst-gf-lifecycle-06`
7. [ ] - `p1` - **FROM** StateCreated **TO** Unhealthy **WHEN** route mounting fails (for example, no REST host is present in the deployment) - `inst-gf-lifecycle-07`

## 5. Definitions of Done

### Gear Registers With the Host Runtime and Mounts Its Routes

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gf-registration`

The system **MUST** register this gear with the host runtime, participate in the REST phase by contributing its management and proxy routes to the single shared router the host composes, and mount those routes under the gear-relative `/oagw/v1` prefix with no `/api` segment added by this gear itself.

**Implements**:
- `cpt-cf-oagw-flow-gf-startup`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: None (this entry establishes the path prefix every other feature's endpoints are mounted under; it owns no endpoint of its own)
- DB: None
- Entities: None

### Typed Configuration Surface With Safe Defaults

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gf-config`

The system **MUST** expose a typed configuration surface for `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`, `token_cache_ttl_secs`, and `token_cache_capacity`; apply each key's documented default when that key — or the entire `gears.oagw.config` sub-section — is absent; start successfully in that absent-section case; and ignore configuration keys it does not recognize rather than failing startup because of them.

**Implements**:
- `cpt-cf-oagw-algo-gf-load-config`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: None
- DB: None
- Entities: None

### Tenant Identity Extraction From the Security Context

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gf-tenant-context`

The system **MUST** obtain the calling subject and tenant from the security context the host runtime attaches to each request, scope every control-plane read and write to that tenant, and treat a request with no attached security context as having no tenant scope rather than defaulting to any tenant.

**Implements**:
- `cpt-cf-oagw-algo-gf-tenant-context`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: None
- DB: None
- Entities: None

### Shared In-Memory Control-Plane Store

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gf-shared-state`

The system **MUST** create exactly one in-memory control-plane store during gear initialization, share that same instance between control-plane and data-plane internals for the life of the process, and preserve the per-tenant uniqueness invariants (for example `(tenant_id, alias)` on upstreams) that the persisted-deployment schema documents.

**Implements**:
- `cpt-cf-oagw-algo-gf-init-state`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: None
- DB: `cpt-cf-oagw-db-schema`
- Entities: None

### Canonical Domain-Error-to-Problem+JSON Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gf-error-mapping`

The system **MUST** map every domain error raised anywhere in this gear to an RFC 9457 `application/problem+json` response, using the fixed per-entry status codes from DESIGN.md's named error-code taxonomy where an error carries a named entry, the canonical category-to-status mapping in Section 3 otherwise — including `unauthenticated` -> 401 and `permission denied` -> 403 — and a 500 "internal" response for any domain error matching neither.

**Implements**:
- `cpt-cf-oagw-algo-gf-error-mapping`
- `cpt-cf-oagw-flow-gf-error-response`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: None
- DB: None
- Entities: None

### Gear Readiness Reporting

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gf-readiness`

The system **MUST** register a named readiness check with the host runtime's readiness aggregator that reports this gear healthy only once configuration resolution, shared-state creation, and route registration have all completed, and unhealthy — naming the incomplete stage — otherwise.

**Implements**:
- `cpt-cf-oagw-algo-gf-readiness`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: None
- DB: None
- Entities: None

## 6. Acceptance Criteria

- [ ] Starting the host runtime with `config/e2e-local.yaml` registers this gear and the server starts without error.
- [ ] With the `gears.oagw.config` sub-section entirely absent from the deployment configuration, the gear still starts successfully and every configuration key takes its documented default value.
- [ ] With `config/e2e-local.yaml`'s `gears.oagw` block in effect, the resolved configuration reports `proxy_timeout_secs` as 2, `allow_http_upstream` as `true`, and `ssrf_policy.enabled` as `false`.
- [ ] With no `token_cache_ttl_secs` or `token_cache_capacity` key supplied, the resolved configuration reports 300 and 10,000 respectively.
- [ ] A `gears.oagw.config` sub-section containing a key the gear does not recognize does not prevent the gear from starting.
- [ ] The gear's management and proxy routes are reachable at `/oagw/v1/...`, and no route is registered under an `/api/oagw/v1/...` path by this gear itself.
- [ ] The shared in-memory control-plane store rejects a second upstream created with an alias already used by another upstream belonging to the same tenant, while permitting the same alias for a different tenant.
- [ ] A write performed through the control-plane store (for example creating an upstream) is visible to a data-plane read against the same store within the same process, without any additional synchronization step — evidencing one shared store rather than independent copies.
- [ ] A domain error carrying a named entry from DESIGN.md's error-code taxonomy returns an `application/problem+json` body whose `status` field matches the HTTP status the taxonomy documents for that entry (for example `RouteNotFound` returns 404).
- [ ] A domain error carrying only a generic canonical category returns the status this feature's mapping specifies for that category (`unauthenticated` -> 401, `permission denied` -> 403, `invalid argument` -> 400, `not found` -> 404, `already exists` -> 409, `failed precondition` -> 409, `unavailable` -> 503, `internal` -> 500).
- [ ] A request that fails a feature's own OAGW-specific authorization check (for example Upstream Management's bind-permission decision) returns `403` via the `permission denied` category, rather than falling through to `500`.
- [ ] A request with no security context attached is treated as having no tenant scope, and any control-plane operation it attempts fails with the `unauthenticated` category, mapped to `401`.
- [ ] An unmapped domain error — one that matches neither a named taxonomy entry nor a recognized canonical category — yields an `application/problem+json` body with a `type`, `title`, `status` of 500, and `detail`, rather than an unhandled server error.
- [ ] Once configuration resolution, shared-state creation, and route registration have all completed, this gear's readiness check reports healthy and the host's aggregate readiness endpoint reflects that health.
