# Feature: Plugin Management API


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Register Custom Plugin](#register-custom-plugin)
  - [List Plugins](#list-plugins)
  - [Get Plugin by ID](#get-plugin-by-id)
  - [Get Plugin Source](#get-plugin-source)
  - [Delete Plugin](#delete-plugin)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Plugin Create Validation](#plugin-create-validation)
  - [Plugin Reference Resolution](#plugin-reference-resolution)
  - [Plugin In-Use Detection](#plugin-in-use-detection)
- [4. States (CDSL)](#4-states-cdsl)
  - [Plugin Lifecycle State Machine](#plugin-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Create Custom Plugin](#create-custom-plugin)
  - [Enforce Per-Tenant Plugin Name Uniqueness](#enforce-per-tenant-plugin-name-uniqueness)
  - [Validate Plugin Config Schema and Declared Phases](#validate-plugin-config-schema-and-declared-phases)
  - [List Plugins with Query Parameters](#list-plugins-with-query-parameters)
  - [Get Plugin by ID](#get-plugin-by-id-1)
  - [Get Plugin Source](#get-plugin-source-1)
  - [Delete Plugin with In-Use Conflict Detection](#delete-plugin-with-in-use-conflict-detection)
  - [Plugin Identification Model Resolution](#plugin-identification-model-resolution)
  - [No Replace Operation on Plugin Resources](#no-replace-operation-on-plugin-resources)
  - [Track Plugin Lifecycle State](#track-plugin-lifecycle-state)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-plugin-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-plugin-management`

## 1. Feature Context

### 1.1 Overview

This feature gives tenants a management surface for custom plugin definitions: creating, listing, inspecting, deleting them, and retrieving their stored source, while named built-in plugins stay resolved from an in-process registry and are never persisted here.

### 1.2 Purpose

Custom plugins let tenants extend the Auth/Guard/Transform chain beyond built-ins without recompiling the gateway. This feature exists to give operators a CRUD surface for those custom definitions, enforce their immutability once created, and guard deletion so a plugin still bound to an upstream or route cannot be removed until unbound.

**Requirements**: `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-nfr-multi-tenancy`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`

Persistence for this feature is in-process for the graded deployment; no database is configured for OAGW. `cpt-cf-oagw-db-schema` is cited throughout only as informing the plugin entity's shape and its per-tenant name-uniqueness and plugin-binding invariants, not as an actual SQL table.

The platform's API gateway authenticates and authorizes every request to this management API ahead of this gear's mounted router, established by `cpt-cf-oagw-feature-gear-foundation`; this feature specifies no permission check of its own.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Registers and manages system-wide custom plugin definitions, and deletes plugins no longer needed. |
| `cpt-cf-oagw-actor-tenant-admin` | Registers, lists, inspects, and deletes custom plugin definitions scoped to their own tenant. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` — this feature needs the mounted gear-relative router and the RFC 9457 error contract before it can expose the `/oagw/v1/plugins` endpoints. This feature's create, list, get, and source endpoints build independently of `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-route-management`, and both chains trace back only to `cpt-cf-oagw-feature-gear-foundation`. The delete endpoint's plugin-in-use conflict check, however, reads upstream and route plugin bindings and cannot be exercised end-to-end until those two features exist; the conflict response shape it produces follows the example recorded against `cpt-cf-oagw-adr-request-routing`.

## 2. Actor Flows (CDSL)

**Use cases**: none dedicated in PRD.md; this feature realizes `cpt-cf-oagw-fr-plugin-system` for custom plugin definitions.

### Register Custom Plugin

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-register-plugin`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Actor submits a valid plugin definition and receives a created plugin resource with a UUID-backed GTS identifier.

**Error Scenarios**:
- Payload fails validation (unknown plugin type, malformed config schema, or an out-of-range declared phase).
- A plugin with the same name already exists for the calling tenant.

**Steps**:
1. [ ] - `p1` - Actor sends a create request with plugin type, name, config schema, declared phases, and source text - `inst-plugin-register-01`
2. [ ] - `p1` - API: POST /oagw/v1/plugins (plugin definition request; created plugin resource response) - `inst-plugin-register-02`
3. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-create-validation` against the submitted payload for the calling tenant - `inst-plugin-register-03`
4. [ ] - `p1` - **IF** validation fails - `inst-plugin-register-04`
   1. [ ] - `p1` - **RETURN** 400 ValidationError with field-level detail - `inst-plugin-register-05`
5. [ ] - `p1` - **ELSE** - `inst-plugin-register-06`
   1. [ ] - `p1` - Assign a new UUID and store the plugin definition in the Registered lifecycle state - `inst-plugin-register-07`
6. [ ] - `p1` - **RETURN** 201 Created with the stored plugin resource - `inst-plugin-register-08`

### List Plugins

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-list-plugins`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Actor lists their tenant's stored custom plugin definitions, optionally filtered, projected, and paginated.

**Error Scenarios**:
- An unsupported query parameter is rejected with a validation error.

**Steps**:
1. [ ] - `p1` - Actor sends a list request with optional `$filter`, `$select`, `$top`, `$skip` parameters - `inst-plugin-list-01`
2. [ ] - `p1` - API: GET /oagw/v1/plugins (OData query parameters; paginated plugin summary list response) - `inst-plugin-list-02`
3. [ ] - `p1` - Scope the result set to the calling tenant's stored plugin definitions only, excluding named built-in plugins - `inst-plugin-list-03`
4. [ ] - `p1` - **RETURN** 200 with the filtered, projected, paginated list - `inst-plugin-list-04`

### Get Plugin by ID

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-get-plugin`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Actor fetches a single stored plugin definition owned by their tenant.

**Error Scenarios**:
- The identifier does not resolve to a stored plugin owned by the calling tenant.

**Steps**:
1. [ ] - `p1` - Actor sends a get request naming a plugin's GTS identifier - `inst-plugin-get-01`
2. [ ] - `p1` - API: GET /oagw/v1/plugins/{id} (plugin identifier path parameter; plugin resource response) - `inst-plugin-get-02`
3. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-ref-resolution` against the path identifier for the calling tenant - `inst-plugin-get-03`
4. [ ] - `p1` - **IF** resolution fails or the plugin belongs to another tenant - `inst-plugin-get-04`
   1. [ ] - `p1` - **RETURN** 404 - `inst-plugin-get-05`
5. [ ] - `p1` - **ELSE** - `inst-plugin-get-06`
   1. [ ] - `p1` - **RETURN** 200 with the plugin resource, excluding source text - `inst-plugin-get-07`

### Get Plugin Source

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-get-plugin-source`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Actor retrieves the stored source text of a custom, UUID-backed plugin definition.

**Error Scenarios**:
- The identifier names a named built-in plugin, which has no stored source.
- The identifier does not resolve to a stored plugin owned by the calling tenant.

**Steps**:
1. [ ] - `p1` - Actor sends a get-source request naming a plugin's GTS identifier - `inst-plugin-source-01`
2. [ ] - `p1` - API: GET /oagw/v1/plugins/{id}/source (plugin identifier path parameter; source text response) - `inst-plugin-source-02`
3. [ ] - `p1` - **IF** the identifier's instance part is not a UUID - `inst-plugin-source-03`
   1. [ ] - `p1` - **RETURN** 404, since named plugins carry no stored source - `inst-plugin-source-04`
4. [ ] - `p1` - **ELSE** - `inst-plugin-source-05`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-ref-resolution` against the path identifier for the calling tenant - `inst-plugin-source-06`
5. [ ] - `p1` - **IF** resolution fails or the plugin belongs to another tenant - `inst-plugin-source-07`
   1. [ ] - `p1` - **RETURN** 404 - `inst-plugin-source-08`
6. [ ] - `p1` - **RETURN** 200 with the stored source text of the resolved plugin - `inst-plugin-source-09`

### Delete Plugin

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-delete-plugin`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Actor deletes a stored plugin definition that no upstream or route binding currently references.

**Error Scenarios**:
- The plugin is still referenced by at least one upstream or route binding.
- The identifier does not resolve to a stored plugin owned by the calling tenant.

**Steps**:
1. [ ] - `p1` - Actor sends a delete request naming a plugin's GTS identifier - `inst-plugin-delete-01`
2. [ ] - `p1` - API: DELETE /oagw/v1/plugins/{id} (plugin identifier path parameter; empty or conflict response) - `inst-plugin-delete-02`
3. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-ref-resolution` against the path identifier for the calling tenant - `inst-plugin-delete-03`
4. [ ] - `p1` - **IF** resolution fails or the plugin belongs to another tenant - `inst-plugin-delete-04`
   1. [ ] - `p1` - **RETURN** 404 - `inst-plugin-delete-05`
5. [ ] - `p1` - Run `cpt-cf-oagw-algo-plugin-in-use-detection` against the resolved plugin for the calling tenant - `inst-plugin-delete-06`
6. [ ] - `p1` - **IF** the plugin is in use - `inst-plugin-delete-07`
   1. [ ] - `p1` - **RETURN** 409 Conflict with the plugin-in-use problem document naming referencing upstreams and routes - `inst-plugin-delete-08`
7. [ ] - `p1` - **ELSE** - `inst-plugin-delete-09`
   1. [ ] - `p1` - Remove the stored plugin definition and mark its lifecycle terminal - `inst-plugin-delete-10`
8. [ ] - `p1` - **RETURN** 204 No Content - `inst-plugin-delete-11`

## 3. Processes / Business Logic (CDSL)

### Plugin Create Validation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-create-validation`

**Input**: Create-plugin request payload (plugin type, name, config schema, declared phases, source text) and the calling tenant.

**Output**: A validated plugin entity ready for persistence, or a validation error with field-level detail.

**Steps**:
1. [ ] - `p1` - Parse `plugin_type` from the payload and reject any value outside `auth`, `guard`, `transform` - `inst-plugin-createval-01`
2. [ ] - `p1` - Validate the `name` field is present and non-empty - `inst-plugin-createval-02`
3. [ ] - `p1` - Query stored plugin definitions for the calling tenant by `name` - `inst-plugin-createval-03`
4. [ ] - `p1` - **IF** a plugin with the same name already exists for this tenant - `inst-plugin-createval-04`
   1. [ ] - `p1` - **RETURN** 409 Conflict, since plugin name is unique per tenant regardless of plugin type - `inst-plugin-createval-05`
5. [ ] - `p1` - Validate `config_schema` is a well-formed JSON Schema object - `inst-plugin-createval-06`
6. [ ] - `p1` - **FOR EACH** declared phase in the payload's `phases` array - `inst-plugin-createval-07`
   1. [ ] - `p1` - Reject the phase unless it belongs to the phase set permitted for the declared `plugin_type`, per `cpt-cf-oagw-adr-plugin-system`: `guard` permits `on_request` and `on_response`; `transform` permits `on_request`, `on_response`, and `on_error`; `auth` permits no declared phases - `inst-plugin-createval-08`
7. [ ] - `p1` - **IF** any prior validation step failed - `inst-plugin-createval-09`
   1. [ ] - `p1` - **RETURN** 400 ValidationError aggregating every field-level failure - `inst-plugin-createval-10`
8. [ ] - `p1` - **RETURN** the validated plugin entity with a newly generated UUID, ready to persist as Registered - `inst-plugin-createval-11`

### Plugin Reference Resolution

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-ref-resolution`

**Input**: A GTS plugin identifier string (`gts.cf.core.oagw.{type}_plugin.v1~{instance}`) and the calling tenant.

**Output**: A resolved plugin descriptor (stored custom plugin or in-process named plugin), or a not-found error.

**Steps**:
1. [ ] - `p1` - Parse the identifier's base type to extract `{type}` and validate it is one of `auth`, `guard`, `transform` - `inst-plugin-refres-01`
2. [ ] - `p1` - Extract the instance part following `~` - `inst-plugin-refres-02`
3. [ ] - `p1` - **IF** the instance part parses as a UUID - `inst-plugin-refres-03`
   1. [ ] - `p1` - Look up the stored plugin definition by `id` in the in-process control-plane store, scoped to the calling tenant - `inst-plugin-refres-04`
   2. [ ] - `p1` - **IF** no row exists, or the stored `plugin_type` does not match `{type}` - `inst-plugin-refres-05`
      1. [ ] - `p1` - **RETURN** a not-found result - `inst-plugin-refres-06`
4. [ ] - `p1` - **ELSE** - `inst-plugin-refres-07`
   1. [ ] - `p1` - Look up the instance part in the in-process named-plugin registry for `{type}` - `inst-plugin-refres-08`
   2. [ ] - `p1` - **IF** the identifier is not registered - `inst-plugin-refres-09`
      1. [ ] - `p1` - **RETURN** a not-found result - `inst-plugin-refres-10`
5. [ ] - `p1` - **RETURN** the resolved plugin descriptor with its type, identity, and storage origin - `inst-plugin-refres-11`

### Plugin In-Use Detection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-in-use-detection`

**Input**: A resolved custom plugin identifier and the calling tenant.

**Output**: Whether the plugin is bound to any upstream or route, with the identifiers of every referencing resource.

**Steps**:
1. [ ] - `p1` - Normalize the plugin identifier to its canonical GTS string form - `inst-plugin-inuse-01`
2. [ ] - `p1` - Scan the calling tenant's upstream auth-plugin reference in the in-process control-plane store for a match - `inst-plugin-inuse-02`
3. [ ] - `p1` - Scan the calling tenant's upstream guard/transform plugin bindings in the in-process control-plane store for a match - `inst-plugin-inuse-03`
4. [ ] - `p1` - Scan the calling tenant's route guard/transform plugin bindings in the in-process control-plane store for a match - `inst-plugin-inuse-04`
5. [ ] - `p1` - **FOR EACH** match found in the prior three scans - `inst-plugin-inuse-05`
   1. [ ] - `p1` - Collect the referencing upstream identifier or route identifier - `inst-plugin-inuse-06`
6. [ ] - `p1` - **IF** any references were collected - `inst-plugin-inuse-07`
   1. [ ] - `p1` - **RETURN** in-use with the collected upstream and route identifier lists - `inst-plugin-inuse-08`
7. [ ] - `p1` - **ELSE** - `inst-plugin-inuse-09`
   1. [ ] - `p1` - **RETURN** not-in-use - `inst-plugin-inuse-10`

## 4. States (CDSL)

### Plugin Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-plugin-lifecycle`

**States**: Registered, Bound, Unlinked, Deleted

**Initial State**: Registered

**Transitions**:
1. [ ] - `p1` - **FROM** Registered **TO** Bound **WHEN** an upstream or route binding first references the plugin - `inst-plugin-state-01`
2. [ ] - `p1` - **FROM** Bound **TO** Unlinked **WHEN** the last upstream or route binding referencing the plugin is removed - `inst-plugin-state-02`
3. [ ] - `p1` - **FROM** Unlinked **TO** Bound **WHEN** an upstream or route binding references the plugin again - `inst-plugin-state-03`
4. [ ] - `p1` - **FROM** Registered **TO** Deleted **WHEN** a delete request succeeds while no binding has ever referenced the plugin - `inst-plugin-state-04`
5. [ ] - `p1` - **FROM** Unlinked **TO** Deleted **WHEN** a delete request succeeds while no binding currently references the plugin - `inst-plugin-state-05`
6. [ ] - `p1` - **FROM** Bound **TO** Bound **WHEN** a delete request is rejected with a plugin-in-use conflict - `inst-plugin-state-06`

## 5. Definitions of Done

### Create Custom Plugin

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-create`

The system **MUST** persist a new custom plugin definition from a validated create payload at `POST /oagw/v1/plugins`, assigning a UUID-backed GTS identifier.

**Implements**:
- `cpt-cf-oagw-flow-register-plugin`

**Touches**:
- API: `POST /oagw/v1/plugins`
- Entities: `Plugin`

### Enforce Per-Tenant Plugin Name Uniqueness

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-name-uniqueness`

The system **MUST** reject a create request naming a plugin whose `name` already exists for the calling tenant, regardless of plugin type, with `409 Conflict`.

**Implements**:
- `cpt-cf-oagw-algo-plugin-create-validation`

**Touches**:
- API: `POST /oagw/v1/plugins`
- Entities: `Plugin`

### Validate Plugin Config Schema and Declared Phases

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-config-schema-validation`

The system **MUST** reject a create request whose `config_schema` is not a well-formed JSON Schema object, or whose declared `phases` fall outside the set permitted for the payload's plugin type — `guard`: `on_request`, `on_response`; `transform`: `on_request`, `on_response`, `on_error`; `auth`: none — per `cpt-cf-oagw-adr-plugin-system`, with `400 ValidationError`.

**Implements**:
- `cpt-cf-oagw-algo-plugin-create-validation`

**Touches**:
- API: `POST /oagw/v1/plugins`
- Entities: `Plugin`

### List Plugins with Query Parameters

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-list`

The system **MUST** list the calling tenant's stored plugin definitions at `GET /oagw/v1/plugins`, honoring `$filter`, `$select`, `$top`, and `$skip`.

**Implements**:
- `cpt-cf-oagw-flow-list-plugins`

**Touches**:
- API: `GET /oagw/v1/plugins`
- Entities: `Plugin`

### Get Plugin by ID

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-get`

The system **MUST** return a tenant-owned plugin resource at `GET /oagw/v1/plugins/{id}`, or a plain `404` problem document when the identifier does not resolve for the calling tenant.

**Implements**:
- `cpt-cf-oagw-flow-get-plugin`
- `cpt-cf-oagw-algo-plugin-ref-resolution`

**Touches**:
- API: `GET /oagw/v1/plugins/{id}`
- Entities: `Plugin`

### Get Plugin Source

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-get-source`

The system **MUST** return the stored source text of a UUID-backed plugin at `GET /oagw/v1/plugins/{id}/source`, and a plain `404` problem document for a named built-in identifier or an unresolved custom identifier.

**Implements**:
- `cpt-cf-oagw-flow-get-plugin-source`
- `cpt-cf-oagw-algo-plugin-ref-resolution`

**Touches**:
- API: `GET /oagw/v1/plugins/{id}/source`
- Entities: `Plugin`

### Delete Plugin with In-Use Conflict Detection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-delete`

The system **MUST** delete an unreferenced tenant-owned plugin at `DELETE /oagw/v1/plugins/{id}` with `204 No Content`, and **MUST** reject deletion with `409 Conflict` naming every referencing upstream and route when the plugin is still bound.

**Implements**:
- `cpt-cf-oagw-flow-delete-plugin`
- `cpt-cf-oagw-algo-plugin-in-use-detection`

**Touches**:
- API: `DELETE /oagw/v1/plugins/{id}`
- Entities: `Plugin`

### Plugin Identification Model Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-identification`

The system **MUST** distinguish UUID-backed custom plugin identifiers from named built-in identifiers on every read, source, and delete path, resolving each against the correct store.

**Implements**:
- `cpt-cf-oagw-algo-plugin-ref-resolution`

**Touches**:
- API: `GET /oagw/v1/plugins/{id}`
- API: `GET /oagw/v1/plugins/{id}/source`
- API: `DELETE /oagw/v1/plugins/{id}`
- Entities: `Plugin`

### No Replace Operation on Plugin Resources

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-no-replace`

The system **MUST NOT** serve any update or replace verb on `/oagw/v1/plugins/{id}`; changing a plugin's behavior **MUST** require creating a new plugin and rebinding upstream or route references to it.

**Implements**:
- `cpt-cf-oagw-flow-register-plugin`

**Touches**:
- API: `POST /oagw/v1/plugins`
- Entities: `Plugin`

### Track Plugin Lifecycle State

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-lifecycle-tracking`

The system **MUST** track each stored plugin's lifecycle across Registered, Bound, and Unlinked states as upstream and route bindings are added and removed.

**Implements**:
- `cpt-cf-oagw-state-plugin-lifecycle`

**Touches**:
- Entities: `Plugin`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/plugins` with a valid payload returns `201 Created` with a body whose `id` matches `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`.
- [ ] `POST /oagw/v1/plugins` naming a `name` already used by another plugin for the same tenant returns `409 Conflict`.
- [ ] `POST /oagw/v1/plugins` with a `plugin_type` outside `auth`, `guard`, `transform` returns `400 ValidationError`.
- [ ] `POST /oagw/v1/plugins` with a `config_schema` that is not a well-formed JSON Schema object returns `400 ValidationError`.
- [ ] `POST /oagw/v1/plugins` with `plugin_type` set to `auth` and `phases` set to `["on_request"]` returns `400 ValidationError`, since auth plugins permit no declared phases.
- [ ] `GET /oagw/v1/plugins` accepts `$filter`, `$select`, `$top`, and `$skip` and returns only the calling tenant's stored plugin definitions.
- [ ] `GET /oagw/v1/plugins/{id}` for an existing tenant-owned plugin returns `200` with the plugin resource body.
- [ ] `GET /oagw/v1/plugins/{id}` response body carries no `source_code` or other source-text field; the stored source is retrievable only from `GET /oagw/v1/plugins/{id}/source`.
- [ ] `GET /oagw/v1/plugins/{id}` for an identifier not owned by the calling tenant returns `404` with an `application/problem+json` body.
- [ ] `GET /oagw/v1/plugins/{id}/source` for a UUID-backed plugin returns `200` with the stored source text.
- [ ] `GET /oagw/v1/plugins/{id}/source` for a named built-in plugin identifier returns `404` with an `application/problem+json` body.
- [ ] `DELETE /oagw/v1/plugins/{id}` for an unreferenced plugin returns `204 No Content`.
- [ ] `DELETE /oagw/v1/plugins/{id}` for a plugin bound to an upstream or route returns `409 Conflict` with `application/problem+json` fields `type`, `title`, `status`, `detail`, `plugin_id`, and `referenced_by.upstreams`/`referenced_by.routes` naming every referencing resource.
- [ ] No `PUT` or `PATCH` method is served on `/oagw/v1/plugins/{id}`; attempting either returns a routing-level rejection, not a successful replace.
- [ ] Every route registered by this feature is reachable at `/oagw/v1/plugins...` and never under an `/api/oagw/v1/...` prefix.
