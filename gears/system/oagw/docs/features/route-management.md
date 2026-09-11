# Feature: Route Management API


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Route](#create-route)
  - [List Routes](#list-routes)
  - [Get Route by ID](#get-route-by-id)
  - [Replace Route](#replace-route)
  - [Delete Route](#delete-route)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Route Match Shape and gRPC-Branch Validation](#route-match-shape-and-grpc-branch-validation)
  - [Route Match-Rule Uniqueness Check](#route-match-rule-uniqueness-check)
  - [Route Upstream-Ownership Resolution](#route-upstream-ownership-resolution)
  - [Route Tenant-Scope Resolution](#route-tenant-scope-resolution)
- [4. States (CDSL)](#4-states-cdsl)
  - [Route Enablement State Machine](#route-enablement-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Cross-Cutting Dispositions](#cross-cutting-dispositions)
  - [Route CRUD Endpoints](#route-crud-endpoints)
  - [Route Schema-Shape Validation](#route-schema-shape-validation)
  - [Route Upstream Ownership and Immutability](#route-upstream-ownership-and-immutability)
  - [Route Match-Rule Uniqueness Enforcement](#route-match-rule-uniqueness-enforcement)
  - [Route Enable/Disable Field Ownership](#route-enabledisable-field-ownership)
  - [Route Tenant Scoping and Ancestor Invisibility](#route-tenant-scoping-and-ancestor-invisibility)
  - [Route Error Response Mapping](#route-error-response-mapping)
  - [Route gRPC Match Schema-Conformant Persistence](#route-grpc-match-schema-conformant-persistence)
  - [Route List OData Query Parameters](#route-list-odata-query-parameters)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-route-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-route-management`
## 1. Feature Context

### 1.1 Overview

Deliver the five tenant-scoped CRUD operations (`POST`/`GET` list/`GET` by id/`PUT`/`DELETE`) for Route resources at `/oagw/v1/routes`, which attach HTTP or gRPC match rules to a caller-owned Upstream, including schema-conformant field validation, match-rule uniqueness enforcement within an upstream, and `upstream_id` ownership/immutability rules.

### 1.2 Purpose

A Route is the configuration object that decides which inbound proxy requests (handled by proxy-core, `cpt-cf-oagw-feature-proxy-core`) are matched to a given Upstream and how. This feature owns the entire management-plane lifecycle of that object — validating and persisting HTTP match rules (methods/path/query-allowlist/path-suffix mode), accepting and persisting the schema-defined gRPC match branch without implementing any gRPC proxy behavior, enforcing that no two enabled routes under one upstream collide on `(path, priority)` for an overlapping method, and enforcing that a route's `upstream_id` is validated against the calling tenant's own upstreams at create time and is thereafter immutable. It does not implement proxy-time route matching, plugin execution, or rate-limit enforcement — those are 2.5, 2.9, and 2.8 respectively; this feature only persists the fields those later features read.

**Requirements**: `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-fr-enable-disable`, `cpt-cf-oagw-nfr-input-validation`, `cpt-cf-oagw-nfr-multi-tenancy`

**Use case**: `cpt-cf-oagw-usecase-configure-route`

**Interface**: `cpt-cf-oagw-interface-management-api`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**IMPORTANT — the `{id}` path parameter accepts both a bare UUID and the anonymous GTS form.** `route.v1.schema.json`'s `id` field is declared `{type: string, format: uuid}` — a bare UUID — while `DESIGN.md`'s Resource Identification Pattern documents API path parameters as the anonymous GTS identifier `gts.cf.core.oagw.route.v1~{uuid}`. This feature reconciles the two rather than picking one: `GET`/`PUT`/`DELETE /oagw/v1/routes/{id}` **MUST** accept `{id}` supplied in either form for the same resource (normalizing to the bare UUID before the tenant-scoped lookup), and the `id` field returned in every response body **MUST** always be the bare UUID, per `route.v1.schema.json`.

**Security, reliability, data-integrity, observability, rollback**: All five operations require Bearer-token authentication and the `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` permission per DESIGN.md's Authentication & Authorization table; this feature adds no authorization logic beyond enforcing tenant scoping from the calling SecurityContext's `tenant_id` (`cpt-cf-oagw-algo-route-tenant-scope-resolve` / `cpt-cf-oagw-algo-route-upstream-ownership-resolve`). Multi-table writes (route row plus its HTTP/gRPC match keys, method rows, tag rows, and plugin binding rows) MUST be atomic (single transaction) per DESIGN.md's Key Invariants — a failure partway through create/replace MUST leave no partial route visible, and no compensating/rollback logic beyond transaction abort is required because this feature makes no outbound network calls and resolves no credentials. Structured audit-log emission for route create/update/delete populates gear-foundation's audit-log scaffold (`event`, `tenant_id`, `principal_id`, `path`, `method`, `status`) with this feature's own values; per-request Prometheus metrics are explicitly scoped to the proxy hot path (2.5) and are not part of this management-plane feature's testable surface — not applicable here for that documented reason.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, reads, replaces, and deletes Route resources scoped to their tenant, attaching match rules to an owned Upstream. |
| `cpt-cf-oagw-actor-tenant-admin` | Performs the same five operations within the sharing/permission constraints granted by ancestor tenants. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-oagw-usecase-configure-route`

### Create Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-create`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator submits a schema-valid Route body referencing an Upstream owned by their own tenant; a new Route is persisted with a server-generated `id` and returned.

**Error Scenarios**:
- Request body fails `route.v1.schema.json` shape validation (e.g. `match` has both or neither of `http`/`grpc`, `http.methods` empty or contains an unsupported verb, `tags[]` entry fails `^[a-z0-9_-]+$`) → `400 ValidationError`.
- `upstream_id` does not exist, or exists but is not owned by the calling tenant (including an ancestor tenant's upstream) → `400 ValidationError`.
- An already-enabled route under the same upstream shares `(path, priority)` for a method the candidate route also declares, and the candidate is being created enabled → `409 Conflict` (status + RFC 9457 envelope only; see `cpt-cf-oagw-dod-route-error-mapping`).

**Steps**:
1. [x] - `p1` - Operator sends `POST /oagw/v1/routes` with a Route body per `route.v1.schema.json`, optionally including the schema-compatible `enabled` and `priority` additions - `inst-route-create-request`
2. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-match-validate` against the request body - `inst-route-create-validate-shape`
3. [x] - `p1` - **IF** shape validation fails - `inst-route-create-shape-if`
   1. [x] - `p1` - **RETURN** `400 ValidationError` with per-field violation detail - `inst-route-create-shape-fail`
4. [x] - `p1` - **ELSE** - `inst-route-create-shape-else`
   1. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-upstream-ownership-resolve` for the request's `upstream_id` scoped to the calling `tenant_id` - `inst-route-create-ownership`
5. [x] - `p1` - **IF** the referenced upstream does not exist, or is not owned by the calling tenant - `inst-route-create-ownership-if`
   1. [x] - `p1` - **RETURN** `400 ValidationError` - `inst-route-create-ownership-fail`
6. [x] - `p1` - **ELSE** - `inst-route-create-ownership-else`
   1. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-uniqueness-check` for the candidate `(path, priority, methods)` within the resolved upstream - `inst-route-create-uniqueness`
7. [x] - `p1` - **IF** the uniqueness check reports a collision against another enabled route - `inst-route-create-uniqueness-if`
   1. [x] - `p1` - **RETURN** `409 Conflict` — status and RFC 9457 envelope only, no GTS `type` asserted (`cpt-cf-oagw-dod-route-error-mapping`) - `inst-route-create-uniqueness-fail`
8. [x] - `p1` - **ELSE** - `inst-route-create-uniqueness-else`
   1. [x] - `p1` - DB: INSERT `oagw_route` (with server-generated `id`, calling `tenant_id`, resolved `upstream_id`, `enabled` defaulted to `true` when omitted) plus its `oagw_route_http_match`/`oagw_route_grpc_match`, `oagw_route_method`, and `oagw_route_tag` rows, atomically in one transaction - `inst-route-create-persist`
9. [x] - `p1` - **RETURN** `201 Created` with the persisted Route body - `inst-route-create-return`

### List Routes

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-list`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator lists Route resources scoped to their own tenant, optionally filtered/sorted/paged via OData query parameters.

**Error Scenarios**:
- None specific to this operation beyond the shared authentication/authorization failures already owned by gear-foundation; ancestor-tenant routes are never returned (silently excluded, not surfaced as an error).

**Steps**:
1. [x] - `p1` - Operator sends `GET /oagw/v1/routes` with optional `$filter`, `$select`, `$orderby`, `$top` (default 50, max 100), `$skip` query parameters - `inst-route-list-request`
2. [x] - `p1` - DB: SELECT `oagw_route` rows WHERE `tenant_id` = calling tenant, applying the OData filter/select/orderby/pagination parameters - `inst-route-list-query`
3. [x] - `p1` - **RETURN** `200 OK` with the paged list of Route bodies scoped to the calling tenant only (no ancestor-tenant routes included) - `inst-route-list-return`

### Get Route by ID

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-get`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator retrieves a single Route resource they own by `id`.

**Error Scenarios**:
- The `id` does not exist, or exists under a different tenant (including an ancestor tenant) → `404` (status + RFC 9457 envelope only; see `cpt-cf-oagw-dod-route-error-mapping`).

**Steps**:
1. [x] - `p1` - Operator sends `GET /oagw/v1/routes/{id}`, where `{id}` is either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}` for the target route - `inst-route-get-request`
2. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-tenant-scope-resolve` for `{id}` scoped to the calling `tenant_id`, normalizing `{id}` to its bare UUID form first when supplied as the anonymous GTS identifier - `inst-route-get-resolve`
3. [x] - `p1` - **IF** no matching row is found for the calling tenant - `inst-route-get-if`
   1. [x] - `p1` - **RETURN** `404` — status and RFC 9457 envelope only, no GTS `type` asserted - `inst-route-get-fail`
4. [x] - `p1` - **ELSE** - `inst-route-get-else`
   1. [x] - `p1` - **RETURN** `200 OK` with the resolved Route body - `inst-route-get-return`

### Replace Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-replace`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator fully replaces an owned Route's match rules, tags, plugin bindings, rate-limit configuration, and/or `enabled` state; `upstream_id` is not part of the update DTO and remains unchanged.

**Error Scenarios**:
- The `id` does not exist, or exists under a different tenant (including an ancestor tenant) → `404` (status + RFC 9457 envelope only; see `cpt-cf-oagw-dod-route-error-mapping`).
- Replacement body fails shape validation → `400 ValidationError`.
- The replacement body includes an `upstream_id` value that differs from the persisted one → `400 ValidationError` (immutability violation; `upstream_id` is absent from the update DTO per DESIGN.md's CRUD semantics).
- The replacement's match rules collide with another enabled route's `(path, priority)` for an overlapping method under the same upstream → `409 Conflict` (status + RFC 9457 envelope only; see `cpt-cf-oagw-dod-route-error-mapping`).

**Steps**:
1. [x] - `p1` - Operator sends `PUT /oagw/v1/routes/{id}` with a full replacement body per `route.v1.schema.json`, excluding `upstream_id` and `id`; `{id}` is either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}` for the target route - `inst-route-replace-request`
2. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-tenant-scope-resolve` for `{id}` scoped to the calling `tenant_id`, normalizing `{id}` to its bare UUID form first when supplied as the anonymous GTS identifier - `inst-route-replace-resolve`
3. [x] - `p1` - **IF** no matching row is found for the calling tenant - `inst-route-replace-notfound-if`
   1. [x] - `p1` - **RETURN** `404` — status and RFC 9457 envelope only, no GTS `type` asserted - `inst-route-replace-notfound-fail`
4. [x] - `p1` - **ELSE** - `inst-route-replace-notfound-else`
   1. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-match-validate` against the replacement body - `inst-route-replace-validate-shape`
5. [x] - `p1` - **IF** shape validation fails, or the request body carries an `upstream_id` differing from the persisted value - `inst-route-replace-shape-if`
   1. [x] - `p1` - **RETURN** `400 ValidationError` - `inst-route-replace-shape-fail`
6. [x] - `p1` - **ELSE** - `inst-route-replace-shape-else`
   1. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-uniqueness-check` for the replacement's `(path, priority, methods)`, excluding the route being replaced from the collision scan - `inst-route-replace-uniqueness`
7. [x] - `p1` - **IF** the uniqueness check reports a collision against another enabled route - `inst-route-replace-uniqueness-if`
   1. [x] - `p1` - **RETURN** `409 Conflict` — status and RFC 9457 envelope only, no GTS `type` asserted (`cpt-cf-oagw-dod-route-error-mapping`) - `inst-route-replace-uniqueness-fail`
8. [x] - `p1` - **ELSE** - `inst-route-replace-uniqueness-else`
   1. [x] - `p1` - DB: full-replacement UPDATE of `oagw_route` and its match/method/tag/plugin-binding rows in one transaction, retaining the existing `id`, `tenant_id`, and `upstream_id`; any optional field omitted from the request body is cleared to its schema default (e.g. omitted `enabled` resets to `true`, omitted `path_suffix_mode` resets to `append`) - `inst-route-replace-persist`
9. [x] - `p1` - **RETURN** `200 OK` with the replaced Route body - `inst-route-replace-return`

### Delete Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-delete`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator deletes an owned Route by `id`; the route and its match/method/tag/plugin-binding rows are removed.

**Error Scenarios**:
- The `id` does not exist, or exists under a different tenant (including an ancestor tenant) → `404` (status + RFC 9457 envelope only; see `cpt-cf-oagw-dod-route-error-mapping`).

**Steps**:
1. [x] - `p1` - Operator sends `DELETE /oagw/v1/routes/{id}`, where `{id}` is either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}` for the target route - `inst-route-delete-request`
2. [x] - `p1` - System runs `cpt-cf-oagw-algo-route-tenant-scope-resolve` for `{id}` scoped to the calling `tenant_id`, normalizing `{id}` to its bare UUID form first when supplied as the anonymous GTS identifier - `inst-route-delete-resolve`
3. [x] - `p1` - **IF** no matching row is found for the calling tenant - `inst-route-delete-if`
   1. [x] - `p1` - **RETURN** `404` — status and RFC 9457 envelope only, no GTS `type` asserted - `inst-route-delete-fail`
4. [x] - `p1` - **ELSE** - `inst-route-delete-else`
   1. [x] - `p1` - DB: DELETE `oagw_route` row WHERE `id` = `{id}` AND `tenant_id` = calling tenant, cascading to its `oagw_route_http_match`/`oagw_route_grpc_match`, `oagw_route_method`, `oagw_route_tag`, and `oagw_route_plugin` rows - `inst-route-delete-persist`
   2. [x] - `p1` - **RETURN** `204 No Content` - `inst-route-delete-return`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly.

### Route Match Shape and gRPC-Branch Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-match-validate`

**Input**: candidate Route request body (create: full body per `route.v1.schema.json`; replace: full body excluding `upstream_id`/`id`)

**Output**: a normalized/defaulted Route field set, or a `400 ValidationError` carrying every accumulated field-level violation

**Steps**:
1. [x] - `p1` - Parse the request body and confirm `match` is present with **exactly one** of `http`/`grpc` (`route.v1.schema.json`'s `oneOf`) and no properties other than `http`/`grpc` - `inst-match-validate-oneof`
2. [x] - `p1` - **IF** `match.http` is present - `inst-match-validate-http-if`
   1. [x] - `p1` - Validate `methods` is a non-empty array (`minItems: 1`) whose entries are each one of `GET`/`POST`/`PUT`/`DELETE`/`PATCH` - `inst-match-validate-http-methods`
   2. [x] - `p1` - Validate `path` is a non-empty string - `inst-match-validate-http-path`
   3. [x] - `p1` - Default `query_allowlist` to `[]` when absent; treat an empty array as "allow no query parameters" (not "allow all") per the schema's documented default semantics - `inst-match-validate-http-query-allowlist`
   4. [x] - `p1` - Default `path_suffix_mode` to `append` when absent; validate the value is one of `disabled`/`append` - `inst-match-validate-http-suffix-mode`
   5. [x] - `p1` - Require the schema-compatible `priority` field (root object accepts additional properties; consistent with the Route domain-model entity's `+Int priority`) to be present alongside `match.http`, since it participates in the `(path, priority, method)` uniqueness key - `inst-match-validate-http-priority`
3. [x] - `p1` - **ELSE** (`match.grpc` is present) - `inst-match-validate-grpc-else`
   1. [x] - `p1` - Validate `service` and `method` are each non-empty strings - `inst-match-validate-grpc-fields`
   2. [x] - `p1` - Accept and persist the `grpc` branch schema-conformantly; do not evaluate, require, or reserve any proxy-time matching/uniqueness behavior for it, since no gRPC proxy code path exists or is reachable in this system (`DESIGN.md` §3.1, §4.7 item 7; `PRD.md` §4.2) - `inst-match-validate-grpc-persist`
4. [x] - `p1` - Validate every entry of `tags[]` matches `^[a-z0-9_-]+$` - `inst-match-validate-tags`
5. [x] - `p1` - Validate `plugins.sharing` is one of `private`/`inherit`/`enforce` (default `private`) and every `plugins.items[]` entry is a syntactically well-formed GTS-identifier string, without resolving or executing it (resolution/execution is `cpt-cf-oagw-feature-plugin-execution`'s concern); default `items` to `[]` - `inst-match-validate-plugins`
6. [x] - `p1` - **IF** `rate_limit` is present - `inst-match-validate-ratelimit-if`
   1. [x] - `p1` - Validate `sustained.rate` is present and `>= 1`; default `sustained.window` to `second`, `burst.capacity` to `sustained.rate` when unspecified, `scope` to `tenant`, `strategy` to `reject`, and `cost` to `1`, persisting these fields only (no enforcement) - `inst-match-validate-ratelimit-fields`
7. [x] - `p1` - Default `enabled` to `true` when omitted (schema-compatible addition, consistent with the Route domain-model's `+Boolean enabled`) - `inst-match-validate-enabled-default`
8. [x] - `p1` - **IF** the request body contains a `cors` object - `inst-match-validate-cors-if`
   1. [x] - `p1` - Accept it at the shape-validation level (the Route root object has no `additionalProperties: false` constraint) but do not persist it as route-level CORS configuration — `route.v1.schema.json` defines a `cors` sub-schema under `definitions` that no top-level Route property references, so Route resources carry no functional `cors` field in this round - `inst-match-validate-cors-ignore`
9. [x] - `p1` - **TRY** accumulate every violation found in steps 1-6 rather than stopping at the first - `inst-match-validate-try`
10. [x] - `p1` - **CATCH** one or more accumulated violations - `inst-match-validate-catch`
    1. [x] - `p1` - **RETURN** `400 ValidationError` with the accumulated field-level violation list - `inst-match-validate-catch-return`
11. [x] - `p1` - **RETURN** the normalized, defaulted Route field set - `inst-match-validate-return`

### Route Match-Rule Uniqueness Check

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-uniqueness-check`

**Input**: resolved `upstream_id`, candidate `enabled`, and (only when `match.http` is present) candidate `path`, `priority`, `methods[]`; on replace, the `route_id` being replaced

**Output**: pass, or `409 Conflict`

**Steps**:
1. [x] - `p1` - **IF** the candidate route is being persisted with `enabled: false` - `inst-uniqueness-disabled-if`
   1. [x] - `p1` - **RETURN** pass — the documented invariant restricts only pairs of **enabled** routes - `inst-uniqueness-disabled-return`
2. [x] - `p1` - **ELSE IF** the candidate route's match is `grpc` (no `match.http`) - `inst-uniqueness-grpc-if`
   1. [x] - `p1` - **RETURN** pass — the source documents specify the `(path_prefix, priority)`-per-method uniqueness invariant only for HTTP match rules; no gRPC route uniqueness invariant is documented - `inst-uniqueness-grpc-return`
3. [x] - `p1` - **ELSE** - `inst-uniqueness-http-else`
   1. [x] - `p1` - DB: SELECT enabled `oagw_route` rows (and their `oagw_route_http_match`/`oagw_route_method` rows) under the same `upstream_id`, excluding `route_id` (on replace) - `inst-uniqueness-query`
   2. [x] - `p1` - **FOR EACH** other enabled route returned - `inst-uniqueness-foreach-route`
      1. [x] - `p1` - **FOR EACH** method in the intersection of the candidate's `methods[]` and the other route's `methods[]` - `inst-uniqueness-foreach-method`
         1. [x] - `p1` - **IF** the other route's `path` equals the candidate's `path` **AND** the other route's `priority` equals the candidate's `priority` - `inst-uniqueness-collision-if`
            1. [x] - `p1` - **RETURN** `409 Conflict` — per controller decision D1, this collision asserts HTTP status `409` and the RFC 9457 envelope shape only; it MUST NOT assert the `400`-scoped `ValidationError`/`RouteError` GTS `type` (`gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`), since reusing that one identifier across two different HTTP statuses would defeat `type`-based client dispatch (see `cpt-cf-oagw-dod-route-error-mapping`) - `inst-uniqueness-collision-return`
4. [x] - `p1` - **RETURN** pass — no collision found - `inst-uniqueness-pass-return`

### Route Upstream-Ownership Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-upstream-ownership-resolve`

**Input**: `upstream_id` from the create request, calling `tenant_id`

**Output**: the resolved Upstream record, or `400 ValidationError`

**Steps**:
1. [x] - `p1` - DB: SELECT `oagw_upstream` WHERE `id` = `upstream_id` AND `tenant_id` = calling `tenant_id` — no tenant-hierarchy walk is performed here; unlike proxy-time alias resolution (2.5), a route may only reference an upstream directly owned by the calling tenant, never an ancestor's - `inst-ownership-query`
2. [x] - `p1` - **IF** no row is found (the upstream does not exist, or exists but belongs to a different tenant, including an ancestor tenant) - `inst-ownership-notfound-if`
   1. [x] - `p1` - **RETURN** `400 ValidationError` per `route.v1.schema.json` field-reference validation, not `404` (the object being validated is the *reference*, not the Route resource's own visibility) - `inst-ownership-notfound-return`
3. [x] - `p1` - **ELSE** - `inst-ownership-else`
   1. [x] - `p1` - This check MUST NOT reject the reference based on the resolved upstream's endpoint `scheme` (including plaintext `http`, legal to declare per `cpt-cf-oagw-feature-upstream-management` when `allow_http_upstream: true`) — scheme legality and plaintext-connection enforcement belong to 2.2/2.5, not this feature - `inst-ownership-no-scheme-restriction`
   2. [x] - `p1` - **RETURN** the resolved Upstream record - `inst-ownership-return`

This algorithm runs only at create time. On replace, `upstream_id` is immutable and absent from the update DTO; the persisted `upstream_id` is retained unchanged and this resolution is not re-run.

### Route Tenant-Scope Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-tenant-scope-resolve`

**Input**: route `id` (path parameter, accepted as either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}` and normalized to the bare UUID before lookup), calling `tenant_id`

**Output**: the resolved Route record, or `404` (status + RFC 9457 envelope only; see `cpt-cf-oagw-dod-route-error-mapping`)

**Steps**:
1. [x] - `p1` - DB: SELECT `oagw_route` WHERE `id` = `{normalized bare UUID}` AND `tenant_id` = calling `tenant_id` - `inst-tenantscope-query`
2. [x] - `p1` - **IF** no row is found (the route does not exist, or exists under a different tenant, including an ancestor tenant) - `inst-tenantscope-notfound-if`
   1. [x] - `p1` - **RETURN** `404` — status and RFC 9457 envelope only; `RouteNotFound` (`gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`) is defined for proxy-time route matching (2.5)'s no-match case, a distinct scenario from this management-API by-id lookup miss, and per controller decision D1 is not asserted here. Ancestor routes are never visible, readable, replaceable, or deletable through this management API, even though they remain visible to proxy-time route matching in 2.5's tenant-chain walk - `inst-tenantscope-notfound-return`
3. [x] - `p1` - **ELSE** - `inst-tenantscope-else`
   1. [x] - `p1` - **RETURN** the resolved Route record for the calling GET-by-id / PUT / DELETE handler - `inst-tenantscope-return`

The list operation (`cpt-cf-oagw-flow-route-list`) applies the same `tenant_id` filter directly in its query rather than invoking a per-row not-found branch; no row belonging to an ancestor tenant ever surfaces in a list result.

## 4. States (CDSL)

### Route Enablement State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-route-enablement`

**States**: Enabled, Disabled

**Initial State**: Enabled (the schema-compatible `enabled` field defaults to `true` when omitted at create time; an operator may explicitly create a route already Disabled by setting `enabled: false` in the create request)

**Transitions**:
1. [x] - `p1` - **FROM** Enabled **TO** Disabled **WHEN** a `PUT /oagw/v1/routes/{id}` replacement body explicitly sets `enabled: false` - `inst-route-state-to-disabled`
2. [x] - `p1` - **FROM** Disabled **TO** Enabled **WHEN** a `PUT /oagw/v1/routes/{id}` replacement body sets `enabled: true`, or omits `enabled` entirely (full-replacement semantics clear an omitted optional field to its schema default of `true`, per `DESIGN.md`'s PUT/Replace CRUD semantics) - `inst-route-state-to-enabled`

This feature owns only the field's persistence, its default, and these write-time transitions. Excluding a Disabled route from proxy-time route matching is `cpt-cf-oagw-feature-proxy-core`'s (2.5) concern, not implemented here.

## 5. Definitions of Done

### Cross-Cutting Dispositions

The following review domains are explicitly dispositioned for this feature, rather than left silent:

- **Performance**: Not applicable as a dedicated budget — this feature's five operations are low-rate, latency-insensitive control-plane CRUD against the config-store, not the request-forwarding hot path; per-request latency budgets are owned by `cpt-cf-oagw-feature-proxy-core` (2.5).
- **UX/Accessibility**: Not applicable — this is a machine-to-machine REST management API with no user interface; there is no visual or interactive surface for accessibility criteria to apply to.
- **Compliance/Privacy**: Not applicable because no personal or regulated data is stored — a Route record holds match-rule configuration (methods, path, priority, plugin/rate-limit references) referencing a caller-owned Upstream; it carries no credential material and no end-user personal data.
- **Resilience/Recovery**: Not applicable beyond the transactional-atomicity guarantee already stated above (Security/reliability/data-integrity/observability/rollback) — this feature performs no outbound network calls and owns no retry, failover, or circuit-breaker behavior of its own.
- **Data Privacy**: Not applicable for the same reason as Compliance/Privacy above — no end-user personal data flows through or is retained by this feature's CRUD surface.
- **External Integrations**: Not applicable as a live-integration concern — this feature only validates and persists a reference (`upstream_id`) to a caller-owned Upstream and identifiers referencing plugins (`plugins.items[]`); it never itself calls out to an upstream or a plugin at write time.
- **Config/Health**: Not applicable — this feature introduces no gear-configuration flags or health-check endpoints of its own.

### Route CRUD Endpoints

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-crud-endpoints`

The system **MUST** implement all five Route operations at the exact gear-relative paths `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}` (no `/api` prefix; the gear registers gear-relative routes).

**Implements**:
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-route-list`
- `cpt-cf-oagw-flow-route-get`
- `cpt-cf-oagw-flow-route-replace`
- `cpt-cf-oagw-flow-route-delete`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

### Route Schema-Shape Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-schema-validation`

The system **MUST** validate every create/replace request body against `route.v1.schema.json`'s enumerated fields and defaults: `id` (read-only, server-generated, never accepted from the client), `tags[]` pattern, `upstream_id`, `match` one-of `http`/`grpc`, `match.http.methods`/`path`/`query_allowlist`/`path_suffix_mode`, `match.grpc.service`/`method`, `plugins.sharing`/`items[]`, `rate_limit`'s sub-fields — plus the two documented schema-versus-domain-model additions `enabled` and `priority`, which the schema's permissive root object allows even though they are not separately enumerated among the excerpted top-level schema properties.

**Implements**:
- `cpt-cf-oagw-algo-route-match-validate`

**Constraints**: `cpt-cf-oagw-nfr-input-validation`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `Route`, `HTTP match`, `gRPC match`

### Route Upstream Ownership and Immutability

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-upstream-ownership`

The system **MUST** validate, at create time only, that `upstream_id` references an Upstream owned by the calling tenant (never an ancestor's), rejecting a non-existent or non-owned reference with `400 ValidationError`; `upstream_id` **MUST** be immutable thereafter (absent from the replace DTO; any attempt to change it via `PUT` is rejected with `400 ValidationError`).

**Implements**:
- `cpt-cf-oagw-algo-route-upstream-ownership-resolve`
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-route-replace`

**Constraints**: `cpt-cf-oagw-nfr-multi-tenancy`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`, `Upstream`

### Route Match-Rule Uniqueness Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-match-uniqueness`

The system **MUST** reject, with `409 Conflict`, a create or replace whose HTTP match rule would make two enabled routes under the same upstream share `(path, priority)` for an overlapping method; a disabled candidate route or either route's `grpc` match is exempt from this invariant, per the source documents.

**Implements**:
- `cpt-cf-oagw-algo-route-uniqueness-check`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`, `HTTP match`

### Route Enable/Disable Field Ownership

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-enable-disable`

The system **MUST** persist the Route `enabled` boolean (default `true`), accept explicit `true`/`false` values on create and replace, and reset it to `true` on a replace that omits the field, without enforcing it against live proxy traffic (that enforcement is `cpt-cf-oagw-feature-proxy-core`'s concern).

**Implements**:
- `cpt-cf-oagw-state-route-enablement`

**Constraints**: none beyond `cpt-cf-oagw-fr-enable-disable`'s field-ownership scope

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

### Route Tenant Scoping and Ancestor Invisibility

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-tenant-scope`

The system **MUST** scope all five operations strictly to the calling `tenant_id`; `GET`/`PUT`/`DELETE` by `id` (accepting `{id}` as either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}`) **MUST** return `404` (status + RFC 9457 envelope only, per `cpt-cf-oagw-dod-route-error-mapping` — no GTS `type` asserted) for a route that does not exist or belongs to a different tenant (including an ancestor tenant), and `GET` (list) **MUST** never include an ancestor-tenant route in its results.

**Implements**:
- `cpt-cf-oagw-algo-route-tenant-scope-resolve`
- `cpt-cf-oagw-flow-route-list`
- `cpt-cf-oagw-flow-route-get`
- `cpt-cf-oagw-flow-route-replace`
- `cpt-cf-oagw-flow-route-delete`

**Constraints**: `cpt-cf-oagw-principle-tenant-scope`, `cpt-cf-oagw-nfr-multi-tenancy`

**Touches**:
- API: `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

### Route Error Response Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-error-mapping`

The system **MUST** return the following status/body combinations for the Route management API, all rendered through gear-foundation's RFC 9457 envelope (`cpt-cf-oagw-principle-rfc9457`) with `X-OAGW-Error-Source: gateway` (`cpt-cf-oagw-principle-error-source`) for every error case:

| HTTP Status | Scenario | GTS `type` identifier | Note |
|---|---|---|---|
| `201` | Create succeeds | n/a (success; no problem+json body) | — |
| `200` | Get/List/Replace succeeds | n/a (success) | — |
| `204` | Delete succeeds | n/a (success; no body) | — |
| `400` | Shape-validation failure, or non-existent/non-owned `upstream_id`, or an attempted `upstream_id` change on replace | `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` | Documented in `DESIGN.md`'s Error Response Format table under both the `RouteError` and `ValidationError` labels, which share this one GTS identifier |
| `404` | Route not found for the calling tenant, including an ancestor's route (management-API by-id lookup miss on `GET`/`PUT`/`DELETE`) | n/a — status + RFC 9457 envelope only (per controller decision D1) | `DESIGN.md`'s Tenant Scoping table documents management-API not-found as a bare `404` for all three resource kinds, with no `type`. `RouteNotFound` (`gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`) is the only `404`-class GTS identifier `DESIGN.md`'s Error Response Format table defines, but it is scoped to proxy-time route matching (2.5)'s no-match case, a distinct scenario from this one; it is therefore not asserted here, and this feature introduces no new GTS identifier |
| `409` | Match-rule uniqueness collision | n/a — status + RFC 9457 envelope only (per controller decision D1) | `DESIGN.md`'s Error Response Format table documents only one `409` identifier (`PluginInUse`, scoped to plugin-deletion conflicts), which does not apply here. Reusing the `400`-scoped `ValidationError`/`RouteError` identifier at `409` would make one GTS `type` resolve to two different HTTP statuses and defeat `type`-based client dispatch, so per controller decision D1 this feature asserts HTTP status `409` and the RFC 9457 envelope shape only, per `DECOMPOSITION.md` §2.3's explicit but unmapped "`409 Conflict` on collision" instruction, without minting an undocumented new GTS identifier |

**Implements**:
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-route-get`
- `cpt-cf-oagw-flow-route-replace`
- `cpt-cf-oagw-flow-route-delete`
- `cpt-cf-oagw-algo-route-uniqueness-check`
- `cpt-cf-oagw-algo-route-upstream-ownership-resolve`
- `cpt-cf-oagw-algo-route-tenant-scope-resolve`

**Touches**:
- API: `POST /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`

### Route gRPC Match Schema-Conformant Persistence

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-grpc-persist`

The system **MUST** accept and persist a `match.grpc` branch (`service`, `method`) exactly as `route.v1.schema.json` defines it, and **MUST NOT** implement, invoke, or reserve any gRPC proxy-time matching or forwarding behavior for it — `DESIGN.md` states no gRPC proxy code path is implemented or reachable in this system, and `PRD.md` §4.2 excludes gRPC proxying platform-wide for this round.

**Implements**:
- `cpt-cf-oagw-algo-route-match-validate`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `gRPC match`

### Route List OData Query Parameters

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-list-query`

The system **MUST** support `$filter`, `$select`, `$orderby`, `$top` (default 50, max 100), and `$skip` on `GET /oagw/v1/routes`, per `DESIGN.md`'s Route List Query Parameters table, applied after tenant scoping.

**Implements**:
- `cpt-cf-oagw-flow-route-list`

**Touches**:
- API: `GET /oagw/v1/routes`
- Entities: `Route`

## 6. Acceptance Criteria

- [x] `POST /oagw/v1/routes` with a schema-valid `match.http` body referencing an upstream owned by the caller's tenant returns `201` with a server-generated `id`, and any client-supplied `id` in the request body is disregarded.
- [x] `POST /oagw/v1/routes` with `match` containing both `http` and `grpc` is rejected with `400 ValidationError`; `match` containing neither is likewise rejected with `400 ValidationError`.
- [x] `POST /oagw/v1/routes` with `match.http.methods` empty, or containing a verb outside `GET`/`POST`/`PUT`/`DELETE`/`PATCH`, is rejected with `400 ValidationError`.
- [x] `POST /oagw/v1/routes` omitting `match.http.query_allowlist` persists an empty array whose runtime meaning is "allow no query parameters," not "allow all."
- [x] `POST /oagw/v1/routes` omitting `match.http.path_suffix_mode` persists `append`; an explicit `disabled` value is persisted as given.
- [x] `POST /oagw/v1/routes` with a `tags[]` entry that does not match `^[a-z0-9_-]+$` is rejected with `400 ValidationError`.
- [x] `POST /oagw/v1/routes` with `match.grpc.service`/`method` populated and no `match.http` is accepted, persisted, and returned unchanged on a subsequent `GET`, with no gRPC proxy-time behavior exercised or exercisable through this API.
- [x] `POST /oagw/v1/routes` with `upstream_id` referencing a non-existent upstream, or an upstream owned by a different tenant (including an ancestor tenant), is rejected with `400 ValidationError`.
- [x] `POST /oagw/v1/routes` with `upstream_id` referencing the caller's own upstream that declares a plaintext `http` endpoint scheme succeeds (this feature adds no scheme-based restriction).
- [x] Creating a second enabled route on the same upstream with the same `(path, priority)` and an overlapping method as an existing enabled route is rejected with `409 Conflict`; creating it with a disjoint method set, a different `priority`, or as `enabled: false` succeeds.
- [x] `PUT /oagw/v1/routes/{id}` with a request body that includes an `upstream_id` different from the persisted value is rejected with `400 ValidationError`; the persisted `upstream_id` is unaffected.
- [x] `PUT /oagw/v1/routes/{id}` omitting `enabled` resets the persisted value to `true`, per full-replacement clear-to-default semantics.
- [x] `PUT /oagw/v1/routes/{id}` whose new match rules collide with a different existing enabled route's `(path, priority)` for an overlapping method is rejected with `409 Conflict`; replacing a route with itself's unchanged match rules succeeds.
- [x] `GET /oagw/v1/routes/{id}` for a route owned by a different tenant, or for a route belonging to an ancestor tenant, returns `404` (status + RFC 9457 envelope only, no GTS `type` asserted); it never returns `403`.
- [x] `GET /oagw/v1/routes` never includes a route belonging to an ancestor or descendant tenant in its results, regardless of `$filter`.
- [x] `DELETE /oagw/v1/routes/{id}` for an owned route returns `204` and removes the route and all of its match/method/tag/plugin-binding rows; repeating the same `DELETE` returns `404` (status + RFC 9457 envelope only, no GTS `type` asserted).
- [x] Every error response from this API's five endpoints is `application/problem+json`, carries `X-OAGW-Error-Source: gateway`, and uses the `type`/`status` pairing documented in `cpt-cf-oagw-dod-route-error-mapping`.
- [x] A `cors` object submitted in a create or replace request body does not cause a shape-validation failure and is not persisted or interpreted as route-level CORS configuration.
- [x] `GET /oagw/v1/routes` honors `$top`/`$skip` pagination and defaults to at most 50 results when `$top` is omitted, never returning more than 100.
- [x] `GET /oagw/v1/routes/{id}` succeeds identically (`200 OK`, identical body) whether `{id}` is supplied as the bare UUID `id` value or as the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}` for the same route.
