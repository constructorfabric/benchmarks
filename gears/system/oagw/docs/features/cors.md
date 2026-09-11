# Feature: CORS


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Scope Exclusions](#15-scope-exclusions)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Preflight Short-Circuit](#preflight-short-circuit)
  - [Actual Cross-Origin Request Enforcement](#actual-cross-origin-request-enforcement)
  - [CORS Configuration Validation](#cors-configuration-validation)
  - [Hierarchical CORS Origin Resolution](#hierarchical-cors-origin-resolution)
  - [Catalog-Only `cors` Plugin Identifier](#catalog-only-cors-plugin-identifier)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Preflight Response Construction](#preflight-response-construction)
  - [Origin Matching](#origin-matching)
  - [Actual-Request CORS Evaluation](#actual-request-cors-evaluation)
  - [CORS Response Header Assembly](#cors-response-header-assembly)
  - [CORS Configuration Validation](#cors-configuration-validation-1)
  - [Hierarchical CORS Origin Set Merge](#hierarchical-cors-origin-set-merge)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Handler-Level Preflight Responder](#handler-level-preflight-responder)
  - [Actual-Request Origin Enforcement](#actual-request-origin-enforcement)
  - [Actual-Request Method Enforcement](#actual-request-method-enforcement)
  - [Exact, Port-Sensitive, Protocol-Sensitive Origin Matching](#exact-port-sensitive-protocol-sensitive-origin-matching)
  - [CORS Response Headers on Actual Requests](#cors-response-headers-on-actual-requests)
  - [CORS Configuration Validation and the Credentials-Wildcard Rejection](#cors-configuration-validation-and-the-credentials-wildcard-rejection)
  - [Hierarchical CORS Origin Union and Enforcement](#hierarchical-cors-origin-union-and-enforcement)
  - [Deny-by-Default Posture](#deny-by-default-posture)
  - [Catalog-Only `cors` Guard Identifier](#catalog-only-cors-guard-identifier)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-cors-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-cors`

## 1. Feature Context

### 1.1 Overview

Implements the built-in CORS handler of the OAGW Data Plane: a permissive, handler-level preflight responder that answers browser `OPTIONS` requests locally, and strict origin and method enforcement applied to actual cross-origin requests after upstream resolution and before forwarding. CORS is configured per upstream and per route through the dedicated `cors` configuration field, is deny-by-default, and participates in the tenant hierarchy through the `private` / `inherit` / `enforce` sharing modes.

### 1.2 Purpose

Browser-based clients cannot use a proxied API unless the gateway answers preflight requests locally and enforces an origin policy on the requests that follow. This feature realizes the decision recorded in `cpt-cf-oagw-adr-cors`, which selects the built-in CORS handler over both alternatives that ADR rejects: CORS is a first-class field on `Upstream` and `Route` configuration and is handled by core Data Plane logic, not by a `GuardPlugin` implementation. Its components are the preflight responder at the proxy handler level and the origin and method validator applied to actual requests, all within the CORS slice of `cpt-cf-oagw-component-model`.

Both CORS positions are bound to one canonical proxy-path order. `cpt-cf-oagw-feature-request-proxy` owns that order — it is the entry that dispatches the proxy path — and this feature conforms to it rather than restating it. The order it fixes is: **proxy invoke permission → preflight detection (short-circuit, answers locally) → alias resolution → tenant resolution → route match → actual-request CORS check → plugin chain → upstream dispatch**. The preflight short-circuit precedes alias normalization and route matching: it is answered at the handler level, without executing the plugin chain, without matching a route and without contacting the upstream, so the browser receives an answer even when the alias is unknown or the upstream is unavailable. The CORS content of that answer is resolved best-effort — the addressed alias and the caller's tenant, when the request carries both, select the upstream whose `cors` block supplies the effective `allow_credentials` and the matched verdict — so an unresolvable alias still yields the permissive `204`. The actual-request CORS check precedes the plugin chain, so a rejected origin never reaches a plugin, and it follows route match, which is the stage that supplies the effective `CorsConfig` the check evaluates.

Two behaviors are split with entry `cpt-cf-oagw-feature-request-proxy`, which this feature depends on: the proxy path owns the detection of an `OPTIONS` preflight and the handler-level short-circuit that answers it at the preflight-detection point of the canonical order of `cpt-cf-oagw-feature-request-proxy` §2, with no route match, no plugin execution and no upstream round trip, while this feature owns the CORS-correct content of that response and all actual-request origin and method enforcement. Entry 2.4 explicitly excludes CORS origin and method enforcement on actual requests.

CORS evaluation is a constant-time set-membership comparison over the effective origin and method sets and adds no upstream round trip for a preflight. This feature sets no latency target of its own: the PRD latency NFR is proxy-scoped and owned by `cpt-cf-oagw-feature-request-proxy`. Per-collection cardinality is deliberately unbounded by OAGW configuration, and the bounded resource is the request body, which the platform enforces ahead of the handler; that is a recorded deliberate posture, not an invented maximum.

Every preflight short-circuit, every actual-request origin or method rejection, and every fail-closed merged-configuration rejection emits the structured observability record of the shared contract owned by `cpt-cf-oagw-feature-observability-and-operability`; this feature supplies only the outcome label - `preflight_short_circuit`, `origin_not_allowed`, `method_not_allowed`, or `merged_config_rejected` - and mints no new metric name here.

**Requirements**: `cpt-cf-oagw-nfr-input-validation` - the CORS-configuration validation slice only. The system **MUST** reject a CORS configuration that combines `allow_credentials: true` with a wildcard `*` origin, a `cors` block whose `allowed_methods` fall outside the legal method set, or an `allowed_origins` entry that is neither `*` nor a valid origin URI. General request validation enforcement on the proxy path is owned by entry 2.4, and the rendering of the resulting `4xx` problem+json bodies is owned by the shared error contract of `cpt-cf-oagw-feature-error-handling`; neither is re-delivered here.

Already satisfied upstream and inherited, not re-delivered: `cpt-cf-oagw-fr-hierarchical-config`. The union of CORS origins under `inherit` and the forced origin set under `enforce` are the CORS slice of the merge semantics the PRD marks as done.

**Principles**: none. This entry introduces no new design principle; it realizes the decision recorded in `cpt-cf-oagw-adr-cors`.

**Constraints**: none. No DESIGN constraint is introduced by this entry; the security posture it implements (deny-by-default, no regex origin patterns, port- and protocol-sensitive matching, `Vary: Origin` always) is the security posture of `cpt-cf-oagw-adr-cors`.

**Sequences**: none. Preflight and actual-request handling are branches of `cpt-cf-oagw-seq-proxy-flow`, which is owned by entry 2.4; DESIGN.md defines no separate CORS sequence.

**Errors**: the two CORS GTS error types are `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, both returned with `403 Forbidden` on actual requests only. The problem+json body shape is rendered by the shared error contract of `cpt-cf-oagw-feature-error-handling`; this feature supplies the GTS type, the status, and the detail text.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Browser client of the proxy path: sends the preflight that is answered locally and the actual cross-origin request whose origin and method are enforced before forwarding |
| `cpt-cf-oagw-actor-platform-operator` | Configures the `cors` field on upstreams and routes, owns the deny-by-default posture, and is refused when attempting to bind the catalog-only `cors` guard identifier as a plugin |
| `cpt-cf-oagw-actor-tenant-admin` | Owns tenant-level CORS overrides whose origin set is unioned with, or locked by, the ancestor configuration under the sharing modes |
| `cpt-cf-oagw-actor-upstream-service` | Receives only actual requests that passed the origin and method checks; never receives a preflight request |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADRs**: [ADR 0004 - CORS](../ADR/0004-cors.md) (`cpt-cf-oagw-adr-cors`)
- **Schemas**: [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json), [schemas/route.v1.schema.json](../schemas/route.v1.schema.json)
- **Dependencies**: `cpt-cf-oagw-feature-request-proxy`. Preflight handling and actual-request origin enforcement are both behaviors of the proxy handler and depend on upstream resolution. `cpt-cf-oagw-feature-error-handling`, `cpt-cf-oagw-feature-rate-limiting`, and this feature are mutually independent and are developed in parallel once `cpt-cf-oagw-feature-request-proxy` exists.

### 1.5 Scope Exclusions

The following areas are excluded from this feature and remain owned by the entries named below; nothing delivered here pre-implements them:

- Audit emission of configuration writes: the structured record that a `cors` write produces is emitted by the shared observability contract of `cpt-cf-oagw-feature-observability-and-operability`. This feature supplies only the CORS outcome label named in §1.2 and emits no record of its own.
- Control Plane L1 and Data Plane hot-config cache invalidation: both caches, and the invalidation that a configuration write triggers, are owned by `cpt-cf-oagw-feature-observability-and-operability` per `cpt-cf-oagw-adr-data-plane-caching` and `cpt-cf-oagw-adr-state-management`. This feature holds no cache and participates in no invalidation path.
- Data privacy: no personal data and no secret material traverses the CORS path. `CorsConfig` carries origins, methods, header names, and two booleans, so no privacy processing surface is provided here.
- Persistence and database operations: the `cors` field is stored on the upstream and route records through the repository boundary and its in-memory implementation owned by `cpt-cf-oagw-feature-gear-foundation`. This feature performs no I/O of its own.
- Cache integration: OAGW caches no response per `cpt-cf-oagw-principle-no-cache`, and this feature caches no origin set, no verdict, and no preflight answer.
- Health and diagnostics: the readiness and health surface is owned by `cpt-cf-oagw-feature-observability-and-operability`. The CORS handler contributes no probe and no diagnostic endpoint.
- Regulatory compliance: the CORS path processes no regulated data category, so no compliance processing surface is provided here.
- Accessibility: the gear exposes no user interface surface, so no accessibility requirement applies to this feature.
- Rollout and rollback: CORS ships inside the single `oagw` crate with no schema migration and no feature flag beyond `cors.enabled`, so no rollout or rollback procedure is delivered here.
- Performance budget: this feature guarantees only the constant-time set comparison named in §1.2. The end-to-end latency assertion of the proxy path is the PRD latency NFR, which is proxy-scoped and owned by `cpt-cf-oagw-feature-request-proxy`, and no benchmark ceiling is declared here.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-proxy-request` is the end-user use case whose cross-origin branch this feature implements; `cpt-cf-oagw-usecase-configure-upstream` and `cpt-cf-oagw-usecase-configure-route` are the use cases through which the `cors` field and its sharing mode are configured.

`allow_credentials` governs only the outbound `Access-Control-Allow-Credentials` response header. OAGW never inspects inbound cookies or bearer tokens to compute it, never echoes request credentials, and emits the header only when the effective `allow_credentials` is true and the matched verdict is `exact`.

### Preflight Short-Circuit

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-cors-preflight`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An `OPTIONS` request carrying `Origin` and `Access-Control-Request-Method` is answered at the proxy handler level with a permissive `204 No Content` that echoes the requested origin, method, and headers within the reflection bounds of §3, carries `Access-Control-Max-Age: 86400`, carries `Access-Control-Allow-Credentials: true` only when the effective `allow_credentials` is true and the matched verdict is `exact`, carries `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, and has an empty body.
- The preflight short-circuit is taken at the preflight-detection point of the canonical order owned by `cpt-cf-oagw-feature-request-proxy` §2, where it precedes alias normalization and route matching: the answer is built at the handler level without executing the plugin chain and without contacting the upstream, so the browser receives an answer even when the alias is unknown or the upstream is unavailable.
- Every preflight short-circuit emits the structured observability record of the shared contract owned by `cpt-cf-oagw-feature-observability-and-operability`, with this feature supplying the outcome label `preflight_short_circuit` and nothing else, as §1.2 states.

**Error Scenarios**:
- An `OPTIONS` request that lacks either `Origin` or `Access-Control-Request-Method` is not a preflight: it receives no permissive CORS response and continues on the ordinary proxy path, where route matching and its method allowlist decide the outcome. When the matched route's method allowlist excludes `OPTIONS` the request is rejected with `405` by the method check owned by `cpt-cf-oagw-feature-request-proxy`, and when no route matches the proxy path returns `404`; no `Access-Control-*` header is added in either case.
- A preflight is never forwarded to the upstream, so an upstream that implements its own CORS policy is never consulted at preflight time.
- Per-request auth and plugin checks are skipped for a preflight by design; global and edge rate limiting and WAF/DDoS controls still apply to it and are provided by the platform, not by this feature.

**Steps**:
1. [x] - `p1` - Receive the request at the proxy path and inspect its method and headers - `inst-cors-pf-1`
2. [x] - `p1` - **IF** the method is `OPTIONS` and both an `Origin` header and an `Access-Control-Request-Method` header are present - `inst-cors-pf-2`
   1. [x] - `p1` - Treat the request as a CORS preflight and short-circuit it at the proxy handler at the preflight-detection point of the canonical order in §1.2, before the actual-request CORS check, the plugin chain, and upstream dispatch - `inst-cors-pf-3`
   2. [x] - `p1` - Build the preflight response with `cpt-cf-oagw-algo-cors-preflight-response` - `inst-cors-pf-4`
   3. [x] - `p1` - **RETURN** the `204 No Content` preflight response with an empty body and never forward the request to the upstream - `inst-cors-pf-5`
3. [x] - `p1` - **IF** the method is `OPTIONS` but either `Origin` or `Access-Control-Request-Method` is absent - `inst-cors-pf-6`
   1. [x] - `p1` - Emit no permissive CORS response and continue on the ordinary proxy path: an `OPTIONS` without the preflight signature falls through to that path, where the matched route's method allowlist rejects it with `405` through the method check owned by `cpt-cf-oagw-feature-request-proxy` when `OPTIONS` is excluded and the proxy path returns `404` when no route matches, and no `Access-Control-*` header is added in either case - `inst-cors-pf-7`
4. [x] - `p1` - **RETURN** the ordinary proxy-path result for a request that is not a CORS preflight - `inst-cors-pf-8`

### Actual Cross-Origin Request Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-cors-actual-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An actual request that carries an `Origin` header is evaluated at the actual-request CORS check point of the canonical order in §1.2 - after upstream resolution and route match, before the plugin chain and upstream dispatch: the origin is checked against the effective `allowed_origins` set first, then the method against the effective `allowed_methods` set, and only then is the request forwarded.
- A request that passes both checks is forwarded and the response carries `Access-Control-Allow-Origin`, `Access-Control-Expose-Headers` from `expose_headers`, `Access-Control-Allow-Credentials` from `allow_credentials`, and `Vary: Origin`.
- Every actual-request origin or method rejection emits the structured observability record of the shared contract owned by `cpt-cf-oagw-feature-observability-and-operability`, with this feature supplying the outcome label `origin_not_allowed` or `method_not_allowed` and nothing else, as §1.2 states.

**Error Scenarios**:
- The origin is not in the effective `allowed_origins` set: the request is rejected with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` at the actual-request CORS check point of the canonical order in §1.2 - after route match, before the plugin chain and upstream dispatch - and the upstream never receives it.
- The method is not in the effective `allowed_methods` set: the request is rejected with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, and only after the origin check has passed.
- The effective CORS configuration is disabled: no origin or method check is applied and no CORS response header is emitted, so the cross-origin read is denied by the browser's same-origin policy rather than by a `403`; a non-browser client that sends `Origin` is unaffected.

**Steps**:
1. [x] - `p1` - Continue the proxy path at the actual-request CORS check point of the canonical order in §1.2, after the alias has been resolved across the tenant hierarchy and the route has been matched, and before the plugin chain executes and the request is forwarded - `inst-cors-act-1`
2. [x] - `p1` - Read the effective `CorsConfig` for the resolved upstream and matched route - `inst-cors-act-2`
3. [x] - `p1` - **IF** the request carries no `Origin` header - `inst-cors-act-3`
   1. [x] - `p1` - Evaluate the request without CORS checks and forward it as a non-CORS request - `inst-cors-act-4`
4. [x] - `p1` - **IF** the effective `cors.enabled` is false - `inst-cors-act-5`
   1. [x] - `p1` - Apply no origin or method check, forward the request, and add no `Access-Control-*` response header while still emitting `Vary: Origin` - `inst-cors-act-6`
5. [x] - `p1` - Match the request origin against the effective `allowed_origins` set with `cpt-cf-oagw-algo-cors-origin-match` - `inst-cors-act-7`
6. [x] - `p1` - **IF** the origin does not match - `inst-cors-act-8`
   1. [x] - `p1` - Reject the request with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` before forwarding, emit `Vary: Origin` on the rejection, and emit the shared observability record with the outcome label `origin_not_allowed` - `inst-cors-act-9`
7. [x] - `p1` - Check the request method against the effective `allowed_methods` set - `inst-cors-act-10`
8. [x] - `p1` - **IF** the method is not allowed - `inst-cors-act-11`
   1. [x] - `p1` - Reject the request with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` before forwarding, emit `Vary: Origin` on the rejection, and emit the shared observability record with the outcome label `method_not_allowed` - `inst-cors-act-12`
9. [x] - `p1` - Forward the request to the selected upstream endpoint - `inst-cors-act-13`
10. [x] - `p1` - Add the CORS response headers to the response with `cpt-cf-oagw-algo-cors-response-headers` - `inst-cors-act-14`
11. [x] - `p1` - **RETURN** the upstream response carrying the CORS response headers and `Vary: Origin` - `inst-cors-act-15`

### CORS Configuration Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-cors-config-validation`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A `cors` block submitted on an upstream or on a route is accepted when it satisfies the configuration schema. Inside a `cors` block that is present, `enabled` is required and is never defaulted; the declared defaults apply to `sharing` (`private`), `allowed_methods` (`GET` and `POST`), `expose_headers` (empty), and `allow_credentials` (`false`) only.
- `cors.enabled` falls back to `false` only when the whole `cors` block is absent, so CORS is deny-by-default: an upstream or route with no `cors` block, or with `enabled: false`, grants no cross-origin access.

**Error Scenarios**:
- `allow_credentials: true` combined with a wildcard `*` origin is rejected at configuration validation time, not at request time.
- An `allowed_methods` entry outside `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS` is rejected.
- An `allowed_origins` entry that is neither `*` nor a valid origin URI is rejected; no regex or pattern form of origin is accepted.
- A `cors` block without `enabled` is rejected, because `enabled` is the required key.
- A rejected configuration is not stored, so no invalid CORS posture can reach the proxy path.

**Steps**:
1. [x] - `p1` - Receive a `cors` block as part of an upstream or route create or full replacement - `inst-cors-cfg-1`
2. [x] - `p1` - Require the `enabled` key and reject the block when it is absent, and apply the declared defaults for `sharing`, `allowed_methods`, `expose_headers`, and `allow_credentials` only, because `enabled` is never defaulted inside a `cors` block that is present - `inst-cors-cfg-2`
3. [x] - `p1` - Validate that every `allowed_origins` entry is either the literal `*` or a valid origin URI carrying scheme, host, and an explicit or default port - `inst-cors-cfg-3`
4. [x] - `p1` - Validate that every `allowed_methods` entry is in `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS` - `inst-cors-cfg-4`
5. [x] - `p1` - **IF** `allow_credentials` is true and `allowed_origins` contains `*` - `inst-cors-cfg-5`
   1. [x] - `p1` - Reject the configuration at validation time with an error naming the conflicting fields, and store nothing - `inst-cors-cfg-6`
6. [x] - `p1` - **IF** any other rule above is violated - `inst-cors-cfg-7`
   1. [x] - `p1` - Reject the configuration with a validation error naming the offending field - `inst-cors-cfg-8`
7. [x] - `p1` - **RETURN** the accepted `CorsConfig` with `enabled` explicit and every default materialized - `inst-cors-cfg-9`

### Hierarchical CORS Origin Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-cors-hierarchical-origins`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Under `sharing: inherit`, the descendant's effective origin set is the union of the ancestor origin set and the descendant origin set, so `https://app.example.com` inherited from the ancestor and `https://admin.example.com` added by the descendant yield both.
- Under `sharing: enforce`, the ancestor origin set is retained unchanged and the descendant cannot add origins.
- Under `sharing: private`, the ancestor CORS configuration contributes nothing to a descendant's effective configuration.
- A descendant origin addition is applied only when the write that stored it was authorized by the owning resource's override permission. Authorization is a write-time property and not a request-time one: this feature exercises it through the management write path owned by entries 2.2 and 2.3, and the merge itself is evaluated on already-stored, already-authorized configuration and performs no per-request permission check.

**Error Scenarios**:
- A descendant submits an origin addition under an ancestor layer whose CORS configuration carries `sharing: enforce`: the addition is discarded and the enforced ancestor origin set is used, including across alias shadowing.
- A descendant layer's origin addition was stored by a write that the owning resource's override permission did not authorize: the addition is discarded and the union is computed without it, so an unauthorized origin can never widen an ancestor's origin set.
- A merged effective configuration combines `allow_credentials: true` with a wildcard `*` origin inherited from an ancestor: the combination is fail-closed rather than served, so an inherited wildcard can never be turned into a credential-bearing policy by a descendant. The request is rejected with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, the problem+json body is rendered by `cpt-cf-oagw-feature-error-handling`, `Vary: Origin` is emitted, and the stored records are unchanged because no merge result is persisted; the rejection is observable through the shared observability record with the outcome label `merged_config_rejected`.

**Steps**:
1. [x] - `p1` - Collect the CORS layers in priority order: the upstream base configuration, the matched route configuration, and the tenant chain from root to leaf - `inst-cors-hier-1`
2. [x] - `p1` - Merge the origin sets with `cpt-cf-oagw-algo-cors-origin-set-merge` according to the sharing mode carried by the ancestor layer - `inst-cors-hier-2`
3. [x] - `p1` - **IF** a descendant layer contributes an origin addition under `sharing: inherit` - `inst-cors-hier-8`
   1. [x] - `p1` - Apply the addition only when the write that stored it was authorized by the owning resource's override permission - `gts.cf.core.oagw.upstream.v1~:override` for an upstream layer and `gts.cf.core.oagw.route.v1~:override` for a route layer - resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers, and discard it otherwise; the permission check is a write-time property of the management write path owned by entries 2.2 and 2.3, and this merge performs no per-request permission check of its own - `inst-cors-hier-8-1`
4. [x] - `p1` - **IF** an ancestor CORS layer carries `sharing: enforce` - `inst-cors-hier-3`
   1. [x] - `p1` - Keep the ancestor origin set and discard every descendant origin addition, including across alias shadowing - `inst-cors-hier-3-1`
5. [x] - `p1` - **IF** an ancestor CORS layer carries `sharing: private` and the requester is a descendant - `inst-cors-hier-4`
   1. [x] - `p1` - Skip that layer so it contributes nothing to the descendant's effective CORS configuration - `inst-cors-hier-4-1`
6. [x] - `p1` - Re-apply the credentials-and-wildcard rule of `cpt-cf-oagw-algo-cors-config-validate` to the merged effective configuration - `inst-cors-hier-5`
7. [x] - `p1` - **IF** the merged effective configuration combines `allow_credentials: true` with a `*` origin - `inst-cors-hier-6`
   1. [x] - `p1` - Fail closed: reject the request with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` rendered by `cpt-cf-oagw-feature-error-handling` with `Vary: Origin` emitted, rather than serve a credential-bearing wildcard policy, and persist no merged configuration, leaving the stored records unchanged and the shared observability record carrying the outcome label `merged_config_rejected` - `inst-cors-hier-6-1`
8. [x] - `p1` - **RETURN** the effective `CorsConfig` used by `cpt-cf-oagw-flow-cors-actual-request` - `inst-cors-hier-7`

### Catalog-Only `cors` Plugin Identifier

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-cors-catalog-identifier`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The guard identifier `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1` remains catalog-only in the types registry: it documents that CORS preflight validation exists as core Data Plane configuration on `Upstream.cors` and `Route.cors`, and it is not resolvable through a plugin registry.
- CORS behavior is reached exclusively through the `cors` configuration field and the built-in handler, so no plugin ordering can place CORS behind another plugin.

**Error Scenarios**:
- A binding that names the `cors` guard identifier in `plugins.items[].plugin_ref` is rejected at binding time as unresolvable, exactly as for the other catalog-only identifiers.

**Steps**:
1. [x] - `p1` - Treat `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1` as a cataloged type identifier with no backing guard implementation and no registry resolution - `inst-cors-cat-1`
2. [x] - `p1` - **IF** a `plugins.items[].plugin_ref` binding names the `cors` guard identifier - `inst-cors-cat-2`
   1. [x] - `p1` - Reject the binding at binding time instead of resolving it, and store no plugin binding for it - `inst-cors-cat-3`
3. [x] - `p1` - **RETURN** a configuration surface in which CORS is reachable only through the `cors` field of an upstream or route - `inst-cors-cat-4`

## 3. Processes / Business Logic (CDSL)

### Preflight Response Construction

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-cors-preflight-response`

**Input**: an inbound `OPTIONS` request carrying `Origin`, `Access-Control-Request-Method`, and, when the browser sends one, `Access-Control-Request-Headers`, together with the effective `allow_credentials` and the `cpt-cf-oagw-algo-cors-origin-match` verdict that the preflight call-in of `cpt-cf-oagw-feature-request-proxy` delivers.
**Output**: a `204 No Content` response with the preflight header set and an empty body.

**Steps**:
1. [x] - `p1` - Confirm the preflight signature: method `OPTIONS`, an `Origin` header present, and an `Access-Control-Request-Method` header present - `inst-cors-alg-pf-1`
2. [x] - `p1` - Copy the request `Origin` value verbatim into `Access-Control-Allow-Origin` without rejecting the request on it, because preflight reflection is not a policy grant and the actual request is re-checked - `inst-cors-alg-pf-2`
3. [x] - `p1` - Copy `Access-Control-Request-Method` into `Access-Control-Allow-Methods` only when it is in the legal method set, and, when present, copy `Access-Control-Request-Headers` into `Access-Control-Allow-Headers` only as a verbatim comma-joined list that carries no CR, LF, NUL, or other control character, contains no header name longer than 4096 bytes, and stays within a total header value bound of 4096 bytes; a value failing that check is dropped and the header is omitted - `inst-cors-alg-pf-3`
4. [x] - `p1` - Set `Access-Control-Max-Age: 86400` so the browser caches the preflight result for a day - `inst-cors-alg-pf-4`
5. [x] - `p1` - Set `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` so no shared cache reuses one preflight answer for another - `inst-cors-alg-pf-5`
6. [x] - `p1` - Emit `Access-Control-Allow-Credentials: true` on the preflight response when the effective `allow_credentials` is true and the matched verdict is `exact`, because a wildcard match can never be credentialed per the validation rule, and emit no `Access-Control-Expose-Headers` on a preflight response - `inst-cors-alg-pf-6`
7. [x] - `p1` - **RETURN** `204 No Content` with an empty body and no upstream round trip - `inst-cors-alg-pf-7`

### Origin Matching

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-cors-origin-match`

**Input**: the request `Origin` header value and the effective `allowed_origins` set.
**Output**: a verdict of `exact`, `wildcard`, or `no_match`, and the configured entry matched when the verdict is `exact`.

**Steps**:
1. [x] - `p1` - Serialize both sides to the canonical origin form before comparing: `scheme://host[:port]`, with scheme and host lowercased and the port omitted when it equals the scheme default, which is `443` for `https` and `80` for `http`, so a configured entry and a request origin serialize to the same string - `inst-cors-alg-om-1`
2. [x] - `p1` - Compare the serialized forms for equality across scheme, host, and port, port-sensitively on the serialized form, so `https://app.example.com:443` and `https://app.example.com` serialize to the same entry while `:8443` never equals either, and a configured entry carrying a non-default port never equals the same origin without it - `inst-cors-alg-om-2`
3. [x] - `p1` - Treat the comparison as protocol-sensitive, so `http` and `https` origins of the same host are distinct entries - `inst-cors-alg-om-3`
4. [x] - `p1` - Apply no regex, wildcard-host, suffix, or registrable-domain relaxation, so an entry for `https://example.com` can never be matched by `https://evil.com.example.com` - `inst-cors-alg-om-4`
5. [x] - `p1` - **IF** no entry equals the serialized request origin and the effective `allowed_origins` set contains `*` - `inst-cors-alg-om-5`
   1. [x] - `p1` - Return the `wildcard` verdict, which is legal only while `allow_credentials` is false - `inst-cors-alg-om-6`
6. [x] - `p1` - **IF** an entry equals the serialized request origin exactly - `inst-cors-alg-om-7`
   1. [x] - `p1` - Return the `exact` verdict together with that entry, and use that entry as the `Access-Control-Allow-Origin` value; an exact entry always wins over a `*` wildcard when both are present in the effective set - `inst-cors-alg-om-8`
7. [x] - `p1` - **RETURN** `no_match` when no entry equals the request origin and no wildcard is present, which produces the `403` origin rejection - `inst-cors-alg-om-9`

### Actual-Request CORS Evaluation

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-cors-request-evaluation`

**Input**: the effective `CorsConfig` for the resolved upstream and matched route, the request method, the request `Origin` header, and the position of the request in the proxy path after upstream resolution.
**Output**: a decision to forward the request with CORS response headers, or a `403` rejection carrying one of the two CORS GTS error types.

**Steps**:
1. [x] - `p1` - Run the evaluation after the alias and route have been resolved with tenant context and before the request is forwarded - `inst-cors-alg-ev-1`
2. [x] - `p1` - Consider only requests that carry an `Origin` header as CORS subjects, and leave requests without one to the ordinary proxy path - `inst-cors-alg-ev-2`
3. [x] - `p1` - **IF** the effective `cors.enabled` is false - `inst-cors-alg-ev-3`
   1. [x] - `p1` - Apply no origin or method check and forward the request with no `Access-Control-*` response header, leaving the cross-origin denial to the browser - `inst-cors-alg-ev-4`
4. [x] - `p1` - Check the origin first with `cpt-cf-oagw-algo-cors-origin-match` and reject with `403` `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` when there is no match - `inst-cors-alg-ev-5`
5. [x] - `p1` - Check the method second against the effective `allowed_methods` set and reject with `403` `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` when it is absent - `inst-cors-alg-ev-6`
6. [x] - `p1` - Order the checks so a disallowed origin is never reported as a method failure and a disallowed method is reported only for an allowed origin - `inst-cors-alg-ev-7`
7. [x] - `p1` - Forward the request to the upstream only after both checks pass - `inst-cors-alg-ev-8`
8. [x] - `p1` - **RETURN** the forward decision or the `403` rejection with its GTS error type - `inst-cors-alg-ev-9`

### CORS Response Header Assembly

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-cors-response-headers`

**Input**: the effective `CorsConfig`, the matched origin, and the response returned for the forwarded request.
**Output**: the response with the CORS response headers added.

**Steps**:
1. [x] - `p1` - Set `Access-Control-Allow-Origin` to the exact configured entry that matched, or to `*` when the effective set is the wildcard and `allow_credentials` is false - `inst-cors-alg-hd-1`
2. [x] - `p1` - Set `Access-Control-Expose-Headers` from the effective `expose_headers` list, and omit the header when the list is empty - `inst-cors-alg-hd-2`
3. [x] - `p1` - Set `Access-Control-Allow-Credentials: true` when the effective `allow_credentials` is true, and omit the header otherwise - `inst-cors-alg-hd-3`
4. [x] - `p1` - Set `Vary: Origin` on every CORS-relevant response, including rejections and CORS-disabled responses, so no shared cache serves one origin's headers to another: when the upstream response already carries a `Vary` list, append `Origin` to it rather than overwrite it - `inst-cors-alg-hd-4`
5. [x] - `p1` - Set the computed `Access-Control-Allow-Origin` only when the upstream response did not already set one, and leave the upstream response body and every non-CORS response header unmodified; on a rejection generated by OAGW the OAGW header set is authoritative - `inst-cors-alg-hd-5`
6. [x] - `p1` - **RETURN** the response with the CORS headers applied - `inst-cors-alg-hd-6`

### CORS Configuration Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-algo-cors-config-validate`

**Input**: a `cors` block submitted on an upstream or a route, or an effective configuration produced by the hierarchy merge.
**Output**: an accepted `CorsConfig`, or a rejection naming the offending fields.

**Steps**:
1. [x] - `p1` - Require the `enabled` key and reject the block when it is absent, because `enabled` is never defaulted inside a `cors` block that is present; it falls back to `false` only when the whole `cors` block is absent - `inst-cors-alg-cv-1`
2. [x] - `p1` - Apply the declared defaults to the fields that carry them: `sharing` `private`, `allowed_methods` `GET` and `POST`, `expose_headers` empty, and `allow_credentials` `false` - `inst-cors-alg-cv-2`
3. [x] - `p1` - Validate every `allowed_origins` entry as either the literal `*` or a valid origin URI, reject any other form including a pattern or a bare host, and store each accepted entry in the canonical origin form `scheme://host[:port]` with scheme and host lowercased and the port omitted when it equals the scheme default (`443` for `https`, `80` for `http`), so a configured entry and a request origin serialize to the same string and a configured entry carrying a non-default port never equals the same origin without it - `inst-cors-alg-cv-3`
4. [x] - `p1` - Validate every `allowed_methods` entry against `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS` - `inst-cors-alg-cv-4`
5. [x] - `p1` - Validate `sharing` against `private`, `inherit`, and `enforce` - `inst-cors-alg-cv-5`
6. [x] - `p1` - **IF** `allow_credentials` is true and `allowed_origins` contains `*` - `inst-cors-alg-cv-6`
   1. [x] - `p1` - Reject the configuration at validation time and store nothing, so the combination can never reach request time - `inst-cors-alg-cv-7`
7. [x] - `p1` - Re-apply the same rule to an effective configuration produced by the merge, so an inherited wildcard cannot be combined with a descendant's credentials - `inst-cors-alg-cv-8`
8. [x] - `p1` - **RETURN** the accepted `CorsConfig` or the validation rejection - `inst-cors-alg-cv-9`

### Hierarchical CORS Origin Set Merge

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-cors-origin-set-merge`

**Input**: the ancestor CORS layers from root to leaf, the matched route layer, and the descendant CORS layer, each carrying `sharing` with the value `private`, `inherit`, or `enforce`.
**Output**: the effective `allowed_origins` set and the effective CORS field values.

**Steps**:
1. [x] - `p1` - Start from the most specific CORS layer present as the working origin set, honoring the priority order Upstream (base) < Route < Tenant - `inst-cors-alg-mg-1`
2. [x] - `p1` - **IF** an ancestor CORS layer carries `sharing: inherit` - `inst-cors-alg-mg-2`
   1. [x] - `p1` - Union the ancestor origin set with the descendant origin set, preserving every origin from both - `inst-cors-alg-mg-3`
3. [x] - `p1` - **IF** a descendant origin addition is about to enter the working origin set - `inst-cors-alg-mg-12`
   1. [x] - `p1` - Apply the addition only when the write that stored it was authorized by the owning resource's override permission - `gts.cf.core.oagw.upstream.v1~:override` for an upstream layer and `gts.cf.core.oagw.route.v1~:override` for a route layer - resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers, and drop it otherwise; authorization is a write-time property exercised through the management write path owned by entries 2.2 and 2.3, so this merge evaluates already-stored, already-authorized configuration and performs no per-request permission check - `inst-cors-alg-mg-12-1`
4. [x] - `p1` - **IF** an ancestor CORS layer carries `sharing: enforce` - `inst-cors-alg-mg-4`
   1. [x] - `p1` - Keep the ancestor origin set as-is and discard every descendant addition, so a descendant cannot add origins - `inst-cors-alg-mg-5`
5. [x] - `p1` - **IF** an ancestor CORS layer carries `sharing: private` - `inst-cors-alg-mg-6`
   1. [x] - `p1` - Contribute nothing to a descendant's effective origin set - `inst-cors-alg-mg-7`
6. [x] - `p1` - Treat the union as add-only, so an origin inherited from an ancestor can never be removed by a descendant - `inst-cors-alg-mg-8`
7. [x] - `p1` - Merge `enabled`, `allowed_methods`, `expose_headers`, and `allow_credentials` with the sharing-mode rules of the hierarchical merge engine delivered by entry `cpt-cf-oagw-feature-gear-foundation`, and evaluate `enabled` on the effective configuration - `inst-cors-alg-mg-9`
   1. [x] - `p1` - Apply the four per-field rules that the engine resolves for this feature: `enabled` takes the more specific value present and stays absent when no layer specifies it; `allowed_methods` and `expose_headers` union under `inherit`, take the ancestor set under `enforce`, and contribute nothing under `private`; `allow_credentials` escalates monotonically, so `true` on any contributing layer wins and can never be turned off by a descendant - `inst-cors-alg-mg-9-1`
8. [x] - `p1` - Validate the merged result with `cpt-cf-oagw-algo-cors-config-validate` before it is served to the request path, and persist no merged configuration: a merged result that fails the credentials-and-wildcard rule is not stored and the stored records are left unchanged - `inst-cors-alg-mg-10`
9. [x] - `p1` - **RETURN** the effective `CorsConfig` - `inst-cors-alg-mg-11`

## 4. States (CDSL)

No explicit lifecycle states in this feature. CORS is configured per upstream and per route, holds no state of its own, and owns no documented schema table; the lifecycle of the record that carries the `cors` field is the stored-configuration-record lifecycle delivered by `cpt-cf-oagw-feature-gear-foundation`.

## 5. Definitions of Done

### Handler-Level Preflight Responder

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-preflight`

The system **MUST** detect a CORS preflight at the proxy handler level, defined as an `OPTIONS` request carrying both an `Origin` header and an `Access-Control-Request-Method` header, and **MUST** answer it with a permissive `204 No Content` that echoes the requested origin in `Access-Control-Allow-Origin`, the requested method in `Access-Control-Allow-Methods` when it is in the legal method set, and the requested headers in `Access-Control-Allow-Headers` within the reflection bounds of `cpt-cf-oagw-algo-cors-preflight-response`, with `Access-Control-Max-Age: 86400`, `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, and an empty body. The responder **MUST** emit `Access-Control-Allow-Credentials: true` when the effective `allow_credentials` is true and the matched verdict is `exact`, and **MUST** omit that header and omit `Access-Control-Expose-Headers` in every other case. The responder **MUST NOT** execute the plugin chain, contact the upstream, or forward the preflight to the upstream. The responder **MUST NOT** validate the origin or the method, because enforcement is deferred to the actual request.

**Implements**:
- `cpt-cf-oagw-flow-cors-preflight`
- `cpt-cf-oagw-algo-cors-preflight-response`

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `CorsConfig` (read for the effective `allow_credentials` and the matched verdict only)
- Tests: integration tests in `tests/cors_preflight.rs` asserting the `204` status, the echoed headers within the reflection bounds, `Access-Control-Max-Age`, the three-part `Vary`, the presence of `Access-Control-Allow-Credentials: true` for a credentialed upstream and its absence otherwise, the empty body, and that no upstream call is recorded

### Actual-Request Origin Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-origin-enforcement`

The system **MUST** validate the `Origin` header of an actual cross-origin request against the effective `allowed_origins` set after upstream resolution and before the request is forwarded, and **MUST** reject a disallowed origin with `403 Forbidden` and the GTS error type `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` before the upstream receives the request. The origin check **MUST** precede the method check, and the rejection **MUST** carry `Vary: Origin`.

**Implements**:
- `cpt-cf-oagw-flow-cors-actual-request`
- `cpt-cf-oagw-algo-cors-request-evaluation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `CorsConfig`
- Tests: integration tests in `tests/cors_enforcement.rs` asserting the `403`, the `origin_not_allowed` GTS type, the ordering against the method check, and that the upstream receives no rejected request

### Actual-Request Method Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-method-enforcement`

The system **MUST** check the method of an actual cross-origin request against the effective `allowed_methods` set after the origin check has passed and before forwarding, and **MUST** reject a disallowed method with `403 Forbidden` and the GTS error type `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`.

**Implements**:
- `cpt-cf-oagw-flow-cors-actual-request`
- `cpt-cf-oagw-algo-cors-request-evaluation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `CorsConfig`
- Tests: integration tests in `tests/cors_enforcement.rs` asserting the `403`, the `method_not_allowed` GTS type, and that an allowed origin with a disallowed method is the only way this rejection is produced

### Exact, Port-Sensitive, Protocol-Sensitive Origin Matching

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-origin-matching`

The system **MUST** match origins by exact equality across scheme, host, and port after both sides are serialized to the canonical origin form `scheme://host[:port]` with scheme and host lowercased and a scheme-default port omitted, **MUST** treat the comparison as port-sensitive and protocol-sensitive on that serialized form, and **MUST NOT** implement any regex, wildcard-host, suffix, or registrable-domain relaxation of an origin entry. An exact entry **MUST** always win over a `*` wildcard when both are present in the effective set. The wildcard entry `*` **MUST** match any origin and **MUST** be accepted only while `allow_credentials` is false.

**Implements**:
- `cpt-cf-oagw-algo-cors-origin-match`

**Touches**:
- API: none
- Entities: `CorsConfig`
- Tests: unit tests in a sibling `*_tests.rs` module of the CORS handler covering an exact match, a same-host origin on a different port, a same-host origin on a different scheme, a lookalike host such as `https://evil.com.example.com`, the canonical serialization of `https://app.example.com:443` to `https://app.example.com` and the difference of `:8443`, exact-over-wildcard precedence, and the wildcard verdict

### CORS Response Headers on Actual Requests

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-cors-response-headers`

The system **MUST** add `Access-Control-Allow-Origin` with the matched configured origin, `Access-Control-Expose-Headers` from the effective `expose_headers`, and `Access-Control-Allow-Credentials: true` from the effective `allow_credentials` to the response of an allowed actual request, and **MUST** always include `Vary: Origin` on every CORS-relevant response so that no shared cache serves one origin's CORS headers to another. The system **MUST** leave the upstream response body and every non-CORS response header unmodified.

`allow_credentials` governs only the outbound `Access-Control-Allow-Credentials` response header. The system **MUST NOT** inspect inbound cookies or bearer tokens to compute it, **MUST NOT** echo request credentials, and **MUST** emit the header only when the effective `allow_credentials` is true and the matched verdict is `exact`. When the upstream response already carries a `Vary` list, the system **MUST** append `Origin` to it rather than overwrite it, and **MUST** set its computed `Access-Control-Allow-Origin` only when the upstream did not set one; on a rejection generated by OAGW the OAGW header set is authoritative.

**Implements**:
- `cpt-cf-oagw-flow-cors-actual-request`
- `cpt-cf-oagw-algo-cors-response-headers`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `CorsConfig`
- Tests: integration tests in `tests/cors_enforcement.rs` asserting the four response headers on an allowed request, the omitted headers when unconfigured, `Vary: Origin` on allowed, rejected, and CORS-disabled responses, the appended rather than overwritten upstream `Vary`, and the preserved upstream `Access-Control-Allow-Origin`

### CORS Configuration Validation and the Credentials-Wildcard Rejection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-config-validation`

The system **MUST** validate the `cors` block of an upstream or a route at configuration validation time: `enabled` is required inside a `cors` block that is present and falls back to `false` only when the whole `cors` block is absent, `allowed_origins` entries are the literal `*` or valid origin URIs stored in the canonical origin form, `allowed_methods` entries are within `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, and `sharing` is within `private`, `inherit`, `enforce`. The system **MUST** reject `allow_credentials: true` combined with a wildcard `*` origin at validation time rather than at request time, **MUST** store nothing for a rejected block, and **MUST** re-apply the same rule to a merged effective configuration so an inherited wildcard can never be combined with a descendant's credentials. A merged effective configuration that combines the two **MUST** be fail-closed rather than served: the request is rejected with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, the body is rendered by `cpt-cf-oagw-feature-error-handling`, `Vary: Origin` is emitted, the merge result is never persisted, and the rejection is observable through the shared observability record with the outcome label `merged_config_rejected`. This is the CORS-configuration validation slice of `cpt-cf-oagw-nfr-input-validation`; general request-validation enforcement on the proxy path is owned by entry 2.4.

**Implements**:
- `cpt-cf-oagw-flow-cors-config-validation`
- `cpt-cf-oagw-algo-cors-config-validate`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`, `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `CorsConfig`
- Tests: unit tests in a sibling `*_tests.rs` module covering the credentials-and-wildcard rejection, the method enum, the origin forms, the required `enabled` key, and the merged-configuration re-check

### Hierarchical CORS Origin Union and Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-cors-hierarchical-origins`

The system **MUST** resolve the effective `allowed_origins` set through the sharing modes of the hierarchical merge engine delivered by entry `cpt-cf-oagw-feature-gear-foundation`: under `sharing: inherit` the descendant origin set is the union of the ancestor origin set and the descendant origin set, and under `sharing: enforce` the ancestor origin set is retained and a descendant cannot add origins. The union **MUST** be add-only, so an inherited origin cannot be removed by a descendant, and an ancestor layer carrying `sharing: private` **MUST** contribute nothing to a descendant's effective configuration. A descendant origin addition **MUST** be applied only when the write that stored it was authorized by the owning resource's override permission - `gts.cf.core.oagw.upstream.v1~:override` for an upstream layer and `gts.cf.core.oagw.route.v1~:override` for a route layer, resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers. That authorization is a write-time property and not a request-time one: it is exercised through the management write path owned by entries 2.2 and 2.3, and the merge **MUST** evaluate already-stored, already-authorized configuration and **MUST** perform no per-request permission check.

**Implements**:
- `cpt-cf-oagw-flow-cors-hierarchical-origins`
- `cpt-cf-oagw-algo-cors-origin-set-merge`

**Touches**:
- API: none
- Entities: `CorsConfig`
- Tests: unit tests in a sibling `*_tests.rs` module covering the union under `inherit`, the retained set under `enforce`, the invisible `private` layer, add-only union, enforcement across alias shadowing, the discard of a descendant addition whose storing write was not authorized by the override permission, and the fail-closed `403` with the `merged_config_rejected` outcome label for a merged credentials-and-wildcard result that is never persisted

### Deny-by-Default Posture

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-deny-by-default`

The system **MUST** default `cors.enabled` to `false`, so an upstream or route with no `cors` block grants no cross-origin access. When the effective `cors.enabled` is false, the system **MUST** apply no origin or method check, **MUST** emit no `Access-Control-*` response header, and **MUST** still emit `Vary: Origin`, so the cross-origin read is denied by the browser's same-origin policy rather than by a gateway error and a non-browser client that sends `Origin` is unaffected. Deny-by-default governs actual requests. The preflight responder is permissive by design because it grants nothing on its own: the browser enforces the actual request against the preflight answer, and the actual request is re-checked here, so a permissive preflight never widens what the gateway forwards.

**Implements**:
- `cpt-cf-oagw-flow-cors-config-validation`
- `cpt-cf-oagw-flow-cors-actual-request`
- `cpt-cf-oagw-algo-cors-request-evaluation`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `CorsConfig`
- Tests: integration tests in `tests/cors_enforcement.rs` asserting that an unconfigured upstream yields no `Access-Control-*` header, still yields `Vary: Origin`, and does not yield a `403`

### Catalog-Only `cors` Guard Identifier

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-catalog-identifier`

The system **MUST** keep `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1` catalog-only: it is registered for types-registry cataloging as the identifier that describes CORS preflight validation, it is not resolvable through a guard-plugin registry, and a `plugins.items[].plugin_ref` binding that names it **MUST** be rejected at binding time. CORS behavior **MUST** be reachable only through the `cors` field of an upstream or a route and the built-in handler, per `cpt-cf-oagw-adr-cors`.

**Implements**:
- `cpt-cf-oagw-flow-cors-catalog-identifier`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `POST /oagw/v1/routes`
- Entities: `CorsConfig`
- Tests: integration tests in `tests/cors_catalog.rs` asserting that a `cors` guard binding is rejected and that no guard plugin named `cors` is registered in the guard registry

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-unit-tests`

The system **MUST** be covered by unit tests inside the `oagw` crate as sibling `*_tests.rs` modules for the CORS logic that does not require a running router: origin matching including the canonical origin serialization, port and protocol sensitivity, exact-over-wildcard precedence, and the absence of pattern matching, the credentials-and-wildcard rejection, the method enum validation, the preflight reflection bounds including a CR/LF-bearing `Access-Control-Request-Headers` value, the four scalar merge rules for `enabled`, `allowed_methods`, `expose_headers`, and `allow_credentials` cross-referenced against `cpt-cf-oagw-dod-gear-foundation-merge-engine` for the engine-level assertions, the hierarchical origin union and enforcement, the discard of an unauthorized descendant origin addition, the deny-by-default evaluation, and the preflight header set.

**Implements**:
- `cpt-cf-oagw-algo-cors-origin-match`
- `cpt-cf-oagw-algo-cors-config-validate`
- `cpt-cf-oagw-algo-cors-origin-set-merge`
- `cpt-cf-oagw-algo-cors-preflight-response`

**Touches**:
- API: none
- Entities: `CorsConfig`
- Tests: unit tests in `src/domain/cors_tests.rs` and in the sibling `*_tests.rs` module of the CORS slice of the proxy handler delivered by entry 2.8 inside the `oagw` crate at `gears/system/oagw/oagw/src/`; the exact module path of that slice is left to the implementation entry

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-cors-integration-tests`

The system **MUST** be covered by integration-style tests inside the `oagw` crate's `tests/` directory that exercise the proxy path end to end for CORS: a permissive `204` preflight with echoed headers and no upstream call, an allowed actual request carrying the CORS response headers, a disallowed origin rejected with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, a disallowed method rejected with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, a credentials-and-wildcard configuration rejected at validation time, and a two-level tenant hierarchy exercising the origin union under `inherit` and the retained set under `enforce`. No test for this feature is placed under `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-flow-cors-preflight`
- `cpt-cf-oagw-flow-cors-actual-request`
- `cpt-cf-oagw-flow-cors-config-validation`
- `cpt-cf-oagw-flow-cors-hierarchical-origins`
- `cpt-cf-oagw-dod-cors-preflight`
- `cpt-cf-oagw-dod-cors-origin-enforcement`
- `cpt-cf-oagw-dod-cors-method-enforcement`
- `cpt-cf-oagw-dod-cors-config-validation`
- `cpt-cf-oagw-dod-cors-hierarchical-origins`

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]`, `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `Upstream`, `Route`, `CorsConfig`
- Tests: integration tests in `tests/cors_preflight.rs`, `tests/cors_enforcement.rs`, and `tests/cors_hierarchy.rs`

## 6. Acceptance Criteria

- [x] An `OPTIONS` request to `/oagw/v1/proxy/{alias}[/{path_suffix}]` carrying `Origin` and `Access-Control-Request-Method` returns `204 No Content` with the requested origin, method, and headers echoed within the reflection bounds, `Access-Control-Max-Age: 86400`, `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, and an empty body, and the test asserts that no upstream call and no plugin execution occurred (DoD `cpt-cf-oagw-dod-cors-preflight`).
- [x] An `OPTIONS` preflight against an upstream whose effective `allow_credentials` is true and whose matched verdict is `exact` carries `Access-Control-Allow-Credentials: true`, and the same preflight omits that header for a wildcard match, for a non-credentialed configuration, and for a disabled configuration (DoD `cpt-cf-oagw-dod-cors-preflight`).
- [x] An `OPTIONS` request carrying an `Access-Control-Request-Headers` value that contains a CR or LF, or that exceeds the 4096-byte bound, is answered `204` with `Access-Control-Allow-Headers` omitted and is never forwarded to the upstream (DoD `cpt-cf-oagw-dod-cors-preflight`, `cpt-cf-oagw-dod-cors-unit-tests`).
- [x] An `OPTIONS` request lacking `Origin` or `Access-Control-Request-Method` receives no permissive CORS response and is handled by the ordinary proxy path instead (DoD `cpt-cf-oagw-dod-cors-preflight`).
- [x] An actual cross-origin request whose origin is in the effective `allowed_origins` set is forwarded and its response carries `Access-Control-Allow-Origin`, `Access-Control-Expose-Headers`, `Access-Control-Allow-Credentials` when the effective `allow_credentials` is true and the matched verdict is `exact`, and `Vary: Origin` (DoD `cpt-cf-oagw-dod-cors-response-headers`).
- [x] An actual cross-origin request with a disallowed origin returns `403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and `Vary: Origin`, and the upstream receives no such request (DoD `cpt-cf-oagw-dod-cors-origin-enforcement`).
- [x] An actual cross-origin request with an allowed origin and a disallowed method returns `403` with `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, and the origin check is reported first when both are disallowed (DoD `cpt-cf-oagw-dod-cors-method-enforcement`).
- [x] `https://app.example.com:8443`, `http://app.example.com`, and `https://evil.com.example.com` are all rejected against an `allowed_origins` entry of `https://app.example.com`, `https://app.example.com:443` matches that same entry, and no configuration form of origin produces a different verdict (DoD `cpt-cf-oagw-dod-cors-origin-matching`).
- [x] An exact `allowed_origins` entry and a `*` entry are both present in the effective set: a request whose origin equals the exact entry receives the `exact` verdict and that entry as `Access-Control-Allow-Origin`, never the wildcard verdict (DoD `cpt-cf-oagw-dod-cors-origin-matching`).
- [x] A `cors` block with `allow_credentials: true` and `allowed_origins: ["*"]` is rejected at configuration validation time and nothing is stored, and the same rejection fires when the wildcard is inherited from an ancestor rather than declared locally; in that merged case the in-flight request is rejected with `403` and `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` carrying `Vary: Origin`, no merged configuration is persisted, and the shared observability record carries the outcome label `merged_config_rejected` (DoD `cpt-cf-oagw-dod-cors-config-validation`).
- [x] A `cors` block with a method outside `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, an origin that is neither `*` nor a valid origin URI, a `sharing` value outside `private`, `inherit`, `enforce`, or a missing `enabled` key is rejected at configuration validation time (DoD `cpt-cf-oagw-dod-cors-config-validation`).
- [x] A parent layer with `allowed_origins: ["https://app.example.com"]` and `sharing: inherit` plus a child layer with `allowed_origins: ["https://admin.example.com"]` yields the union of both origins, and neither origin can be removed by the descendant (DoD `cpt-cf-oagw-dod-cors-hierarchical-origins`).
- [x] An ancestor CORS layer with `sharing: enforce` yields its own origin set for a descendant request, and an origin the descendant submits is discarded, including when alias shadowing selects the descendant upstream (DoD `cpt-cf-oagw-dod-cors-hierarchical-origins`).
- [x] An ancestor CORS layer with `sharing: private` contributes no origin to a descendant's effective configuration (DoD `cpt-cf-oagw-dod-cors-hierarchical-origins`).
- [x] A descendant layer whose stored origin addition was written without the owning resource's override permission - `gts.cf.core.oagw.upstream.v1~:override` for an upstream layer or `gts.cf.core.oagw.route.v1~:override` for a route layer - is unioned without that addition, while an addition written by an authorized write through the management write path of entries 2.2 and 2.3 is unioned, and neither case performs a per-request permission check during the merge (DoD `cpt-cf-oagw-dod-cors-hierarchical-origins`).
- [x] An upstream or route with no `cors` block, or with `enabled: false`, forwards an `Origin`-bearing request with no `Access-Control-*` response header, still emits `Vary: Origin`, and returns no `403` (DoD `cpt-cf-oagw-dod-cors-deny-by-default`).
- [x] A `plugins.items[].plugin_ref` binding naming `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1` is rejected at binding time, and no guard plugin named `cors` is resolvable through the guard registry (DoD `cpt-cf-oagw-dod-cors-catalog-identifier`).
- [x] Every test for this feature lives inside the `oagw` crate as a sibling `*_tests.rs` module or a file under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-cors-unit-tests`, `cpt-cf-oagw-dod-cors-integration-tests`).
