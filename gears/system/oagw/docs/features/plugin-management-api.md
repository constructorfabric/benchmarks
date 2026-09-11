# Feature: Plugin Management API

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Custom Plugin Flow](#create-custom-plugin-flow)
  - [List Plugins Flow](#list-plugins-flow)
  - [Get Plugin Flow](#get-plugin-flow)
  - [Get Plugin Source Flow](#get-plugin-source-flow)
  - [Delete Plugin Flow](#delete-plugin-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Plugin Reference Resolution Algorithm](#plugin-reference-resolution-algorithm)
  - [Plugin In-Use Scan Algorithm](#plugin-in-use-scan-algorithm)
  - [Plugin Create Validation Algorithm](#plugin-create-validation-algorithm)
- [4. States (CDSL)](#4-states-cdsl)
  - [Plugin Lifecycle State Machine](#plugin-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement Plugin Creation](#implement-plugin-creation)
  - [Implement Plugin Listing with OData Query Support](#implement-plugin-listing-with-odata-query-support)
  - [Implement Plugin Read and Source-Fetch Endpoints](#implement-plugin-read-and-source-fetch-endpoints)
  - [Implement Plugin Deletion with Reference Guard](#implement-plugin-deletion-with-reference-guard)
  - [Enforce Plugin Immutability](#enforce-plugin-immutability)
  - [Conform Plugin Error Responses to RFC 9457](#conform-plugin-error-responses-to-rfc-9457)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-plugin-management-api-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-plugin-management-api`

## 1. Feature Context

### 1.1 Overview

This feature exposes the create, list, read, source-fetch, and delete surface for custom
plugin definitions. A custom plugin is a tenant-authored script written in Starlark (a
Python-like scripting language OAGW stores and returns but does not execute in this build).
There is no update route — plugins are immutable once created.

### 1.2 Purpose

The Plugin Management API lets a platform operator or tenant administrator register, inspect,
and retire the auth, guard, and transform plugins that upstream and route configurations bind
to. It covers only the storage and catalogue half of the plugin system; the traits that run
plugins during a proxied request belong to other features.

**Requirements**: `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-nfr-multi-tenancy`, `cpt-cf-oagw-nfr-input-validation`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`, `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

**Plugin entity**: a `Plugin` record has `id` (the stored UUID), `tenant_id`, `plugin_type`
(`auth`, `guard`, or `transform`), `name`, `description`, `config_schema` (a JSON Schema object
describing the plugin's configuration shape), `phases` (transform plugins only — any of
`on_request`, `on_response`, `on_error`, naming which stage of a proxied call the plugin
touches), `source_code` (the stored Starlark text), `last_used_at`, and `gc_eligible_at` (the
latter two support the future garbage-collection sweep described under Out of Scope below).

**Plugin identification model**: every plugin is addressed in the API by a GTS (Global Type
System — the platform's schema-and-instance identifier scheme) identifier. Named plugins —
built into the gateway or shipped by a deployed gear — use
`gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{name}.v1` and resolve through an in-process
registry; they are never stored in the plugin store and are never subject to garbage
collection. Custom plugins use `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` and are stored,
keyed by that UUID. `cpt-cf-oagw-algo-plugin-api-ref-resolution` (Section 3) defines the exact
four-step lookup this feature performs whenever it is handed such an identifier.

**Built-in catalogue**: not every documented identifier resolves. The registry actually
answers for auth `noop`, `apikey`, `oauth2_client_cred`, and `oauth2_client_cred_basic`; for
guard `required_headers`; and for transform `request_id`. The remaining identifiers exist only
for types-registry cataloguing and fail resolution: auth `basic.v1` and `bearer.v1` are
reserved with no backing implementation and fail with `"unknown auth plugin"`; guard
`timeout.v1` and `cors.v1` name core data-plane functionality (request timeout enforcement and
CORS validation) rather than a `GuardPlugin` implementation; transform `logging.v1` and
`metrics.v1` name core instrumentation (structured logging and Prometheus metrics) rather than
a `TransformPlugin` implementation. `required_headers.v1` is the only guard identifier this
feature's callers may bind through `plugins.items[].plugin_ref`.

**Tenant scoping and permissions**: every plugin read, create, and delete is scoped to the
calling tenant; a plugin owned by a different tenant is invisible and reported as 404, matching
the invisibility rule applied to upstreams and routes. Named plugins are global registry
entries, not tenant data, so every tenant sees the same catalogue. Each plugin type carries its
own permission on the Bearer token presented to the Management API:

| Permission | Allows |
|---|---|
| `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}` | Create, read, delete auth plugins |
| `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}` | Create, read, delete guard plugins |
| `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` | Create, read, delete transform plugins |

**Out of scope**: Starlark source is stored and served verbatim but never executed — no
Starlark runtime is enabled in this build, so `cpt-cf-oagw-nfr-starlark-sandbox`'s sandboxed
execution requirements have nothing to run against here. The periodic garbage-collection sweep
that deletes plugin rows once `gc_eligible_at` has passed is also deferred; this feature still
enforces the synchronous in-use check on every `DELETE` (Section 3), so an unlinked plugin
remains deletable on demand even though the automatic sweep does not run.

### 1.3 Actors

- `cpt-cf-oagw-actor-platform-operator` - Registers and retires system-wide custom plugins and consults the built-in catalogue before binding a plugin to an upstream or route.
- `cpt-cf-oagw-actor-tenant-admin` - Creates tenant-scoped custom plugins, reviews their stored source, and deletes plugins no longer referenced.

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Design elements**: `cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-component-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-db-schema`, `cpt-cf-oagw-adr-plugin-system`
- **Dependencies**: `cpt-cf-oagw-feature-route-management-api`

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-fr-plugin-system`

### Create Custom Plugin Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-api-create`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A well-formed plugin definition is stored and a UUID-backed GTS identifier is returned

**Error Scenarios**:
- `plugin_type` is not `auth`, `guard`, or `transform`
- `phases` is supplied for a non-transform `plugin_type`
- `config_schema` is not a syntactically valid JSON Schema object
- `name` is empty, or already used by another plugin for this tenant
- `phases` is empty or contains a value outside `on_request`/`on_response`/`on_error` when `plugin_type` is `transform`
- `source_code` is empty
- Bearer token lacks the matching `{type}_plugin.v1~:create` permission

**Steps**:
1. [ ] - `p1` - Tenant admin submits a plugin definition (`plugin_type`, `name`, `description`, `config_schema`, `phases`, `source_code`) - `inst-create-1`
2. [ ] - `p1` - API: POST /oagw/v1/plugins ({ plugin_type, name, description, config_schema, phases, source_code }) - `inst-create-2`
3. [ ] - `p1` - **IF** caller lacks the `gts.cf.core.oagw.{plugin_type}_plugin.v1~:create` permission matching the submitted `plugin_type` - `inst-create-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed (`application/problem+json`, `X-OAGW-Error-Source: gateway`) - `inst-create-3a`
4. [ ] - `p1` - **ELSE** - `inst-create-4`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-api-create-validate` against the submitted body - `inst-create-4a`
   2. [ ] - `p1` - **IF** validation passes - `inst-create-4b`
      1. [ ] - `p1` - Generate a server-side UUID for the plugin instance - `inst-create-4b1`
      2. [ ] - `p1` - Store: INSERT oagw_plugin (id, tenant_id, plugin_type, name, description, config_schema, phases, source_code, last_used_at=null, gc_eligible_at=null) - `inst-create-4b2`
      3. [ ] - `p1` - Assemble `id = gts.cf.core.oagw.{plugin_type}_plugin.v1~{uuid}` and `plugin_uuid = {uuid}` - `inst-create-4b3`
      4. [ ] - `p1` - **RETURN** 201 Created with the stored plugin body - `inst-create-4b4`
   3. [ ] - `p1` - **ELSE** - `inst-create-4c`
      1. [ ] - `p1` - **RETURN** 400 ValidationError (`application/problem+json`, `X-OAGW-Error-Source: gateway`) - `inst-create-4c1`

### List Plugins Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-api-list`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The tenant's stored custom plugins are returned, filtered and paginated as requested

**Error Scenarios**:
- `$top` or `$skip` is not a non-negative integer
- Caller holds none of the three `{type}_plugin.v1~:read` permissions (`auth_plugin`, `guard_plugin`, `transform_plugin`)

**Steps**:
1. [ ] - `p1` - Operator requests the plugin catalogue with optional OData (Open Data Protocol — the query-parameter convention this API reuses from the upstream and route list endpoints) parameters - `inst-list-1`
2. [ ] - `p1` - API: GET /oagw/v1/plugins?$filter=...&$select=...&$top=...&$skip=... - `inst-list-2`
3. [ ] - `p1` - **IF** caller holds none of `gts.cf.core.oagw.auth_plugin.v1~:read`, `gts.cf.core.oagw.guard_plugin.v1~:read`, `gts.cf.core.oagw.transform_plugin.v1~:read` - `inst-list-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed (`application/problem+json`, `X-OAGW-Error-Source: gateway`) - `inst-list-3a`
4. [ ] - `p1` - **ELSE** - `inst-list-4`
   1. [ ] - `p1` - Store: SELECT oagw_plugin WHERE tenant_id = :tenant - `inst-list-4a`
   2. [ ] - `p1` - **IF** `$filter` is present - `inst-list-4b`
      1. [ ] - `p1` - Apply the OData filter expression to the in-memory result set - `inst-list-4b1`
   3. [ ] - `p1` - **IF** `$select` is present - `inst-list-4c`
      1. [ ] - `p1` - Project only the requested fields for each returned plugin - `inst-list-4c1`
   4. [ ] - `p1` - Apply `$skip` offset then `$top` limit (default 50, max 100) - `inst-list-4d`
   5. [ ] - `p1` - **RETURN** 200 with the paginated list of tenant-scoped custom plugins, limited to plugin types for which the caller holds the matching `read` permission; named plugins never appear here because they are not stored - `inst-list-4e`

### Get Plugin Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-api-get`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A stored plugin's full metadata, including its `source_code` field, is returned

**Error Scenarios**:
- `{id}` names a plugin owned by a different tenant, a named/reserved identifier, or an unknown UUID
- Caller lacks the `gts.cf.core.oagw.{type}_plugin.v1~:read` permission matching the `{type}` parsed from `{id}`

**Steps**:
1. [ ] - `p1` - Caller requests a plugin by its GTS identifier - `inst-get-1`
2. [ ] - `p1` - API: GET /oagw/v1/plugins/{id} - `inst-get-2`
3. [ ] - `p1` - **IF** caller lacks the `gts.cf.core.oagw.{type}_plugin.v1~:read` permission for the `{type}` parsed from `{id}`'s schema part - `inst-get-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed (`application/problem+json`, `X-OAGW-Error-Source: gateway`) - `inst-get-3a`
4. [ ] - `p1` - **ELSE** - `inst-get-4`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-api-ref-resolution` on `{id}` - `inst-get-4a`
   2. [ ] - `p1` - **IF** resolution returns a stored plugin owned by the caller's tenant - `inst-get-4b`
      1. [ ] - `p1` - **RETURN** 200 with the full plugin record - `inst-get-4b1`
   3. [ ] - `p1` - **ELSE** - `inst-get-4c`
      1. [ ] - `p1` - **RETURN** 404 RouteNotFound-shaped Problem Details (`application/problem+json`, `X-OAGW-Error-Source: gateway`). `RouteNotFound` is reused here because it is the only 404 identifier the supplied error table defines. DESIGN.md's `PluginNotFound` is deliberately not used, since that identifier names a 503 for a different, data-plane failure mode - `inst-get-4c1`

### Get Plugin Source Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-api-get-source`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The stored Starlark source is returned verbatim, unmodified and unexecuted

**Error Scenarios**:
- `{id}` names a plugin owned by a different tenant, a named/reserved identifier, or an unknown UUID
- Caller lacks the `gts.cf.core.oagw.{type}_plugin.v1~:read` permission matching the `{type}` parsed from `{id}`

**Steps**:
1. [ ] - `p1` - Caller requests the raw source of a plugin - `inst-src-1`
2. [ ] - `p1` - API: GET /oagw/v1/plugins/{id}/source - `inst-src-2`
3. [ ] - `p1` - **IF** caller lacks the `gts.cf.core.oagw.{type}_plugin.v1~:read` permission for the `{type}` parsed from `{id}`'s schema part - `inst-src-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed (`application/problem+json`, `X-OAGW-Error-Source: gateway`) - `inst-src-3a`
4. [ ] - `p1` - **ELSE** - `inst-src-4`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-api-ref-resolution` on `{id}` - `inst-src-4a`
   2. [ ] - `p1` - **IF** resolution returns a stored plugin owned by the caller's tenant - `inst-src-4b`
      1. [ ] - `p1` - **RETURN** 200 `text/plain` body equal to the stored `source_code`, returned verbatim and never run - `inst-src-4b1`
   3. [ ] - `p1` - **ELSE** - `inst-src-4c`
      1. [ ] - `p1` - **RETURN** 404 RouteNotFound-shaped Problem Details - `inst-src-4c1`

### Delete Plugin Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-api-delete`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An unreferenced plugin is permanently removed from the store

**Error Scenarios**:
- `{id}` does not resolve to a plugin owned by the caller's tenant
- The plugin is still bound to an upstream or a route
- Caller lacks the `gts.cf.core.oagw.{type}_plugin.v1~:delete` permission matching the `{type}` parsed from `{id}`

**Steps**:
1. [ ] - `p1` - Tenant admin requests removal of a plugin - `inst-del-1`
2. [ ] - `p1` - API: DELETE /oagw/v1/plugins/{id} - `inst-del-2`
3. [ ] - `p1` - **IF** caller lacks the `gts.cf.core.oagw.{type}_plugin.v1~:delete` permission for the `{type}` parsed from `{id}`'s schema part - `inst-del-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed (`application/problem+json`, `X-OAGW-Error-Source: gateway`) - `inst-del-3a`
4. [ ] - `p1` - **ELSE** - `inst-del-4`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-api-ref-resolution` on `{id}` - `inst-del-4a`
   2. [ ] - `p1` - **IF** resolution does not return a stored plugin owned by the caller's tenant - `inst-del-4b`
      1. [ ] - `p1` - **RETURN** 404 RouteNotFound-shaped Problem Details - `inst-del-4b1`
   3. [ ] - `p1` - **ELSE** - `inst-del-4c`
      1. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-api-in-use-scan` on the resolved `plugin_uuid` - `inst-del-4c1`
      2. [ ] - `p1` - **IF** the scan's `referenced_by.upstreams` and `referenced_by.routes` are both empty - `inst-del-4c2`
         1. [ ] - `p1` - Store: DELETE oagw_plugin WHERE id = :uuid AND tenant_id = :tenant - `inst-del-4c2a`
         2. [ ] - `p1` - **RETURN** 204 No Content - `inst-del-4c2b`
      3. [ ] - `p1` - **ELSE** - `inst-del-4c3`
         1. [ ] - `p1` - **RETURN** 409 Conflict (PluginInUse) with `type`, `title`, `status`, `detail`, `plugin_id`, and the populated `referenced_by` object - `inst-del-4c3a`

## 3. Processes / Business Logic (CDSL)

### Plugin Reference Resolution Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-api-ref-resolution`

**Input**: a GTS plugin identifier (e.g. the `{id}` path parameter), of the form
`gts.cf.core.oagw.{type}_plugin.v1~{instance}`

**Output**: `Stored(plugin_uuid, plugin_type, tenant_id)`, `Named(plugin_ref)`, or `NotFound`

**Steps**:
1. [ ] - `p1` - Parse the identifier into its schema part (`gts.cf.core.oagw.{type}_plugin.v1`) and the instance part after `~` - `inst-resolve-1`
2. [ ] - `p1` - **IF** the instance part parses as a valid UUID - `inst-resolve-2`
   1. [ ] - `p1` - Store: SELECT oagw_plugin WHERE tenant_id = :tenant AND id = :uuid - `inst-resolve-2a`
3. [ ] - `p1` - **IF** a row was found and its stored `plugin_type` matches the requested `{type}_plugin` schema - `inst-resolve-3`
   1. [ ] - `p1` - **RETURN** `Stored(uuid, plugin_type, tenant_id)` - `inst-resolve-3a`
4. [ ] - `p1` - **ELSE IF** the instance part is not a UUID (a dotted name, e.g. `cf.core.oagw.apikey.v1`) - `inst-resolve-4`
   1. [ ] - `p1` - Look up the name against the in-process registry for the requested `plugin_type` - `inst-resolve-4a`
5. [ ] - `p1` - **IF** the name is a registry-resolvable built-in (auth: `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`; guard: `required_headers`; transform: `request_id`) - `inst-resolve-5`
   1. [ ] - `p1` - **RETURN** `Named(plugin_ref)` - `inst-resolve-5a`
6. [ ] - `p1` - **ELSE** - `inst-resolve-6`
   1. [ ] - `p1` - **RETURN** `NotFound` — covers a missing or schema-mismatched UUID row, catalog-only identifiers (`basic.v1`, `bearer.v1`, `timeout.v1`, `cors.v1`, `logging.v1`, `metrics.v1`), and unrecognized names; an auth-type lookup additionally carries the detail `"unknown auth plugin"` - `inst-resolve-6a`
7. [ ] - `p1` - **RETURN** to the caller a persistence rule: always store `plugin_ref`; store `plugin_uuid` only when the result was `Stored` - `inst-resolve-7`

### Plugin In-Use Scan Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-api-in-use-scan`

**Input**: `plugin_uuid` of a stored plugin, `tenant_id`

**Output**: `referenced_by { upstreams: [gts identifiers], routes: [gts identifiers] }`

**Steps**:
1. [ ] - `p1` - Store: SELECT parent_id FROM oagw_upstream_plugin WHERE tenant_id = :tenant AND plugin_uuid = :uuid - `inst-scan-1`
2. [ ] - `p1` - Store: SELECT parent_id FROM oagw_route_plugin WHERE tenant_id = :tenant AND plugin_uuid = :uuid - `inst-scan-2`
3. [ ] - `p1` - Store: SELECT id FROM oagw_upstream WHERE tenant_id = :tenant AND auth_plugin_uuid = :uuid - `inst-scan-3`
4. [ ] - `p1` - **FOR EACH** upstream id found in steps 1 and 3 - `inst-scan-4`
   1. [ ] - `p1` - Add its `gts.cf.core.oagw.upstream.v1~{id}` identifier to `referenced_by.upstreams`, de-duplicated - `inst-scan-4a`
5. [ ] - `p1` - **FOR EACH** route id found in step 2 - `inst-scan-5`
   1. [ ] - `p1` - Add its `gts.cf.core.oagw.route.v1~{id}` identifier to `referenced_by.routes` - `inst-scan-5a`
6. [ ] - `p1` - **RETURN** `referenced_by` — both arrays empty means the plugin is unreferenced and deletable - `inst-scan-6`

### Plugin Create Validation Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-api-create-validate`

**Input**: `POST /oagw/v1/plugins` request body

**Output**: `{ valid: bool, errors: [String] }`

**Steps**:
1. [ ] - `p1` - Parse and normalize `plugin_type`, `name`, `description`, `config_schema`, `phases`, `source_code` - `inst-cval-1`
2. [ ] - `p1` - **IF** `plugin_type` is not one of `auth`, `guard`, `transform` - `inst-cval-2`
   1. [ ] - `p1` - Add error: invalid `plugin_type` - `inst-cval-2a`
3. [ ] - `p1` - **IF** `name` is empty, or already used by another plugin for this tenant - `inst-cval-3`
   1. [ ] - `p1` - Add error: `name` must be unique per tenant - `inst-cval-3a`
4. [ ] - `p1` - **IF** `phases` is present and `plugin_type` is not `transform` - `inst-cval-4`
   1. [ ] - `p1` - Add error: `phases` is only valid for transform plugins - `inst-cval-4a`
5. [ ] - `p1` - **IF** `plugin_type` is `transform` and `phases` is empty or contains a value outside `on_request`/`on_response`/`on_error` - `inst-cval-5`
   1. [ ] - `p1` - Add error: invalid `phases` value - `inst-cval-5a`
6. [ ] - `p1` - **IF** `config_schema` is present and is not a syntactically valid JSON Schema object - `inst-cval-6`
   1. [ ] - `p1` - Add error: `config_schema` must be a valid JSON Schema - `inst-cval-6a`
7. [ ] - `p1` - **IF** `source_code` is empty - `inst-cval-7`
   1. [ ] - `p1` - Add error: `source_code` is required - `inst-cval-7a`
8. [ ] - `p1` - **RETURN** `{ valid: errors.length === 0, errors }` - `inst-cval-8`

## 4. States (CDSL)

### Plugin Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-plugin-api-lifecycle`

**States**: created, bound, unlinked, gc-eligible, deleted

**Initial State**: created

**Transitions**:
1. [ ] - `p1` - **FROM** created **TO** bound **WHEN** a binding referencing the plugin's `plugin_uuid` is added — an `oagw_upstream_plugin`/`oagw_route_plugin` row, or an upstream's `auth_plugin_uuid` column - `inst-lc-1`
2. [ ] - `p1` - **FROM** bound **TO** unlinked **WHEN** the last referencing binding is removed and `cpt-cf-oagw-algo-plugin-api-in-use-scan` returns zero bindings - `inst-lc-2`
3. [ ] - `p1` - **FROM** unlinked **TO** bound **WHEN** a new binding references the plugin again - `inst-lc-3`
4. [ ] - `p1` - **FROM** unlinked **TO** gc-eligible **WHEN** `gc_eligible_at` is stamped with the TTL expiry timestamp (default 30 days per DESIGN.md) at the moment the plugin becomes unlinked - `inst-lc-4`
5. [ ] - `p1` - **FROM** created **TO** deleted **WHEN** DELETE /oagw/v1/plugins/{id} succeeds for a plugin that was never bound - `inst-lc-5`
6. [ ] - `p1` - **FROM** unlinked **TO** deleted **WHEN** DELETE /oagw/v1/plugins/{id} succeeds while the plugin is unlinked - `inst-lc-6`
7. [ ] - `p1` - **FROM** gc-eligible **TO** deleted **WHEN** DELETE /oagw/v1/plugins/{id} succeeds while the plugin awaits garbage collection, or — out of scope for this build — a periodic garbage-collection sweep runs after `gc_eligible_at` has passed - `inst-lc-7`

## 5. Definitions of Done

### Implement Plugin Creation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-api-create`

The system **MUST** accept `POST /oagw/v1/plugins`, validate the payload with
`cpt-cf-oagw-algo-plugin-api-create-validate`, and persist an accepted custom plugin under a
server-generated UUID assembled into a GTS identifier of the form
`gts.cf.core.oagw.{plugin_type}_plugin.v1~{uuid}`.

**Implements**:
- `cpt-cf-oagw-flow-plugin-api-create`
- `cpt-cf-oagw-algo-plugin-api-create-validate`

**Touches**:
- API: `POST /oagw/v1/plugins`
- DB Table: `cpt-cf-oagw-db-schema`
- Entities: `Plugin`

### Implement Plugin Listing with OData Query Support

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-api-list`

The system **MUST** list the calling tenant's stored custom plugins via `GET /oagw/v1/plugins`,
applying `$filter`, `$select`, `$top` (default 50, max 100), and `$skip` exactly as the
upstream and route list endpoints do.

**Implements**:
- `cpt-cf-oagw-flow-plugin-api-list`

**Touches**:
- API: `GET /oagw/v1/plugins`
- DB Table: `cpt-cf-oagw-db-schema`
- Entities: `Plugin`

### Implement Plugin Read and Source-Fetch Endpoints

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-api-read`

The system **MUST** resolve the `{id}` path parameter through
`cpt-cf-oagw-algo-plugin-api-ref-resolution` for both `GET /oagw/v1/plugins/{id}` and
`GET /oagw/v1/plugins/{id}/source`, returning the stored record or the raw `source_code` for a
tenant-owned, UUID-backed plugin, and a `404 RouteNotFound`-shaped Problem Details response for
every other case.

**Implements**:
- `cpt-cf-oagw-flow-plugin-api-get`
- `cpt-cf-oagw-flow-plugin-api-get-source`
- `cpt-cf-oagw-algo-plugin-api-ref-resolution`

**Touches**:
- API: `GET /oagw/v1/plugins/{id}`
- API: `GET /oagw/v1/plugins/{id}/source`
- DB Table: `cpt-cf-oagw-db-schema`
- Entities: `Plugin`

### Implement Plugin Deletion with Reference Guard

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-api-delete`

The system **MUST** run `cpt-cf-oagw-algo-plugin-api-in-use-scan` across
`oagw_upstream_plugin`, `oagw_route_plugin`, and every upstream's `auth_plugin_uuid` column
before deleting a plugin, returning `204 No Content` when unreferenced and `409 Conflict`
(`PluginInUse`) with a populated `referenced_by` object otherwise.

**Implements**:
- `cpt-cf-oagw-flow-plugin-api-delete`
- `cpt-cf-oagw-algo-plugin-api-in-use-scan`
- `cpt-cf-oagw-state-plugin-api-lifecycle`

**Touches**:
- API: `DELETE /oagw/v1/plugins/{id}`
- DB Table: `cpt-cf-oagw-db-schema`
- Entities: `Plugin`

### Enforce Plugin Immutability

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-api-immutable`

The system **MUST NOT** register a `PUT` or `PATCH` route under `/oagw/v1/plugins/{id}`.
Changing a plugin's behavior **MUST** be done by creating a new plugin via `POST` and
re-binding the affected upstream and route references to the new identifier.

**Implements**:
- `cpt-cf-oagw-state-plugin-api-lifecycle`

**Constraints**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- Entities: `Plugin`

### Conform Plugin Error Responses to RFC 9457

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-api-errors`

The system **MUST** render every plugin-endpoint error (`400`, `401`, `404`, `409`) as an
`application/problem+json` body carrying the matching GTS `type` from DESIGN.md's error table,
plus an `X-OAGW-Error-Source: gateway` header on every response.

**Implements**:
- `cpt-cf-oagw-flow-plugin-api-create`
- `cpt-cf-oagw-flow-plugin-api-delete`

**Constraints**: `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

**Touches**:
- API: `POST /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins/{id}`
- API: `GET /oagw/v1/plugins/{id}/source`
- API: `DELETE /oagw/v1/plugins/{id}`
- Entities: `Plugin`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/plugins` with a valid custom plugin body returns `201 Created` and a UUID-backed GTS identifier of the form `gts.cf.core.oagw.{plugin_type}_plugin.v1~{uuid}`
- [ ] `GET /oagw/v1/plugins/{id}/source` returns the stored `source_code` verbatim for a UUID-backed plugin
- [ ] `DELETE /oagw/v1/plugins/{id}` for an unreferenced plugin returns `204 No Content`
- [ ] `DELETE /oagw/v1/plugins/{id}` for a plugin bound to an upstream or a route returns `409 Conflict` with a `referenced_by` object naming that upstream or route
- [ ] No `PUT` (or `PATCH`) route exists under `/oagw/v1/plugins/{id}`
- [ ] `GET /oagw/v1/plugins` honors `$filter`, `$select`, `$top`, and `$skip`, and returns only plugins owned by the caller's tenant
- [ ] `GET /oagw/v1/plugins/{id}` for a reserved, catalog-only identifier (e.g. `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1`) returns a `404 RouteNotFound`-shaped Problem Details response, since only custom UUID-backed plugins are stored
- [ ] `GET /oagw/v1/plugins/{id}` for a tenant-owned, UUID-backed plugin returns `200 OK` with the full stored record, including its `source_code` field
- [ ] A request against any plugin endpoint with no valid Bearer token, or a token lacking the matching `{type}_plugin.v1~:{create|read|delete}` permission, returns `401 AuthenticationFailed`
