# Feature: Plugin Catalog and Bindings


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Non-Applicability Dispositions](#15-non-applicability-dispositions)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Custom Plugin Definition](#create-custom-plugin-definition)
  - [List Plugin Catalog](#list-plugin-catalog)
  - [Retrieve Plugin Definition](#retrieve-plugin-definition)
  - [Retrieve Custom Plugin Source](#retrieve-custom-plugin-source)
  - [Delete Plugin Definition](#delete-plugin-definition)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Validate Plugin Binding List](#validate-plugin-binding-list)
  - [Check Plugin In-Use](#check-plugin-in-use)
- [4. States (CDSL)](#4-states-cdsl)
  - [Custom Plugin Definition Lifecycle](#custom-plugin-definition-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Serve the Built-in and Catalog-Only Plugin Catalog](#serve-the-built-in-and-catalog-only-plugin-catalog)
  - [Manage Immutable Custom Plugin Definitions](#manage-immutable-custom-plugin-definitions)
  - [Reject Deletion of an In-Use Plugin](#reject-deletion-of-an-in-use-plugin)
  - [Validate Ordered Plugin Bindings at Write Time](#validate-ordered-plugin-bindings-at-write-time)
  - [Document Deferred Execution of Custom Plugin Source](#document-deferred-execution-of-custom-plugin-source)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-pm-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-plugin-management`
## 1. Feature Context

### 1.1 Overview

This feature maintains the catalog of Auth, Guard, and Transform plugins that upstreams and routes can bind to, manages the create/list/read/delete lifecycle of immutable custom plugin definitions, and validates every plugin binding written onto an upstream or route.

### 1.2 Purpose

The gateway needs a single, consistent inventory of what a request-processing chain can be built from: built-in behaviors that ship with the gear, catalog identifiers that are reserved but not runnable, and tenant-authored custom definitions. This feature owns that inventory and the write-time checks (identifier resolution, ordering, in-use protection) that keep an upstream's or route's `plugins` configuration internally consistent before it ever reaches a live request.

Every store operation named in Sections 2 and 3 below (create, list, get, get-source, delete, in-use check) reads or writes the single shared in-memory control-plane store that Gear Foundation creates (`cpt-cf-oagw-algo-gf-init-state`), not a SQL database: the graded configuration runs with no `database:` section, so `oagw_plugin`, `oagw_upstream_plugin`, and `oagw_route_plugin` — the entities `cpt-cf-oagw-db-schema` documents for the persisted deployment — are held as in-process state with the same identity, tenant-scoping, and referential relationships the persisted-deployment schema describes.

**Requirements**: `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-builtin-plugins`, `cpt-cf-oagw-nfr-starlark-sandbox`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Reviews the built-in plugin catalog and manages system-wide custom plugin definitions and their bindings. |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, inspects, and deletes tenant-scoped custom plugin definitions and binds them to that tenant's upstreams and routes. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`

### 1.5 Non-Applicability Dispositions

- **Inbound authentication and authorization**: performed by the host runtime before a request reaches this feature; this feature performs no OAGW-specific permission check of its own beyond the ordinary tenant scoping documented in each flow (an identifier that resolves to another tenant's custom definition is `404`, not `403`).
- **User interface**: this feature exposes no user interface, so accessibility and UX checklist domains are not applicable.
- **Regulated or personal data**: this feature stores no regulated or personal data; a custom plugin definition's stored source text is tenant-authored code, not personal data, and this feature does not execute it (`cpt-cf-oagw-dod-pm-execution-deferral`).

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: None. PRD.md defines no dedicated use case for plugin catalog management; the flows below realize `cpt-cf-oagw-fr-plugin-system` and `cpt-cf-oagw-fr-builtin-plugins` directly.

### Create Custom Plugin Definition

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-pm-create`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The actor submits a plugin type (auth, guard, or transform), a name, a configuration schema, and source text; the system stores an immutable definition and returns it with a server-generated identifier.

**Error Scenarios**:
- The submitted plugin type, name, configuration schema, or source text fails structural validation.
- A definition with the same name already exists for the calling tenant.

**Steps**:
1. [ ] - `p2` - Actor submits a plugin definition naming its plugin kind (auth, guard, or transform), a display name, a description, a configuration schema, and Starlark source text - `inst-pm-create-submit`
2. [ ] - `p2` - API: POST /oagw/v1/plugins (plugin kind, name, description, config schema, source text -> created plugin definition) - `inst-pm-create-api`
3. [ ] - `p2` - Validate that the plugin kind is one of auth, guard, or transform and that the configuration schema and source text are present and well-formed - `inst-pm-create-validate`
4. [ ] - `p2` - **IF** validation fails - `inst-pm-create-if-invalid`
   1. [ ] - `p2` - **RETURN** 400 validation error identifying the offending field - `inst-pm-create-400`
5. [ ] - `p2` - **ELSE** - `inst-pm-create-else`
   1. [ ] - `p2` - Generate a UUID-backed anonymous GTS identifier scoped to the plugin kind (`gts.cf.core.oagw.{kind}_plugin.v1~{uuid}`) - `inst-pm-create-gen-id`
   2. [ ] - `p2` - Persist the definition, scoped to the calling tenant, together with its identifier, plugin kind, name, description, configuration schema, and source text - `inst-pm-create-insert`
6. [ ] - `p2` - **RETURN** 201 with the stored plugin definition, including its identifier and source text - `inst-pm-create-201`

### List Plugin Catalog

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-pm-list`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The actor lists the combined catalog: built-in entries, catalog-only entries, and the calling tenant's custom definitions, each tagged with its plugin kind and whether it is actually resolvable at binding time.

**Error Scenarios**:
- None; an empty tenant scope returns the built-in and catalog-only entries with an empty custom-definition set.

**Steps**:
1. [ ] - `p2` - Actor requests the plugin catalog, optionally filtered by plugin kind, with pagination parameters - `inst-pm-list-submit`
2. [ ] - `p2` - API: GET /oagw/v1/plugins (`$filter`, `$select`, `$top` default 50 max 100, `$skip` -> paginated list) - `inst-pm-list-api`
3. [ ] - `p2` - Assemble the fixed set of built-in and catalog-only entries for all three plugin kinds - `inst-pm-list-builtins`
4. [ ] - `p2` - Retrieve the calling tenant's custom definitions from the plugin store - `inst-pm-list-query`
5. [ ] - `p2` - Merge built-in, catalog-only, and custom entries, apply the requested filter and pagination - `inst-pm-list-merge`
6. [ ] - `p2` - **RETURN** 200 with the merged, paginated list - `inst-pm-list-200`

### Retrieve Plugin Definition

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-pm-get`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The actor retrieves a single catalog entry (built-in, catalog-only, or the tenant's own custom definition) by its GTS identifier.

**Error Scenarios**:
- The identifier does not resolve to any catalog entry, or resolves to a custom definition owned by another tenant.

**Steps**:
1. [ ] - `p2` - Actor requests a plugin definition by its GTS identifier - `inst-pm-get-submit`
2. [ ] - `p2` - API: GET /oagw/v1/plugins/{id} (GTS plugin identifier -> plugin definition) - `inst-pm-get-api`
3. [ ] - `p2` - Parse the identifier's instance part; a UUID resolves against custom definitions, otherwise it resolves against the built-in/catalog-only registry - `inst-pm-get-resolve`
4. [ ] - `p2` - **IF** the identifier is UUID-backed - `inst-pm-get-if-uuid`
   1. [ ] - `p2` - Look up the definition by identifier, scoped to the calling tenant - `inst-pm-get-query`
5. [ ] - `p2` - **IF** no entry is found, or a UUID-backed definition belongs to another tenant - `inst-pm-get-if-missing`
   1. [ ] - `p2` - **RETURN** 404 not found - `inst-pm-get-404`
6. [ ] - `p2` - **ELSE** - `inst-pm-get-else`
   1. [ ] - `p2` - **RETURN** 200 with the resolved definition - `inst-pm-get-200`

### Retrieve Custom Plugin Source

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-pm-get-source`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The actor retrieves the stored source text of a custom plugin definition owned by their tenant.

**Error Scenarios**:
- The identifier names a built-in or catalog-only entry (no source text exists), an unknown identifier, or a custom definition owned by another tenant.

**Steps**:
1. [ ] - `p2` - Actor requests the source text of a plugin definition by its GTS identifier - `inst-pm-src-submit`
2. [ ] - `p2` - API: GET /oagw/v1/plugins/{id}/source (GTS plugin identifier -> source text) - `inst-pm-src-api`
3. [ ] - `p2` - **IF** the identifier is not UUID-backed (built-in or catalog-only) - `inst-pm-src-if-named`
   1. [ ] - `p2` - **RETURN** 404 not found (named plugins carry no source text) - `inst-pm-src-404-named`
4. [ ] - `p2` - **ELSE** - `inst-pm-src-else`
   1. [ ] - `p2` - Look up the definition's source text by identifier, scoped to the calling tenant - `inst-pm-src-query`
   2. [ ] - `p2` - **IF** no row is found, or the row belongs to another tenant - `inst-pm-src-if-missing`
      1. [ ] - `p2` - **RETURN** 404 not found - `inst-pm-src-404`
   3. [ ] - `p2` - **ELSE** - `inst-pm-src-else-found`
      1. [ ] - `p2` - **RETURN** 200 with the stored source text - `inst-pm-src-200`

### Delete Plugin Definition

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-pm-delete`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The actor deletes a custom plugin definition that is bound to no upstream or route.

**Error Scenarios**:
- The identifier names a built-in or catalog-only entry, an unknown identifier, or a custom definition owned by another tenant.
- The custom definition is still referenced by at least one upstream or route binding.

**Steps**:
1. [ ] - `p2` - Actor requests deletion of a plugin definition by its GTS identifier - `inst-pm-del-submit`
2. [ ] - `p2` - API: DELETE /oagw/v1/plugins/{id} (GTS plugin identifier -> no content, or conflict) - `inst-pm-del-api`
3. [ ] - `p2` - **IF** the identifier is not UUID-backed (built-in or catalog-only) - `inst-pm-del-if-named`
   1. [ ] - `p2` - **RETURN** 404 not found (built-in and catalog-only entries are not deletable resources) - `inst-pm-del-404-named`
4. [ ] - `p2` - **ELSE** - `inst-pm-del-else`
   1. [ ] - `p2` - Look up the definition by identifier, scoped to the calling tenant - `inst-pm-del-query`
   2. [ ] - `p2` - **IF** no definition is found, or it belongs to another tenant - `inst-pm-del-if-missing`
      1. [ ] - `p2` - **RETURN** 404 not found - `inst-pm-del-404`
   3. [ ] - `p2` - **ELSE** - `inst-pm-del-else-found`
      1. [ ] - `p2` - Run `cpt-cf-oagw-algo-pm-check-in-use` against the resolved identifier - `inst-pm-del-check`
      2. [ ] - `p2` - **IF** the identifier is referenced by any upstream or route binding - `inst-pm-del-if-inuse`
         1. [ ] - `p2` - **RETURN** 409 with error type PluginInUse and a `referenced_by` body shaped as an object with two arrays, `upstreams` and `routes`, each holding the identifiers of the referencing resources - `inst-pm-del-409`
      3. [ ] - `p2` - **ELSE** - `inst-pm-del-else-free`
         1. [ ] - `p2` - Remove the definition from the plugin store - `inst-pm-del-delete`
         2. [ ] - `p2` - **RETURN** 204 no content - `inst-pm-del-204`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Validate Plugin Binding List

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-pm-validate-binding`

This process is invoked by the upstream and route write flows owned by `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-route-management` whenever a `plugins.items` list (guard and transform bindings) or, for an upstream, a standalone auth-plugin reference is submitted; those flows do not duplicate this validation.

**Deviation from DESIGN.md**: `cpt-cf-oagw-design-domain-model` describes a plugin binding with a `(position, plugin_ref, plugin_uuid, config)` shape — an explicit per-entry position field, an identifier, and a configuration payload. The frozen `upstream.v1.schema.json` and `route.v1.schema.json` both define `plugins.items` as a flat array of plain identifier strings, with no per-entry position field and no per-entry configuration payload on the wire. `(position, plugin_ref, plugin_uuid, config)` describes the persisted deployment's storage shape only; the wire contract this process validates against is the plain string array, an entry's position is simply its index in that array, and no configuration payload accompanies an entry on the wire in this configuration.

**Input**: The submitting resource's `plugins.items` array — a flat array of plain plugin-identifier strings, each either a full GTS identifier or, on upstream writes only, a bare UUID string (the frozen `route.v1.schema.json` admits only the GTS form for route `plugins.items` entries) — and, separately, the upstream's single auth-plugin identifier field (routes carry no `auth` property, so this half of the input applies to upstream writes only; see PM-3).

**Output**: The validated bindings, in their submitted array order, ready for storage; plus the validated auth-plugin identifier for an upstream write; or a rejection naming the first offending entry.

**Steps**:
1. [ ] - `p2` - Treat the auth-plugin identifier as a scalar field on the upstream resource, never as an entry in the `plugins.items` array - `inst-pm-bind-auth-scalar`
2. [ ] - `p2` - **FOR EACH** entry in the submitted `plugins.items` array, in array order (the entry's position is its zero-based array index; the wire format carries no separate position field) - `inst-pm-bind-foreach`
   1. [ ] - `p2` - **IF** the entry contains a `~` separator - `inst-pm-bind-if-gts`
      1. [ ] - `p2` - Parse it as a full GTS identifier and take the instance part following `~` - `inst-pm-bind-parse-gts`
   2. [ ] - `p2` - **ELSE** (the entry is a bare UUID string with no `~`; accepted on upstream writes only) - `inst-pm-bind-else-bare-uuid`
      1. [ ] - `p2` - Treat the entry directly as the instance part - `inst-pm-bind-parse-bare-uuid`
   3. [ ] - `p2` - **IF** the instance part parses as a UUID - `inst-pm-bind-if-uuid`
      1. [ ] - `p2` - Resolve against the tenant's custom plugin definitions and confirm the definition's plugin kind is guard or transform; both the GTS form and the bare-UUID form of the same instance part resolve to the same custom plugin definition - `inst-pm-bind-resolve-uuid`
   4. [ ] - `p2` - **ELSE** - `inst-pm-bind-else-named`
      1. [ ] - `p2` - Resolve against the built-in registry for the guard or transform plugin kinds - `inst-pm-bind-resolve-named`
   5. [ ] - `p2` - **IF** resolution fails because the identifier is unknown, names a catalog-only entry with no backing implementation, or names an entry of a plugin kind other than guard or transform - `inst-pm-bind-if-unresolved`
      1. [ ] - `p2` - **RETURN** 400 validation error identifying the offending array index and identifier, rejecting the entire binding list - `inst-pm-bind-400`
3. [ ] - `p2` - **IF** an auth-plugin identifier is present on the upstream resource (never on a route, which has no `auth` field) - `inst-pm-bind-if-auth`
   1. [ ] - `p2` - Resolve it using the same `~`/bare-UUID parsing rule, requiring plugin kind auth, and reject with a 400 validation error naming the auth field if resolution fails - `inst-pm-bind-auth-resolve`
4. [ ] - `p2` - **RETURN** the validated ordered bindings, in their submitted array order — an entry may resolve to a guard or a transform in either order; `plugins.items` is a flat array with no per-entry slot marker, and partitioning the chain into guard-then-transform execution order is a runtime concern owned by `cpt-cf-oagw-feature-traffic-policy`, not this validation step — plus the validated auth-plugin identifier when present, ready for persistence - `inst-pm-bind-return`

### Check Plugin In-Use

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-pm-check-in-use`

**Input**: A UUID-backed custom plugin identifier belonging to the calling tenant.

**Output**: A `referenced_by` object with two arrays, `upstreams` and `routes`, each holding the distinct identifiers of the resources that currently reference the plugin; both arrays are empty when the plugin is unreferenced.

**Steps**:
1. [ ] - `p2` - Scan the upstream plugin bindings for entries referencing this identifier, collecting the distinct parent upstream identifiers - `inst-pm-inuse-upstream-plugin`
2. [ ] - `p2` - Scan the route plugin bindings for entries referencing this identifier, collecting the distinct parent route identifiers - `inst-pm-inuse-route-plugin`
3. [ ] - `p2` - Scan upstream records whose auth-plugin field references this identifier, collecting the distinct upstream identifiers bound via that scalar field - `inst-pm-inuse-auth-column`
4. [ ] - `p2` - Merge the upstream identifiers from steps 1 and 3 into `referenced_by.upstreams`, and the route identifiers from step 2 into `referenced_by.routes`, each deduplicated - `inst-pm-inuse-merge`
5. [ ] - `p2` - **RETURN** the `referenced_by` object with its `upstreams` and `routes` arrays - `inst-pm-inuse-return`

## 4. States (CDSL)

### Custom Plugin Definition Lifecycle

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-pm-lifecycle`

**States**: Active, Deleted

**Initial State**: Active

**Transitions**:
1. [ ] - `p2` - **FROM** Active **TO** Deleted **WHEN** the owning tenant requests deletion and `cpt-cf-oagw-algo-pm-check-in-use` reports no referencing upstream or route - `inst-pm-state-delete`

No Updated or Replaced state exists: `cpt-cf-oagw-principle-plugin-immutable` means a definition has exactly one content revision for its entire Active lifetime; a behavior change is always a new definition (a new Active instance) plus re-binding, never a transition on the existing one.

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Serve the Built-in and Catalog-Only Plugin Catalog

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-pm-catalog`

The system **MUST** serve a fixed catalog covering all three plugin kinds: for Auth, the served entries `noop`, `apikey`, `oauth2_client_cred`, and `oauth2_client_cred_basic` alongside the catalog-only entries `basic` and `bearer`, which carry no backing implementation; for Guard, the served entry `required_headers` alongside the catalog-only entries `timeout` and `cors`; for Transform, the served entry `request_id` alongside the catalog-only entries `logging` and `metrics`. Each catalog entry **MUST** report whether it is actually resolvable at binding time or is catalog-only.

**Implements**:
- `cpt-cf-oagw-flow-pm-list`
- `cpt-cf-oagw-flow-pm-get`

**Touches**:
- API: `GET /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Plugin`

### Manage Immutable Custom Plugin Definitions

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-pm-custom-crud`

The system **MUST** support creating, listing, retrieving, retrieving the source text of, and deleting tenant-scoped custom plugin definitions, and **MUST NOT** expose any operation that replaces or mutates a definition's stored plugin kind, configuration schema, or source text after creation; a behavior change is always a new definition plus a re-binding of the resources that reference it.

**Implements**:
- `cpt-cf-oagw-flow-pm-create`
- `cpt-cf-oagw-flow-pm-list`
- `cpt-cf-oagw-flow-pm-get`
- `cpt-cf-oagw-flow-pm-get-source`
- `cpt-cf-oagw-flow-pm-delete`

**Touches**:
- API: `POST /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins/{id}`
- API: `GET /oagw/v1/plugins/{id}/source`
- API: `DELETE /oagw/v1/plugins/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

### Reject Deletion of an In-Use Plugin

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-pm-in-use-delete`

The system **MUST** reject deletion of a custom plugin definition that is referenced by at least one upstream or route binding with a 409 response whose body identifies the error as PluginInUse and carries a `referenced_by` object with two arrays, `upstreams` and `routes`, each listing the identifiers of the referencing resources (empty when that resource type does not reference the plugin).

**Implements**:
- `cpt-cf-oagw-flow-pm-delete`
- `cpt-cf-oagw-algo-pm-check-in-use`

**Touches**:
- API: `DELETE /oagw/v1/plugins/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

### Validate Ordered Plugin Bindings at Write Time

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-pm-binding-validation`

The system **MUST**, on every upstream or route write that carries a `plugins.items` list, resolve every entry's identifier — a full GTS identifier, or, on upstream writes only, a bare UUID string, per the frozen wire schemas (the route schema admits only the GTS form) — with the UUID-backed instance resolved against custom definitions and the named form resolved against the built-in registry, and reject the whole write with a 400 validation error naming the offending array index when any entry names an identifier that does not resolve, names a catalog-only entry with no backing implementation, or names an entry of a plugin kind other than guard or transform. The system **MUST**, on every upstream write that carries an auth-plugin field (routes carry no `auth` property), keep that identity on the upstream's dedicated `auth` field rather than as an entry in `plugins.items`. `plugins.items` ordering **MUST** be treated as the entry's array index; the wire format carries no per-entry position field or configuration payload (see the Deviation from DESIGN.md note under `cpt-cf-oagw-algo-pm-validate-binding`).

**Implements**:
- `cpt-cf-oagw-algo-pm-validate-binding`

**Touches**:
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_upstream_plugin`
- DB Table: `oagw_route_plugin`
- Entities: `Plugin`

### Document Deferred Execution of Custom Plugin Source

- [ ] `p2` - **ID**: `cpt-cf-oagw-dod-pm-execution-deferral`

The system **MUST** store a custom plugin definition's source text without interpreting or executing it in this configuration: `cpt-cf-oagw-nfr-starlark-sandbox` requires a sandboxed execution environment (no network I/O, no file I/O, no imports, enforced timeout and memory limits), and building that sandbox is explicitly out of this feature's scope, deferred to the data-plane plugin-chain execution work that consumes these bindings. A custom plugin bound to an upstream or route **MUST** therefore behave as a documented no-op wherever the plugin chain would otherwise invoke it at request time, rather than being silently skipped without record or causing a request failure.

**Implements**:
- `cpt-cf-oagw-flow-pm-create`
- `cpt-cf-oagw-algo-pm-validate-binding`

**Touches**:
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/plugins` with a valid auth, guard, or transform definition returns 201 with a body containing a server-generated UUID-backed GTS identifier matching the submitted plugin kind.
- [ ] `POST /oagw/v1/plugins` with a missing or unrecognized plugin kind, or with malformed configuration schema or source text, returns 400.
- [ ] `GET /oagw/v1/plugins` returns 200 with a body listing, for each of the three plugin kinds, exactly the served built-in identifiers (`noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic` for auth; `required_headers` for guard; `request_id` for transform) and the catalog-only identifiers (`basic`, `bearer` for auth; `timeout`, `cors` for guard; `logging`, `metrics` for transform), each entry marked with its resolvable-or-catalog-only status, plus the calling tenant's own custom definitions.
- [ ] `GET /oagw/v1/plugins` respects `$top` (default 50, max 100) and `$skip` pagination on the returned list.
- [ ] `GET /oagw/v1/plugins/{id}` for a served built-in identifier, a catalog-only identifier, or the calling tenant's own custom definition returns 200 with that entry's details.
- [ ] `GET /oagw/v1/plugins/{id}` for an identifier that resolves to no catalog entry, or to a custom definition owned by a different tenant, returns 404.
- [ ] `GET /oagw/v1/plugins/{id}/source` for a UUID-backed custom plugin definition owned by the calling tenant returns 200 with the exact source text supplied at creation.
- [ ] `GET /oagw/v1/plugins/{id}/source` for a built-in or catalog-only identifier, an unresolvable identifier, or a custom definition owned by a different tenant returns 404.
- [ ] There is no `PUT /oagw/v1/plugins/{id}` route; a plugin definition's stored plugin kind, configuration schema, and source text are unchanged for the lifetime of its identifier, and the only way to change plugin behavior is creating a new plugin definition via `POST /oagw/v1/plugins` and re-binding the upstream or route to the new identifier.
- [ ] `DELETE /oagw/v1/plugins/{id}` for a custom plugin definition referenced by no upstream or route binding returns 204, and a subsequent `GET /oagw/v1/plugins/{id}` for that identifier returns 404.
- [ ] `DELETE /oagw/v1/plugins/{id}` for a custom plugin definition currently bound to at least one upstream or route returns 409 with a body identifying the error as PluginInUse and a `referenced_by` object whose `referenced_by.upstreams` and `referenced_by.routes` array fields list the identifiers of every referencing upstream and every referencing route respectively.
- [ ] `DELETE /oagw/v1/plugins/{id}` for a served built-in or catalog-only identifier returns 404, since neither is a deletable resource.
- [ ] Submitting an upstream or route write whose `plugins.items` list contains an identifier that resolves to neither a custom definition nor a built-in registry entry is rejected with 400 and the write does not persist any part of the submitted binding list.
- [ ] Submitting an upstream or route write whose `plugins.items` list names a catalog-only identifier (for example the guard `timeout` or `cors` identifier) is rejected with 400, distinguishing it from a successfully bound served identifier of the same plugin kind.
- [ ] An upstream write whose `plugins.items` entry is a bare UUID string (no `~`) resolves to the same custom plugin definition as the equivalent full GTS identifier form for that instance; a route write whose `plugins.items` entry is a bare UUID string is rejected with 400, since the frozen `route.v1.schema.json` admits only the GTS form.
- [ ] A `plugins.items` array's stored order matches its submitted array order exactly; there is no separate position field on the wire, and an entry may resolve to a guard or a transform in either order within the array.
- [ ] An upstream's auth-plugin identity is accepted and stored as a single scalar field on `auth.type`, never as an entry in the `plugins.items` array; the Route schema carries no `auth` property, so no route write may submit a standalone auth-plugin identifier.
- [ ] A custom plugin definition successfully bound to an upstream or route does not cause request failures or execute its source text at proxy time in this configuration; the plugin-chain execution that would invoke it is owned by `cpt-cf-oagw-feature-traffic-policy`, and this feature's responsibility ends at storing the definition and validating the binding.
