# Feature: Route Management API


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Route Flow](#create-route-flow)
  - [List Routes Flow](#list-routes-flow)
  - [Get Route Flow](#get-route-flow)
  - [Replace Route Flow](#replace-route-flow)
  - [Delete Route Flow](#delete-route-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Route Body Validation Algorithm](#route-body-validation-algorithm)
  - [Match-Rule Uniqueness Algorithm](#match-rule-uniqueness-algorithm)
  - [List Query Algorithm](#list-query-algorithm)
- [4. States (CDSL)](#4-states-cdsl)
  - [Route Lifecycle State Machine](#route-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement Route CRUD Endpoints](#implement-route-crud-endpoints)
  - [Enforce Route Body Validation](#enforce-route-body-validation)
  - [Enforce Match-Rule Uniqueness](#enforce-match-rule-uniqueness)
  - [Enforce Tenant Scoping on Every Operation](#enforce-tenant-scoping-on-every-operation)
  - [Enforce Per-Endpoint Authorization](#enforce-per-endpoint-authorization)
  - [Emit RFC 9457 Error Envelopes](#emit-rfc-9457-error-envelopes)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Applicability](#7-applicability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-route-management-api-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-route-management-api`

## 1. Feature Context

### 1.1 Overview

Five REST endpoints for creating, listing, fetching, replacing, and deleting routes — the
per-upstream matching rules that decide which inbound proxy requests are allowed through and
which upstream behavior they trigger.

### 1.2 Purpose

Operators and tenant administrators need a way to declare, before proxying goes live, which
methods, paths, and query parameters are reachable on a given upstream. This feature exposes
that declaration surface. It builds on `cpt-cf-oagw-feature-upstream-management-api` because
every route names an `upstream_id` that must already be visible through upstream CRUD, and it
is itself a prerequisite for `cpt-cf-oagw-feature-proxy-data-plane-http`, which matches inbound
requests against the routes this feature persists.

**Requirements**: `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-usecase-configure-route`

**Principles**: None newly covered here; `cpt-cf-oagw-principle-tenant-scope` governs every flow
below exactly as it does elsewhere, and remains attributed to
`cpt-cf-oagw-feature-resource-model-and-store` per DECOMPOSITION §2.4, which allocates this
feature no covered principle and no covered constraint.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, replaces, and deletes routes across any tenant it manages; defines system-wide matching rules. |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, replaces, and deletes routes scoped to its own tenant, within the upstreams it can address. |

Each flow below names one illustrative actor for readability. Either actor may call any of the
five endpoints, subject to the permission scope its bearer token actually carries.

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-usecase-configure-route`, `cpt-cf-oagw-nfr-input-validation`, `cpt-cf-oagw-nfr-multi-tenancy`, `cpt-cf-oagw-interface-management-api`
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-design-domain-model` (§3.1 Route entity), `cpt-cf-oagw-interface-api` (§3.3 API Contracts, CRUD Semantics, Tenant Scoping, Error Response Format), `cpt-cf-oagw-db-schema` (§3.6 `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, `oagw_route_tag`), `cpt-cf-oagw-adr-request-routing` (route match determinism), `cpt-cf-oagw-adr-error-source-distinction`
- **Schema**: [schemas/route.v1.schema.json](../schemas/route.v1.schema.json)
- **DECOMPOSITION**: `cpt-cf-oagw-feature-route-management-api` (section 2.4), Mandatory Override 1 (gear-relative `/oagw/v1/...` paths, no `/api` prefix), Scope Reality (in-memory tenant-scoped store, no gRPC dispatch)
- **Dependencies**: `cpt-cf-oagw-feature-upstream-management-api`, `cpt-cf-oagw-feature-resource-model-and-store`
  (the tenant-scoped store write invariant this feature's API-layer algorithm restates; see
  `cpt-cf-oagw-algo-resource-model-store-write-invariants`). Where the two documents describe
  the same create-route behaviour, the store-level invariant in
  `cpt-cf-oagw-feature-resource-model-and-store` is canonical. This feature's algorithm below is
  a restatement of that invariant at the API layer and **MUST NOT** diverge from it.

## 2. Actor Flows (CDSL)

Every step below runs behind the shared inbound Bearer-token gate established by
`cpt-cf-oagw-feature-gear-foundation`: a request with a missing or invalid token, or a valid
token whose scopes do not include the route permission for the operation being performed, is
rejected with `401` before any other step executes. This reuses `AuthenticationFailed`
(`gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`), the only `401` row in DESIGN.md's error
table (§3.3 Error Response Format); DESIGN.md documents it for outbound-to-upstream credential
failures, and this feature reuses the same GTS type for inbound bearer-token/permission
failures on the management surface, since the table defines no second `401` type. Reusing this
type does not change its fixed `type` and `title` fields, which still read "Authentication to
upstream failed." Every 401 response below **MUST** write its `detail` field to describe the
actual inbound cause instead: a missing bearer token, or a token lacking the required route
permission. This keeps a reader from being misled by the reused `title` text.

Every route-match conflict below returns `409` named `RouteMatchConflict`
(`gts.cf.core.errors.err.v1~cf.oagw.route.match_conflict.v1`). DESIGN.md's error table (§3.3)
defines no `409` row for a route match conflict — its only `409` row, `PluginInUse`, covers a
different condition — so this feature introduces this GTS literal as the named identifier for
every 409 site below.

Every error response below is RFC 9457 (a standard for machine-readable HTTP error bodies)
`application/problem+json` and carries `X-OAGW-Error-Source: gateway`, since these are
gateway-originated errors, never upstream passthrough.

**Use cases**: `cpt-cf-oagw-usecase-configure-route`

### Create Route Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-route-api-create`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A route naming an `upstream_id` owned by the caller's tenant, with a well-formed `match.http`
  or `match.grpc` block, is created with a server-generated `id` and `enabled: true`.

**Error Scenarios**:
- Caller lacks `gts.cf.core.oagw.route.v1~:create` permission.
- `upstream_id` does not exist, or exists only for an ancestor tenant.
- `match` carries both `http` and `grpc`, or neither.
- The candidate match rule duplicates an existing route's `(path, priority, method)` (HTTP) or
  `(service, method)` (gRPC) tuple within the same upstream.

**Steps**:
1. [ ] - `p1` - Operator sends `POST /oagw/v1/routes` with `{ upstream_id, match, tags?, plugins?, rate_limit?, enabled? }` - `inst-route-create-1`
2. [ ] - `p1` - API: check `gts.cf.core.oagw.route.v1~:create` via the shared Bearer-token gate - `inst-route-create-2`
3. [ ] - `p1` - **IF** the token is missing or lacks `route.v1~:create` - `inst-route-create-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed problem+json - `inst-route-create-3a`
4. [ ] - `p1` - **ELSE** validate the body against `route.v1.schema.json` (`match` `oneOf` `{http|grpc}`, `additionalProperties: false` on `match`, `http_match`/`grpc_match`, tag pattern `^[a-z0-9_-]+$`) - `inst-route-create-4`
5. [ ] - `p1` - **IF** schema validation fails - `inst-route-create-5`
   1. [ ] - `p1` - **RETURN** 400 ValidationError problem+json naming the failing field(s) - `inst-route-create-5a`
6. [ ] - `p1` - **ELSE** - `inst-route-create-6`
   1. [ ] - `p1` - DB: SELECT upstream FROM store WHERE id = upstream_id AND tenant_id = caller_tenant - `inst-route-create-6a`
7. [ ] - `p1` - **IF** no upstream row matches (missing entirely, or exists only for an ancestor tenant) - `inst-route-create-7`
   1. [ ] - `p1` - **RETURN** 400 ValidationError problem+json ("upstream_id does not exist for this tenant") - `inst-route-create-7a`
8. [ ] - `p1` - **ELSE** CALL `cpt-cf-oagw-algo-route-api-match-uniqueness`(tenant_id, upstream_id, match, exclude_id=None) - `inst-route-create-8`
9. [ ] - `p1` - **IF** a conflicting route is found - `inst-route-create-9`
   1. [ ] - `p1` - **RETURN** 409 RouteMatchConflict problem+json naming the conflicting route's `id` - `inst-route-create-9a`
10. [ ] - `p1` - **ELSE** - `inst-route-create-10`
    1. [ ] - `p1` - DB: INSERT route (id=uuid, tenant_id, upstream_id, match, tags, plugins, rate_limit, enabled=true default) into store - `inst-route-create-10a`
    2. [ ] - `p1` - **RETURN** 201 Created with the persisted route body, `id` as `gts.cf.core.oagw.route.v1~{uuid}` - `inst-route-create-10b`

### List Routes Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-route-api-list`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The caller's own routes are returned, paginated and optionally filtered/sorted/projected by
  OData query parameters.

**Error Scenarios**:
- Caller lacks `gts.cf.core.oagw.route.v1~:read` permission.

**Steps**:
1. [ ] - `p1` - Admin sends `GET /oagw/v1/routes?$filter=...&$select=...&$orderby=...&$top=...&$skip=...` - `inst-route-list-1`
2. [ ] - `p1` - API: check `gts.cf.core.oagw.route.v1~:read` - `inst-route-list-2`
3. [ ] - `p1` - **IF** the token is missing or lacks `route.v1~:read` - `inst-route-list-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed problem+json - `inst-route-list-3a`
4. [ ] - `p1` - **ELSE** CALL `cpt-cf-oagw-algo-route-api-list-query`(tenant_id, filter, select, orderby, top, skip) - `inst-route-list-4`
5. [ ] - `p1` - DB: SELECT routes FROM store WHERE tenant_id = caller_tenant (ancestor-tenant routes never included) - `inst-route-list-5`
6. [ ] - `p1` - **RETURN** 200 with the paginated, filtered, projected route list - `inst-route-list-6`

### Get Route Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-route-api-get`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A route owned by the caller's tenant is returned in full.

**Error Scenarios**:
- Caller lacks `gts.cf.core.oagw.route.v1~:read` permission.
- Route `id` does not exist, or exists only for an ancestor tenant.

**Steps**:
1. [ ] - `p1` - Admin sends `GET /oagw/v1/routes/{id}` - `inst-route-get-1`
2. [ ] - `p1` - API: check `gts.cf.core.oagw.route.v1~:read` - `inst-route-get-2`
3. [ ] - `p1` - **IF** the token is missing or lacks `route.v1~:read` - `inst-route-get-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed problem+json - `inst-route-get-3a`
4. [ ] - `p1` - **ELSE** DB: SELECT route FROM store WHERE id = {id} AND tenant_id = caller_tenant - `inst-route-get-4`
5. [ ] - `p1` - **IF** no row matches (missing entirely, or exists only for an ancestor tenant) - `inst-route-get-5`
   1. [ ] - `p1` - **RETURN** 404 RouteNotFound problem+json - `inst-route-get-5a`
6. [ ] - `p1` - **ELSE** - `inst-route-get-6`
   1. [ ] - `p1` - **RETURN** 200 with the route body - `inst-route-get-6a`

### Replace Route Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-route-api-replace`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- An existing route owned by the caller's tenant is fully replaced; `upstream_id` is retained
  unchanged because the update DTO (data transfer object, the shape the API accepts for this
  operation) has no `upstream_id` field at all.

**Error Scenarios**:
- Caller lacks `gts.cf.core.oagw.route.v1~:override` permission.
- Route `id` does not exist, or exists only for an ancestor tenant.
- The replacement `match` carries both `http` and `grpc`, or neither.
- The replacement match rule duplicates another existing route's `(path, priority, method)` /
  `(service, method)` tuple within the same upstream.

**Steps**:
1. [ ] - `p1` - Operator sends `PUT /oagw/v1/routes/{id}` with `{ match, tags?, plugins?, rate_limit?, enabled? }` (no `upstream_id` field in this DTO) - `inst-route-replace-1`
2. [ ] - `p1` - API: check `gts.cf.core.oagw.route.v1~:override` - `inst-route-replace-2`
3. [ ] - `p1` - **IF** the token is missing or lacks `route.v1~:override` - `inst-route-replace-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed problem+json - `inst-route-replace-3a`
4. [ ] - `p1` - **ELSE** DB: SELECT existing route FROM store WHERE id = {id} AND tenant_id = caller_tenant - `inst-route-replace-4`
5. [ ] - `p1` - **IF** no row matches (missing entirely, or exists only for an ancestor tenant) - `inst-route-replace-5`
   1. [ ] - `p1` - **RETURN** 404 RouteNotFound problem+json - `inst-route-replace-5a`
6. [ ] - `p1` - **ELSE** validate the body against `route.v1.schema.json` minus `upstream_id`/`id` (immutable, server-controlled) - `inst-route-replace-6`
7. [ ] - `p1` - **IF** schema validation fails - `inst-route-replace-7`
   1. [ ] - `p1` - **RETURN** 400 ValidationError problem+json naming the failing field(s) - `inst-route-replace-7a`
8. [ ] - `p1` - **ELSE** CALL `cpt-cf-oagw-algo-route-api-match-uniqueness`(tenant_id, existing.upstream_id, match, exclude_id={id}) - `inst-route-replace-8`
9. [ ] - `p1` - **IF** a conflicting route is found - `inst-route-replace-9`
   1. [ ] - `p1` - **RETURN** 409 RouteMatchConflict problem+json naming the conflicting route's `id` - `inst-route-replace-9a`
10. [ ] - `p1` - **ELSE** - `inst-route-replace-10`
    1. [ ] - `p1` - DB: UPDATE route SET match, tags, plugins, rate_limit, enabled = new values; `upstream_id`, `id`, `tenant_id` unchanged - `inst-route-replace-10a`
    2. [ ] - `p1` - **RETURN** 200 with the replaced route body - `inst-route-replace-10b`

### Delete Route Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-route-api-delete`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A route owned by the caller's tenant is permanently removed from the store.

**Error Scenarios**:
- Caller lacks `gts.cf.core.oagw.route.v1~:delete` permission.
- Route `id` does not exist, or exists only for an ancestor tenant.

**Steps**:
1. [ ] - `p1` - Operator sends `DELETE /oagw/v1/routes/{id}` - `inst-route-delete-1`
2. [ ] - `p1` - API: check `gts.cf.core.oagw.route.v1~:delete` - `inst-route-delete-2`
3. [ ] - `p1` - **IF** the token is missing or lacks `route.v1~:delete` - `inst-route-delete-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed problem+json - `inst-route-delete-3a`
4. [ ] - `p1` - **ELSE** DB: SELECT route FROM store WHERE id = {id} AND tenant_id = caller_tenant - `inst-route-delete-4`
5. [ ] - `p1` - **IF** no row matches (missing entirely, or exists only for an ancestor tenant) - `inst-route-delete-5`
   1. [ ] - `p1` - **RETURN** 404 RouteNotFound problem+json - `inst-route-delete-5a`
6. [ ] - `p1` - **ELSE** - `inst-route-delete-6`
   1. [ ] - `p1` - DB: DELETE route FROM store WHERE id = {id} AND tenant_id = caller_tenant - `inst-route-delete-6a`
   2. [ ] - `p1` - **RETURN** 204 No Content - `inst-route-delete-6b`

## 3. Processes / Business Logic (CDSL)

### Route Body Validation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-route-api-validate-body`

**Input**: Raw JSON request body for create or replace, calling tenant id

**Output**: A typed, validated route (or replacement) payload, or a list of field-level errors

**Steps**:
1. [ ] - `p1` - Parse JSON body into the create/replace DTO shape (replace DTO omits `upstream_id`, `id`) - `inst-validate-1`
2. [ ] - `p1` - **IF** `match` is absent, or contains neither `http` nor `grpc`, or contains both - `inst-validate-2`
   1. [ ] - `p1` - Add error: "match must contain exactly one of http or grpc" - `inst-validate-2a`
3. [ ] - `p1` - **IF** `match.http` present - `inst-validate-3`
   1. [ ] - `p1` - Reject unknown keys under `match.http` (`additionalProperties: false`) - `inst-validate-3a`
   2. [ ] - `p1` - **IF** `methods` is empty or contains a value outside `GET|POST|PUT|DELETE|PATCH` - `inst-validate-3b`
      1. [ ] - `p1` - Add error: "methods must be a non-empty list of GET, POST, PUT, DELETE, PATCH" - `inst-validate-3b-i`
   3. [ ] - `p1` - **IF** `path` is empty - `inst-validate-3c`
      1. [ ] - `p1` - Add error: "path must be non-empty" - `inst-validate-3c-i`
   4. [ ] - `p1` - Default `query_allowlist` to `[]` when absent - `inst-validate-3d`
   5. [ ] - `p1` - **IF** `path_suffix_mode` is present, verify it is one of `"disabled"`, `"append"` (default `"append"` when absent) - `inst-validate-3e`
4. [ ] - `p1` - **IF** `match.grpc` present - `inst-validate-4`
   1. [ ] - `p1` - Reject unknown keys under `match.grpc` (`additionalProperties: false`) - `inst-validate-4a`
   2. [ ] - `p1` - **IF** `service` or `method` is empty - `inst-validate-4b`
      1. [ ] - `p1` - Add error: "service and method are both required and non-empty" - `inst-validate-4b-i`
5. [ ] - `p1` - **FOR EACH** tag in `tags` - `inst-validate-5`
   1. [ ] - `p1` - **IF** tag does not match `^[a-z0-9_-]+$` - `inst-validate-5a`
      1. [ ] - `p1` - Add error: "tag '{tag}' violates the allowed tag pattern" - `inst-validate-5a-i`
6. [ ] - `p1` - **IF** `plugins.sharing` is present, verify it is one of `"private"`, `"inherit"`, `"enforce"` (default `"private"` when absent); default `plugins.items` to `[]` when absent; accept each item as an opaque `gts-identifier` string without resolving or executing it (plugin resolution belongs to `cpt-cf-oagw-feature-plugin-management-api` / `cpt-cf-oagw-feature-policy-and-plugins`) - `inst-validate-6`
7. [ ] - `p1` - **IF** `rate_limit` present - `inst-validate-7`
   1. [ ] - `p1` - Reject unknown keys under `rate_limit` (`additionalProperties: false`) - `inst-validate-7a`
   2. [ ] - `p1` - **IF** `rate_limit.sharing` is present, verify it is one of `"private"`, `"inherit"`, `"enforce"` (default `"private"`) - `inst-validate-7b`
   3. [ ] - `p1` - **IF** `rate_limit.algorithm` is present, verify it is one of `"token_bucket"`, `"sliding_window"` (default `"token_bucket"`) - `inst-validate-7c`
   4. [ ] - `p1` - **IF** `rate_limit.sustained.rate` is absent or less than `1` - `inst-validate-7d`
      1. [ ] - `p1` - Add error: "rate_limit.sustained.rate is required and must be an integer >= 1" - `inst-validate-7d-i`
   5. [ ] - `p1` - **IF** `rate_limit.sustained.window` is present, verify it is one of `"second"`, `"minute"`, `"hour"`, `"day"` (default `"second"`) - `inst-validate-7e`
   6. [ ] - `p1` - **IF** `rate_limit.burst.capacity` is present, verify it is an integer `>= 1` - `inst-validate-7f`
   7. [ ] - `p1` - **IF** `rate_limit.scope` is present, verify it is one of `"global"`, `"tenant"`, `"user"`, `"ip"`, `"route"` (default `"tenant"`) - `inst-validate-7g`
   8. [ ] - `p1` - **IF** `rate_limit.strategy` is present, verify it is one of `"reject"`, `"queue"`, `"degrade"` (default `"reject"`) - `inst-validate-7h`
   9. [ ] - `p1` - **IF** `rate_limit.cost` is present, verify it is an integer `>= 1` (default `1`) - `inst-validate-7i`
8. [ ] - `p1` - Default `enabled` to `true` when absent - `inst-validate-8`
9. [ ] - `p1` - **RETURN** { valid: errors.length === 0, errors, normalized_route } - `inst-validate-9`

### Match-Rule Uniqueness Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-route-api-match-uniqueness`

**Input**: Calling tenant id, `upstream_id`, candidate `match` (already schema-valid), the id of
the route being replaced (`exclude_id`, `None` for create)

**Output**: `{ conflict: bool, conflicting_route_id: Option<uuid> }`

Because `route.v1.schema.json` exposes no client-settable `priority` field, this build assigns
every stored route a fixed internal priority of `0`. DESIGN.md's determinism invariant reads: "no
two **enabled** routes under same upstream may share `(path_prefix, priority)` for same method."
Since priority never varies, this build enforces that invariant for HTTP routes as `(path,
method)`, filtered to enabled routes only. A disabled route never blocks a new or replaced route
from claiming the same `(path, method)`. A future schema revision that adds a client-settable
`priority` field would restore the full three-part comparison without changing this algorithm's
shape.

This algorithm restates, at the API layer, the canonical store-level invariant defined by
`cpt-cf-oagw-algo-resource-model-store-write-invariants` (step `inst-storeinv-2`) in
`cpt-cf-oagw-feature-resource-model-and-store`. That canonical check scans the target upstream's
**enabled** routes for one sharing `(method, path)` with the candidate, excluding the route being
replaced. The steps below **MUST NOT** diverge from that canonical tuple or its enabled-only
filtering. The gRPC `(service, method)` comparison below
is this feature's extension to a shape the store-level algorithm does not itself enumerate, kept
consistent with the same enabled-only filtering.

**Steps**:
1. [ ] - `p1` - DB: SELECT routes FROM store WHERE tenant_id = tenant_id AND upstream_id = upstream_id AND enabled == true AND id != exclude_id - `inst-uniq-1`
2. [ ] - `p1` - **IF** candidate.match.http present - `inst-uniq-2`
   1. [ ] - `p1` - **FOR EACH** existing **enabled** route with `match.http` in the selected set - `inst-uniq-2a`
      1. [ ] - `p1` - **IF** existing.match.http.path == candidate.match.http.path AND priority == priority (fixed `0`) AND existing.match.http.methods intersects candidate.match.http.methods - `inst-uniq-2a-i`
         1. [ ] - `p1` - **RETURN** { conflict: true, conflicting_route_id: existing.id } - `inst-uniq-2a-i-1`
3. [ ] - `p1` - **ELSE** (candidate.match.grpc present) - `inst-uniq-3`
   1. [ ] - `p1` - **FOR EACH** existing **enabled** route with `match.grpc` in the selected set - `inst-uniq-3a`
      1. [ ] - `p1` - **IF** existing.match.grpc.service == candidate.match.grpc.service AND existing.match.grpc.method == candidate.match.grpc.method - `inst-uniq-3a-i`
         1. [ ] - `p1` - **RETURN** { conflict: true, conflicting_route_id: existing.id } - `inst-uniq-3a-i-1`
4. [ ] - `p1` - **RETURN** { conflict: false, conflicting_route_id: None } - `inst-uniq-4`

### List Query Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-route-api-list-query`

**Input**: Calling tenant id, raw `$filter`, `$select`, `$orderby`, `$top`, `$skip` string
parameters (OData — a standard query-string syntax for filtering, sorting, and paging over
collections)

**Output**: A tenant-scoped, filtered, sorted, projected, paginated route list

**Steps**:
1. [ ] - `p1` - Parse `$filter` into a predicate over route fields (e.g., `upstream_id eq '{uuid}'`); on parse failure, treat as "no filter" rather than rejecting the whole request - `inst-list-1`
2. [ ] - `p1` - Parse `$select` into a field projection list; empty/absent means "all fields" - `inst-list-2`
3. [ ] - `p1` - Parse `$orderby` into a field + direction pair - `inst-list-3`
4. [ ] - `p1` - **IF** `$top` is absent - `inst-list-4`
   1. [ ] - `p1` - Set effective top to `50` - `inst-list-4a`
5. [ ] - `p1` - **ELSE IF** `$top` > `100` - `inst-list-5`
   1. [ ] - `p1` - Clamp effective top to `100` - `inst-list-5a`
6. [ ] - `p1` - Set effective skip to `$skip` if present, else `0` - `inst-list-6`
7. [ ] - `p1` - DB: SELECT routes FROM store WHERE tenant_id = tenant_id AND {filter} ORDER BY {orderby} - `inst-list-7`
8. [ ] - `p1` - Apply skip/top pagination window to the ordered result set - `inst-list-8`
9. [ ] - `p1` - Apply the `$select` projection to each returned route - `inst-list-9`
10. [ ] - `p1` - **RETURN** the paginated, projected list - `inst-list-10`

## 4. States (CDSL)

### Route Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-route-api-lifecycle`

**States**: absent, active, disabled, deleted

**Initial State**: absent

**Transitions**:
1. [ ] - `p1` - **FROM** absent **TO** active **WHEN** `POST /oagw/v1/routes` succeeds (`enabled` defaults to `true`) - `inst-lifecycle-1`
2. [ ] - `p1` - **FROM** active **TO** disabled **WHEN** `PUT /oagw/v1/routes/{id}` sets `enabled: false` - `inst-lifecycle-2`
3. [ ] - `p1` - **FROM** disabled **TO** active **WHEN** `PUT /oagw/v1/routes/{id}` sets `enabled: true` - `inst-lifecycle-3`
4. [ ] - `p1` - **FROM** active **TO** deleted **WHEN** `DELETE /oagw/v1/routes/{id}` succeeds - `inst-lifecycle-4`
5. [ ] - `p1` - **FROM** disabled **TO** deleted **WHEN** `DELETE /oagw/v1/routes/{id}` succeeds - `inst-lifecycle-5`

A route in the `disabled` state is excluded from route matching at proxy time (`cpt-cf-oagw-fr-enable-disable`); this feature is responsible only for persisting the `enabled` flag and
returning it accurately from every CRUD response, since the matching logic itself belongs to
`cpt-cf-oagw-feature-proxy-data-plane-http`.

## 5. Definitions of Done

### Implement Route CRUD Endpoints

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-api-crud`

The system **MUST** expose `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET
/oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, and `DELETE /oagw/v1/routes/{id}` — the
Override-1, gear-relative paths with no `/api` prefix — reading from and writing to the
tenant-scoped in-memory store, and persisting the `enabled` boolean (default `true`) supplied on
create or replace.

**Implements**:
- `cpt-cf-oagw-flow-route-api-create`
- `cpt-cf-oagw-flow-route-api-list`
- `cpt-cf-oagw-flow-route-api-get`
- `cpt-cf-oagw-flow-route-api-replace`
- `cpt-cf-oagw-flow-route-api-delete`
- `cpt-cf-oagw-state-route-api-lifecycle`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `GET /oagw/v1/routes`
- API: `GET /oagw/v1/routes/{id}`
- API: `PUT /oagw/v1/routes/{id}`
- API: `DELETE /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

### Enforce Route Body Validation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-route-api-validation`

The system **MUST** validate every create and replace request body against `route.v1.schema.json`:
`match` **MUST** contain exactly one of `http`/`grpc` (`oneOf`, `additionalProperties: false`),
`match.http.methods` **MUST** be non-empty and drawn from `GET|POST|PUT|DELETE|PATCH`,
`match.http.path` **MUST** be non-empty, `match.grpc.service`/`match.grpc.method` **MUST** both
be non-empty, and every `tags` entry **MUST** match `^[a-z0-9_-]+$`. Any violation **MUST**
return `400 ValidationError`.

**Implements**:
- `cpt-cf-oagw-algo-route-api-validate-body`
- `cpt-cf-oagw-flow-route-api-create`
- `cpt-cf-oagw-flow-route-api-replace`

**Constraints**: None. DECOMPOSITION §2.4 allocates this feature no covered design principle and
no covered design constraint; tenant scoping still governs this validation as behaviour, but is
attributed to `cpt-cf-oagw-feature-resource-model-and-store`.

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `PUT /oagw/v1/routes/{id}`
- Entities: `Route`

### Enforce Match-Rule Uniqueness

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-route-api-uniqueness`

The system **MUST** reject, with `409 RouteMatchConflict`, any create or replace whose HTTP match
rule duplicates an **enabled** existing route's `(path, method)` tuple within the same upstream —
`route.v1.schema.json` has no client-settable `priority`, so every route holds a fixed internal
priority of `0` and the DESIGN.md invariant `(path, priority, method)` collapses to `(path,
method)` — or whose gRPC match rule duplicates an **enabled** existing route's `(service,
method)` tuple within the same upstream, excluding the route being replaced from its own
comparison set. A disabled route sharing the same tuple **MUST NOT** block the write.

**Implements**:
- `cpt-cf-oagw-algo-route-api-match-uniqueness`
- `cpt-cf-oagw-flow-route-api-create`
- `cpt-cf-oagw-flow-route-api-replace`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

### Enforce Tenant Scoping on Every Operation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-route-api-tenant-scope`

The system **MUST** reject, with `400 ValidationError`, a create or replace whose `upstream_id`
does not exist for the calling tenant or exists only for an ancestor tenant. The system **MUST**
return `404 RouteNotFound` from `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, and
`DELETE /oagw/v1/routes/{id}` when the route id does not exist or belongs only to an ancestor
tenant, and list results **MUST** never include a route owned by an ancestor tenant. The
`upstream_id` field **MUST** be immutable: absent from the replace DTO, and unchanged by replace
regardless of what the request body contains.

**Implements**:
- `cpt-cf-oagw-flow-route-api-create`
- `cpt-cf-oagw-flow-route-api-list`
- `cpt-cf-oagw-flow-route-api-get`
- `cpt-cf-oagw-flow-route-api-replace`
- `cpt-cf-oagw-flow-route-api-delete`

**Constraints**: None. DECOMPOSITION §2.4 allocates this feature no covered design principle and
no covered design constraint; tenant scoping still governs this enforcement as behaviour, but is
attributed to `cpt-cf-oagw-feature-resource-model-and-store`.

**Touches**:
- API: `GET /oagw/v1/routes/{id}`
- API: `PUT /oagw/v1/routes/{id}`
- API: `DELETE /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

### Enforce Per-Endpoint Authorization

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-route-api-authz`

The system **MUST** gate `POST /oagw/v1/routes` behind `gts.cf.core.oagw.route.v1~:create`,
`GET /oagw/v1/routes` and `GET /oagw/v1/routes/{id}` behind
`gts.cf.core.oagw.route.v1~:read`, `PUT /oagw/v1/routes/{id}` behind
`gts.cf.core.oagw.route.v1~:override`, and `DELETE /oagw/v1/routes/{id}` behind
`gts.cf.core.oagw.route.v1~:delete`, returning `401 AuthenticationFailed` when the bearer token
is missing or lacks the required permission, before any other request processing runs.

**Implements**:
- `cpt-cf-oagw-flow-route-api-create`
- `cpt-cf-oagw-flow-route-api-list`
- `cpt-cf-oagw-flow-route-api-get`
- `cpt-cf-oagw-flow-route-api-replace`
- `cpt-cf-oagw-flow-route-api-delete`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `GET /oagw/v1/routes`
- API: `GET /oagw/v1/routes/{id}`
- API: `PUT /oagw/v1/routes/{id}`
- API: `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`

### Emit RFC 9457 Error Envelopes

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-route-api-errors`

Every `400`, `401`, `404`, and `409` response emitted by these five endpoints **MUST** be
`application/problem+json` per RFC 9457, carrying `type`, `title`, `status`, `detail`,
`instance`, and the OAGW extension fields (`upstream_id`, `path`, `trace_id`), and **MUST**
carry an `X-OAGW-Error-Source: gateway` header, since none of these errors are upstream
passthrough.

**Implements**:
- `cpt-cf-oagw-flow-route-api-create`
- `cpt-cf-oagw-flow-route-api-list`
- `cpt-cf-oagw-flow-route-api-get`
- `cpt-cf-oagw-flow-route-api-replace`
- `cpt-cf-oagw-flow-route-api-delete`

**Constraints**: `cpt-cf-oagw-adr-error-source-distinction`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `GET /oagw/v1/routes`
- API: `GET /oagw/v1/routes/{id}`
- API: `PUT /oagw/v1/routes/{id}`
- API: `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/routes` with a valid `upstream_id` owned by the caller's tenant and a
  well-formed `match.http` block returns `201 Created` with a server-generated `id`.
- [ ] `POST /oagw/v1/routes` with an `upstream_id` that does not exist for the calling tenant
  (missing entirely, or belonging only to an ancestor tenant) returns `400 ValidationError`.
- [ ] `POST /oagw/v1/routes` whose `match` contains both `http` and `grpc`, or neither, is
  rejected with `400 ValidationError`.
- [ ] `POST /oagw/v1/routes` duplicating an **enabled** existing route's `(path, method)` within
  the same upstream returns `409 RouteMatchConflict`. The schema exposes no client-settable
  `priority`, so every route holds a fixed internal priority of `0`, collapsing DESIGN.md's
  `(path, priority, method)` invariant to `(path, method)`.
- [ ] `POST /oagw/v1/routes` whose `(path, method)` matches an existing **disabled** route within
  the same upstream is created successfully as a new, separate route: a disabled route does not
  block the write.
- [ ] `PUT /oagw/v1/routes/{id}` cannot change `upstream_id`: the field is absent from the
  replace DTO, and the stored value is unchanged even if the request body includes it.
- [ ] `DELETE /oagw/v1/routes/{id}` followed by `GET /oagw/v1/routes/{id}` returns
  `404 RouteNotFound`.
- [ ] `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, and `DELETE /oagw/v1/routes/{id}`
  each return `404 RouteNotFound` for a route id that belongs only to an ancestor tenant.
- [ ] `POST /oagw/v1/routes` with a well-formed `match.grpc` block (`service` and `method` both
  present) returns `201 Created` and the stored route round-trips through `GET`, even though no
  gRPC proxy dispatch path exists in this build.
- [ ] `GET /oagw/v1/routes` without `$top` returns at most `50` routes; `$top=150` is clamped to
  the documented maximum of `100`.
- [ ] Every `400`, `401`, `404`, and `409` response from these five endpoints is
  `application/problem+json` and carries `X-OAGW-Error-Source: gateway`.

## 7. Applicability

- **UX**: Not applicable. This is a machine-consumed management API with no rendered interface;
  every caller is an automated client or an operator issuing JSON requests directly, so no
  visual, layout, or interaction design applies.
- **Compliance**: Not applicable in this build. A route stores only method, path, an
  `upstream_id` reference, tags, and plugin identifiers — no personal data — so no additional
  regulatory obligation arises beyond the tenant scoping already covered by
  `cpt-cf-oagw-feature-resource-model-and-store`.
- **Operations**: Substantive content applies. DESIGN.md §4.3 (Audit Logging) names route
  create/update/delete among the "Config changes" logged as structured JSON events; this feature
  relies on that shared logging path rather than defining a separate one. The Prometheus metrics
  in DESIGN.md §4.2 are scoped to proxied data-plane requests, not to this management surface, so
  no metric here is specific to route CRUD.
- **Performance**: Not applicable beyond baseline request handling. Route CRUD runs against an
  in-memory, tenant-scoped store with no external calls, so it carries no distinct latency budget,
  caching strategy, or throughput target beyond the general request handling that
  `cpt-cf-oagw-feature-gear-foundation` already provides.
