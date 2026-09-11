# Feature: Route Management


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
  - [Route Schema Validation](#route-schema-validation)
  - [Exactly-One-Match Enforcement](#exactly-one-match-enforcement)
  - [Upstream Reference Resolution](#upstream-reference-resolution)
  - [Upstream ID Immutability Check](#upstream-id-immutability-check)
  - [Route Conflict Detection](#route-conflict-detection)
  - [Upstream Deletion Cascade](#upstream-deletion-cascade)
- [4. States (CDSL)](#4-states-cdsl)
  - [Route Lifecycle State Machine](#route-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Route Creation Endpoint and Schema Validation](#route-creation-endpoint-and-schema-validation)
  - [HTTP Match Field Validation](#http-match-field-validation)
  - [gRPC Match Field Validation](#grpc-match-field-validation)
  - [Exactly-One-Match Enforcement](#exactly-one-match-enforcement-1)
  - [Upstream Reference Resolution](#upstream-reference-resolution-1)
  - [Upstream ID Immutability on Replace](#upstream-id-immutability-on-replace)
  - [Duplicate-Match Conflict Detection](#duplicate-match-conflict-detection)
  - [Enable/Disable and Priority Fields](#enabledisable-and-priority-fields)
  - [Route CRUD Endpoint Surface](#route-crud-endpoint-surface)
  - [List Query Parameter Support](#list-query-parameter-support)
  - [Tenant Scoping on Route CRUD](#tenant-scoping-on-route-crud)
  - [Cascade Delete of Routes on Upstream Deletion](#cascade-delete-of-routes-on-upstream-deletion)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-route-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-route-management`
## 1. Feature Context

### 1.1 Overview

This feature gives operators CRUD control over routes at `/oagw/v1/routes`. Each route belongs to exactly one upstream and defines the method/path or gRPC service/method rules that later select it.

### 1.2 Purpose

The feature validates route payloads against `route.v1.schema.json`, resolves and locks the owning `upstream_id`, and rejects routes that duplicate another enabled route's matching key. It exists so later config-resolution and proxy features can trust stored routes as structurally sound and mutually non-conflicting.

The platform's API gateway authenticates and authorizes every request to this management API ahead of this gear's mounted router, established by `cpt-cf-oagw-feature-gear-foundation`; this feature specifies no permission check of its own.

Persistence for this feature is in-process for the graded deployment; no database is configured for OAGW. `cpt-cf-oagw-db-schema` is cited here only as informing the route entity's shape, its cascade-delete relationship to its owning upstream, and the match-key uniqueness invariant, not as an actual SQL table.

**Requirements**: `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-fr-enable-disable`, `cpt-cf-oagw-nfr-multi-tenancy`, `cpt-cf-oagw-nfr-input-validation`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, replaces, and deletes routes under upstreams they administer. |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, lists, replaces, and deletes routes scoped to their own tenant. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-upstream-management` — routes validate and reference an existing `upstream_id`, so the upstream management surface, established by that feature, must already accept creates and reads before route creation can resolve a reference.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor and describe the end-to-end flow of creating, listing, replacing, and deleting a route.

**Use Cases**:
- [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`

### Create Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-create-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator submits a route referencing an upstream owned by their tenant, and the system persists it, returning 201 Created.

**Error Scenarios**:
- The referenced `upstream_id` does not resolve to an upstream owned by the calling tenant.
- The `match` object fails schema validation, or its `path` and `priority` match another enabled route under the same upstream whose method set intersects the candidate's.

**Steps**:
1. [ ] - `p1` - Operator sends POST /oagw/v1/routes with tags, upstream_id, match, plugins, and rate_limit fields - `inst-create-route-request`
2. [ ] - `p1` - API: POST /oagw/v1/routes (request: route payload; response: created Route with generated id) - `inst-create-route-api`
3. [ ] - `p1` - **IF** upstream_id does not resolve to an upstream owned by the calling tenant - `inst-create-route-upstream-check`
   1. [ ] - `p1` - **RETURN** 400 ValidationError identifying the unresolved upstream_id - `inst-create-route-upstream-fail`
4. [ ] - `p1` - **ELSE** validate match, http_match/grpc_match fields, and their defaults per route.v1.schema.json - `inst-create-route-schema-check`
5. [ ] - `p1` - **IF** schema validation fails - `inst-create-route-schema-branch`
   1. [ ] - `p1` - **RETURN** 400 ValidationError listing every non-conforming field - `inst-create-route-schema-fail`
6. [ ] - `p1` - **ELSE** compare the candidate's method set, path, and priority against every other enabled route under the same upstream_id - `inst-create-route-conflict-check`
7. [ ] - `p1` - **IF** an enabled route already shares the same path and priority, and its method set intersects the candidate's method set in at least one method - `inst-create-route-conflict-branch`
   1. [ ] - `p1` - **RETURN** 409 Conflict without persisting the candidate route - `inst-create-route-conflict-fail`
8. [ ] - `p1` - **ELSE** persist the route and **RETURN** 201 Created with the stored Route representation - `inst-create-route-success`

### List Routes

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-list-routes`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator retrieves a paginated, tenant-scoped list of routes filtered and ordered per the supplied query parameters.

**Error Scenarios**:
- A supplied `$filter` or `$orderby` expression references a field the routes list does not support.

**Steps**:
1. [ ] - `p1` - Operator sends GET /oagw/v1/routes with optional $filter, $select, $orderby, $top, and $skip parameters - `inst-list-route-request`
2. [ ] - `p1` - API: GET /oagw/v1/routes (request: OData query parameters; response: paginated Route array) - `inst-list-route-api`
3. [ ] - `p1` - **IF** a supplied $filter or $orderby expression references an unsupported field - `inst-list-route-query-check`
   1. [ ] - `p1` - **RETURN** 400 ValidationError identifying the unsupported query expression - `inst-list-route-query-fail`
4. [ ] - `p1` - **ELSE** apply $top (default 50, max 100) and $skip to the tenant-scoped route set - `inst-list-route-paginate`
5. [ ] - `p1` - **RETURN** 200 with the filtered, selected, and ordered Route array - `inst-list-route-success`

### Get Route by ID

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-get-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator retrieves a single route owned by their tenant, identified by its route id.

**Error Scenarios**:
- The route id does not resolve to a route owned by the calling tenant.

**Steps**:
1. [ ] - `p1` - Operator sends GET /oagw/v1/routes/{id} naming the route's id - `inst-get-route-request`
2. [ ] - `p1` - API: GET /oagw/v1/routes/{id} (request: route id path parameter; response: stored Route representation) - `inst-get-route-api`
3. [ ] - `p1` - **IF** no route with that id exists for the calling tenant - `inst-get-route-lookup`
   1. [ ] - `p1` - **RETURN** 404 - `inst-get-route-notfound`
4. [ ] - `p1` - **ELSE** **RETURN** 200 with the stored Route representation - `inst-get-route-success`

### Replace Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-replace-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator replaces an existing route's match, tags, plugins, rate_limit, enabled, or priority fields while upstream_id stays fixed.

**Error Scenarios**:
- The replace payload supplies a different upstream_id than the stored route.
- The replaced match fails schema validation, or its `path` and `priority` match another enabled route whose method set intersects the candidate's.

**Steps**:
1. [ ] - `p1` - Operator sends PUT /oagw/v1/routes/{id} with the full replacement route payload - `inst-replace-route-request`
2. [ ] - `p1` - API: PUT /oagw/v1/routes/{id} (request: full route payload; response: replaced Route) - `inst-replace-route-api`
3. [ ] - `p1` - **IF** the route id does not exist for the calling tenant - `inst-replace-route-lookup`
   1. [ ] - `p1` - **RETURN** 404 - `inst-replace-route-notfound`
4. [ ] - `p1` - **ELSE IF** the payload supplies an upstream_id different from the stored route's upstream_id - `inst-replace-route-immutable-check`
   1. [ ] - `p1` - **RETURN** 400 ValidationError rejecting the upstream_id reassignment - `inst-replace-route-immutable-fail`
5. [ ] - `p1` - **ELSE** validate match, http_match/grpc_match fields, and their defaults per route.v1.schema.json - `inst-replace-route-schema-check`
6. [ ] - `p1` - **IF** schema validation fails - `inst-replace-route-schema-branch`
   1. [ ] - `p1` - **RETURN** 400 ValidationError listing every non-conforming field - `inst-replace-route-schema-fail`
7. [ ] - `p1` - **ELSE** compare the candidate's method set, path, and priority against every other enabled route under the same upstream_id - `inst-replace-route-conflict-check`
8. [ ] - `p1` - **IF** a different enabled route already shares the same path and priority, and its method set intersects the candidate's method set in at least one method - `inst-replace-route-conflict-branch`
   1. [ ] - `p1` - **RETURN** 409 Conflict without persisting the change - `inst-replace-route-conflict-fail`
9. [ ] - `p1` - **ELSE** persist the full replacement and **RETURN** 200 with the replaced Route representation - `inst-replace-route-success`

### Delete Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-delete-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Operator deletes a tenant-owned route, which is removed from route matching immediately.

**Error Scenarios**:
- The route id does not exist for the calling tenant.

**Steps**:
1. [ ] - `p1` - Operator sends DELETE /oagw/v1/routes/{id} - `inst-delete-route-request`
2. [ ] - `p1` - API: DELETE /oagw/v1/routes/{id} (request: route id; response: empty body) - `inst-delete-route-api`
3. [ ] - `p1` - **IF** the route id does not exist for the calling tenant - `inst-delete-route-lookup`
   1. [ ] - `p1` - **RETURN** 404 - `inst-delete-route-notfound`
4. [ ] - `p1` - **ELSE** remove the route record - `inst-delete-route-remove`
5. [ ] - `p1` - **RETURN** 204 with an empty body - `inst-delete-route-success`

## 3. Processes / Business Logic (CDSL)

Internal validation and lookup routines shared by the create and replace flows above.

### Route Schema Validation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-route-schema-validation`

**Input**: a candidate route payload (tags, upstream_id, match, plugins, rate_limit) submitted on create or replace.

**Output**: validated route data, or a 400 ValidationError listing every non-conforming field.

**Steps**:
1. [ ] - `p1` - Parse the payload against route.v1.schema.json's top-level required properties upstream_id and match - `inst-schema-parse`
2. [ ] - `p1` - **IF** match does not contain exactly one of http or grpc - `inst-schema-match-check`
   1. [ ] - `p1` - **RETURN** 400 ValidationError citing the match field - `inst-schema-match-fail`
3. [ ] - `p1` - **ELSE IF** the http branch is present - `inst-schema-http-branch`
   1. [ ] - `p1` - Validate http_match.methods is non-empty and drawn from the schema's method enum, and http_match.path has minimum length 1 - `inst-schema-http-validate`
4. [ ] - `p1` - **ELSE** validate grpc_match.service and grpc_match.method each have minimum length 1 - `inst-schema-grpc-validate`
5. [ ] - `p1` - Apply the query_allowlist default of an empty array when omitted, and accept path_suffix_mode as either append or disabled, persisting whichever value is supplied and defaulting to append only when the field is omitted - `inst-schema-defaults`
6. [ ] - `p1` - Accept enabled and priority as application-level fields defined by DESIGN.md and PRD.md, never validated against route.v1.schema.json - `inst-schema-app-fields`
7. [ ] - `p1` - **RETURN** validated route data - `inst-schema-return`

### Exactly-One-Match Enforcement

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-exactly-one-match-enforcement`

**Input**: the route payload's match object.

**Output**: acceptance of the single populated branch, or a 400 ValidationError.

**Steps**:
1. [ ] - `p1` - Check whether match carries an http key, a grpc key, both, or neither - `inst-exactly-one-check`
2. [ ] - `p1` - **IF** neither key is present - `inst-exactly-one-none`
   1. [ ] - `p1` - **RETURN** 400 ValidationError requiring exactly one of http or grpc - `inst-exactly-one-none-fail`
3. [ ] - `p1` - **ELSE IF** both keys are present - `inst-exactly-one-both`
   1. [ ] - `p1` - **RETURN** 400 ValidationError rejecting the combined match - `inst-exactly-one-both-fail`
4. [ ] - `p1` - **ELSE** **RETURN** the single populated branch for downstream field validation - `inst-exactly-one-success`

### Upstream Reference Resolution

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-reference-resolution`

**Input**: upstream_id from the route payload and the calling tenant identity.

**Output**: the referenced upstream record, or a 400 ValidationError.

**Steps**:
1. [ ] - `p1` - Look up the upstream by upstream_id scoped to the calling tenant; ancestor-tenant upstreams are not addressable here - `inst-upstream-ref-lookup`
2. [ ] - `p1` - **IF** no matching upstream is found - `inst-upstream-ref-check`
   1. [ ] - `p1` - **RETURN** 400 ValidationError indicating the upstream reference does not exist - `inst-upstream-ref-fail`
3. [ ] - `p1` - **ELSE** **RETURN** the resolved upstream record for match-conflict comparison - `inst-upstream-ref-success`

### Upstream ID Immutability Check

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-id-immutability-check`

**Input**: the existing route's upstream_id and the upstream_id supplied on a replace request.

**Output**: acceptance with the resolved upstream_id, or a 400 ValidationError.

**Steps**:
1. [ ] - `p1` - Compare the stored route's upstream_id to the value supplied in the PUT payload, if any - `inst-immutable-compare`
2. [ ] - `p1` - **IF** the replace payload omits upstream_id - `inst-immutable-omitted`
   1. [ ] - `p1` - Retain the existing upstream_id unchanged - `inst-immutable-retain`
3. [ ] - `p1` - **ELSE IF** the supplied upstream_id differs from the stored value - `inst-immutable-diff`
   1. [ ] - `p1` - **RETURN** 400 ValidationError rejecting the attempted upstream reassignment - `inst-immutable-fail`
4. [ ] - `p1` - **ELSE** **RETURN** acceptance and continue replace processing - `inst-immutable-success`

### Route Conflict Detection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-route-conflict-detection`

**Input**: the candidate route's HTTP method set, path, and priority, plus every other enabled route stored under the same upstream_id.

**Output**: acceptance, or a 409 Conflict.

A conflict exists when the candidate route's `path` and `priority` both match an existing enabled route's `path` and `priority`, and the two routes' method sets intersect in at least one method; two method sets need not be identical to conflict, and a differing `priority` alone never conflicts.

**Steps**:
1. [ ] - `p1` - **FOR EACH** enabled route already stored under the same upstream_id - `inst-conflict-loop`
   1. [ ] - `p1` - Compare its path and priority to the candidate route's values, and intersect its method set with the candidate's method set - `inst-conflict-compare`
2. [ ] - `p1` - **IF** any existing enabled route shares the same path and priority as the candidate, and its method set intersects the candidate's method set in at least one method - `inst-conflict-check`
   1. [ ] - `p1` - **RETURN** 409 Conflict without persisting the candidate route - `inst-conflict-fail`
3. [ ] - `p1` - **ELSE** **RETURN** acceptance, allowing the candidate route to be persisted - `inst-conflict-success`

### Upstream Deletion Cascade

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-route-cascade-delete`

**Input**: the `upstream_id` of an upstream that has just been deleted, and every route currently stored under that `upstream_id` in the in-process control-plane store.

**Output**: confirmation that no route remains bound to the deleted `upstream_id`.

**Steps**:
1. [ ] - `p1` - **FOR EACH** route stored under the deleted upstream's `upstream_id` - `inst-cascade-delete-loop`
   1. [ ] - `p1` - Remove the route record from the in-process control-plane store, standing in for the database's cascade-delete constraint on `upstream_id` - `inst-cascade-delete-remove`
2. [ ] - `p1` - **RETURN** confirmation that no route referencing the deleted `upstream_id` remains stored - `inst-cascade-delete-return`

## 4. States (CDSL)

### Route Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-route-lifecycle`

**States**: Enabled, Disabled, Deleted

**Initial State**: Enabled

**Transitions**:
1. [ ] - `p1` - **FROM** Enabled **TO** Disabled **WHEN** an operator replaces the route with enabled set to false - `inst-state-enabled-to-disabled`
2. [ ] - `p1` - **FROM** Disabled **TO** Enabled **WHEN** an operator replaces the route with enabled set to true - `inst-state-disabled-to-enabled`
3. [ ] - `p1` - **FROM** Enabled **TO** Deleted **WHEN** an operator deletes the route by id - `inst-state-enabled-to-deleted`
4. [ ] - `p1` - **FROM** Disabled **TO** Deleted **WHEN** an operator deletes the route by id - `inst-state-disabled-to-deleted`

A Disabled route stays reachable through GET, PUT, and DELETE on the management API, but a Disabled route is excluded entirely from route matching at proxy time.

## 5. Definitions of Done

### Route Creation Endpoint and Schema Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-schema-validation`

The system **MUST** implement `POST /oagw/v1/routes`, validating the payload against `route.v1.schema.json`'s required `upstream_id` and `match` fields before persisting.

**Implements**:
- `cpt-cf-oagw-flow-create-route`
- `cpt-cf-oagw-algo-route-schema-validation`

**Touches**:
- API: `POST /oagw/v1/routes`
- Entities: `Route`, `MatchConfig`

### HTTP Match Field Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-http-match-validation`

The system **MUST** reject an `http` match missing `methods` or `path`, enforce `methods` `minItems` 1 drawn from the schema's method enum and `path` `minLength` 1, default `query_allowlist` to an empty array when omitted, and accept `path_suffix_mode` as either `append` or `disabled`, persisting an explicit value unchanged and defaulting to `append` only when the field is omitted.

**Implements**:
- `cpt-cf-oagw-algo-route-schema-validation`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `PUT /oagw/v1/routes/{id}`
- Entities: `MatchConfig`

### gRPC Match Field Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-grpc-match-validation`

The system **MUST** accept and store a `grpc` match requiring `service` and `method`, each with minimum length 1, without exercising any gRPC proxy code path.

**Implements**:
- `cpt-cf-oagw-algo-route-schema-validation`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `PUT /oagw/v1/routes/{id}`
- Entities: `MatchConfig`

### Exactly-One-Match Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-exactly-one-match`

The system **MUST** reject a route payload whose `match` object carries both `http` and `grpc`, or neither, with a 400 ValidationError.

**Implements**:
- `cpt-cf-oagw-algo-exactly-one-match-enforcement`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `PUT /oagw/v1/routes/{id}`
- Entities: `MatchConfig`

### Upstream Reference Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-reference-check`

The system **MUST** resolve `upstream_id` against the calling tenant's own upstreams on create and reject an unresolved reference with a 400 ValidationError.

**Implements**:
- `cpt-cf-oagw-flow-create-route`
- `cpt-cf-oagw-algo-upstream-reference-resolution`

**Touches**:
- API: `POST /oagw/v1/routes`
- Entities: `Route`

### Upstream ID Immutability on Replace

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-id-immutability`

The system **MUST** retain the stored `upstream_id` when a replace payload omits it, and reject any replace attempt that supplies a different `upstream_id` with a 400 ValidationError.

**Implements**:
- `cpt-cf-oagw-flow-replace-route`
- `cpt-cf-oagw-algo-upstream-id-immutability-check`

**Touches**:
- API: `PUT /oagw/v1/routes/{id}`
- Entities: `Route`

### Duplicate-Match Conflict Detection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-conflict-detection`

The system **MUST** reject a create or replace when the candidate route's `path` and `priority` match another enabled route under the same `upstream_id` and their HTTP method sets intersect in at least one method, returning a 409 Conflict.

**Implements**:
- `cpt-cf-oagw-flow-create-route`
- `cpt-cf-oagw-flow-replace-route`
- `cpt-cf-oagw-algo-route-conflict-detection`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `PUT /oagw/v1/routes/{id}`
- Entities: `Route`

### Enable/Disable and Priority Fields

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-enable-disable-fields`

The system **MUST** accept and persist `enabled` and `priority` as application-level route fields defined by DESIGN.md and PRD.md, excluding a disabled route from route matching, without validating either field against `route.v1.schema.json`.

**Implements**:
- `cpt-cf-oagw-state-route-lifecycle`

**Touches**:
- Entities: `Route`

### Route CRUD Endpoint Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-crud-endpoints`

The system **MUST** expose gear-relative `POST`, `GET` list, `GET` by id, `PUT`, and `DELETE` operations at `/oagw/v1/routes`, returning `201`, `200`, `200`, `200`, and `204` respectively on success.

**Implements**:
- `cpt-cf-oagw-flow-create-route`
- `cpt-cf-oagw-flow-list-routes`
- `cpt-cf-oagw-flow-get-route`
- `cpt-cf-oagw-flow-replace-route`
- `cpt-cf-oagw-flow-delete-route`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `GET /oagw/v1/routes`
- API: `GET /oagw/v1/routes/{id}`
- API: `PUT /oagw/v1/routes/{id}`
- API: `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`

### List Query Parameter Support

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-route-list-query-params`

The system **MUST** support `$filter`, `$select`, `$orderby`, `$top` (default 50, max 100), and `$skip` query parameters on `GET /oagw/v1/routes`.

**Implements**:
- `cpt-cf-oagw-flow-list-routes`

**Touches**:
- API: `GET /oagw/v1/routes`
- Entities: `Route`

### Tenant Scoping on Route CRUD

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-tenant-scoping`

The system **MUST** scope every route CRUD operation to the calling tenant, returning `404` for a route id or an `upstream_id` belonging to another or an ancestor tenant.

**Implements**:
- `cpt-cf-oagw-flow-create-route`
- `cpt-cf-oagw-flow-list-routes`
- `cpt-cf-oagw-flow-get-route`
- `cpt-cf-oagw-flow-replace-route`
- `cpt-cf-oagw-flow-delete-route`

**Touches**:
- API: `POST /oagw/v1/routes`
- API: `GET /oagw/v1/routes`
- API: `GET /oagw/v1/routes/{id}`
- API: `PUT /oagw/v1/routes/{id}`
- API: `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`

### Cascade Delete of Routes on Upstream Deletion

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-cascade-delete`

The system **MUST** remove every route bound to an upstream from the in-process control-plane store when that upstream is deleted, leaving no orphaned route referencing a deleted `upstream_id`.

**Implements**:
- `cpt-cf-oagw-algo-route-cascade-delete`

**Touches**:
- Entities: `Route`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/routes` with a body omitting both `upstream_id` and `match` returns `400` with a problem+json body naming both missing fields.
- [ ] `POST /oagw/v1/routes` with `match` containing both `http` and `grpc` keys returns `400` ValidationError rejecting the combined match.
- [ ] `POST /oagw/v1/routes` with `match` containing neither `http` nor `grpc` returns `400` ValidationError requiring exactly one match type.
- [ ] `POST /oagw/v1/routes` with `http.methods` omitted or an empty array returns `400` citing the `methods` field's `minItems` 1 requirement.
- [ ] `POST /oagw/v1/routes` with `http.methods` containing a value outside `GET`, `POST`, `PUT`, `DELETE`, `PATCH` returns `400` ValidationError.
- [ ] `POST /oagw/v1/routes` with `http.path` an empty string returns `400` citing the `path` field's `minLength` 1 requirement.
- [ ] `POST /oagw/v1/routes` omitting `http.query_allowlist` persists the route with `query_allowlist` defaulted to an empty array.
- [ ] `POST /oagw/v1/routes` omitting `http.path_suffix_mode` persists the route with `path_suffix_mode` defaulted to `append`.
- [ ] `POST /oagw/v1/routes` with `http.path_suffix_mode` explicitly set to `disabled` persists the route with `path_suffix_mode` stored as `disabled`, unchanged.
- [ ] `POST /oagw/v1/routes` with `grpc.service` or `grpc.method` omitted returns `400` ValidationError citing the missing field.
- [ ] `POST /oagw/v1/routes` with `grpc.service` or `grpc.method` an empty string returns `400` citing the corresponding field's `minLength` 1 requirement.
- [ ] `POST /oagw/v1/routes` with an `upstream_id` that does not exist for the calling tenant returns `400` ValidationError.
- [ ] `POST /oagw/v1/routes` with an `upstream_id` belonging to another tenant returns `400` ValidationError, identical to a nonexistent reference.
- [ ] `POST /oagw/v1/routes` with the same `path` and `priority` as another enabled route under the same `upstream_id`, and a `methods` array that shares at least one method with that route's `methods` even though the two arrays differ, returns `409` Conflict.
- [ ] `POST /oagw/v1/routes` with the same `methods` and `path` as another enabled route under the same `upstream_id`, but a different `priority`, returns `201 Created` with both routes persisted.
- [ ] `PUT /oagw/v1/routes/{id}` supplying a different `upstream_id` than the stored route returns `400` ValidationError rejecting the reassignment.
- [ ] `PUT /oagw/v1/routes/{id}` omitting `upstream_id` retains the route's existing `upstream_id` unchanged in the stored representation.
- [ ] `PUT /oagw/v1/routes/{id}` setting `enabled` to `false` persists the change without `route.v1.schema.json` validating the `enabled` field.
- [ ] `GET /oagw/v1/routes/{id}` for an existing, tenant-owned route returns `200` with the stored Route representation.
- [ ] `GET /oagw/v1/routes/{id}` for a route belonging to another tenant returns `404`, not `403`.
- [ ] `GET /oagw/v1/routes` returns `200` with results filtered, selected, and ordered per `$filter`, `$select`, and `$orderby`, paginated per `$top` (default 50, max 100) and `$skip`.
- [ ] `DELETE /oagw/v1/routes/{id}` for an existing, tenant-owned route returns `204` with an empty body.
- [ ] `DELETE /oagw/v1/routes/{id}` for a route id that does not exist for the calling tenant returns `404`.
- [ ] `DELETE /oagw/v1/upstreams/{id}` for an upstream with routes bound to it removes those routes from the in-process control-plane store, so a subsequent `GET /oagw/v1/routes/{id}` for any of them returns `404`.
- [ ] Every `400` and `409` response from the routes endpoints is `application/problem+json` with `type`, `title`, `status`, and `detail` fields, and sets `X-OAGW-Error-Source: gateway`.
