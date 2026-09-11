# Feature: Route Management


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Authorization Disposition](#15-authorization-disposition)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Route](#create-route)
  - [List Routes](#list-routes)
  - [Get Route](#get-route)
  - [Replace Route](#replace-route)
  - [Delete Route](#delete-route)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Validate Route Match Payload](#validate-route-match-payload)
  - [Check Match-Determinism Invariant](#check-match-determinism-invariant)
  - [Remove Routes for a Deleted Upstream](#remove-routes-for-a-deleted-upstream)
- [4. Definitions of Done](#4-definitions-of-done)
  - [Create Route Endpoint](#create-route-endpoint)
  - [List Routes Endpoint](#list-routes-endpoint)
  - [Get Route Endpoint](#get-route-endpoint)
  - [Replace Route Endpoint](#replace-route-endpoint)
  - [Delete Route Endpoint](#delete-route-endpoint)
  - [Match-Determinism Enforcement](#match-determinism-enforcement)
  - [Route Enable/Disable Field](#route-enabledisable-field)
  - [Route-Level Policy-Field Validation](#route-level-policy-field-validation)
  - [Cascade Delete on Upstream Removal](#cascade-delete-on-upstream-removal)
- [5. Acceptance Criteria](#5-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-rm-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-route-management`

## 1. Feature Context

### 1.1 Overview

The Route Management feature owns the Route resource: the control-plane object that binds an HTTP match rule (method, path, query allowlist) to a specific upstream and carries the route's own rate-limit and plugin overrides. It exposes create, list, read, replace, and delete operations under `/oagw/v1/routes`, and it is the sole enforcer of the match-determinism invariant that keeps route lookup unambiguous.

### 1.2 Purpose

This feature implements the CRUD surface that `cpt-cf-oagw-fr-route-mgmt` requires for routes, and the route half of the enable/disable behavior that `cpt-cf-oagw-fr-enable-disable` requires — every route carries its own `enabled` boolean (default `true`), and a disabled route is excluded from route matching. The upstream half of enable/disable (a disabled upstream rejecting all proxy requests with `503`, and ancestor-disabled upstreams staying disabled for descendants) belongs entirely to the Upstream Management feature; this feature neither implements nor re-describes it beyond the boundary just stated.

All route CRUD operations are scoped to the calling tenant per `cpt-cf-oagw-principle-tenant-scope`: an operator can only create, list, read, replace, or delete routes under upstreams visible to their own tenant, and a route belonging to another tenant is invisible (404), never merely forbidden.

`match.grpc` is accepted and stored because the frozen schema declares it, but this configuration deliberately defers gRPC proxying: DESIGN.md states no gRPC proxy code path is implemented or reachable. A route created with a `grpc` match is therefore persisted and returned exactly like any other route — it is not rejected and not silently dropped — but it is never a candidate for request matching in this configuration; that exclusion is a deliberate deferral, not an oversight.

**Requirements**: `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-fr-enable-disable`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, reads, replaces, and deletes routes on behalf of any tenant they administer. |
| `cpt-cf-oagw-actor-tenant-admin` | Performs the same create/list/read/replace/delete operations, scoped to their own tenant's visible upstreams and routes. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`

### 1.5 Authorization Disposition

Generic request authentication and coarse authorization are performed by the host runtime before a request reaches this feature; this feature does not re-implement them. This feature owns exactly one OAGW-specific authorization decision of its own: the `gts.cf.core.oagw.route.v1~:create` permission precondition on Create Route (`cpt-cf-oagw-usecase-configure-route`), which fails with `403` through `cpt-cf-oagw-algo-gf-error-mapping`'s `permission denied` category. List, Get, Replace, and Delete Route rely solely on tenant-scoping (a route belonging to another tenant is invisible — `404`, never merely forbidden) and the host runtime's coarse authorization; this feature adds no further feature-specific permission check for those operations.

In this configuration, however, this feature performs no OAGW-specific permission checks of its own: the `gts.cf.core.oagw.route.v1~:create` precondition the PRD's use case names is not separately enforced by this feature's own logic — inbound authentication and coarse authorization performed by the host runtime are the only gate a Create Route request passes through before this feature's tenant-scoped CRUD logic runs. The 401 and 403 canonical categories exist in `cpt-cf-oagw-algo-gf-error-mapping` and are exercised by unit tests, but this feature raises them only for the cases it owns (tenant-scoping's `404` substitutes for what would otherwise be a `403` on List, Get, Replace, and Delete, per the disposition above); it raises no independent `403` of its own on Create Route in this configuration.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-oagw-usecase-configure-route`

### Create Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-rm-create-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator submits a route referencing a visible upstream and a well-formed, non-colliding `match.http` rule; the system stores the route with a server-generated id and returns it.
- The operator submits a route with a `match.grpc` rule; the system stores it structurally without making it reachable by any proxy path.

**Error Scenarios**:
- The caller does not hold `gts.cf.core.oagw.route.v1~:create` permission.
- `upstream_id` does not resolve to an upstream visible to the calling tenant.
- `match` is missing, has neither `http` nor `grpc`, or has both.
- `match.http` is missing `methods` or `path`, `methods` is empty or contains a value outside the method allowlist, or `path` is empty.
- The candidate's method(s) and `path` collide with another currently-enabled route under the same upstream.

**Steps**:
1. [ ] - `p1` - Operator submits a route definition with `upstream_id`, `match` (`http` or `grpc`), and optional `tags`, `plugins`, `rate_limit` - `inst-rm-create-submit`
2. [ ] - `p1` - {API: POST /oagw/v1/routes (request: route fields without `id`; response: created route including server-generated `id` and defaulted fields)} - `inst-rm-create-api`
3. [ ] - `p1` - **IF** the caller does not hold `gts.cf.core.oagw.route.v1~:create` permission (the precondition `cpt-cf-oagw-usecase-configure-route` states) - `inst-rm-create-if-no-perm`
   1. [ ] - `p1` - **RETURN** `403` via `cpt-cf-oagw-algo-gf-error-mapping`'s `permission denied` category - `inst-rm-create-return-403`
4. [ ] - `p1` - **ELSE** System resolves `upstream_id` against the set of upstreams visible to the calling tenant - `inst-rm-create-resolve-upstream`
5. [ ] - `p1` - **IF** `upstream_id` does not resolve to a visible upstream - `inst-rm-create-if-upstream-missing`
   1. [ ] - `p1` - **RETURN** `400 ValidationError` - `inst-rm-create-return-400-upstream`
6. [ ] - `p1` - **ELSE** validate `match`, `tags`, and `rate_limit` via `cpt-cf-oagw-algo-rm-validate-match-payload` (exactly one of `http`/`grpc`; for `http`, non-empty `methods` from the method allowlist and a non-empty `path` are required; `query_allowlist` defaults to `[]`; `path_suffix_mode` defaults to `append`) - `inst-rm-create-validate-match`
7. [ ] - `p1` - **IF** validation fails - `inst-rm-create-if-invalid`
   1. [ ] - `p1` - **RETURN** `400 ValidationError` with field-level detail - `inst-rm-create-return-400-match`
8. [ ] - `p1` - **ELSE** check the match-determinism invariant (`cpt-cf-oagw-algo-rm-check-match-determinism`) against the upstream's other currently-enabled routes - `inst-rm-create-check-duplicate`
9. [ ] - `p1` - **IF** a colliding enabled route exists - `inst-rm-create-if-duplicate`
   1. [ ] - `p1` - **RETURN** `409 Conflict` - `inst-rm-create-return-409`
10. [ ] - `p1` - **ELSE** persist the route with a server-generated `id`, `enabled` defaulted to `true`, and the supplied/defaulted fields - `inst-rm-create-persist`
11. [ ] - `p1` - **RETURN** `201 Created` with the stored route - `inst-rm-create-return-201`

### List Routes

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-rm-list-routes`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator lists the routes visible to their tenant, optionally paged with `$top`/`$skip` and filtered/shaped with `$filter`/`$select`/`$orderby`.

**Error Scenarios**:
- None beyond the host runtime's coarse authentication/authorization handling described in Section 1.5; this feature adds no route-specific permission check for listing.

**Steps**:
1. [ ] - `p1` - Operator requests the route collection, optionally with `$top`, `$skip`, `$filter`, `$select`, `$orderby` - `inst-rm-list-submit`
2. [ ] - `p1` - {API: GET /oagw/v1/routes (request: OData query parameters; response: paged list of routes)} - `inst-rm-list-api`
3. [ ] - `p1` - System resolves `$top` to the supplied value, defaulting to 50 and capping at 100 - `inst-rm-list-top`
4. [ ] - `p1` - System filters the stored route collection to those visible to the calling tenant, then applies `$skip`/`$top` - `inst-rm-list-filter`
5. [ ] - `p1` - **RETURN** `200 OK` with the resulting page of routes - `inst-rm-list-return`

### Get Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-rm-get-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator retrieves a route that belongs to their tenant by id.

**Error Scenarios**:
- The id does not exist, or exists but belongs to another tenant.

**Steps**:
1. [ ] - `p1` - Operator requests a route by id - `inst-rm-get-submit`
2. [ ] - `p1` - {API: GET /oagw/v1/routes/{id} (request: route id; response: the route)} - `inst-rm-get-api`
3. [ ] - `p1` - System looks up the route by id, scoped to the calling tenant - `inst-rm-get-lookup`
4. [ ] - `p1` - **IF** no matching route is visible to the calling tenant - `inst-rm-get-if-missing`
   1. [ ] - `p1` - **RETURN** `404 NotFound` - `inst-rm-get-return-404`
5. [ ] - `p1` - **ELSE** - `inst-rm-get-else`
   1. [ ] - `p1` - **RETURN** `200 OK` with the route - `inst-rm-get-return-200`

### Replace Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-rm-replace-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator fully replaces an own-tenant route's `match`, `tags`, `plugins`, `rate_limit`, and `enabled` fields; `upstream_id` is untouched because it is immutable and absent from the replacement payload.

**Error Scenarios**:
- The id does not exist, or exists but belongs to another tenant.
- The replacement `match` fails the same validation applied at create time.
- The replacement's method(s) and `path` collide with another currently-enabled route under the same upstream.

**Steps**:
1. [ ] - `p1` - Operator submits a full replacement route definition (`match`, `tags`, `plugins`, `rate_limit`, `enabled`) that carries no `upstream_id` field - `inst-rm-replace-submit`
2. [ ] - `p1` - {API: PUT /oagw/v1/routes/{id} (request: full route DTO without `upstream_id`; response: the replaced route, including its unchanged `upstream_id`)} - `inst-rm-replace-api`
3. [ ] - `p1` - System looks up the route by id, scoped to the calling tenant - `inst-rm-replace-lookup`
4. [ ] - `p1` - **IF** no matching route is visible to the calling tenant - `inst-rm-replace-if-missing`
   1. [ ] - `p1` - **RETURN** `404 NotFound` - `inst-rm-replace-return-404`
5. [ ] - `p1` - **ELSE** validate the replacement `match` via `cpt-cf-oagw-algo-rm-validate-match-payload` - `inst-rm-replace-validate`
6. [ ] - `p1` - **IF** validation fails - `inst-rm-replace-if-invalid`
   1. [ ] - `p1` - **RETURN** `400 ValidationError` with field-level detail - `inst-rm-replace-return-400`
7. [ ] - `p1` - **ELSE** check the match-determinism invariant (`cpt-cf-oagw-algo-rm-check-match-determinism`) against the upstream's other currently-enabled routes, excluding the route being replaced - `inst-rm-replace-check-duplicate`
8. [ ] - `p1` - **IF** a colliding enabled route exists - `inst-rm-replace-if-duplicate`
   1. [ ] - `p1` - **RETURN** `409 Conflict` - `inst-rm-replace-return-409`
9. [ ] - `p1` - **ELSE** overwrite `match`, `tags`, `plugins`, `rate_limit`, and `enabled`, leaving `id`, tenant, and `upstream_id` unchanged - `inst-rm-replace-persist`
10. [ ] - `p1` - **RETURN** `200 OK` with the replaced route - `inst-rm-replace-return-200`

### Delete Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-rm-delete-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator deletes an own-tenant route by id.

**Error Scenarios**:
- The id does not exist, or exists but belongs to another tenant.

**Steps**:
1. [ ] - `p1` - Operator requests deletion of a route by id - `inst-rm-delete-submit`
2. [ ] - `p1` - {API: DELETE /oagw/v1/routes/{id} (request: route id; response: empty body)} - `inst-rm-delete-api`
3. [ ] - `p1` - System looks up the route by id, scoped to the calling tenant - `inst-rm-delete-lookup`
4. [ ] - `p1` - **IF** no matching route is visible to the calling tenant - `inst-rm-delete-if-missing`
   1. [ ] - `p1` - **RETURN** `404 NotFound` - `inst-rm-delete-return-404`
5. [ ] - `p1` - **ELSE** remove the route from the route collection - `inst-rm-delete-remove`
6. [ ] - `p1` - **RETURN** `204 No Content` - `inst-rm-delete-return-204`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Validate Route Match Payload

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-rm-validate-match-payload`

**Deviation from DESIGN.md**: `cpt-cf-oagw-design-domain-model` and this feature's DECOMPOSITION entry (`cpt-cf-oagw-feature-route-management`, §2.3) list `CorsConfig` among Route's domain entities and place "route-level rate limit/CORS/plugin overrides" in this feature's scope. The frozen `route.v1.schema.json` defines a `cors` object under `definitions`, but never references it from the Route's top-level `properties` — so route-level CORS is not part of the wire contract in this configuration, regardless of what the domain model or DECOMPOSITION scope text describe. This validation step therefore validates `tags`, `plugins`, and `rate_limit` (all of which the schema does expose at the top level) but accepts no `cors` field on a route body at all: an unrecognized `cors` property is rejected the same as any other undeclared property. CORS is configured on the upstream only in this configuration — see `cpt-cf-oagw-feature-upstream-management`'s `cors` field and `cpt-cf-oagw-dod-um-cors-validation`. No route-level CORS field is invented to fill this gap.

**Input**: a candidate `match` object, plus `tags`, `plugins`, and `rate_limit` if supplied.

**Output**: a normalized route payload with defaults applied, or a validation error identifying the failing field(s).

**Steps**:
1. [ ] - `p1` - Reject the payload if `match` is absent, or if it does not have exactly one of `http`/`grpc` - `inst-rm-validate-match-shape`
2. [ ] - `p1` - **IF** `grpc` is present - `inst-rm-validate-match-if-grpc`
   1. [ ] - `p1` - Accept it structurally (require non-empty `service` and `method`) and mark the route as not reachable by any proxy path, per the deliberate deferral of gRPC proxying - `inst-rm-validate-match-grpc-accept`
3. [ ] - `p1` - **ELSE** (`http` is present) - `inst-rm-validate-match-else-http`
   1. [ ] - `p1` - Require `methods` to be a non-empty array drawn only from `GET`, `POST`, `PUT`, `DELETE`, `PATCH` - `inst-rm-validate-match-methods`
   2. [ ] - `p1` - Require `path` to be a non-empty string - `inst-rm-validate-match-path`
   3. [ ] - `p1` - Default `query_allowlist` to `[]` when omitted; an empty allowlist permits no query parameters, not all of them - `inst-rm-validate-match-query-allowlist`
   4. [ ] - `p1` - Default `path_suffix_mode` to `append` when omitted; otherwise require `disabled` or `append` - `inst-rm-validate-match-suffix-mode`
4. [ ] - `p1` - Default `plugins.sharing` to `private` and `plugins.items` to `[]` when `plugins` is supplied without them; identifier resolution and binding validation of `plugins.items` belong to the Plugin Catalog and Bindings feature, not this validation step - `inst-rm-validate-match-plugins`
5. [ ] - `p1` - Validate `tags`, when present, is an array of strings each matching the schema's `^[a-z0-9_-]+$` pattern; reject the payload naming `tags` if any entry does not match - `inst-rm-validate-match-tags`
6. [ ] - `p1` - Validate and default `rate_limit`, when present, exactly as `cpt-cf-oagw-algo-um-validate-payload` does for upstreams: require `rate_limit.sustained.rate`, defaulting `rate_limit.algorithm` to `token_bucket`, `rate_limit.sustained.window` to `second`, `rate_limit.burst.capacity` to `rate_limit.sustained.rate` when absent, `rate_limit.scope` to `tenant`, `rate_limit.strategy` to `reject`, and `rate_limit.cost` to `1`; reject the payload naming `rate_limit` if `sustained.rate` is absent while `rate_limit` is supplied - `inst-rm-validate-match-rate-limit`
7. [ ] - `p1` - Default `enabled` to `true` when omitted - `inst-rm-validate-match-enabled-default`
8. [ ] - `p1` - **RETURN** the normalized payload, or a `400 ValidationError` enumerating the failing field(s) - `inst-rm-validate-match-return`

### Check Match-Determinism Invariant

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-rm-check-match-determinism`

**Deviation from DESIGN.md**: `cpt-cf-oagw-design-domain-model` models Route with a first-class `priority` integer used to order same-method matches, and `cpt-cf-oagw-db-schema` states the invariant as keying on `(path_prefix, priority)` per method. The frozen `route.v1.schema.json` field contract carries no `priority` property. In this configuration no priority field is accepted, stored, or compared: the invariant below keys on `(upstream_id, method, path)` alone, and the tie-breaking role DESIGN.md assigned to `priority` is instead resolved at proxy time by longest-path-prefix match — the more specific of two non-identical, non-colliding paths always wins, so no numeric tie-breaker is ever needed. Rationale: the frozen schema is this configuration's binding field-level contract, so accepting an undeclared `priority` field would create persisted state with no wire representation and no enforced meaning, while longest-prefix match already yields a deterministic, total order over any set of non-colliding path prefixes.

**Input**: the target `upstream_id`, the normalized `http` match's `methods` and `path`, and (for a replace) the id of the route being replaced so it can exclude itself.

**Output**: no conflict, or a `409 Conflict`.

**Steps**:
1. [ ] - `p1` - Collect the `(method, path)` pairs of every other route that is currently enabled under the same `upstream_id`, excluding the route being replaced, if any - `inst-rm-determinism-collect`
2. [ ] - `p1` - **IF** any of the candidate's `(method, path)` pairs matches a pair in that collection - `inst-rm-determinism-if-collision`
   1. [ ] - `p1` - **RETURN** `409 Conflict` - `inst-rm-determinism-return-409`
3. [ ] - `p1` - **ELSE** **RETURN** no conflict - `inst-rm-determinism-return-ok`

### Remove Routes for a Deleted Upstream

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-rm-cascade-remove-routes`

Deleting an upstream (an operation owned by the Upstream Management feature) cascades to every route registered under it. This is distinct from disabling an upstream, which does not delete or otherwise alter its routes and is entirely the Upstream Management feature's own enable/disable behavior under `cpt-cf-oagw-fr-enable-disable`.

**Input**: the `upstream_id` of an upstream that the Upstream Management feature has just deleted.

**Output**: the route collection with every route owned by that upstream removed.

**Steps**:
1. [ ] - `p1` - Upstream Management's delete-upstream operation invokes this process with the deleted upstream's id, after the upstream record itself has been removed - `inst-rm-cascade-invoke`
2. [ ] - `p1` - **FOR EACH** route in the route collection whose `upstream_id` equals the deleted upstream's id - `inst-rm-cascade-for-each`
   1. [ ] - `p1` - Remove the route from the collection - `inst-rm-cascade-remove-one`
3. [ ] - `p1` - **RETURN** the updated route collection, with no route left referencing the deleted `upstream_id` - `inst-rm-cascade-return`

## 4. Definitions of Done

### Create Route Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rm-create-route`

The system **MUST** implement `POST /oagw/v1/routes` per `cpt-cf-oagw-flow-rm-create-route`, applying `cpt-cf-oagw-algo-rm-validate-match-payload` and `cpt-cf-oagw-algo-rm-check-match-determinism`, and returning `201`/`400`/`409` exactly as those flows specify.

**Implements**:
- `cpt-cf-oagw-flow-rm-create-route`

**Touches**:
- API: `POST /oagw/v1/routes`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`, `MatchConfig`

### List Routes Endpoint

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rm-list-routes`

The system **MUST** implement `GET /oagw/v1/routes` per `cpt-cf-oagw-flow-rm-list-routes`, returning only routes visible to the calling tenant and honoring `$top` (default 50, max 100) and `$skip`.

**Implements**:
- `cpt-cf-oagw-flow-rm-list-routes`

**Touches**:
- API: `GET /oagw/v1/routes`
- Entities: `Route`

### Get Route Endpoint

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rm-get-route`

The system **MUST** implement `GET /oagw/v1/routes/{id}` per `cpt-cf-oagw-flow-rm-get-route`, returning `404` when the id is absent or belongs to another tenant.

**Implements**:
- `cpt-cf-oagw-flow-rm-get-route`

**Touches**:
- API: `GET /oagw/v1/routes/{id}`
- Entities: `Route`

### Replace Route Endpoint

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rm-replace-route`

The system **MUST** implement `PUT /oagw/v1/routes/{id}` per `cpt-cf-oagw-flow-rm-replace-route` as a full replacement that accepts no `upstream_id` field, re-validates `match` via `cpt-cf-oagw-algo-rm-validate-match-payload`, and re-checks `cpt-cf-oagw-algo-rm-check-match-determinism` excluding the route itself.

**Implements**:
- `cpt-cf-oagw-flow-rm-replace-route`

**Touches**:
- API: `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`, `MatchConfig`

### Delete Route Endpoint

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rm-delete-route`

The system **MUST** implement `DELETE /oagw/v1/routes/{id}` per `cpt-cf-oagw-flow-rm-delete-route`, returning `204` and removing the route from the route collection.

**Implements**:
- `cpt-cf-oagw-flow-rm-delete-route`

**Touches**:
- API: `DELETE /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

### Match-Determinism Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rm-match-determinism`

The system **MUST** enforce the match-determinism invariant (`cpt-cf-oagw-algo-rm-check-match-determinism`) on every create and every replace, comparing only `(upstream_id, method, path)` — never a `priority` field, which this configuration's frozen schema does not carry — and leaving ordering among non-colliding, non-identical paths to longest-path-prefix resolution at proxy time.

**Implements**:
- `cpt-cf-oagw-flow-rm-create-route`
- `cpt-cf-oagw-flow-rm-replace-route`
- `cpt-cf-oagw-algo-rm-check-match-determinism`

**Touches**:
- Entities: `Route`, `MatchConfig`

### Route Enable/Disable Field

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rm-enable-disable`

The system **MUST** carry an `enabled` boolean (default `true`) on every route, exclude disabled routes from the match-determinism check on create and replace, and accept `enabled` toggling through full replacement of the route.

**Implements**:
- `cpt-cf-oagw-flow-rm-create-route`
- `cpt-cf-oagw-flow-rm-replace-route`
- `cpt-cf-oagw-algo-rm-check-match-determinism`

**Touches**:
- Entities: `Route`

### Route-Level Policy-Field Validation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-rm-policy-validation`

The system **MUST** validate a route's `tags` array against the schema's `^[a-z0-9_-]+$` pattern, and **MUST** validate and default a route's `rate_limit` object exactly as `cpt-cf-oagw-algo-um-validate-payload` does for upstreams (requiring `sustained.rate`, defaulting `algorithm` to `token_bucket`, `sustained.window` to `second`, `burst.capacity` to `sustained.rate`, `scope` to `tenant`, `strategy` to `reject`, and `cost` to `1`), on both create and replace. The system **MUST NOT** accept a `cors` field on a route body: the frozen `route.v1.schema.json` defines `cors` only under `definitions` and never references it from the Route's top-level `properties`, so route-level CORS is not part of the wire contract in this configuration — this is a documented deviation from `cpt-cf-oagw-design-domain-model`'s Route entity list, and CORS remains configured on the upstream only (see `cpt-cf-oagw-feature-upstream-management`).

**Implements**:
- `cpt-cf-oagw-flow-rm-create-route`
- `cpt-cf-oagw-flow-rm-replace-route`
- `cpt-cf-oagw-algo-rm-validate-match-payload`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `RateLimitConfig`

### Cascade Delete on Upstream Removal

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-rm-cascade-delete`

The system **MUST** remove every route owned by an upstream (`cpt-cf-oagw-algo-rm-cascade-remove-routes`) when that upstream is deleted, invoked from the Upstream Management feature's delete-upstream operation, so that no route ever outlives its `upstream_id`.

**Implements**:
- `cpt-cf-oagw-algo-rm-cascade-remove-routes`

**Touches**:
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Route`

## 5. Acceptance Criteria

- [ ] `POST /oagw/v1/routes` with a valid, visible `upstream_id` and a well-formed `match.http` (non-empty `methods` from the allowlist, non-empty `path`) returns `201` with a server-generated `id`, `enabled: true`, `query_allowlist: []`, and `path_suffix_mode: "append"` when those fields were omitted from the request.
- [ ] `POST /oagw/v1/routes` whose `upstream_id` does not resolve to an upstream visible to the calling tenant returns `400`, not `404`.
- [ ] `POST /oagw/v1/routes` whose `match` contains neither `http` nor `grpc`, or contains both, returns `400`.
- [ ] `POST /oagw/v1/routes` whose `match.http` omits `methods`, supplies an empty `methods` array, supplies a method outside `GET`/`POST`/`PUT`/`DELETE`/`PATCH`, or omits or empties `path`, returns `400`.
- [ ] `POST /oagw/v1/routes` whose `methods`/`path` duplicate a currently-enabled route under the same `upstream_id` returns `409`.
- [ ] `POST /oagw/v1/routes` whose `methods`/`path` duplicate only a currently-disabled route under the same `upstream_id` returns `201`, because disabled routes are excluded from the match-determinism check.
- [ ] `POST /oagw/v1/routes` with a `match.grpc` object (`service`, `method`) returns `201` and the stored route is retrievable by `GET`, even though no proxy path serves it in this configuration.
- [ ] `GET /oagw/v1/routes` returns only routes visible to the calling tenant, defaults `$top` to 50 when omitted, caps `$top` at 100, and honors `$skip`.
- [ ] `GET /oagw/v1/routes/{id}` returns `200` with the route when it belongs to the calling tenant, and `404` when the id is absent or belongs to another tenant.
- [ ] `PUT /oagw/v1/routes/{id}` on an existing own-tenant route returns `200` with the stored route's `upstream_id` unchanged, even though the replacement payload carries no `upstream_id` field.
- [ ] `PUT /oagw/v1/routes/{id}` whose replacement `methods`/`path` duplicate a currently-enabled route other than itself under the same `upstream_id` returns `409`.
- [ ] `PUT /oagw/v1/routes/{id}` that sets `enabled: true` on a route whose `methods`/`path` now duplicate another currently-enabled route under the same `upstream_id` returns `409`.
- [ ] `PUT /oagw/v1/routes/{id}` for an id absent or belonging to another tenant returns `404`.
- [ ] `DELETE /oagw/v1/routes/{id}` returns `204`, and a subsequent `GET /oagw/v1/routes/{id}` on the same id returns `404`.
- [ ] Deleting the parent upstream removes every route whose `upstream_id` referenced it; a subsequent `GET` on any of those route ids returns `404`.
- [ ] A route created without an `enabled` field defaults to `enabled: true`.
- [ ] `POST /oagw/v1/routes` submitted by a caller lacking `gts.cf.core.oagw.route.v1~:create` permission returns `403`.
- [ ] `POST /oagw/v1/routes` whose `tags` array contains an entry not matching `^[a-z0-9_-]+$` returns `400`.
- [ ] `POST /oagw/v1/routes` whose `rate_limit` is supplied without `sustained.rate` returns `400`; supplying only `rate_limit.sustained.rate` returns `201` with `rate_limit.algorithm` defaulted to `token_bucket`, `rate_limit.sustained.window` defaulted to `second`, `rate_limit.burst.capacity` defaulted to the supplied `sustained.rate`, `rate_limit.scope` defaulted to `tenant`, `rate_limit.strategy` defaulted to `reject`, and `rate_limit.cost` defaulted to `1`.
- [ ] `POST /oagw/v1/routes` with a `cors` object in the body returns `400` for an unrecognized property; a route's proxied requests are governed entirely by the upstream's `cors` configuration, never a route-level one.
