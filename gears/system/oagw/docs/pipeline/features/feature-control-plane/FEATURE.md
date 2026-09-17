# Feature: OAGW Control Plane


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Manage Upstreams](#manage-upstreams)
  - [Manage Routes](#manage-routes)
  - [Manage Plugins](#manage-plugins)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Effective Configuration Resolution](#effective-configuration-resolution)
  - [JSON-Schema Validation](#json-schema-validation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream and Route Enablement State Machine](#upstream-and-route-enablement-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Control-Plane Persistence](#control-plane-persistence)
  - [Upstream CRUD](#upstream-crud)
  - [Route CRUD](#route-crud)
  - [Plugin CRUD and Binding](#plugin-crud-and-binding)
  - [Configuration Hierarchy](#configuration-hierarchy)
  - [Cache Invalidation](#cache-invalidation)
  - [Control-Plane Test Harness](#control-plane-test-harness)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p2` - **ID**: `cpt-cf-oagw-featstatus-control-plane`

- [x] `p2` - `cpt-cf-oagw-feature-control-plane`
## 1. Feature Context

### 1.1 Overview

The control plane is the authoritative management surface of the gear: CRUD over the `oagw_upstream`, `oagw_route`, and `oagw_plugin` control-plane tables behind `/oagw/v1/upstreams`, `/oagw/v1/routes`, and `/oagw/v1/plugins`, with authoritative JSON-Schema request validation, tenant scoping and authz enforcement, alias rules with their 400/409/404 outcomes, the plugin-instance binding rules that produce the 409 `PluginInUse` outcome, the hierarchy semantics (private/inherit/enforce with stricter-wins rate merging, plugin concatenation, CORS union, tags add-only union) resolved by the effective-configuration process, and the L1 config cache population plus invalidation (writes flush the data-plane L1 and concurrent readers observe a consistent lineage).

### 1.2 Purpose

The routing matrix, plugin runtime, and rate-limit semantics of the authoritative ADRs 0001-0009 presuppose a management surface that persists operators' intended configuration as relational rows and resolves them into the effective configuration the data plane executes. Without this feature every write to the management API is an unhandled path, and the data plane would have no authoritative inputs to enforce. This feature delivers the persist-and-consult contract: validated writes land in `oagw_*` tables; reads resolve the effective configuration through the documented hierarchy; caches are kept consistent so operators' changes take effect bounded and no write is silently lost. It also owns the control-plane share of caching per ADR 0005/0006 (CP L1 at capacity 10 000 with invalidation-driven refresh) and of state ownership per ADR 0006.

**Requirements**: `cpt-cf-oagw-fr-control-plane-crud`, `cpt-cf-oagw-fr-config-hierarchy`, `cpt-cf-oagw-fr-config-caching`, `cpt-cf-oagw-usecase-manage-upstream`

**Principles**: None — `cpt-cf-oagw-principle-cp-authoritative-dp-bounded` is owned by feature-data-plane.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-gateway-operator` | The sole management-plane caller: submits upstream, route, and plugin write/read requests; is answered with authoritative schema, alias, binding, and hierarchy semantics |
| `cpt-cf-oagw-actor-authz-resolver` | Receives the authz decisions on every management operation and on the shared config-tree inspection (management writes are gated before any persistence occurs) |
| `cpt-cf-oagw-actor-tenant-resolver` | Maps the authenticated caller to a tenant scope that scopes every CRUD operation to that tenant's own rows |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADR**: [0004-cors.md](../ADR/0004-cors.md), [0005-data-plane-caching.md](../ADR/0005-data-plane-caching.md), [0006-state-management.md](../ADR/0006-state-management.md), [0007-error-source-distinction.md](../ADR/0007-error-source-distinction.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` (registration, config model, lifecycle, route mounting)
- **Interfaces**: `cpt-cf-oagw-interface-oagw-rest-surface`, `cpt-cf-oagw-interface-management-api` (reference, host-composed as `/api/oagw/v1/...`), `cpt-cf-oagw-interface-proxy-api` (reference)
- **Components**: `cpt-cf-oagw-component-control-plane`, `cpt-cf-oagw-component-rest-surface`
- **Sequences**: `cpt-cf-oagw-seq-management-write`
- **Schemas**: `route.v1.schema.json`, `upstream.v1.schema.json`

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-manage-upstream`

### Manage Upstreams

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-control-plane-manage-upstreams`

**Actor**: `cpt-cf-oagw-actor-gateway-operator`

**Success Scenarios**:
- An operator creates, reads, updates, lists, and deletes upstreams for their tenant; each write lands in `oagw_upstream`, refreshes the effective configuration for affected routes, and invalidates the CP and DP L1 caches.
- Upstream aliases satisfy the naming rules; shadowing and use-in-route rules produce the documented 400/409/404 outcomes.

**Error Scenarios**:
- Creating an upstream whose alias shadows an existing managed alias returns 400.
- Deleting an upstream still referenced by a route or violating ownership returns 409 or 404 as documented by the alias/binding rules.

**Steps**:
1. [x] - `p1` - Operator submits `API: (POST|GET|PUT|DELETE) /oagw/v1/upstreams[/{alias}]` with a tenant-scoped auth context - `inst-upstream-request`
2. [x] - `p1` - Authz resolver evaluates the management operation (gated before any persistence) - `inst-authz-gate`
3. [x] - `p1` - **IF** the caller lacks the required permission - `inst-authz-forbidden`
   1. [x] - `p1` - **TRY** authoring the authorized management error response - `inst-authz-error`
   2. [x] - `p1` - **CATCH** internal error during error construction - `inst-authz-error-fallback`
   3. [x] - `p1` - **RETURN** the error response to the operator - `inst-authz-return`
4. [x] - `p1` - **ELSE** proceed - `inst-authz-allowed`
5. [x] - `p1` - Tenant resolver maps the caller to the tenant scope - `inst-tenant-scope`
6. [x] - `p1` - Validate the request body against `upstream.v1.schema.json` - `inst-schema-validate`
7. [x] - `p1` - **IF** the request body is not schema-valid - `inst-schema-invalid`
   1. [x] - `p1` - **RETURN** 400 with the violation details - `inst-schema-400`
8. [x] - `p1` - **ELSE** proceed - `inst-schema-valid`
9. [x] - `p1` - Apply the alias rules; resolve write outcomes - `inst-alias-rules`
10. [x] - `p1` - Persist the row in `oagw_upstream` (create/update) or remove it (delete) - `inst-persist-upstream`
11. [x] - `p1` - **IF** the write conflicts with a route/plugin binding or ownership rule - `inst-binding-conflict`
    1. [x] - `p1` - **RETURN** 409 (or 404 for missing rows) per the binding rules - `inst-binding-409`
12. [x] - `p1` - **ELSE** the write succeeds - `inst-write-ok`
    1. [x] - `p1` - Refresh the effective configuration for affected routes and invalidate the CP and DP L1 caches - `inst-invalidate`
    2. [x] - `p1` - **RETURN** the authoritative result (entity, 204 for delete) to the operator - `inst-return-ok`

### Manage Routes

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-control-plane-manage-routes`

**Actor**: `cpt-cf-oagw-actor-gateway-operator`

**Success Scenarios**:
- An operator creates, reads, updates, lists, and deletes routes; each write lands in `oagw_route` and `oagw_route_http_match`, resolves hierarchy inputs, and invalidates the CP and DP L1 caches.
- Disabled upstreams referenced by a route produce the documented 409 outcome on write.

**Error Scenarios**:
- A route references an unknown alias or an upstream bound in a conflicting way, producing a documented 400/409/404; the control plane never persists an inconsistent route.

**Steps**:
1. [x] - `p1` - Operator submits `API: (POST|GET|PUT|DELETE) /oagw/v1/routes[/{id}]` with a tenant-scoped auth context - `inst-route-request`
2. [x] - `p1` - Authz resolver evaluates the management operation - `inst-route-authz`
3. [x] - `p1` - **IF** unauthorized - `inst-route-forbidden`
   1. [x] - `p1` - **RETURN** the authorized management error response - `inst-route-forbidden-return`
4. [x] - `p1` - **ELSE** proceed - `inst-route-allowed`
5. [x] - `p1` - Map caller to the tenant scope - `inst-route-tenant`
6. [x] - `p1` - Validate against `route.v1.schema.json` (including the `http.match` block into `oagw_route_http_match`) - `inst-route-schema`
7. [x] - `p1` - **IF** schema-invalid - `inst-route-schema-invalid`
   1. [x] - `p1` - **RETURN** 400 with violation details - `inst-route-400`
8. [x] - `p1` - **ELSE** proceed - `inst-route-schema-valid`
9. [x] - `p1` - Validate referenced upstreams (existence, enabled state, ownership) - `inst-route-upstream-check`
10. [x] - `p1` - **IF** a referenced upstream is missing or disabled - `inst-route-disabled-upstream`
    1. [x] - `p1` - **RETURN** 409 (disabled) or 404/400 (missing/unknown per the rules) - `inst-route-409`
11. [x] - `p1` - **ELSE** proceed - `inst-route-valid-upstreams`
12. [x] - `p1` - Persist the route rows (`oagw_route` and its match rows) - `inst-persist-route`
13. [x] - `p1` - Refresh the effective configuration and invalidate the CP and DP L1 caches - `inst-route-invalidate`
14. [x] - `p1` - **RETURN** the authoritative result to the operator - `inst-route-return-ok`

### Manage Plugins

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-control-plane-manage-plugins`

**Actor**: `cpt-cf-oagw-actor-gateway-operator`

**Success Scenarios**:
- An operator registers a standalone plugin instance, binds it to an upstream or route, unbinds it, and removes it; writes land in `oagw_plugin`, `oagw_upstream_plugin`, and `oagw_route_plugin` with the documented bind-once semantics.
- Bindings concatenate in documented order during effective-configuration resolution; rebinding updates binding tables without orphaning instances.

**Error Scenarios**:
- Binding an already-bound plugin instance returns 409 `PluginInUse`; removing a bound instance returns 409; the control plane never leaves an orphaned binding.

**Steps**:
1. [x] - `p1` - Operator submits `API: (POST|GET|PUT|DELETE) /oagw/v1/plugins[/{id}]` (instance CRUD) or a bind/unbind operation linking a plugin to an upstream/route - `inst-plugin-request`
2. [x] - `p1` - Authz resolver evaluates the management operation - `inst-plugin-authz`
3. [x] - `p1` - **IF** unauthorized - `inst-plugin-forbidden`
   1. [x] - `p1` - **RETURN** the authorized management error response - `inst-plugin-forbidden-return`
4. [x] - `p1` - **ELSE** proceed - `inst-plugin-allowed`
5. [x] - `p1` - Map caller to the tenant scope - `inst-plugin-tenant`
6. [x] - `p1` - Validate the instance config against the registered plugin type schema - `inst-plugin-schema`
7. [x] - `p1` - **IF** schema-invalid - `inst-plugin-schema-invalid`
   1. [x] - `p1` - **RETURN** 400 with violation details - `inst-plugin-400`
8. [x] - `p1` - **ELSE** proceed - `inst-plugin-schema-valid`
9. [x] - `p1` - **IF** the operation is a bind - `inst-plugin-bind`
   1. [x] - `p1` - **IF** the instance or target does not exist or the instance is already bound elsewhere - `inst-plugin-in-use`
      1. [x] - `p1` - **RETURN** 404 (missing) or 409 `PluginInUse` (already bound) - `inst-plugin-409`
   2. [x] - `p1` - **ELSE** insert a binding row in `oagw_upstream_plugin` or `oagw_route_plugin` - `inst-plugin-bind-row`
10. [x] - `p1` - **ELSE IF** the operation removes a binding or instance - `inst-plugin-unbind`
    1. [x] - `p1` - **IF** the instance is still bound elsewhere - `inst-plugin-still-bound`
       1. [x] - `p1` - **RETURN** 409 - `inst-plugin-unbind-409`
    2. [x] - `p1` - **ELSE** delete the binding or instance row - `inst-plugin-delete-row`
11. [x] - `p1` - Invalidate the CP and DP L1 caches and refresh affected effective configurations - `inst-plugin-invalidate`
12. [x] - `p1` - **RETURN** the authoritative result to the operator - `inst-plugin-return-ok`

## 3. Processes / Business Logic (CDSL)

### Effective Configuration Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-control-plane-effective-config-resolution`

**Input**: tenant config tree (system defaults as low-priority root, tenant override, per-upstream/route branches), the operator's CRUD lineage, cache state

**Output**: the effective configuration consumed by the data plane (merged hierarchy with stricter-wins rate limits, concatenated plugins, CORS union, tags add-only union, per-branch enablement)

**Steps**:
1. [x] - `p1` - Load the tenant config tree from the `oagw_config`/hierarchy tables; treat system defaults as root, tenant override above it, per-route/upstream branches at their depth - `inst-load-tree`
2. [x] - `p1` - Merge in hierarchy order honoring private (branch-local), inherit (take nearest ancestor), and enforce (stricter-wins override) semantics per config key - `inst-merge-hierarchy`
3. [x] - `p1` - Merge rate limits: where both ancestor and branch set a limit, the stricter value wins - `inst-merge-rate`
4. [x] - `p1` - Concatenate plugins in documented binding order (no silent reordering) - `inst-plugin-concat`
5. [x] - `p1` - Union CORS allow lists and combine tags add-only - `inst-union`
6. [x] - `p1` - Resolve each configured upstream alias, honoring shadowing rules, to its live target set for load balancing - `inst-resolve-alias`
7. [x] - `p1` - **IF** an ancestor disables a branch - `inst-ancestor-disabled`
   1. [x] - `p1` - Propagate the disabled state downward (descendants are not routable) - `inst-disable-prop`
8. [x] - `p1` - **ELSE** the branch remains eligible - `inst-branch-eligible`
9. [x] - `p1` - Project the resolved configuration into the data-plane shape and store it so the DP can consume it - `inst-project`
10. [x] - `p1` - **RETURN** the effective configuration - `inst-return-effective`

### JSON-Schema Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-control-plane-json-schema-validation`

**Input**: an inbound management request body (upstream, route, plugin instance or its schema-declared config)

**Output**: validated request ready for persistence, or a 400 containing the violation details

**Steps**:
1. [x] - `p1` - Determine the governing schema: `upstream.v1.schema.json`, `route.v1.schema.json`, or the plugin type's registered config schema - `inst-governing-schema`
2. [x] - `p1` - Validate the body against its schema (type, required fields, shapes, enums) - `inst-validate-body`
3. [x] - `p1` - **IF** an additional property or a violation is present - `inst-violation`
   1. [x] - `p1` - **RETURN** 400 with violation details and a hint to consult the schema - `inst-return-400`
4. [x] - `p1` - **ELSE** accept the body - `inst-accept`
   1. [x] - `p1` - **RETURN** the validated body for persistence - `inst-return-valid-body`

## 4. States (CDSL)

### Upstream and Route Enablement State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-control-plane-enablement`

**States**: `Enabled`, `Disabled`, `Ancestor-Disabled`

**Initial State**: `Enabled` on create

**Transitions**:
1. [x] - `p1` - **FROM** Enabled **TO** Disabled **WHEN** an operator sets the entity's enabled flag to false - `inst-disable`
2. [x] - `p1` - **FROM** Disabled **TO** Enabled **WHEN** an operator sets the enabled flag back to true - `inst-enable`
3. [x] - `p1` - **FROM** Enabled **TO** Ancestor-Disabled **WHEN** an ancestor branch in the config hierarchy disables (enforce semantics propagate) - `inst-ancestor-disable`
4. [x] - `p1` - **FROM** Ancestor-Disabled **TO** Enabled **WHEN** the ancestor re-enables the branch - `inst-ancestor-enable`
5. [x] - `p1` - **FROM** Disabled **TO** Ancestor-Disabled **WHEN** the ancestor disables an already-disabled branch (state stays non-routable) - `inst-disabled-ancestor-disable`

**Note**: routes referencing a `Disabled` or `Ancestor-Disabled` upstream are not routable; CRUD writes referencing a disabled upstream return 409 per the documented binding rules.

## 5. Definitions of Done

### Control-Plane Persistence

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-persistence`

The system **MUST** hold control-plane configuration in a persistence-free in-memory repository implementing the `oagw_*` domain model (`UpstreamEntity`, `RouteEntity`, `PluginInstanceEntity`), seeded from the validated `OagwConfig` and reflecting the documented authority (`oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_plugin`, `oagw_upstream_plugin`, `oagw_route_plugin`, `oagw_config` table semantics) — the `db` capability and `oagw_*` relational migrations are intentionally not claimed (ADR 0010 persistence-free MVP clause), with every management write observable to concurrent readers and no write silently lost.

**Implements**: `cpt-cf-oagw-flow-control-plane-manage-upstreams`, `cpt-cf-oagw-flow-control-plane-manage-routes`, `cpt-cf-oagw-flow-control-plane-manage-plugins`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`, `cpt-cf-oagw-constraint-workspace-lints`

**Touches**: Entities: `UpstreamEntity`, `RouteEntity`, `PluginInstanceEntity` (persistence-free repository; `oagw_*` table semantics per the domain model)

### Upstream CRUD

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-upstream-crud`

The system **MUST** expose authoritative upstream lifecycle over `/oagw/v1/upstreams[/{alias}]` with tenant scoping, authz gating, JSON-Schema validation, alias rules (shadowing 400; unknown alias references 404; use-in-route conflicts 409), and cache invalidation on every write.

**Implements**: `cpt-cf-oagw-flow-control-plane-manage-upstreams`

**Constraints**: `cpt-cf-oagw-constraint-no-api-segment`

**Touches**: API: `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams[/{alias}]`, `PUT /oagw/v1/upstreams/{alias}`, `DELETE /oagw/v1/upstreams/{alias}` / DB: `oagw_upstream` / Entities: `UpstreamEntity`

### Route CRUD

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-route-crud`

The system **MUST** expose authoritative route lifecycle over `/oagw/v1/routes[/{id}]` with schema validation including the match block into `oagw_route_http_match`, referenced-upstream existence/enabled/ownership checks (409 on disabled, 404/400 on unknown), and cache invalidation on every write.

**Implements**: `cpt-cf-oagw-flow-control-plane-manage-routes`

**Constraints**: `cpt-cf-oagw-constraint-no-api-segment`

**Touches**: API: `POST /oagw/v1/routes`, `GET /oagw/v1/routes[/{id}]`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}` / DB: `oagw_route`, `oagw_route_http_match` / Entities: `RouteEntity`

### Plugin CRUD and Binding

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-plugin-crud`

The system **MUST** expose authoritative plugin-instance lifecycle over `/oagw/v1/plugins[/{id}]` and bind/unbind over `oagw_upstream_plugin`/`oagw_route_plugin` with bind-once semantics returning 409 `PluginInUse` on duplicate binding, 409 on removing a still-bound instance, and 404 for missing targets, and MUST concatenate plugins in documented binding order during resolution.

**Implements**: `cpt-cf-oagw-flow-control-plane-manage-plugins`

**Constraints**: `cpt-cf-oagw-constraint-no-api-segment`

**Touches**: API: `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins[/{id}]`, `PUT /oagw/v1/plugins/{id}`, `DELETE /oagw/v1/plugins/{id}`, bind/unbind operations / DB: `oagw_plugin`, `oagw_upstream_plugin`, `oagw_route_plugin` / Entities: `PluginInstanceEntity`

### Configuration Hierarchy

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-hierarchy`

The system **MUST** resolve the effective configuration through the private/inherit/enforce hierarchy with stricter-wins rate merging, plugin concatenation, CORS union, and tags add-only union, honoring per-branch enablement (a disabled ancestor disables its descendants) so the data plane executes one authoritative merged configuration.

**Implements**: `cpt-cf-oagw-algo-control-plane-effective-config-resolution`, `cpt-cf-oagw-state-control-plane-enablement`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: DB: `oagw_config` + hierarchy inputs / Entities: `EffectiveConfig`

### Cache Invalidation

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-cache-invalidation`

The system **MUST** maintain the CP L1 config cache (capacity 10 000) per ADR 0005/0006, refresh/evict on writes with invalidation-driven lineage so stale effective configurations are never served, and propagate invalidation to flush the DP L1 cache so operator changes take effect bounded, without dropping concurrent writes.

**Implements**: `cpt-cf-oagw-flow-control-plane-manage-upstreams`, `cpt-cf-oagw-flow-control-plane-manage-routes`, `cpt-cf-oagw-flow-control-plane-manage-plugins`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: DB: `oagw_config` (cache refresh) / Entities: `ConfigCache`

### Control-Plane Test Harness

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-control-plane-test-harness`

The system **MUST** test CRUD semantics, schema rejection, tenant scoping, authz gating, alias/binding/hierarchy outcomes, and cache invalidation at the crate level under the workspace lint denials, and MUST NOT author any code under `testing/e2e/gears/oagw/`.

**Implements**: `cpt-cf-oagw-flow-control-plane-manage-upstreams`, `cpt-cf-oagw-algo-control-plane-effective-config-resolution`

**Constraints**: `cpt-cf-oagw-constraint-workspace-lints`, `cpt-cf-oagw-constraint-toolchain`

**Touches**: DB: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_plugin`, `oagw_upstream_plugin`, `oagw_route_plugin`, `oagw_config` / Entities: `UpstreamEntity`, `RouteEntity`, `PluginInstanceEntity`, `EffectiveConfig`

## 6. Acceptance Criteria

- [x] Full CRUD over `/oagw/v1/upstreams`, `/oagw/v1/routes`, and `/oagw/v1/plugins` behaves per the authoritative management API contract at gear-relative paths (no `/api` segment) with schema validation against `upstream.v1.schema.json` and `route.v1.schema.json`.
- [x] Alias rules are enforced: shadowing an existing managed alias returns 400; referencing an unknown alias returns 404; deleting an upstream in use returns 409.
- [x] Plugin binding enforces bind-once: binding an already-bound instance returns 409 `PluginInUse`; removing a still-bound instance returns 409.
- [x] The effective configuration resolves private/inherit/enforce with stricter-wins rates, plugin concatenation in documented order, CORS union, and tags add-only union; an ancestor-disabled branch is not routable.
- [x] Writes invalidate the CP L1 cache and propagate a flush to the DP L1 cache; no stale effective configuration is served after a write.
- [x] Tenant scoping restricts CRUD to the caller's tenant rows; authz gating precedes any persistence and returns authorized error responses.
- [x] Every crate-level control-plane test passes on the configured toolchain; `testing/e2e/gears/oagw/` receives no code from this change.
