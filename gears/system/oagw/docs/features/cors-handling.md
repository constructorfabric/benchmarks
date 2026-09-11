# Feature: CORS Handling


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Browser Preflight Request Flow](#browser-preflight-request-flow)
  - [Browser Actual Cross-Origin Request Flow](#browser-actual-cross-origin-request-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [CORS Preflight Detection and Response](#cors-preflight-detection-and-response)
  - [Origin Matching](#origin-matching)
  - [Method Checking](#method-checking)
  - [Effective CORS Configuration Merge](#effective-cors-configuration-merge)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Preflight Fast Path](#preflight-fast-path)
  - [Actual Cross-Origin Request Validation](#actual-cross-origin-request-validation)
  - [Security Defaults](#security-defaults)
  - [Forwarded-Response Headers](#forwarded-response-headers)
  - [Hierarchical CORS Merge](#hierarchical-cors-merge)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-cors-handling-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-cors-handling`
## 1. Feature Context

### 1.1 Overview

OAGW's built-in CORS handler answers browser preflight `OPTIONS` requests immediately at the handler level, before any upstream resolution takes place, and validates the origin and method of actual cross-origin requests after proxy-core has resolved the upstream/route but before the request is forwarded.

### 1.2 Purpose

Cross-origin browser clients cannot use OAGW's proxy endpoint unless CORS preflight and actual-request semantics are handled correctly and securely. This feature implements the two halves of that behavior described by `cpt-cf-oagw-adr-cors`: a fast, upstream-independent preflight path, and a post-resolution origin/method validation step layered onto the proxy request path that `cpt-cf-oagw-feature-proxy-core` (DECOMPOSITION entry 2.5) establishes. It also implements the hierarchical merge of the `cors` configuration field across the Upstream/tenant chain, reusing the same sharing-mode plumbing proxy-core uses for auth, rate-limit, and plugins. `route.v1.schema.json`'s top-level properties are exactly `id`, `tags`, `upstream_id`, `match`, `plugins`, `rate_limit` — it defines no `cors` property, and the `cors` sub-schema under that schema's `definitions` is referenced by nothing, so Route resources persist no functional `cors` field (see `cpt-cf-oagw-feature-route-management`, DECOMPOSITION entry 2.3). Every merge and validation step below therefore reads Upstream-level `cors` objects only, across the tenant ancestor chain; no Route-level `cors` object ever participates.

At the shared post-resolution policy hook `cpt-cf-oagw-feature-proxy-core` exposes, this feature's origin/method validation runs first, ahead of `cpt-cf-oagw-feature-rate-limiting`'s budget evaluation and `cpt-cf-oagw-feature-plugin-execution`'s Auth/Guard/Transform chain (that fixed order: CORS -> rate limiting -> plugin chain -> forward, is `proxy-core.md`'s Feature Dependencies-level decision, restated here so the two features agree). A request that is simultaneously CORS-invalid and over its rate-limit budget therefore always receives this feature's `403`, never a `429`.

**Requirements**: None — per DECOMPOSITION entry 2.7, CORS has no dedicated PRD `fr-`/`nfr-` identifier; its behavior is scoped entirely by `cpt-cf-oagw-adr-cors` and the Guard Rules rows in DESIGN.md that cite it. The actual-request flow below extends the proxy request path already covered by `cpt-cf-oagw-usecase-proxy-request` and `cpt-cf-oagw-interface-proxy-api` (DECOMPOSITION entry 2.5); it does not re-claim those identifiers as covered by this feature.

**Principles**: None — DECOMPOSITION entry 2.7 lists no Design Principles Covered for this feature.

**Design Components**:

- `cpt-cf-oagw-component-model`
- `cpt-cf-oagw-adr-cors`

**Explicit out-of-scope carried from `DECOMPOSITION.md` §2.7**: the upstream/route CRUD endpoints that persist the `cors` field belong to `cpt-cf-oagw-feature-upstream-management`/`cpt-cf-oagw-feature-route-management` (2.2/2.3), not this feature; and non-CORS request validation — guard rules unrelated to origin and method — belongs to `cpt-cf-oagw-feature-proxy-core` (2.5) and `cpt-cf-oagw-feature-plugin-execution` (2.9), not this feature.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Operates a browser-based client that issues the `OPTIONS` preflight request and the subsequent actual cross-origin request against the OAGW proxy endpoint; consumes the `Access-Control-*` response headers and the `403` CORS problem bodies this feature produces. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADR**: [0004-cors.md](../ADR/0004-cors.md) — `cpt-cf-oagw-adr-cors`, the normative source for this feature
- **Dependencies**: `cpt-cf-oagw-feature-proxy-core` — this feature's actual-request validation runs after proxy-core's alias/route resolution and hierarchical configuration merge and before its forwarding step; the preflight path runs entirely before proxy-core is invoked

## 2. Actor Flows (CDSL)

**Use cases**: None dedicated to CORS. Both flows below are cross-origin-specific specializations of the proxy request path covered by `cpt-cf-oagw-usecase-proxy-request` (DECOMPOSITION entry 2.5); this feature does not claim that identifier as its own coverage.

### Browser Preflight Request Flow

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-cors-browser-preflight-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The browser sends `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]` carrying `Origin` and `Access-Control-Request-Method` (and, typically, `Access-Control-Request-Headers`); OAGW answers `204 No Content` echoing the requested origin, method, and headers, without resolving `{alias}` to any upstream or route and without extracting tenant context, running authentication, or invoking any plugin. The preflight fast path remains subject only to infrastructure-level (global/edge) rate limiting and WAF/DDoS controls that sit outside this feature's own logic — it is exempt from OAGW's own per-route rate limiting (`cpt-cf-oagw-feature-rate-limiting`) and Auth/Guard/Transform chain (`cpt-cf-oagw-feature-plugin-execution`) because neither has a resolved upstream/route to evaluate against yet.

**Error Scenarios**:
- None at this layer: a well-formed preflight (as defined by the detection rule) always receives the permissive `204`, per `cpt-cf-oagw-adr-cors`; unauthorized or misconfigured origins are rejected on the *actual* request, not the preflight (see the next flow). An `OPTIONS` request that does not carry both `Origin` and `Access-Control-Request-Method` is not classified as a preflight and is handed to the normal proxy-core resolution path instead (out of this feature's scope).

**Steps**:
1. [x] - `p1` - The browser sends `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]` with `Origin` and `Access-Control-Request-Method` headers, and optionally `Access-Control-Request-Headers` - `inst-cors-preflight-flow-request`
2. [x] - `p1` - {API: OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}] handler invokes preflight detection (`cpt-cf-oagw-algo-cors-preflight-detect-and-respond`) as the very first step of request handling, before alias/route resolution, tenant-context extraction, authentication, or plugin execution} - `inst-cors-preflight-flow-detect`
3. [x] - `p1` - **IF** the request is classified as a CORS preflight - `inst-cors-preflight-flow-if-detected`
   1. [x] - `p1` - **RETURN** `204 No Content` per `cpt-cf-oagw-algo-cors-preflight-detect-and-respond`, without resolving any upstream, route, or tenant context - `inst-cors-preflight-flow-return-204`
4. [x] - `p1` - **ELSE** - `inst-cors-preflight-flow-else`
   1. [x] - `p1` - Hand the `OPTIONS` request to the standard proxy-core resolution path (`cpt-cf-oagw-feature-proxy-core`) as an ordinary request - `inst-cors-preflight-flow-fallthrough`

### Browser Actual Cross-Origin Request Flow

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The browser sends `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` carrying an `Origin` header (not a preflight). Proxy-core resolves the upstream, route, and the effective `cors` configuration; if the effective `cors.enabled` is `true`, origin and method are validated against the effective configuration before forwarding; on success the request is forwarded and the response carries the documented `Access-Control-*` headers plus `Vary: Origin`.

**Error Scenarios**:
- The request's `Origin` is not a member of the effective `allowed_origins`: rejected `403` with the `cf.oagw.cors.origin_not_allowed.v1` problem body, before any upstream connection is opened.
- The request's method is not a member of the effective `allowed_methods`: rejected `403` with the `cf.oagw.cors.method_not_allowed.v1` problem body, before any upstream connection is opened.

**Steps**:
1. [x] - `p1` - The browser sends `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` carrying an `Origin` header - `inst-cors-actual-flow-request`
2. [x] - `p1` - {API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` — proxy-core (`cpt-cf-oagw-feature-proxy-core`) resolves the upstream and route and computes the effective configuration, including the effective `cors` object via `cpt-cf-oagw-algo-cors-merge-effective-config`} - `inst-cors-actual-flow-resolve`
3. [x] - `p1` - **IF** the resolved effective `cors.enabled` is `false` - `inst-cors-actual-flow-if-disabled`
   1. [x] - `p1` - Perform no origin/method validation and add no `Access-Control-*` response header; proxy-core forwards the request exactly as it would a non-CORS request - `inst-cors-actual-flow-skip-disabled`
4. [x] - `p1` - **ELSE** (effective `cors.enabled` is `true`) - `inst-cors-actual-flow-else-enabled`
   1. [x] - `p1` - Run origin matching (`cpt-cf-oagw-algo-cors-match-origin`) against the request's `Origin` and the effective `allowed_origins` - `inst-cors-actual-flow-check-origin`
   2. [x] - `p1` - **IF** origin matching fails - `inst-cors-actual-flow-if-origin-fail`
      1. [x] - `p1` - **RETURN** `403` with the `cf.oagw.cors.origin_not_allowed.v1` problem body; no upstream connection is opened - `inst-cors-actual-flow-return-403-origin`
   3. [x] - `p1` - **ELSE** - `inst-cors-actual-flow-else-origin-pass`
      1. [x] - `p1` - Run method checking (`cpt-cf-oagw-algo-cors-check-method`) against the request method and the effective `allowed_methods` - `inst-cors-actual-flow-check-method`
      2. [x] - `p1` - **IF** method checking fails - `inst-cors-actual-flow-if-method-fail`
         1. [x] - `p1` - **RETURN** `403` with the `cf.oagw.cors.method_not_allowed.v1` problem body; no upstream connection is opened - `inst-cors-actual-flow-return-403-method`
      3. [x] - `p1` - **ELSE** - `inst-cors-actual-flow-else-method-pass`
         1. [x] - `p1` - Forward the request to the upstream via proxy-core's forwarding mechanics - `inst-cors-actual-flow-forward`
         2. [x] - `p1` - Add `Access-Control-Allow-Origin` (and, when configured, `Access-Control-Expose-Headers` and `Access-Control-Allow-Credentials: true`) and `Vary: Origin` to the upstream's response - `inst-cors-actual-flow-add-headers`
         3. [x] - `p1` - **RETURN** the upstream's response, with the CORS headers attached, to the browser - `inst-cors-actual-flow-return-response`

## 3. Processes / Business Logic (CDSL)

### CORS Preflight Detection and Response

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-cors-preflight-detect-and-respond`

**Input**: The inbound HTTP request's method and header set, evaluated at the proxy handler entry point for `/oagw/v1/proxy/{alias}[/{path_suffix}]`, prior to any other processing.

**Output**: Either a terminal `204 No Content` response (preflight path), or a signal that the request is not a preflight and must continue into proxy-core's normal resolution path.

**Steps**:
1. [x] - `p1` - Read the inbound request's HTTP method and header set - `inst-cors-algo-preflight-parse`
2. [x] - `p1` - **IF** method is `OPTIONS` **AND** an `Origin` header is present **AND** an `Access-Control-Request-Method` header is present - `inst-cors-algo-preflight-if-match`
   1. [x] - `p1` - Classify the request as a CORS preflight - `inst-cors-algo-preflight-classify`
3. [x] - `p1` - **ELSE** - `inst-cors-algo-preflight-else`
   1. [x] - `p1` - Classify the request as not a preflight and **RETURN** control to the normal proxy-core resolution path (no further steps of this algorithm run) - `inst-cors-algo-preflight-not-preflight`
4. [x] - `p1` - **IF** classified as a CORS preflight - `inst-cors-algo-preflight-if-classified`
   1. [x] - `p1` - Set `Access-Control-Allow-Origin` to the literal value of the request's `Origin` header - `inst-cors-algo-preflight-set-allow-origin`
   2. [x] - `p1` - Set `Access-Control-Allow-Methods` to the literal value of the request's `Access-Control-Request-Method` header - `inst-cors-algo-preflight-set-allow-methods`
   3. [x] - `p1` - **IF** `Access-Control-Request-Headers` is present on the request - `inst-cors-algo-preflight-if-headers-present`
      1. [x] - `p1` - Set `Access-Control-Allow-Headers` to that header's literal value - `inst-cors-algo-preflight-set-allow-headers`
   4. [x] - `p1` - Set `Access-Control-Max-Age` to a fixed, non-negative integer number of seconds (an implementation constant; the `cors` schema defines no configurable max-age field) - `inst-cors-algo-preflight-set-max-age`
   5. [x] - `p1` - Set `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers` and `X-OAGW-Error-Source: gateway` — this `204` is produced by OAGW itself before any upstream is ever resolved, consistent with `cpt-cf-oagw-principle-error-source` applying to every response OAGW returns, not only error responses - `inst-cors-algo-preflight-set-vary`
   6. [x] - `p1` - **RETURN** `204 No Content` with an empty body, without resolving any alias, upstream, route, or tenant context, and without invoking authentication or any plugin - `inst-cors-algo-preflight-return`

### Origin Matching

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-cors-match-origin`

**Input**: The request's `Origin` header value; the effective `allowed_origins` array; the effective `allow_credentials` boolean (all three sourced from `cpt-cf-oagw-algo-cors-merge-effective-config`).

**Output**: A match/no-match result and, on a match, the literal value to emit as `Access-Control-Allow-Origin`.

**Steps**:
1. [x] - `p1` - Take the request's `Origin` header value exactly as received — no case-folding, no trailing-slash trimming, no scheme/host/port normalization - `inst-cors-algo-origin-parse`
2. [x] - `p1` - **IF** the effective `allow_credentials` is `true` **AND** the effective `allowed_origins` contains the literal entry `*` - `inst-cors-algo-origin-if-invalid-combo`
   1. [x] - `p1` - Treat the `*` entry as absent for matching purposes for the remainder of this algorithm (never match on it); this is the runtime rejection of an effective configuration combining `allow_credentials: true` with a wildcard origin, applied after hierarchical merge - `inst-cors-algo-origin-drop-wildcard`
3. [x] - `p1` - **FOR EACH** remaining `entry` in the effective `allowed_origins` - `inst-cors-algo-origin-for-each`
   1. [x] - `p1` - **IF** `entry` equals `*` - `inst-cors-algo-origin-if-wildcard-entry`
      1. [x] - `p1` - Record a match for any `Origin` value - `inst-cors-algo-origin-wildcard-match`
   2. [x] - `p1` - **ELSE IF** `entry` is byte-for-byte identical to the request's `Origin` value (scheme, host, and port all significant; no regular-expression matching, no substring/suffix matching) - `inst-cors-algo-origin-if-exact`
      1. [x] - `p1` - Record a match - `inst-cors-algo-origin-exact-match`
4. [x] - `p1` - **IF** no entry matched - `inst-cors-algo-origin-if-none-matched`
   1. [x] - `p1` - **RETURN** no-match - `inst-cors-algo-origin-return-fail`
5. [x] - `p1` - **RETURN** match, together with the `Access-Control-Allow-Origin` value to emit: the request's literal `Origin` value when the matching entry was an explicit origin or when the effective `allow_credentials` is `true`; the literal `*` when the matching entry was the wildcard entry and the effective `allow_credentials` is `false` - `inst-cors-algo-origin-return-success`

### Method Checking

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-cors-check-method`

**Input**: The request's HTTP method; the effective `allowed_methods` array (sourced from `cpt-cf-oagw-algo-cors-merge-effective-config`).

**Output**: A match/no-match result.

**Steps**:
1. [x] - `p1` - Take the request's HTTP method as received - `inst-cors-algo-method-parse`
2. [x] - `p1` - **IF** the request method is present in the effective `allowed_methods` array - `inst-cors-algo-method-if-present`
   1. [x] - `p1` - **RETURN** match - `inst-cors-algo-method-return-success`
3. [x] - `p1` - **ELSE** - `inst-cors-algo-method-else`
   1. [x] - `p1` - **RETURN** no-match - `inst-cors-algo-method-return-fail`

### Effective CORS Configuration Merge

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-cors-merge-effective-config`

**Input**: The `cors` object declared on the resolved Upstream, plus the `cors` object declared on the corresponding Upstream resource of each ancestor tenant in the chain proxy-core walks for alias/route resolution. `route.v1.schema.json`'s top-level properties are exactly `id`, `tags`, `upstream_id`, `match`, `plugins`, `rate_limit`; it defines no top-level `cors` property, so no Route-level `cors` object is ever a contributing input to this algorithm, in this round or any ancestor tenant's.

**Output**: A single effective `cors` object (`enabled`, `allowed_origins`, `allowed_methods`, `expose_headers`, `allow_credentials`) consumed by `cpt-cf-oagw-algo-cors-match-origin`, `cpt-cf-oagw-algo-cors-check-method`, and the response-header step of the actual-request flow.

**Steps**:
1. [ ] - `p1` - Order the contributing `cors` objects from most general to most specific: root tenant ancestor's Upstream first, resolving tenant's Upstream last — reusing proxy-core's own root-to-child tenant hierarchy walk. There is no Route level to interleave into this order, since Route resources carry no `cors` object to contribute - `inst-cors-algo-merge-order`
2. [ ] - `p1` - Initialize the running effective object to the `cors` schema defaults: `enabled: false`, `allowed_methods: ["GET","POST"]`, `expose_headers: []`, `allow_credentials: false`, `allowed_origins: []` - `inst-cors-algo-merge-init`
3. [ ] - `p1` - **FOR EACH** contributing `cors` object, in the order established above - `inst-cors-algo-merge-for-each`
   1. [ ] - `p1` - **IF** this level's own `sharing` value is `private` - `inst-cors-algo-merge-if-private`
      1. [ ] - `p1` - Discard the running effective object accumulated so far and replace it with this level's own declared values only - `inst-cors-algo-merge-private-replace`
   2. [ ] - `p1` - **ELSE IF** this level's own `sharing` value is `inherit` - `inst-cors-algo-merge-if-inherit`
      1. [ ] - `p1` - Set the effective `allowed_origins` to the deduplicated union of the running effective `allowed_origins` and this level's declared `allowed_origins` - `inst-cors-algo-merge-inherit-union`
      2. [ ] - `p1` - Override the effective `enabled`, `allowed_methods`, `expose_headers`, and `allow_credentials` with this level's declared values for any of those fields this level explicitly declares, keeping the running value for any field it omits - `inst-cors-algo-merge-inherit-override`
   3. [ ] - `p1` - **ELSE** (this level's own `sharing` value is `enforce`) - `inst-cors-algo-merge-else-enforce`
      1. [ ] - `p1` - Keep the running effective `allowed_origins`, `enabled`, `allowed_methods`, `expose_headers`, and `allow_credentials` exactly as accumulated from the more general levels; discard any values this more specific level declared for those fields - `inst-cors-algo-merge-enforce-hold`
4. [ ] - `p1` - **IF** the final effective `allow_credentials` is `true` **AND** the final effective `allowed_origins` contains `*` - `inst-cors-algo-merge-if-invalid-final`
   1. [ ] - `p1` - Flag the effective configuration as a credentialed-wildcard configuration, consumed by `cpt-cf-oagw-algo-cors-match-origin`'s wildcard-drop rule at match time (the single-resource form of this same rule is already enforced at write time by the `cors` schema's `if`/`then` clause, owned by `cpt-cf-oagw-feature-upstream-management` and applied to the Upstream's own `cors` object only — Route resources carry no `cors` field for this clause to apply to, since `route.v1.schema.json` defines no top-level `cors` property; this step is the post-merge counterpart) - `inst-cors-algo-merge-flag-invalid`
5. [ ] - `p1` - **RETURN** the effective `cors` object - `inst-cors-algo-merge-return`

## 4. States (CDSL)

No state machine applies to this feature. CORS decision-making is a pure, stateless function evaluated independently on every request: preflight detection and response depend only on the current request's method and headers, and actual-request validation depends only on the effective `cors` configuration resolved fresh for that request plus the request's own `Origin`/method. Neither carries any entity through a multi-step lifecycle of its own. The `cors` object's own persistence lifecycle (create/update/delete) belongs to the Upstream resource it is a field of, which is `cpt-cf-oagw-feature-upstream-management`'s concern (DECOMPOSITION entry 2.2); `cpt-cf-oagw-feature-route-management`'s concern (2.3) is limited to accepting-but-not-persisting a `cors` object submitted in a Route request body, since `route.v1.schema.json` defines no top-level `cors` property — both are explicitly out of scope here per DECOMPOSITION entry 2.7's Out of scope list.

## 5. Definitions of Done

**Security, reliability, data-integrity, observability, and rollback**: Security — origin/method validation is the only access-control decision this feature makes, and it is a pure comparison against the already-merged effective `cors` configuration; it introduces no new credential or secret handling (that remains `cpt-cf-oagw-feature-plugin-execution`'s concern) and rejects before any upstream connection is opened. Reliability — this feature never opens a network connection or blocks on an external dependency to reach its preflight or origin/method decision, so the check itself cannot become an availability risk; a preflight is always answered `204` and an actual-request check is always a synchronous, in-memory evaluation of the resolved effective configuration. Data integrity — this feature persists nothing of its own; the only integrity property it owns is that the effective `cors` object it hands to `cpt-cf-oagw-algo-cors-match-origin`/`cpt-cf-oagw-algo-cors-check-method` is exactly what `cpt-cf-oagw-algo-cors-merge-effective-config` computed for that request, with no stale or cross-request reuse. Observability — a `403` origin-not-allowed or method-not-allowed rejection, and a preflight `204`, are recorded through `cpt-cf-oagw-feature-gear-foundation`'s base audit-log scaffold (status, duration, and the other fixed fields) exactly as any other request is; this feature adds no CORS-specific audit field of its own — the rejecting problem `type` and `X-OAGW-Error-Source: gateway` on the response itself are the observable signal. Rollback — not applicable: this feature introduces no database schema, no migration, and no persisted state of its own, so there is nothing to roll back; the `cors` configuration it reads is Upstream/tenant data owned and rolled back, if ever, by `cpt-cf-oagw-feature-upstream-management`.

**UX/Accessibility**: Not applicable because this feature has no user interface — it is a server-side request-handling behavior consumed only by browser HTTP clients issuing CORS requests, not a rendered UI surface.

**Compliance/Data Privacy**: Not applicable because this feature stores no personal or regulated data; it reads only the request's `Origin` header and HTTP method — values a browser sends on every cross-origin request — and neither is persisted beyond the in-flight request.

**Performance**: DECOMPOSITION entry 2.7 assigns this feature no dedicated NFR of its own (the hot-path latency budget, `cpt-cf-oagw-nfr-low-latency`, is `cpt-cf-oagw-feature-proxy-core`'s Requirements Covered entry). Both halves of this feature add only constant-time, in-memory work to a request already on that path — the preflight fast path is a single header echo with no upstream resolution, and the actual-request check is a string comparison against the already-merged effective `cors` configuration — so neither introduces a distinct performance budget beyond proxy-core's own.

### Preflight Fast Path

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-cors-preflight-fast-path`

The system **MUST** detect a CORS preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) and answer it at the proxy handler level — before alias/route resolution, tenant-context extraction, authentication, and plugin execution — with `204 No Content`, echoing the requested origin, method, and (when present) headers, adding a fixed `Access-Control-Max-Age`, `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, and `X-OAGW-Error-Source: gateway`, and without opening any connection to an upstream.

**Implements**:
- `cpt-cf-oagw-flow-cors-browser-preflight-request`
- `cpt-cf-oagw-algo-cors-preflight-detect-and-respond`

**Constraints**: None — DECOMPOSITION entry 2.7 lists no Design Constraints Covered; preflight behavior is governed entirely by `cpt-cf-oagw-adr-cors`.

**Touches**:
- API: `OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: CORS policy, Preflight decision

### Actual Cross-Origin Request Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-cors-actual-request-validation`

The system **MUST**, after proxy-core's upstream/route resolution and before forwarding — and before `cpt-cf-oagw-feature-rate-limiting`'s budget evaluation and `cpt-cf-oagw-feature-plugin-execution`'s Auth/Guard/Transform chain at that same shared post-resolution hook, so a request that is simultaneously CORS-invalid and over budget receives this feature's `403` rather than a `429` — validate, only when the effective `cors.enabled` is `true` and the request carries an `Origin` header, that the request's `Origin` is a member of the effective `allowed_origins` and that its method is a member of the effective `allowed_methods`, rejecting a non-conforming request with `403`, `X-OAGW-Error-Source: gateway`, and the documented `cf.oagw.cors.origin_not_allowed.v1` / `cf.oagw.cors.method_not_allowed.v1` problem body respectively, before any upstream connection is attempted.

**Implements**:
- `cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request`
- `cpt-cf-oagw-algo-cors-match-origin`
- `cpt-cf-oagw-algo-cors-check-method`

**Constraints**: None — see DECOMPOSITION entry 2.7 (no Design Constraints Covered).

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: CORS policy

### Security Defaults

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-cors-security-defaults`

The system **MUST** treat `cors.enabled: false` (the schema default, applied when no `cors` block resolves to an enabled state anywhere in the effective merge) as "perform no CORS processing" on the actual-request path — no origin/method validation and no `Access-Control-*` response headers, with proxy-core forwarding the request exactly as it would a non-CORS request. The system **MUST** perform exact, scheme-sensitive and port-sensitive string matching for `allowed_origins` with no regular-expression or substring matching. The system **MUST NOT** reject an `http://` origin merely for using a plaintext scheme — `cpt-cf-oagw-adr-cors`'s origin-matching rule is a literal, scheme- and port-sensitive exact match against the configured `allowed_origins` entries, with no separate scheme allowlist of its own; ADR-0004's own "Development (localhost)" configuration example lists `http://localhost:3000`/`http://localhost:5173` as `allowed_origins` entries matched under that same literal rule. This is independent of the `allow_http_upstream` gear-configuration flag, which governs only whether OAGW opens a plaintext connection to an *upstream endpoint*, not which browser `Origin` values are legal to configure or match — turning `allow_http_upstream` off does not, and must not, change whether an `http://` origin can match. The system **MUST** refuse to honor a wildcard `allowed_origins` entry for matching purposes whenever the effective `allow_credentials` is `true`, whether that combination arises from a single resource (already blocked at write time by the schema's `if`/`then` clause) or only after hierarchical merge.

**Implements**:
- `cpt-cf-oagw-algo-cors-match-origin`
- `cpt-cf-oagw-algo-cors-merge-effective-config`

**Constraints**: None — see DECOMPOSITION entry 2.7 (no Design Constraints Covered).

**Touches**:
- Entities: CORS policy

### Forwarded-Response Headers

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-cors-response-headers`

The system **MUST** add `Access-Control-Allow-Origin` (per `cpt-cf-oagw-algo-cors-match-origin`'s emitted value) to every successful forwarded response for a request that passed actual cross-origin validation, and **MUST** additionally add `Access-Control-Expose-Headers` when the effective `expose_headers` is non-empty and `Access-Control-Allow-Credentials: true` when the effective `allow_credentials` is `true`. The system **MUST** add `Vary: Origin` to every response — successful forward or `403` rejection — produced for a resolved upstream/route whose effective `cors.enabled` is `true`, to prevent cache poisoning.

**Implements**:
- `cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request`

**Constraints**: None — see DECOMPOSITION entry 2.7 (no Design Constraints Covered).

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: CORS policy

### Hierarchical CORS Merge

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-cors-hierarchical-merge`

The system **MUST** compute the effective `cors` configuration for a resolved request by layering the resolved Upstream's `cors` object together with each ancestor tenant's corresponding Upstream `cors` object, from most general (the root ancestor) to most specific (the resolving tenant), per `cpt-cf-oagw-algo-cors-merge-effective-config`: unioning `allowed_origins` across a layer transition whose more general side declares `sharing: inherit`, and holding `allowed_origins` (and the other `cors` fields) fixed to the more general level's values across a layer transition whose more general side declares `sharing: enforce`, consistent with DESIGN.md's hierarchical-configuration merge table row for CORS and the ancestor-enforcement rule that shadowing does not bypass. `route.v1.schema.json`'s top-level properties are exactly `id`, `tags`, `upstream_id`, `match`, `plugins`, `rate_limit` — it defines no top-level `cors` property, and the `cors` sub-schema under its `definitions` is referenced by nothing, so no Route-level `cors` object exists to participate in this merge; this feature's hierarchical merge is an Upstream-only, tenant-chain merge.

**Implements**:
- `cpt-cf-oagw-algo-cors-merge-effective-config`

**Constraints**: None — see DECOMPOSITION entry 2.7 (no Design Constraints Covered).

**Touches**:
- Entities: CORS policy

## 6. Acceptance Criteria

- [x] An `OPTIONS` request carrying `Origin` and `Access-Control-Request-Method` against any alias, including a non-existent one, receives `204 No Content` with echoed `Access-Control-Allow-Origin`/`Access-Control-Allow-Methods` (and `Access-Control-Allow-Headers` when `Access-Control-Request-Headers` was sent), `Access-Control-Max-Age`, `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, and `X-OAGW-Error-Source: gateway`, and no connection to any upstream is opened.
- [x] An actual cross-origin request whose `Origin` is in the effective `allowed_origins` and whose method is in the effective `allowed_methods` is forwarded to the upstream, and the response carries `Access-Control-Allow-Origin` per `cpt-cf-oagw-algo-cors-match-origin`, `Vary: Origin`, and — when configured — `Access-Control-Expose-Headers` and/or `Access-Control-Allow-Credentials: true`.
- [x] An actual cross-origin request whose `Origin` is not in the effective `allowed_origins` is rejected `403` with `X-OAGW-Error-Source: gateway` and a `problem+json` body whose `type` is `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`, before any upstream connection is opened.
- [x] An actual cross-origin request whose method is not in the effective `allowed_methods` is rejected `403` with `X-OAGW-Error-Source: gateway` and a `problem+json` body whose `type` is `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, before any upstream connection is opened.
- [x] `Vary: Origin` is present on every response — successful forward or `403` rejection — for a resolved upstream/route whose effective `cors.enabled` is `true`.
- [x] When the effective `cors.enabled` resolves to `false` (the schema default, no `cors` block enabling it anywhere in the chain), no `Access-Control-*` header is added, no origin/method validation is performed, and the request is forwarded exactly as proxy-core's non-CORS path would forward it.
- [x] An effective configuration combining `allow_credentials: true` with an effective `allowed_origins` entry of `*` never grants a match via that wildcard entry; a request whose `Origin` matches none of the remaining explicit entries is rejected `403` with the origin-not-allowed body.
- [x] An `http://` origin present in the effective `allowed_origins` is matched and allowed like any other entry; it is not rejected for using a plaintext scheme.
- [ ] With `sharing: inherit` declared at a more general level, the effective `allowed_origins` is the deduplicated union of that level's and the more specific level's declared origins.
- [ ] With `sharing: enforce` declared at a more general level, an origin declared only at a more specific level and absent from the more general level's list does not match, even though the more specific level's own configuration lists it.
