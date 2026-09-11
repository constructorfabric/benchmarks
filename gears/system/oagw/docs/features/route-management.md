# Feature: Route Management


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Route](#create-route)
  - [List and Read Routes](#list-and-read-routes)
  - [Replace Route](#replace-route)
  - [Delete Route](#delete-route)
  - [Enable and Disable a Route](#enable-and-disable-a-route)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Match Block Validation](#match-block-validation)
  - [Upstream Reference Resolution](#upstream-reference-resolution)
  - [Match-Rule Uniqueness and Determinism](#match-rule-uniqueness-and-determinism)
  - [Route List Query](#route-list-query)
- [4. States (CDSL)](#4-states-cdsl)
  - [Route State Machine](#route-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Route CRUD Lifecycle](#route-crud-lifecycle)
  - [Match Block Exclusivity and Validation](#match-block-exclusivity-and-validation)
  - [Path Suffix and Query Allowlist Configuration](#path-suffix-and-query-allowlist-configuration)
  - [Upstream Reference Validation and Immutability](#upstream-reference-validation-and-immutability)
  - [Match-Rule Uniqueness and Enabled Exclusion](#match-rule-uniqueness-and-enabled-exclusion)
  - [Route-Level Overrides and Plugin Bindings](#route-level-overrides-and-plugin-bindings)
  - [Route List Query Parameters](#route-list-query-parameters)
  - [Strict Tenant Scoping on Route Records](#strict-tenant-scoping-on-route-records)
  - [Route Schema Conformance and Documented Schema Contract](#route-schema-conformance-and-documented-schema-contract)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-route-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-route-management`

## 1. Feature Context

### 1.1 Overview

Implements Control Plane CRUD for routes: the matching rules that map an inbound proxy request to a specific upstream behavior. A route carries a protocol-scoped match block (`http` or `grpc`), an immutable reference to the upstream it belongs to, a priority, an `enabled` flag, and route-level overrides for rate limits, CORS, plugin bindings, and tags. The feature delivers `POST`, `GET` list, `GET` by id, `PUT` full replacement, and `DELETE` at the gear-relative `/oagw/v1/routes` path, together with the match-rule validation, the upstream foreign-key validation, and the match-rule uniqueness invariants that keep the stored route set deterministic for the proxy path.

### 1.2 Purpose

Routes are the matching rules that make an upstream addressable with specific behavior: which methods and paths reach it, which query parameters are permitted, and which route-level overrides apply on top of the upstream base configuration. Without a validated route store, the Data Plane has nothing to match against. This feature builds on the repository boundary, the domain types, and the merge engine delivered by `cpt-cf-oagw-feature-gear-foundation`, and on the upstream store and its alias and uniqueness rules delivered by `cpt-cf-oagw-feature-upstream-management`, because every route carries an `upstream_id` that must resolve to an existing, tenant-owned upstream before the route can be persisted. It realizes the route half of the management REST surface recorded in `cpt-cf-oagw-interface-api` and the Control Plane route CRUD components of `cpt-cf-oagw-component-model`, and it routes every route operation to the Control Plane per `cpt-cf-oagw-adr-request-routing`.

**Requirements** delivered by this feature:

- `p1` - `cpt-cf-oagw-fr-route-mgmt`
- `p1` - `cpt-cf-oagw-usecase-configure-route`
- `p1` - `cpt-cf-oagw-interface-management-api`

**Requirements** inherited as already satisfied upstream and therefore referenced in plain form, not re-delivered here: `cpt-cf-oagw-fr-config-layering` and `cpt-cf-oagw-fr-hierarchical-config` are marked done in the PRD and are realized by the merge engine of `cpt-cf-oagw-feature-gear-foundation`; this feature only supplies the route layer that the merge engine consumes. The route slice of `cpt-cf-oagw-fr-enable-disable` (the `enabled` flag on routes and its exclusion of a disabled route from matching) is delivered here; the upstream slice of that requirement, including the `503` rejection of a disabled upstream, belongs to `cpt-cf-oagw-feature-upstream-management` and to `cpt-cf-oagw-feature-request-proxy`.

**Principles**: none. This entry introduces no design principle. `cpt-cf-oagw-principle-tenant-scope` is inherited from the 2.1 repository layer and from 2.2 and is honored here as strict tenant scoping rather than claimed as new coverage.

**Constraints**: none. This entry introduces no DESIGN constraint; the body-size and scheme constraints are enforced on the proxy path by `cpt-cf-oagw-feature-request-proxy` (`cpt-cf-oagw-constraint-body-limit`, `cpt-cf-oagw-constraint-https-only`).

**Sequences**: none. Route management is a management-plane operation; `cpt-cf-oagw-seq-proxy-flow` is the only sequence DESIGN.md defines and is owned by `cpt-cf-oagw-feature-request-proxy`.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, replaces, and deletes routes under `/oagw/v1/routes` with the `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` permission |
| `cpt-cf-oagw-actor-tenant-admin` | Owns the tenant-scoped route records, lists and reads them, and toggles their `enabled` flag, exercising the same `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` permission set the platform operator carries for the route base type; both actors exercise read and list, which is what every flow below requires of them, and ancestor routes are never visible through this API |
| `cpt-cf-oagw-actor-types-registry` | Holds the base type `gts.cf.core.oagw.route.v1~` under which every route resource identifier in a path parameter resolves |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Schemas**: [schemas/route.v1.schema.json](../schemas/route.v1.schema.json), [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` (repository boundary, merge engine, domain types, route registration), `cpt-cf-oagw-feature-upstream-management` (the upstream store every `upstream_id` must resolve against). `cpt-cf-oagw-feature-request-proxy` depends on this feature and consumes the stored match keys at request time.

**Applicability**: the requirement domains this entry does not carry are excluded explicitly rather than left unaddressed:

- **Performance (PERF)**: not applicable because this entry is a management-plane write path only; the PRD latency budget of `cpt-cf-oagw-nfr-low-latency` is proxy-scoped and is owned by `cpt-cf-oagw-feature-request-proxy` (decomposition entry 2.4), so no management-plane latency target is claimed here.
- **User experience (UX)**: not applicable because the surface is a REST management API; there is no human-facing UI journey for this entry to cover.
- **Compliance (COMPL)**: not applicable because no regulatory regime applies to the route record or to this management surface.
- **Security — data protection (SEC)**: not applicable because a route payload carries no credential material; the `cred://` references governed by `cpt-cf-oagw-principle-cred-isolation` never appear in a route body, so no secret-handling statement is owed here.
- **Reliability — fault tolerance (REL)**: not applicable because the feature has no external dependency; its store is the in-process, config-backed repository of `cpt-cf-oagw-feature-gear-foundation` (graded deviation 5).
- **Integration — event and cache integration (INT)**: not applicable as an owned concern because the invalidation and flush that follow a route write are delegated to `cpt-cf-oagw-feature-observability-and-operability`, per the configuration-write step of the create, replace, and delete flows.
- **Operations — rollout (OPS)**: not applicable because the entry is delivered inside the single `oagw` crate with no schema migration and no rollout strategy; the store is created in place at initialization.

## 2. Actor Flows (CDSL)

**Use cases**:

- `p1` - `cpt-cf-oagw-usecase-configure-route`

The use case above is the operator-facing scenario this feature serves: POST a route with an `upstream_id` and match rules, the system validates the upstream reference and the match-rule format, and the route becomes active for request matching. The proxy-time consumption of the stored rules is not part of this feature.

### Create Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-management-create-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A route payload referencing an existing upstream owned by the calling tenant is validated, assigned a server-generated identifier in the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}`, and persisted with its match block, method allowlist, tags, and ordered plugin bindings in one atomic write.
- A route created without an `enabled` field is stored enabled, per the default declared in `cpt-cf-oagw-fr-enable-disable`.
- A route created without a `priority` field is stored with the declared default `0`; `priority` is an integer, and a higher value wins at match time.
- A route whose `match` block selects `grpc` is accepted and stored as configuration surface, with no gRPC proxy code path behind it (graded deviation 7).
- The accepted write is a configuration write, so it emits the structured audit event of DESIGN §4.3 — fields `event`, `tenant_id`, `principal_id`, the route resource identifier, and the outcome — attributed to the caller's security context; the emitting owner is `cpt-cf-oagw-feature-observability-and-operability`.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the required route permission: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted.
- `upstream_id` does not resolve to an existing upstream owned by the calling tenant, including an ancestor upstream, which is not directly addressable: `400` validation error, with no disclosure of the foreign record.
- The `match` block carries neither `http` nor `grpc`, carries both, or carries keys outside the selected match definition: `400` validation error.
- An `http` match omits `methods` or `path`, carries an empty `methods` list, or carries a method outside `GET`, `POST`, `PUT`, `DELETE`, `PATCH`: `400` validation error.
- A `grpc` match omits `service` or `method`, or either value is empty: `400` validation error.
- A `priority` that is not an integer is supplied: `400` validation error.
- A field outside the published route schema shapes of `docs/schemas/route.v1.schema.json` plus the named, closed set of schema-external API fields `priority`, `enabled`, and `cors` is present: `400` validation error. That union is the whole payload contract; the route domain type of `cpt-cf-oagw-design-domain-model` is a superset of the published schema document, which this entry does not edit, and the remaining fields it carries (`id`, `tenant_id`, `match_type`) are server-assigned or derived rather than accepted.
- A `plugins.items[]` entry names a catalog-only `plugin_ref`: rejected at binding time with `400`, per the binding-time check described in `cpt-cf-oagw-dod-route-management-route-overrides`.
- The match rule collides with an existing enabled route under the same upstream: `409 Conflict` and the store is left unchanged.

**Concurrency posture**: control-plane writes are serialized by the repository's write path (a single-process in-memory store), so the uniqueness check of step 8 and the persist of step 10 execute as one critical section and a detected collision leaves the store unchanged. Cross-instance concurrency control is out of scope per DESIGN §4.7 and is recorded as a limitation on the determinism invariant.

**Steps**:
1. [x] - `p1` - Receive `POST /oagw/v1/routes` with the caller's security context and the route payload - `inst-rm-create-1`
2. [x] - `p1` - Authorize the caller's security context against the route permission set `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` of the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`), resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers - `inst-rm-create-13`
3. [x] - `p1` - Validate the payload against the published route schema shapes plus the closed set of schema-external API fields `priority`, `enabled`, and `cors`, rejecting any field outside that union and applying the declared defaults - `inst-rm-create-2`
4. [x] - `p1` - Resolve `upstream_id` against the calling tenant per `cpt-cf-oagw-algo-route-management-upstream-reference` - `inst-rm-create-3`
5. [x] - `p1` - **IF** the referenced upstream does not exist or is not owned by the calling tenant - `inst-rm-create-4`
   1. [x] - `p1` - Reject with a validation error and return no information about the foreign or ancestor upstream - `inst-rm-create-5`
6. [x] - `p1` - Validate the `match` block per `cpt-cf-oagw-algo-route-management-match-validation` - `inst-rm-create-6`
7. [x] - `p1` - **IF** match validation fails - `inst-rm-create-7`
   1. [x] - `p1` - Reject with a validation error that names the offending match key - `inst-rm-create-8`
8. [x] - `p1` - Check match-rule uniqueness against the enabled routes of the referenced upstream per `cpt-cf-oagw-algo-route-management-uniqueness` - `inst-rm-create-9`
9. [x] - `p1` - **IF** an enabled route under the same upstream already claims the same match keys - `inst-rm-create-10`
   1. [x] - `p1` - Reject with `409 Conflict` and leave the store unchanged - `inst-rm-create-11`
10. [x] - `p1` - Assign the server-generated route identifier, bind the record to the calling tenant, and persist the route together with its match keys, method rows, tags, and ordered plugin bindings as one atomic write - `inst-rm-create-11b`
11. [x] - `p1` - Treat the write as a configuration write that triggers Control Plane L1 invalidation and the Data Plane hot-config flush per `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, and `cpt-cf-oagw-adr-state-management`, through the write-side invalidation and flush surface of `cpt-cf-oagw-feature-observability-and-operability`, which owns the mechanism this feature only invokes - `inst-rm-create-14`
12. [x] - `p1` - **RETURN** the stored route, including its `enabled` default of `true` and its `priority` default of `0` when either field was omitted - `inst-rm-create-12`

### List and Read Routes

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-management-list-routes`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- `GET /oagw/v1/routes` returns the caller's own routes, paged and ordered by the OData query parameters, with `$filter` able to select the routes of one upstream.
- `GET /oagw/v1/routes/{id}` returns one route owned by the calling tenant, including its match block, priority, `enabled` value, and route-level overrides.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the route read permission `gts.cf.core.oagw.route.v1~:read`: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted.
- A route identifier owned by another tenant, including an ancestor tenant, is requested: `404`, indistinguishable from a missing record.
- An unsupported `$filter` or `$orderby` field, or an out-of-range `$top`, is supplied: `400` validation error.

**Steps**:
1. [x] - `p1` - Receive the list or read request with the caller's tenant identifier - `inst-rm-list-1`
2. [x] - `p1` - Authorize the caller's security context against the route permission set `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` of the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`), the read and list operations requiring its `read` element, resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers - `inst-rm-list-8`
3. [x] - `p1` - Bind the query to the caller's tenant identifier so that only own-tenant route records are visible - `inst-rm-list-2`
4. [x] - `p1` - **IF** a single route is addressed by identifier - `inst-rm-list-3`
   1. [x] - `p1` - Resolve the record within the caller's tenant and return not-found for a foreign, ancestor, or missing record without disclosing which of the three it is - `inst-rm-list-3a`
5. [x] - `p1` - For a list request, apply the OData query parameters per `cpt-cf-oagw-algo-route-management-list-query` - `inst-rm-list-4`
6. [x] - `p1` - **IF** a query parameter is unsupported, malformed, or out of range - `inst-rm-list-5`
   1. [x] - `p1` - Reject with a validation error naming the parameter - `inst-rm-list-6`
7. [x] - `p1` - **RETURN** the route records with their match blocks and overrides, and no ancestor-tenant route - `inst-rm-list-7`

### Replace Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-management-replace-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- `PUT /oagw/v1/routes/{id}` performs a full replacement per DESIGN §3.3 (`cpt-cf-oagw-interface-api`): "Full replacement — all fields are overwritten; omitted optional fields are cleared." Omitted optional fields are cleared and the declared defaults are materialized, so a replacement body that omits `enabled` re-enables the route (default `true`, per `cpt-cf-oagw-fr-enable-disable`), one that omits `priority` materializes the default `0`, and a body that omits `rate_limit`, `cors`, `tags`, or `plugins` clears that override. A disabled route is therefore kept disabled by sending `enabled: false` in the replacement body. The match block is re-validated, and match-rule uniqueness is re-checked while excluding the record under replacement.
- The route keeps its identity: `id`, `tenant_id`, and `upstream_id` are retained, so replacing a route never retargets it to a different upstream. `upstream_id` is not part of the update DTO.
- The accepted write is a configuration write, so it emits the structured audit event of DESIGN §4.3 — fields `event`, `tenant_id`, `principal_id`, the route resource identifier, and the outcome — attributed to the caller's security context; the emitting owner is `cpt-cf-oagw-feature-observability-and-operability`.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the required route permission: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted.
- The route is owned by another tenant, an ancestor tenant, or does not exist: `404`.
- `upstream_id` is not part of the route update DTO per DESIGN §3.3 (`cpt-cf-oagw-interface-api`): a replacement body that supplies `upstream_id` is rejected as an immutable-field violation (`400`) regardless of the supplied value, matching or not, because route `upstream_id` is immutable. A body that omits it is the normal case and retargets nothing.
- A `priority` that is not an integer is supplied: `400` validation error.
- A field outside the published route schema shapes of `docs/schemas/route.v1.schema.json` plus the closed set of schema-external API fields `priority`, `enabled`, and `cors` is present: `400` validation error and the stored record is left unchanged.
- A `plugins.items[]` entry names a catalog-only `plugin_ref`: rejected at binding time with `400`.
- The replacement match rule collides with another enabled route under the same upstream: `409 Conflict` and the stored record is left unchanged.
- The replacement match block is invalid: `400` validation error and the stored record is left unchanged.

**Concurrency posture**: control-plane writes are serialized by the repository's write path (a single-process in-memory store), so the uniqueness re-check of step 8 and the rewrite of step 10 execute as one critical section and a detected collision leaves the stored record unchanged. Cross-instance concurrency control is out of scope per DESIGN §4.7 and is recorded as a limitation on the determinism invariant.

**Steps**:
1. [x] - `p1` - Receive `PUT /oagw/v1/routes/{id}` with the caller's tenant identifier and the replacement payload - `inst-rm-replace-1`
2. [x] - `p1` - Authorize the caller's security context against the route permission set `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` of the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`), resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers - `inst-rm-replace-12`
3. [x] - `p1` - Resolve the route within the caller's tenant and reject with `404` when it is missing, foreign, or owned by an ancestor - `inst-rm-replace-2`
4. [x] - `p1` - Retain the immutable fields `id`, `tenant_id`, and `upstream_id` from the stored record - `inst-rm-replace-3`
5. [x] - `p1` - **IF** the replacement body supplies `upstream_id` at all - `inst-rm-replace-4`
   1. [x] - `p1` - Reject the replacement as an immutable-field violation with `400` regardless of the supplied value, because `upstream_id` is not part of the update DTO and route `upstream_id` is immutable, and leave the record unchanged - `inst-rm-replace-5`
6. [x] - `p1` - Validate the replacement match block and route-level overrides per `cpt-cf-oagw-algo-route-management-match-validation` - `inst-rm-replace-6`
7. [x] - `p1` - **IF** match validation fails - `inst-rm-replace-7`
   1. [x] - `p1` - Reject with a validation error and leave the stored record unchanged - `inst-rm-replace-7b`
8. [x] - `p1` - Re-check match-rule uniqueness per `cpt-cf-oagw-algo-route-management-uniqueness`, excluding the route under replacement - `inst-rm-replace-8`
9. [x] - `p1` - **IF** the re-check finds a collision with another enabled route - `inst-rm-replace-9`
   1. [x] - `p1` - Reject with `409 Conflict` and leave the stored record unchanged - `inst-rm-replace-9b`
10. [x] - `p1` - Overwrite every replaceable field, clear omitted optional fields, materialize the declared defaults on the cleared fields, and rewrite the child match, method, tag, and plugin-binding records in one atomic write - `inst-rm-replace-10`
11. [x] - `p1` - Treat the write as a configuration write that triggers Control Plane L1 invalidation and the Data Plane hot-config flush per `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, and `cpt-cf-oagw-adr-state-management`, through the write-side invalidation and flush surface of `cpt-cf-oagw-feature-observability-and-operability`, which owns the mechanism this feature only invokes - `inst-rm-replace-13`
12. [x] - `p1` - **RETURN** the replaced route - `inst-rm-replace-11`

### Delete Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-management-delete-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- `DELETE /oagw/v1/routes/{id}` removes the route and its dependent match, method, tag, and plugin-binding records in one atomic write; a subsequent read returns `404`.
- Deletion has no in-use precondition: routes are leaf configuration records, and the plugin-in-use protection is a plugin-side concern owned by `cpt-cf-oagw-feature-plugin-system`.
- The accepted write is a configuration write, so it emits the structured audit event of DESIGN §4.3 — fields `event`, `tenant_id`, `principal_id`, the route resource identifier, and the outcome — attributed to the caller's security context; the emitting owner is `cpt-cf-oagw-feature-observability-and-operability`.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the required route permission: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted.
- The route is owned by another tenant, an ancestor tenant, or does not exist: `404`, with no disclosure of which case applies.

**Steps**:
1. [x] - `p1` - Receive `DELETE /oagw/v1/routes/{id}` with the caller's tenant identifier - `inst-rm-del-1`
2. [x] - `p1` - Authorize the caller's security context against the route permission set `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` of the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`), resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers - `inst-rm-del-6`
3. [x] - `p1` - Resolve the route within the caller's tenant and reject with `404` when it is missing, foreign, or owned by an ancestor tenant - `inst-rm-del-2`
4. [x] - `p1` - Remove the route record together with its `oagw_route_http_match` or `oagw_route_grpc_match`, `oagw_route_method`, `oagw_route_tag`, and `oagw_route_plugin` child records in one atomic write - `inst-rm-del-3`
5. [x] - `p1` - **IF** the owning upstream is deleted instead - `inst-rm-del-4`
   1. [x] - `p1` - Remove the dependent routes through the documented `oagw_route` foreign-key cascade so no route survives with a dangling `upstream_id` - `inst-rm-del-4b`
6. [x] - `p1` - Treat the write as a configuration write that triggers Control Plane L1 invalidation and the Data Plane hot-config flush per `cpt-cf-oagw-adr-request-routing`, `cpt-cf-oagw-adr-data-plane-caching`, and `cpt-cf-oagw-adr-state-management`, through the write-side invalidation and flush surface of `cpt-cf-oagw-feature-observability-and-operability`, which owns the mechanism this feature only invokes - `inst-rm-del-7`
7. [x] - `p1` - **RETURN** success and leave no queryable trace of the deleted route for the calling tenant - `inst-rm-del-5`

### Enable and Disable a Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-management-enable-disable`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A route is disabled by writing `enabled: false` through a full-replacement `PUT`; the record stays readable and editable through the management API but is excluded from proxy-time route matching.
- A disabled route is re-enabled by the tenant that owns it, and the route becomes eligible for matching again.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the required route permission: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted.
- An enable or disable is attempted on a route owned by another tenant, an ancestor tenant, or a missing record: `404`.
- A re-enable whose re-checked match-rule uniqueness finds a collision: `409 Conflict`, and the record keeps its stored `enabled` value.

**Concurrency posture**: control-plane writes are serialized by the repository's write path (a single-process in-memory store), so the re-checked uniqueness inside the re-enable branch of step 6 and the persist of the new `enabled` value execute as one critical section and a detected collision leaves the stored record unchanged. Cross-instance concurrency control is out of scope per DESIGN §4.7 and is recorded as a limitation on the determinism invariant.

**Steps**:
1. [x] - `p1` - Receive a full-replacement write carrying the desired `enabled` value, because no dedicated status endpoint exists for routes - `inst-rm-enab-1`
2. [x] - `p1` - Authorize the caller's security context against the route permission set `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` of the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`), resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers - `inst-rm-enab-9`
3. [x] - `p1` - Resolve the route within the caller's tenant and reject with `404` when it is missing, foreign, or owned by an ancestor tenant - `inst-rm-enab-2`
4. [x] - `p1` - Store `enabled` as supplied, defaulting to `true` on create per `cpt-cf-oagw-fr-enable-disable`, and materialize the declared default `0` for a body that omits `priority` - `inst-rm-enab-3`
5. [x] - `p1` - **IF** the write disables the route - `inst-rm-enab-4`
   1. [x] - `p1` - Mark the record disabled so that proxy-time route selection, owned by `cpt-cf-oagw-feature-request-proxy`, excludes it - `inst-rm-enab-5`
6. [x] - `p1` - **IF** the write re-enables a disabled route owned by the calling tenant - `inst-rm-enab-6`
   1. [x] - `p1` - Mark the record enabled and re-check match-rule uniqueness, because a route disabled while a conflicting rule existed may collide on re-enable - `inst-rm-enab-7`
7. [x] - `p1` - **RETURN** the updated record with its new `enabled` value - `inst-rm-enab-8`

## 3. Processes / Business Logic (CDSL)

### Match Block Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-management-match-validation`

**Input**: the `match` object of a route payload, the route-level `priority` and `enabled` scalars of the same payload, and the published route schema shapes.
**Output**: an accepted match configuration with its implied match type and defaults materialized, or a validation error.

**Steps**:
1. [x] - `p1` - Require exactly one of `http` or `grpc` inside the `match` object and reject a block that carries neither, both, or any key outside the selected match definition - `inst-rm-mv-1`
2. [x] - `p1` - For an `http` match, require a non-empty `methods` list and a non-empty `path`, and reject a method outside `GET`, `POST`, `PUT`, `DELETE`, `PATCH` - `inst-rm-mv-2`
3. [x] - `p1` - For a `grpc` match, require a non-empty `service` and a non-empty `method`, and record the pair as configuration surface only, because no gRPC proxy code path exists in this decomposition (graded deviation 7) - `inst-rm-mv-3`
4. [x] - `p1` - Apply the `http` match defaults: `query_allowlist` empty, which permits no query parameter, and `path_suffix_mode` `append` - `inst-rm-mv-4`
5. [x] - `p1` - Reject a `path_suffix_mode` outside `disabled | append` - `inst-rm-mv-4b`
6. [x] - `p1` - Validate `priority` as an integer and materialize the declared default `0` when the field is omitted on write; `priority` is a schema-external API field of the payload contract declared in `cpt-cf-oagw-dod-route-management-schema-conformance`, a higher value wins at match time, and the matching execution that consumes it is owned by `cpt-cf-oagw-feature-request-proxy` - `inst-rm-mv-7b`
7. [x] - `p1` - Derive the match type from the selected block, which selects the documented match-key table `oagw_route_http_match` or `oagw_route_grpc_match` - `inst-rm-mv-5`
8. [x] - `p1` - Reject any key that is not part of the published shape of the selected match definition, so an unknown match key cannot silently change matching behavior - `inst-rm-mv-6`
9. [x] - `p1` - **RETURN** the validated match configuration, or the validation error naming the first offending key - `inst-rm-mv-7`

### Upstream Reference Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-management-upstream-reference`

**Input**: a route payload's `upstream_id`, the calling tenant identifier, and the upstream store delivered by `cpt-cf-oagw-feature-upstream-management`.
**Output**: the resolved upstream record, or a validation error.

**Steps**:
1. [x] - `p1` - Look the `upstream_id` up in the upstream store with the caller's tenant identifier bound to the lookup - `inst-rm-ur-1`
2. [x] - `p1` - **IF** no upstream is owned by the calling tenant under that identifier, including an ancestor tenant's upstream, which is not directly addressable through the management API - `inst-rm-ur-2`
   1. [x] - `p1` - Reject with a validation error that does not disclose whether the identifier belongs to an ancestor, a foreign tenant, or nothing - `inst-rm-ur-3`
3. [x] - `p1` - Record the resolved upstream as the route's owning target and keep the route and the upstream in the same tenant scope - `inst-rm-ur-4`
4. [x] - `p1` - Preserve the referenced upstream's `protocol` as configuration context: it is the key that selects the match strategy at request time per `cpt-cf-oagw-adr-request-routing`, and the request-time selection itself is owned by `cpt-cf-oagw-feature-request-proxy` - `inst-rm-ur-5`
5. [x] - `p1` - **RETURN** the resolved upstream, or the validation error - `inst-rm-ur-6`

### Match-Rule Uniqueness and Determinism

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-management-uniqueness`

**Input**: the candidate match keys of a route write (path, priority, and methods for `http`; service and method for `grpc`), the referenced `upstream_id`, the route identifier under replacement when one exists, and the set of routes already stored under that upstream.
**Output**: an accepted write, or a `409 Conflict`.

**Steps**:
1. [x] - `p1` - Collect the enabled routes stored under the same `upstream_id`, excluding the record under replacement - `inst-rm-uniq-1`
2. [x] - `p1` - Compare the candidate HTTP match keys against that set: the same path, the same priority, and a shared method constitute a collision - `inst-rm-uniq-2`
3. [x] - `p1` - **IF** a collision is found - `inst-rm-uniq-3`
   1. [x] - `p1` - Reject with `409 Conflict` naming the colliding keys and leave the store unchanged - `inst-rm-uniq-2b`
4. [x] - `p1` - Apply the same determinism guarantee to a `grpc` match block on its `(service, method)` keys, so the stored configuration stays deterministic even though no gRPC proxy code path consumes it (graded deviation 7) - `inst-rm-uniq-3c`
5. [x] - `p1` - Leave disabled routes out of the comparison, because the determinism invariant holds between enabled routes - `inst-rm-uniq-4`
6. [x] - `p1` - **IF** a disabled route is re-enabled and the re-check of step 2 or step 4 finds a collision - `inst-rm-uniq-5`
   1. [x] - `p1` - Reject the re-enable with `409 Conflict` so no ambiguous configuration is stored - `inst-rm-uniq-3b`
7. [x] - `p1` - Bound the comparison to the enabled routes stored under the referenced `upstream_id`, so the work of the comparison is bounded by that upstream's route count, and set no management-plane latency target, because the PRD latency requirement `cpt-cf-oagw-nfr-low-latency` is proxy-scoped and is owned by `cpt-cf-oagw-feature-request-proxy` (decomposition entry 2.4) - `inst-rm-uniq-6`
8. [x] - `p1` - **RETURN** acceptance, or the conflict - `inst-rm-uniq-4b`

### Route List Query

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-management-list-query`

**Input**: the OData query parameters of a list request and the calling tenant identifier.
**Output**: one page of route records, or a validation error.

**Steps**:
1. [x] - `p1` - Bind the query to the caller's tenant identifier before any filtering is applied - `inst-rm-lq-1`
2. [x] - `p1` - Apply `$filter` over route fields, with `upstream_id eq '{uuid}'` as the documented selection of one upstream's routes - `inst-rm-lq-2`
3. [x] - `p1` - Apply `$select` to project the requested fields, `$orderby` to sort, and `$skip` to offset - `inst-rm-lq-3`
4. [x] - `p1` - Apply `$top` with a default of 50 and a maximum of 100, rejecting a value above the maximum or below 1 rather than silently clamping it - `inst-rm-lq-4`
5. [x] - `p1` - **IF** a parameter is unsupported, malformed, or names a field the route model does not carry - `inst-rm-lq-5`
   1. [x] - `p1` - Reject with a validation error naming the parameter - `inst-rm-lq-6`
6. [x] - `p1` - **RETURN** the page of routes with no ancestor-tenant record included - `inst-rm-lq-7`

## 4. States (CDSL)

### Route State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-route-management-route-lifecycle`

**States**: `absent`, `enabled`, `disabled`, `removed`
**Initial State**: `absent`

A route in the `disabled` state keeps its configuration and stays addressable through the management API; it is only excluded from proxy-time matching, which is owned by `cpt-cf-oagw-feature-request-proxy`. A route whose owning upstream is disabled keeps its own `enabled` value and is unreachable because of the upstream, not because of the route.

**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `enabled` **WHEN** a create is accepted and the record is stored with the default or explicit `enabled: true` - `inst-rm-st-1`
2. [x] - `p1` - **FROM** `absent` **TO** `disabled` **WHEN** a create is accepted with an explicit `enabled: false` - `inst-rm-st-2`
3. [x] - `p1` - **FROM** `enabled` **TO** `disabled` **WHEN** a full replacement writes `enabled: false` for a route owned by the calling tenant - `inst-rm-st-3`
4. [x] - `p1` - **FROM** `disabled` **TO** `enabled` **WHEN** a full replacement writes `enabled: true` for a route owned by the calling tenant and the re-checked match-rule uniqueness finds no collision - `inst-rm-st-4`
5. [x] - `p1` - **FROM** `enabled` **TO** `enabled` **WHEN** a full replacement or create re-validates the record without changing its `enabled` value - `inst-rm-st-5`
6. [x] - `p1` - **FROM** `enabled` **TO** `removed` **WHEN** a delete is accepted for a route owned by the calling tenant - `inst-rm-st-6`
7. [x] - `p1` - **FROM** `disabled` **TO** `removed` **WHEN** a delete is accepted for a disabled route owned by the calling tenant - `inst-rm-st-7`
8. [x] - `p1` - **FROM** `removed` **TO** `absent` **WHEN** the in-memory entry is released together with its match, method, tag, and plugin-binding child records - `inst-rm-st-8`
9. [x] - `p1` - **FROM** `enabled`/`disabled` **TO** `removed` **WHEN** the owning upstream is deleted and the `oagw_route` foreign-key cascade of `cpt-cf-oagw-db-schema` removes the route - `inst-rm-st-9`

**Invalid transitions**: no transition leads out of `removed` except to `absent`, which is the release of the in-memory entry; no create, replace, enable, or disable write revives a `removed` record, because the record is gone and a write addressed at it has no stored route to operate on. A `removed` record is not addressable: reads and writes addressed at one behave as not-found, indistinguishable from a record that never existed, so a removed route returns `404` on read, replace, and delete exactly as a missing or foreign one does.

## 5. Definitions of Done

### Route CRUD Lifecycle

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-route-crud`

The system **MUST** implement the route CRUD lifecycle at the gear-relative path `/oagw/v1/routes`: `POST` to create, `GET` to list, `GET` by id, `PUT` for full replacement, and `DELETE` to remove, with every operation dispatched to the Control Plane route operations per `cpt-cf-oagw-adr-request-routing`. Identifiers **MUST** be server-generated in the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}`, `id` and `tenant_id` **MUST** be immutable, omitted optional fields **MUST** be cleared on replacement with the declared defaults materialized on the cleared fields per DESIGN §3.3 (`cpt-cf-oagw-interface-api`), and no operation **MUST** carry a leading `/api` segment in the graded configuration (graded deviation 1). Every accepted route create, replace, and delete **MUST** emit the structured audit event of DESIGN §4.3 — the fields `event`, `tenant_id`, `principal_id`, the route resource identifier, and the outcome — attributed to the caller's security context, with `cpt-cf-oagw-feature-observability-and-operability` as the emitting owner that this feature supplies the operation outcome to; this feature **MUST NOT** implement the audit emitter.

**Implements**:
- `cpt-cf-oagw-flow-route-management-create-route`
- `cpt-cf-oagw-flow-route-management-list-routes`
- `cpt-cf-oagw-flow-route-management-replace-route`
- `cpt-cf-oagw-flow-route-management-delete-route`
- `cpt-cf-oagw-state-route-management-route-lifecycle`

**Touches**:
- API: `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`, `MatchConfig`
- Tests: unit tests in the REST handler sibling test module for create, list, read, replace, and delete outcomes, plus integration tests in `tests/route_crud.rs` driving the five operations end to end and asserting the emitted audit field set (`event`, `tenant_id`, `principal_id`, resource id, outcome) for one create, one replace, and one delete

### Match Block Exclusivity and Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-match-block`

The system **MUST** accept a route whose `match` block carries exactly one of `http` or `grpc` and **MUST** reject a block that carries neither, both, or any key outside the selected match definition. An `http` match **MUST** carry a non-empty `methods` list drawn from `GET`, `POST`, `PUT`, `DELETE`, `PATCH` and a non-empty `path`; a `grpc` match **MUST** carry a non-empty `service` and a non-empty `method`. The `grpc` variant **MUST** be accepted and stored as configuration and schema surface only: no gRPC proxy code path is implemented or reachable in this decomposition (graded deviation 7), and the request-time choice of match strategy by `upstream.protocol` per `cpt-cf-oagw-adr-request-routing` is owned by `cpt-cf-oagw-feature-request-proxy`.

**Implements**:
- `cpt-cf-oagw-algo-route-management-match-validation`
- `cpt-cf-oagw-flow-route-management-create-route`
- `cpt-cf-oagw-flow-route-management-replace-route`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `Route`, `MatchConfig`
- Tests: unit tests in the domain sibling test module for the exactly-one rule, the `http` method and path requirements, the `grpc` service and method requirements, and unknown-key rejection

### Path Suffix and Query Allowlist Configuration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-path-and-query`

The system **MUST** accept `query_allowlist` and `path_suffix_mode` on an `http` match block and **MUST** materialize the published defaults: an empty `query_allowlist`, which permits no query parameter, and `path_suffix_mode` `append`. The system **MUST** reject a `path_suffix_mode` outside `disabled | append` and **MUST** store the two keys as configuration that the proxy path consumes: rejecting a supplied path suffix under `disabled`, appending it to the matched path under `append`, and rejecting a query parameter that is not allowlisted are enforcement behaviors owned by `cpt-cf-oagw-feature-request-proxy` and are not executed by this feature.

**Implements**:
- `cpt-cf-oagw-algo-route-management-match-validation`
- `cpt-cf-oagw-dod-route-management-match-block`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `MatchConfig`
- Tests: unit tests for the default materialization of both keys, for the `disabled | append` enum, and for an empty `query_allowlist` persisting as "allow none"

### Upstream Reference Validation and Immutability

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-upstream-reference`

The system **MUST** resolve a route's `upstream_id` to an existing upstream owned by the calling tenant before a route is created or replaced, so that a route can never reference a missing, foreign-tenant, or ancestor-tenant upstream, and a rejected reference **MUST NOT** disclose which of the three cases applies. Route `upstream_id` **MUST** be immutable across `PUT` and **MUST NOT** be part of the update DTO per DESIGN §3.3 (`cpt-cf-oagw-interface-api`): the stored value is retained, and a replacement body that supplies `upstream_id` **MUST** be rejected as an immutable-field violation with `400` regardless of the supplied value, matching or not. When the owning upstream is deleted, the dependent routes **MUST** be removed through the documented `oagw_route` foreign-key cascade recorded in `cpt-cf-oagw-db-schema`, so no dangling route survives.

**Implements**:
- `cpt-cf-oagw-algo-route-management-upstream-reference`
- `cpt-cf-oagw-flow-route-management-create-route`
- `cpt-cf-oagw-flow-route-management-replace-route`
- `cpt-cf-oagw-flow-route-management-delete-route`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`, `Upstream`
- Tests: unit tests for the foreign-key check and the immutable `upstream_id`, plus integration tests in `tests/route_upstream_reference.rs` covering an ancestor-tenant upstream, a foreign-tenant upstream, and the cascade on upstream deletion

### Match-Rule Uniqueness and Enabled Exclusion

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-uniqueness`

The system **MUST** reject with `409 Conflict` a route write whose match keys collide with an existing enabled route under the same upstream on the same path, the same priority, and a shared method, re-validating the check on replacement while excluding the record under replacement, and **MUST** apply the same determinism guarantee to a `grpc` match block on its `(service, method)` keys. A route stored with `enabled: false` **MUST** remain addressable through the management API while being excluded from proxy-time matching, the exclusion being executed by `cpt-cf-oagw-feature-request-proxy`; a disabled route **MUST NOT** participate in the uniqueness comparison, and re-enabling one that would collide **MUST** be rejected with `409 Conflict`. The uniqueness check and the persist of the accepted write **MUST** execute as one critical section, because control-plane writes are serialized by the repository's write path (a single-process in-memory store), so a detected collision **MUST** leave the store unchanged; cross-instance concurrency control is out of scope per DESIGN §4.7 and is recorded as a limitation on this determinism invariant.

**Implements**:
- `cpt-cf-oagw-algo-route-management-uniqueness`
- `cpt-cf-oagw-flow-route-management-enable-disable`
- `cpt-cf-oagw-flow-route-management-create-route`
- `cpt-cf-oagw-flow-route-management-replace-route`
- `cpt-cf-oagw-state-route-management-route-lifecycle`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `Route`, `MatchConfig`
- Tests: unit tests for the `409` collision on path plus priority plus method, for the exclusion of the record under replacement, for the re-enable collision, and for the materialization of the `priority` default `0`, plus integration tests in `tests/route_uniqueness.rs`

### Route-Level Overrides and Plugin Bindings

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-route-management-route-overrides`

The system **MUST** accept the route-level `rate_limit`, `cors`, `plugins`, and `tags` fields and store them so that the merge engine delivered by `cpt-cf-oagw-feature-gear-foundation` can merge them: rate limits by `min(ancestor, descendant)`, tags by add-only union, CORS origins by union under `inherit`, and plugin chains by concatenation with upstream bindings executing before route bindings. Plugin bindings **MUST** be stored as ordered `plugins.items[]` references with positions contiguous from zero, `plugin_ref` always stored, and `plugin_uuid` present only when the reference is UUID-backed. This feature **MUST NOT** resolve or execute any plugin and **MUST NOT** implement the resolvability logic: its write path **MUST** invoke the plugin-catalog binding-time resolvability check through the plugin registry boundary delivered by `cpt-cf-oagw-feature-gear-foundation`, the check's logic being owned by `cpt-cf-oagw-feature-plugin-system` and executed, not implemented, by this feature's write path. A catalog-only `plugin_ref` — for example the `cors.v1`, `timeout.v1`, or `basic.v1` catalog-only identifiers `cpt-cf-oagw-feature-plugin-system` registers — **MUST** be rejected at binding time with `400`. No interim unresolved-binding storage state exists, because every entry of this decomposition is delivered in the same crate.

**Implements**:
- `cpt-cf-oagw-flow-route-management-create-route`
- `cpt-cf-oagw-flow-route-management-replace-route`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `Route`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`
- Tests: unit tests for the stored route-level fields, for binding positions contiguous from zero, for `plugin_ref` and `plugin_uuid` agreement, and for the binding-time rejection of a catalog-only `plugin_ref` with `400`

### Route List Query Parameters

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-list-query`

The system **MUST** support the OData list query parameters `$filter`, `$select`, `$orderby`, `$top`, and `$skip` on `GET /oagw/v1/routes`, with `$filter` able to select the routes of one upstream, `$top` defaulting to 50 and capped at 100, and every parameter applied inside the caller's tenant scope. An unsupported, malformed, or out-of-range parameter **MUST** be rejected with a validation error naming the parameter rather than silently ignored.

**Implements**:
- `cpt-cf-oagw-algo-route-management-list-query`
- `cpt-cf-oagw-flow-route-management-list-routes`

**Touches**:
- API: `GET /oagw/v1/routes`
- Entities: `Route`
- Tests: unit tests for `$top` rejecting out-of-range values, `$filter` by `upstream_id`, `$orderby`, and `$skip`, plus integration tests in `tests/route_list_query.rs`

### Strict Tenant Scoping on Route Records

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-tenant-scoping`

The system **MUST** scope every route read and write to the calling tenant through the repository boundary delivered by `cpt-cf-oagw-feature-gear-foundation`, so that a route owned by another tenant, including an ancestor tenant, is indistinguishable from a missing record and returns `404` on read, replace, and delete, and so that no list response ever contains an ancestor-tenant route. Ancestor routes are inherited only at proxy time by the tenant-chain walk owned by `cpt-cf-oagw-feature-request-proxy` and are never visible or modifiable through this API.

**Implements**:
- `cpt-cf-oagw-flow-route-management-list-routes`
- `cpt-cf-oagw-flow-route-management-replace-route`
- `cpt-cf-oagw-flow-route-management-delete-route`
- `cpt-cf-oagw-flow-route-management-enable-disable`

**Touches**:
- API: `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`
- Tests: integration tests in `tests/route_tenant_scope.rs` asserting zero cross-tenant disclosure for read, list, replace, and delete

### Route Schema Conformance and Documented Schema Contract

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-route-management-schema-conformance`

The system **MUST** validate route payloads against the shapes of `docs/schemas/route.v1.schema.json`: required `upstream_id` and `match`, the `http_match` definition requiring `methods` and `path`, the `grpc_match` definition requiring `service` and `method`, both match definitions closed to unknown keys, and the nested `plugins` and `rate_limit` structures with their declared enums and defaults, including the `rate_limit` requirement of a `sustained` block. Payload validation **MUST** be performed against the published route schema shapes **plus** a named, closed set of schema-external API fields — `priority`, `enabled`, and `cors` — and "a field outside the published route schema shape" **MUST** be read as a field outside that union, so the payload contract is closed: any field outside it is rejected with `400`. The reason is that the route domain type of `cpt-cf-oagw-design-domain-model` is a superset of the published schema document, which this entry does not edit; the fields that superset adds on the write path are exactly those three, and the remaining fields it carries (`id`, `tenant_id`, `match_type`) are server-assigned or derived rather than accepted. The route domain type **MUST** additionally carry `priority` and `enabled` per `cpt-cf-oagw-design-domain-model` and `cpt-cf-oagw-fr-enable-disable`, which the published schema document does not enumerate: `priority` **MUST** be an integer that materializes the declared default `0` when it is omitted on write, with a higher value winning at match time and two routes under one upstream that share path, method, and `priority` colliding under `cpt-cf-oagw-dod-route-management-uniqueness`, and `enabled` **MUST** materialize the declared default `true`. The repository **MUST** preserve the documented table shapes `oagw_route` (primary key `id`, foreign key `upstream_id` with cascade), `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method` (primary key `(route_id, method)`), `oagw_route_tag`, and `oagw_route_plugin` as the schema contract materialized by the in-memory, config-backed store (graded deviation 5).

**Implements**:
- `cpt-cf-oagw-algo-route-management-match-validation`
- `cpt-cf-oagw-state-route-management-route-lifecycle`

**Touches**:
- API: none
- Entities: `Route`, `MatchConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`
- Tests: unit tests round-tripping a route against `docs/schemas/route.v1.schema.json`, unit tests for the closed set of schema-external fields and for the `priority` default `0` and the `enabled` default `true`, plus integration tests in `tests/route_schema_contract.rs` asserting the documented key and cascade shapes

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering route CRUD outcomes, match-block validation for both the `http` and `grpc` variants, `query_allowlist` and `path_suffix_mode` defaults, the upstream foreign-key check and the immutable `upstream_id`, match-rule uniqueness conflicts and the enabled exclusion, the OData list parameters, and the route lifecycle transitions, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4.

**Implements**:
- `cpt-cf-oagw-algo-route-management-match-validation`
- `cpt-cf-oagw-algo-route-management-upstream-reference`
- `cpt-cf-oagw-algo-route-management-uniqueness`
- `cpt-cf-oagw-algo-route-management-list-query`
- `cpt-cf-oagw-dod-route-management-path-and-query`

**Touches**:
- API: none
- Entities: `Route`, `MatchConfig`
- Tests: sibling `*_tests.rs` modules of the route handler, route service, and route storage units

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-management-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory that drive route CRUD over the gear-relative REST surface against real upstream records created through `cpt-cf-oagw-feature-upstream-management`, covering a created route becoming readable, a `409` uniqueness conflict, a rejected foreign and ancestor `upstream_id`, a `PUT` that supplies `upstream_id` rejected with `400` as an immutable-field violation, an OData-filtered list, tenant scoping, a request carrying a valid security context that lacks the required route permission returning `403` with the shared canonical permission-denied error surface of `cpt-cf-oagw-feature-error-handling`, and the removal of dependent routes when the owning upstream is deleted, and **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-route-management-route-crud`
- `cpt-cf-oagw-dod-route-management-upstream-reference`
- `cpt-cf-oagw-dod-route-management-uniqueness`
- `cpt-cf-oagw-dod-route-management-tenant-scoping`

**Touches**:
- API: `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- Entities: `Route`, `Upstream`
- Tests: `tests/route_crud.rs` (including the denied-permission `403` outcome), `tests/route_upstream_reference.rs`, `tests/route_uniqueness.rs`, `tests/route_list_query.rs`, `tests/route_tenant_scope.rs`, `tests/route_schema_contract.rs`

## 6. Acceptance Criteria

- [x] A route referencing an upstream owned by the calling tenant is created, receives a server-generated identifier in the form `gts.cf.core.oagw.route.v1~{uuid}`, and is returned by a subsequent `GET /oagw/v1/routes/{id}` (DoD `cpt-cf-oagw-dod-route-management-route-crud`).
- [x] A route payload whose `match` block carries neither `http` nor `grpc`, carries both, or carries an unknown match key is rejected with `400`, and a valid `http` block requires a non-empty method list from `GET`, `POST`, `PUT`, `DELETE`, `PATCH` plus a non-empty path (DoD `cpt-cf-oagw-dod-route-management-match-block`).
- [x] A `grpc` match block carrying `service` and `method` is accepted and stored, and no gRPC proxy code path exists behind it (DoD `cpt-cf-oagw-dod-route-management-match-block`).
- [x] An `http` match block with omitted `query_allowlist` and `path_suffix_mode` persists an empty allowlist and `append`, and a `path_suffix_mode` outside `disabled | append` is rejected (DoD `cpt-cf-oagw-dod-route-management-path-and-query`).
- [x] A route whose `upstream_id` resolves to nothing, to another tenant's upstream, or to an ancestor tenant's upstream is rejected with `400` and no information about the foreign record is disclosed (DoD `cpt-cf-oagw-dod-route-management-upstream-reference`).
- [x] `upstream_id` is not part of the route update DTO: a `PUT` that supplies `upstream_id` is rejected with `400` as an immutable-field violation regardless of the supplied value, and the route remains bound to its original upstream (DoD `cpt-cf-oagw-dod-route-management-upstream-reference`).
- [x] Deleting an upstream removes its routes, and no route with a dangling `upstream_id` remains queryable (DoD `cpt-cf-oagw-dod-route-management-upstream-reference`).
- [x] A second enabled route under the same upstream with the same path, priority, and a shared method is rejected with `409 Conflict`, and the same check applied to a `grpc` block rejects a duplicate `(service, method)` pair (DoD `cpt-cf-oagw-dod-route-management-uniqueness`).
- [x] A route written without `priority` is stored with `0`, a non-integer `priority` is rejected with `400`, and a replacement body that omits `enabled` re-enables the route so a disabled route is kept disabled only by sending `enabled: false` (DoD `cpt-cf-oagw-dod-route-management-schema-conformance`, `cpt-cf-oagw-dod-route-management-route-crud`).
- [x] A route stored with `enabled: false` remains readable and replaceable through the management API and is excluded from proxy-time matching, and re-enabling it is rejected when the re-checked uniqueness finds a collision (DoD `cpt-cf-oagw-dod-route-management-uniqueness`, `cpt-cf-oagw-dod-route-management-route-crud`).
- [x] Route-level `rate_limit`, `cors`, `plugins`, and `tags` are stored with binding positions contiguous from zero and `plugin_ref` always present; the merge behavior itself is asserted by the foundation merge-engine DoD (DoD `cpt-cf-oagw-dod-route-management-route-overrides`, `cpt-cf-oagw-dod-gear-foundation-merge-engine`).
- [x] `GET /oagw/v1/routes` honors `$filter`, `$select`, `$orderby`, `$top` (default 50, max 100), and `$skip` inside the caller's tenant scope, and an unsupported or out-of-range parameter is rejected with `400` (DoD `cpt-cf-oagw-dod-route-management-list-query`).
- [x] A route owned by another tenant, including an ancestor tenant, returns `404` on read, replace, delete, and enable, and never appears in a list response (DoD `cpt-cf-oagw-dod-route-management-tenant-scoping`).
- [x] A request carrying no security context or an invalid one returns `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, and a valid security context that lacks the required route permission returns `403` through the shared canonical permission-denied error surface, with no new OAGW error type minted (DoD `cpt-cf-oagw-dod-route-management-integration-tests`, `cpt-cf-oagw-dod-route-management-route-crud`).
- [x] Route payloads validate against `docs/schemas/route.v1.schema.json` plus the closed set of schema-external API fields `priority`, `enabled`, and `cors`, any field outside that union is rejected with `400`, and the documented route table shapes are preserved by the in-memory, config-backed store (DoD `cpt-cf-oagw-dod-route-management-schema-conformance`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-route-management-unit-tests`, `cpt-cf-oagw-dod-route-management-integration-tests`).
