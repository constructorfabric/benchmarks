# Feature: Plugin Management API


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Custom Plugin](#create-custom-plugin)
  - [List Plugins](#list-plugins)
  - [Get Plugin by ID](#get-plugin-by-id)
  - [Get Plugin Source](#get-plugin-source)
  - [Delete Plugin](#delete-plugin)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Plugin Identifier Parse and Classification](#plugin-identifier-parse-and-classification)
  - [Plugin Resolution](#plugin-resolution)
  - [Plugin Reference Counting](#plugin-reference-counting)
  - [Plugin GC Mark-Then-Sweep Bookkeeping](#plugin-gc-mark-then-sweep-bookkeeping)
- [4. States (CDSL)](#4-states-cdsl)
  - [Plugin Lifecycle State Machine](#plugin-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Cross-Cutting Dispositions](#cross-cutting-dispositions)
  - [Plugin CRUD Surface](#plugin-crud-surface)
  - [Anonymous GTS Identifier Issuance](#anonymous-gts-identifier-issuance)
  - [Plugin Identification and Resolution Model](#plugin-identification-and-resolution-model)
  - [Plugin-In-Use Deletion Protection](#plugin-in-use-deletion-protection)
  - [GC Bookkeeping Fields](#gc-bookkeeping-fields)
  - [Tenant Scoping](#tenant-scoping)
  - [Plugin Immutability](#plugin-immutability)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-plugin-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-plugin-management`
## 1. Feature Context

### 1.1 Overview

This feature delivers the create/list/get/delete lifecycle (no update — plugins are immutable) for custom Starlark Auth, Guard, and Transform plugin resources, plus the GTS-identifier-based plugin identification model — parsing, UUID-backed-vs-named classification, and resolution — that upstream-management, route-management, and plugin-execution depend on to reference plugins without executing them.

### 1.2 Purpose

Custom plugins let tenants extend OAGW's Auth/Guard/Transform chain with tenant-authored Starlark scripts, while named built-in plugins (compiled into the gear) must be referenceable from the same `plugins.items[]` and `auth.type` fields without a stored database row. This feature is the single owner of plugin storage, anonymous-GTS-identifier issuance, plugin-in-use protection on delete, and the parse/classify/resolve algorithm that every other Control-Plane and Data-Plane feature reuses to accept a `plugin_ref` value. It deliberately stops at storage and identification: it stores Starlark `source_code` as opaque text and returns it verbatim from `GET .../source`, but never parses, interprets, or sandboxes it. `cpt-cf-oagw-nfr-starlark-sandbox` is covered only to the "storage and identification only" extent the DECOMPOSITION entry's carve-out states; the sandboxed execution engine itself is deferred beyond this decomposition round because no execution engine for custom plugins exists in any of the nine features of this round.

**IMPORTANT — the `{id}` path parameter accepts both a bare UUID and the anonymous GTS form.** `DESIGN.md`'s Plugin domain-model class (`+UUID id`) and Resource Identification Pattern together establish the same reconciliation this decomposition applies to Upstream and Route: the persisted and returned `id` field on every plugin resource body is the bare UUID, while `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` is the addressable, bindable anonymous GTS form composed from `plugin_type` and that same UUID (issued at creation per `cpt-cf-oagw-dod-plugin-gts-issuance`), not a distinct stored value. `GET`/`DELETE /oagw/v1/plugins/{id}[/source]` **MUST** accept `{id}` supplied in either form for the same resource: `cpt-cf-oagw-algo-plugin-identifier-parse` already classifies a bare UUID directly, resolving `plugin_type` from the stored row itself (rather than requiring a `{type}_plugin` URL segment) when `{id}` is supplied as a bare UUID with no type context.

**Security**: all five operations require Bearer-token authentication and the per-plugin-type management permission from `DESIGN.md`'s Management API permissions table — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}` for Auth plugins, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}` for Guard plugins, and `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` for Transform plugins, selected by the request's `plugin_type` — in addition to tenant scoping (`cpt-cf-oagw-principle-tenant-scope`); this feature never stores or handles credential material — `secret_ref` values live on `Upstream.auth.config`, not on the `Plugin` resource — so `cpt-cf-oagw-nfr-credential-isolation` is not implicated here. Storing but never executing `source_code` means no code-injection or sandbox-escape surface is introduced by this feature.

**Reliability / Data integrity**: `POST` and `DELETE` are single-row, single-transaction operations; `DELETE` is guarded by a live reference-count check (`cpt-cf-oagw-algo-plugin-ref-count`) so a plugin still bound by an upstream or route can never be removed out from under a live configuration.

**Observability**: plugin create/delete are "config change" events per `cpt-cf-oagw-feature-gear-foundation`'s structured audit-log scaffold; this feature introduces no plugin-specific metrics beyond that shared scaffold.

**Rollback**: not applicable. Plugins are immutable (`cpt-cf-oagw-principle-plugin-immutable`) — there is no in-place update to roll back — and a mistaken create or delete is corrected the same way any plugin change is made: create a replacement plugin and re-bind upstream/route references to it.

**Requirements**: `cpt-cf-oagw-nfr-multi-tenancy`, `cpt-cf-oagw-nfr-starlark-sandbox`, `cpt-cf-oagw-interface-management-api`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`, `cpt-cf-oagw-principle-tenant-scope`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, inspects, and deletes system-wide custom plugins; relies on named built-in plugins being recognized as valid bindings without needing a stored row |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, lists, inspects, and deletes tenant-scoped custom plugins used by their own upstreams/routes |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation`

## 2. Actor Flows (CDSL)

None of the PRD `cpt-cf-oagw-usecase-*` entries name plugin CRUD directly; this feature's flows trace instead to `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-builtin-plugins`, and `cpt-cf-oagw-interface-management-api` (see §1.2 Purpose).

### Create Custom Plugin

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-create`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Operator submits a well-formed `plugin_type` (`auth`|`guard`|`transform`), `name`, `config_schema`, and `source_code`; the system stores the plugin, issues an anonymous GTS identifier, and returns `201 Created`.

**Error Scenarios**:
- Missing/invalid `plugin_type`, `name`, or `source_code`, or a `config_schema` that is not a well-formed JSON Schema object, → `400 ValidationError`.
- `(tenant_id, name)` collision with an existing stored plugin → `400 ValidationError` (this feature has no plugin-specific `409` other than `PluginInUse` on delete; a name collision is a request-validation failure caught before insert, backing the `(tenant_id, name)` uniqueness constraint documented in `cpt-cf-oagw-db-schema`).

**Steps**:
1. [x] - `p1` - Operator sends `POST /oagw/v1/plugins` with `{plugin_type, name, description?, config_schema?, source_code}` and a Bearer token - `inst-plugin-create-recv`
2. [x] - `p1` - Validate `plugin_type` is one of `auth`|`guard`|`transform`; `name` and `source_code` are non-empty strings; `config_schema`, if present, is a well-formed JSON Schema object - `inst-plugin-create-validate`
3. [x] - `p1` - **IF** validation fails - `inst-plugin-create-if-invalid`
   1. [x] - `p1` - **RETURN** `400 ValidationError` (`gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`) - `inst-plugin-create-400`
4. [x] - `p1` - **ELSE** - `inst-plugin-create-else-valid`
   1. [x] - `p1` - DB: check `oagw_plugin` for an existing row with `(tenant_id, name)` matching the request - `inst-plugin-create-name-check`
   2. [x] - `p1` - **IF** a name collision is found - `inst-plugin-create-if-collide`
      1. [x] - `p1` - **RETURN** `400 ValidationError` — duplicate plugin name for tenant - `inst-plugin-create-400-name`
   3. [x] - `p1` - **ELSE** - `inst-plugin-create-else-noncollide`
      1. [x] - `p1` - Generate a server-side UUID `id` and compose the anonymous GTS identifier `gts.cf.core.oagw.{plugin_type}_plugin.v1~{id}` per the Plugin Identification Model described alongside `cpt-cf-oagw-design-domain-model` - `inst-plugin-create-issue-gts`
      2. [x] - `p1` - Run the mark phase of `cpt-cf-oagw-algo-plugin-gc-mark-sweep` for the new row (reference count is trivially zero at creation) to set the initial `gc_eligible_at`; set `last_used_at` to `NULL` - `inst-plugin-create-gc-mark`
      3. [x] - `p1` - DB: INSERT `oagw_plugin` (`id`, `tenant_id`, `plugin_type`, `name`, `description`, `config_schema`, `source_code`, `gc_eligible_at`, `last_used_at`) in one transaction - `inst-plugin-create-insert`
      4. [x] - `p1` - **RETURN** `201 Created` with the plugin resource body (identifier, `plugin_type`, `name`, `description`, `config_schema`, `gc_eligible_at`, `last_used_at`; `source_code` excluded from this body — see `cpt-cf-oagw-flow-plugin-get-source`) - `inst-plugin-create-201`

### List Plugins

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-list`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Operator lists all stored custom plugins owned by the calling tenant, optionally filtered/paginated via OData query parameters.

**Error Scenarios**:
- None specific to this operation beyond the gear-wide authentication/authorization contract owned by `cpt-cf-oagw-feature-gear-foundation`.

**Steps**:
1. [x] - `p1` - Operator sends `GET /oagw/v1/plugins[?{$filter,$select,$top,$skip}]` - `inst-plugin-list-recv`
2. [x] - `p1` - DB: SELECT `oagw_plugin` rows filtered by `tenant_id = caller's tenant` (ancestor-tenant plugins are never listed — plugin resources are identified and reused independently of the upstream/route tenant-inheritance chain) - `inst-plugin-list-query`
3. [x] - `p1` - Apply OData `$filter`/`$select`/`$top` (default 50, max 100)/`$skip` to the tenant-scoped result set — `$orderby` is not offered for this list, matching `DESIGN.md`'s Plugin List Query Parameters table - `inst-plugin-list-odata`
4. [x] - `p1` - **RETURN** `200 OK` with the list of plugin resources (each excluding `source_code`) - `inst-plugin-list-200`

### Get Plugin by ID

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-get`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Operator fetches one stored custom plugin's metadata by its bare UUID `id` or its anonymous GTS identifier.

**Error Scenarios**:
- Malformed `{id}` path parameter (matches neither a bare UUID nor `gts.cf.core.oagw.{type}_plugin.v1~{value}`) → `400 ValidationError`.
- Well-formed `{id}` that classifies as a named/built-in identifier (no stored row by design), a UUID-backed identifier with no matching tenant-scoped row, or (when `{id}` carries a `{type}_plugin` segment) a segment that does not match the stored row's `plugin_type` → `404` — status and RFC 9457 envelope only, no GTS `type` asserted. `DESIGN.md`'s Error Response Format table does define a plugin-specific identifier, `PluginNotFound` (`gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`), but it is scoped to `503` for a data-plane plugin-binding resolution failure at proxy time — a materially different scenario from this management-API by-id lookup miss — so it does not apply here; `RouteNotFound` (`404`, scoped to proxy-time route matching) is likewise inapplicable and is not asserted, and this feature introduces no new GTS identifier.

**Steps**:
1. [x] - `p1` - Operator sends `GET /oagw/v1/plugins/{id}`, where `{id}` is either the bare UUID `id` value or the anonymous GTS form `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` for the target plugin - `inst-plugin-get-recv`
2. [x] - `p1` - Run `cpt-cf-oagw-algo-plugin-identifier-parse` on `{id}` - `inst-plugin-get-parse`
3. [x] - `p1` - **IF** parse fails (malformed identifier) - `inst-plugin-get-if-malformed`
   1. [x] - `p1` - **RETURN** `400 ValidationError` - `inst-plugin-get-400`
4. [x] - `p1` - **ELSE IF** parse classifies the identifier as named (no UUID instance) - `inst-plugin-get-if-named`
   1. [x] - `p1` - **RETURN** `404` — named plugins have no stored row to fetch by ID; status and RFC 9457 envelope only, no GTS `type` asserted - `inst-plugin-get-404-named`
5. [x] - `p1` - **ELSE** - `inst-plugin-get-else-uuid`
   1. [x] - `p1` - DB: SELECT `oagw_plugin` WHERE `id = {uuid}` AND `tenant_id = caller's tenant` - `inst-plugin-get-query`
   2. [x] - `p1` - **IF** no row found, or (when `{id}` carried a `{type}_plugin` segment) the stored row's `plugin_type` does not match it - `inst-plugin-get-if-notfound`
      1. [x] - `p1` - **RETURN** `404` — status and RFC 9457 envelope only, no GTS `type` asserted - `inst-plugin-get-404`
   3. [x] - `p1` - **ELSE** - `inst-plugin-get-else-found`
      1. [x] - `p1` - **RETURN** `200 OK` with the plugin resource body (`source_code` excluded) - `inst-plugin-get-200`

### Get Plugin Source

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-get-source`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Operator fetches the raw, unexecuted Starlark `source_code` text of a stored custom plugin.

**Error Scenarios**:
- Same malformed (`400`) / named-or-not-found (`404` — status and RFC 9457 envelope only, no GTS `type` asserted) conditions as `cpt-cf-oagw-flow-plugin-get`.

**Steps**:
1. [x] - `p1` - Operator sends `GET /oagw/v1/plugins/{id}/source`, where `{id}` is either the bare UUID `id` value or the anonymous GTS form `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` for the target plugin - `inst-plugin-source-recv`
2. [x] - `p1` - Run `cpt-cf-oagw-algo-plugin-identifier-parse` and the same classification/lookup logic as `cpt-cf-oagw-flow-plugin-get` (steps `inst-plugin-get-parse` through `inst-plugin-get-if-notfound`) against `{id}` - `inst-plugin-source-lookup`
3. [x] - `p1` - **IF** the identifier is malformed, named, or unresolvable to a tenant-owned row - `inst-plugin-source-if-notfound`
   1. [x] - `p1` - **RETURN** the same `400 ValidationError` / `404` outcome as the corresponding case in `cpt-cf-oagw-flow-plugin-get` - `inst-plugin-source-error`
4. [x] - `p1` - **ELSE** - `inst-plugin-source-else-found`
   1. [x] - `p1` - **RETURN** `200 OK`, `Content-Type: text/plain; charset=utf-8`, body = the stored `source_code` verbatim (no parsing, interpretation, or execution — see §1.2) - `inst-plugin-source-200`

### Delete Plugin

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-delete`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- Operator deletes a stored custom plugin that is not currently bound by any upstream or route.

**Error Scenarios**:
- Malformed/named/not-found `{id}` → same `400 ValidationError` / `404` (status and RFC 9457 envelope only, no GTS `type` asserted) as `cpt-cf-oagw-flow-plugin-get`.
- Plugin is still bound by at least one upstream or route → `409 PluginInUse` with the documented `referenced_by` body.

**Steps**:
1. [x] - `p1` - Operator sends `DELETE /oagw/v1/plugins/{id}`, where `{id}` is either the bare UUID `id` value or the anonymous GTS form `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` for the target plugin - `inst-plugin-delete-recv`
2. [x] - `p1` - Run `cpt-cf-oagw-algo-plugin-identifier-parse` and lookup against `{id}` (identical to `cpt-cf-oagw-flow-plugin-get` steps `inst-plugin-get-parse` through `inst-plugin-get-if-notfound`) - `inst-plugin-delete-lookup`
3. [x] - `p1` - **IF** the identifier is malformed, named, or unresolvable to a tenant-owned row - `inst-plugin-delete-if-notfound`
   1. [x] - `p1` - **RETURN** the same `400 ValidationError` / `404` outcome as `cpt-cf-oagw-flow-plugin-get` - `inst-plugin-delete-error`
4. [x] - `p1` - **ELSE** - `inst-plugin-delete-else-found`
   1. [x] - `p1` - Run `cpt-cf-oagw-algo-plugin-ref-count` for the resolved plugin's `plugin_ref`/`plugin_uuid`, scoped to the caller's tenant - `inst-plugin-delete-refcount`
   2. [x] - `p1` - **IF** reference count `> 0` - `inst-plugin-delete-if-inuse`
      1. [x] - `p1` - **RETURN** `409 PluginInUse` (`gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`) with `plugin_id` and `referenced_by: {upstreams: [...], routes: [...]}` populated from the reference-count scan - `inst-plugin-delete-409`
   3. [x] - `p1` - **ELSE** - `inst-plugin-delete-else-unused`
      1. [x] - `p1` - DB: DELETE `oagw_plugin` WHERE `id = {uuid}` AND `tenant_id = caller's tenant` in one transaction - `inst-plugin-delete-execute`
      2. [x] - `p1` - **RETURN** `204 No Content` - `inst-plugin-delete-204`

## 3. Processes / Business Logic (CDSL)

### Plugin Identifier Parse and Classification

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-identifier-parse`

**Input**: a `plugin_ref` candidate string — either a bare UUID (per `upstream.v1.schema.json`'s `plugins.items[]` `oneOf`, and per D2 the same accepted `{id}` path-parameter form this feature's own `GET`/`DELETE /oagw/v1/plugins/{id}[/source]` endpoints reconcile against the anonymous GTS form) or a full anonymous GTS identifier `gts.cf.core.oagw.{auth|guard|transform}_plugin.v1~{instance}` — plus, for path-addressed operations where the candidate itself carries no type segment, the `{type}_plugin` segment supplied by the URL when one is present, or (for a bare-UUID `{id}`) none, in which case `plugin_type` is resolved from the matching stored row instead.

**Output**: one of `{kind: "uuid", plugin_type, uuid}`, `{kind: "named", plugin_type, token}`, or a parse failure.

**Steps**:
1. [x] - `p1` - **IF** the candidate string matches the bare-UUID form - `inst-parse-if-bare-uuid`
   1. [x] - `p1` - Classify as `kind: "uuid"`; `plugin_type` is taken from the caller-supplied context when one is available (the URL's `{type}_plugin` segment, or the binding field's own type such as `auth.type`) rather than parsed from the string itself — and, for a bare UUID supplied directly as the `{id}` path parameter with no type segment present (`GET`/`DELETE /oagw/v1/plugins/{id}[/source]`), `plugin_type` is instead resolved from the matching `oagw_plugin` row's own stored `plugin_type` column at lookup time - `inst-parse-bare-uuid-classify`
2. [x] - `p1` - **ELSE IF** the candidate string matches `gts.cf.core.oagw.{type}_plugin.v1~{instance}` where `{type}` is one of `auth`|`guard`|`transform` - `inst-parse-if-full-gts`
   1. [x] - `p1` - Split the candidate on `~`; the substring after `~` is the instance part - `inst-parse-split-instance`
   2. [x] - `p1` - **IF** the instance part parses as a valid UUID - `inst-parse-if-instance-uuid`
      1. [x] - `p1` - Classify as `kind: "uuid"`, `plugin_type = {type}`, `uuid = instance` - `inst-parse-classify-uuid`
   3. [x] - `p1` - **ELSE IF** the instance part matches `cf.core.oagw.{token}.v1` and `{token}` is a member of the named-plugin catalog for `{type}` (auth: `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, `basic`, `bearer`; guard: `required_headers`, `timeout`, `cors`; transform: `request_id`, `logging`, `metrics`) - `inst-parse-if-named-token`
      1. [x] - `p1` - Classify as `kind: "named"`, `plugin_type = {type}`, `token` - `inst-parse-classify-named`
   4. [x] - `p1` - **ELSE** - `inst-parse-else-unrecognized`
      1. [x] - `p1` - **RETURN** parse failure — unrecognized plugin identifier (neither a stored-UUID form nor a known named token) - `inst-parse-fail-unrecognized`
3. [x] - `p1` - **ELSE** - `inst-parse-else-malformed`
   1. [x] - `p1` - **RETURN** parse failure — malformed plugin identifier syntax - `inst-parse-fail-malformed`
4. [x] - `p1` - **RETURN** the classification result - `inst-parse-return`

### Plugin Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-resolve-ref`

**Input**: a `plugin_ref` candidate string and the caller's `tenant_id`; invoked both by this feature's ID-addressed operations (§2) and, at write time, by upstream/route CRUD (`cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`) validating `plugins.items[]`/`auth.type` values — those features validate that a `plugin_ref` resolves without executing it.

**Output**: a resolved binding descriptor `{plugin_ref, plugin_uuid | null, storage_backed: bool}`, or a resolution failure.

**Steps**:
1. [x] - `p1` - Run `cpt-cf-oagw-algo-plugin-identifier-parse` on the candidate - `inst-resolve-parse`
2. [x] - `p1` - **IF** parsing fails - `inst-resolve-if-parse-fail`
   1. [x] - `p1` - **RETURN** resolution failure (malformed or unrecognized identifier) - `inst-resolve-fail-parse`
3. [x] - `p1` - **ELSE IF** classification is `kind: "uuid"` - `inst-resolve-if-uuid`
   1. [x] - `p1` - DB: SELECT `oagw_plugin` WHERE `id = uuid` AND `tenant_id = tenant_id` - `inst-resolve-uuid-query`
   2. [x] - `p1` - **IF** no row found, or the row's stored `plugin_type` does not equal the classified `plugin_type` - `inst-resolve-if-uuid-notfound`
      1. [x] - `p1` - **RETURN** resolution failure — no such custom plugin for this tenant - `inst-resolve-fail-uuid-notfound`
   3. [x] - `p1` - **ELSE** - `inst-resolve-else-uuid-found`
      1. [x] - `p1` - **RETURN** `{plugin_ref: full GTS identifier, plugin_uuid: uuid, storage_backed: true}` - `inst-resolve-return-uuid`
4. [x] - `p1` - **ELSE** (classification is `kind: "named"`) - `inst-resolve-else-named`
   1. [x] - `p1` - No storage lookup is performed — named plugins have no `oagw_plugin` row by design - `inst-resolve-named-no-lookup`
   2. [x] - `p1` - **RETURN** `{plugin_ref: full GTS identifier, plugin_uuid: null, storage_backed: false}` - `inst-resolve-return-named`

### Plugin Reference Counting

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-ref-count`

**Input**: a resolved plugin's `plugin_ref`/`plugin_uuid` and the caller's `tenant_id`.

**Output**: `{count: int, referenced_by: {upstreams: [gts upstream identifiers...], routes: [gts route identifiers...]}}`.

**Steps**:
1. [x] - `p1` - DB: SELECT tenant-scoped `oagw_upstream` rows whose scalar `auth_plugin_ref`/`auth_plugin_uuid` columns equal the target plugin's `plugin_ref`/`plugin_uuid` - `inst-refcount-scan-auth-scalar`
2. [x] - `p1` - DB: SELECT tenant-scoped `oagw_upstream_plugin` binding rows whose `(plugin_ref, plugin_uuid)` equal the target plugin, joined back to their owning `oagw_upstream.id` - `inst-refcount-scan-upstream-bindings`
3. [x] - `p1` - DB: SELECT tenant-scoped `oagw_route_plugin` binding rows whose `(plugin_ref, plugin_uuid)` equal the target plugin, joined back to their owning `oagw_route.id` - `inst-refcount-scan-route-bindings`
4. [x] - `p1` - Union the distinct upstream IDs from steps `inst-refcount-scan-auth-scalar` and `inst-refcount-scan-upstream-bindings` into `referenced_by.upstreams`; union the distinct route IDs from step `inst-refcount-scan-route-bindings` into `referenced_by.routes` - `inst-refcount-union`
5. [x] - `p1` - **RETURN** `count = len(referenced_by.upstreams) + len(referenced_by.routes)` and `referenced_by` - `inst-refcount-return`

### Plugin GC Mark-Then-Sweep Bookkeeping

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-gc-mark-sweep`

**Input**: a stored custom plugin row and its current reference count (from `cpt-cf-oagw-algo-plugin-ref-count`). Named plugins are exempt (see step 1).

**Output**: the plugin row's updated `gc_eligible_at` value. This algorithm never deletes rows — it is the mark phase only.

**Steps**:
1. [x] - `p1` - **IF** the plugin is a named/registry-resolved plugin (no `oagw_plugin` row) - `inst-gc-if-named`
   1. [x] - `p1` - **RETURN** without modification — named plugins are never subject to GC bookkeeping - `inst-gc-return-named`
2. [x] - `p1` - **ELSE** (mark phase, owned by this feature; invoked at plugin creation, and by any write path that changes this plugin's binding count — including `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-route-management` mutating `plugins.items[]`/`auth.type`) - `inst-gc-else-mark`
   1. [x] - `p1` - **IF** reference count `== 0` **AND** `gc_eligible_at IS NULL` - `inst-gc-if-newly-unlinked`
      1. [x] - `p1` - Set `gc_eligible_at = now() + gc_ttl_days` (default 30 days, per the Plugin Lifecycle Management behavior described alongside `cpt-cf-oagw-design-domain-model`) - `inst-gc-set-eligible`
   2. [x] - `p1` - **ELSE IF** reference count `> 0` **AND** `gc_eligible_at IS NOT NULL` - `inst-gc-if-relinked`
      1. [x] - `p1` - Clear `gc_eligible_at` to `NULL` — the plugin is referenced again and is no longer GC-eligible - `inst-gc-clear-eligible`
   3. [x] - `p1` - **ELSE** - `inst-gc-else-noop`
      1. [x] - `p1` - No change — the mark state already matches the current reference count - `inst-gc-noop`
3. [x] - `p1` - **RETURN** the (possibly unchanged) `gc_eligible_at` - `inst-gc-return`

**Note**: the sweep phase — a periodic job that deletes `oagw_plugin` rows whose `gc_eligible_at` has elapsed — is explicitly out of scope for this feature (DECOMPOSITION §2.4 Out of scope: "background job scheduling is not a REST-observable boundary for this round"). This algorithm's contract ends at persisting an accurate `gc_eligible_at`. `last_used_at` is persisted and exposed by this feature but is only ever written by the (out-of-scope) plugin execution path, `cpt-cf-oagw-feature-plugin-execution`; this feature initializes it to `NULL` at creation and never updates it thereafter.

## 4. States (CDSL)

### Plugin Lifecycle State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-plugin-lifecycle`

**States**: Unreferenced, Referenced, Deleted

**Initial State**: Unreferenced

**Transitions**:
1. [x] - `p1` - **FROM** Unreferenced **TO** Referenced **WHEN** an upstream's `auth_plugin_ref`/`auth_plugin_uuid` scalar columns, or an `oagw_upstream_plugin`/`oagw_route_plugin` binding row, is created pointing at this plugin's `plugin_ref`/`plugin_uuid` — a write performed by `cpt-cf-oagw-feature-upstream-management` or `cpt-cf-oagw-feature-route-management`, which invokes `cpt-cf-oagw-algo-plugin-gc-mark-sweep` afterward per that algorithm's contract - `inst-lifecycle-unreferenced-to-referenced`
2. [x] - `p1` - **FROM** Referenced **TO** Unreferenced **WHEN** the last binding/scalar reference to this plugin is removed, driving its reference count (per `cpt-cf-oagw-algo-plugin-ref-count`) to zero - `inst-lifecycle-referenced-to-unreferenced`
3. [x] - `p1` - **FROM** Unreferenced **TO** Deleted **WHEN** `cpt-cf-oagw-flow-plugin-delete` executes with a reference count of zero (`204` returned); this transition is immediate and independent of whether `gc_eligible_at` has elapsed — it does not require or wait for the (out-of-scope) periodic GC sweep - `inst-lifecycle-unreferenced-to-deleted`

**Note**: a `cpt-cf-oagw-flow-plugin-delete` request received while the plugin is Referenced does **not** transition state; it is rejected with `409 PluginInUse` and the plugin remains Referenced.

## 5. Definitions of Done

### Cross-Cutting Dispositions

The following review domains are explicitly dispositioned for this feature, rather than left silent:

- **Performance**: Not applicable as a dedicated budget — this feature's create/list/get/get-source/delete operations are low-rate, latency-insensitive control-plane CRUD against the config-store, not the request-forwarding hot path; per-request latency budgets are owned by `cpt-cf-oagw-feature-proxy-core` (2.5).
- **UX/Accessibility**: Not applicable — this is a machine-to-machine REST management API with no user interface; there is no visual or interactive surface for accessibility criteria to apply to.
- **Compliance/Privacy**: Not applicable because no personal or regulated data is stored — a Plugin record holds tenant-authored Starlark `source_code` and a JSON Schema, never credential material or end-user personal data (`secret_ref` values live only on `Upstream.auth.config`, per §1.2).
- **Resilience/Recovery**: Not applicable beyond the single-row/single-transaction guarantee already stated under Reliability/Data integrity above — this feature performs no outbound network calls and owns no retry, failover, or circuit-breaker behavior of its own.
- **Data Privacy**: Not applicable for the same reason as Compliance/Privacy above — no end-user personal data flows through or is retained by this feature's CRUD surface.
- **External Integrations**: Not applicable as a live-integration concern — this feature stores `source_code` as opaque text and never parses, interprets, sandboxes, or executes it (§1.2), so it makes no outbound call to any execution engine or external system at write or read time.
- **Config/Health**: Not applicable — this feature introduces no gear-configuration flags or health-check endpoints of its own; the (out-of-scope) periodic GC sweep job's scheduling configuration, if any, belongs to that future work, not this feature.

### Plugin CRUD Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-crud-surface`

The system **MUST** implement `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source`, and `DELETE /oagw/v1/plugins/{id}`, with no `PUT`/replace operation for this resource.

**Implements**:
- `cpt-cf-oagw-flow-plugin-create`
- `cpt-cf-oagw-flow-plugin-list`
- `cpt-cf-oagw-flow-plugin-get`
- `cpt-cf-oagw-flow-plugin-get-source`
- `cpt-cf-oagw-flow-plugin-delete`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-principle-plugin-immutable`, `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `POST /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins/{id}`
- API: `GET /oagw/v1/plugins/{id}/source`
- API: `DELETE /oagw/v1/plugins/{id}`
- DB: `cpt-cf-oagw-db-schema`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

### Anonymous GTS Identifier Issuance

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-gts-issuance`

The system **MUST** issue a server-generated anonymous GTS identifier `gts.cf.core.oagw.{plugin_type}_plugin.v1~{uuid}` on every successful `POST /oagw/v1/plugins`, and **MUST NOT** accept a client-supplied `id`.

**Implements**:
- `cpt-cf-oagw-flow-plugin-create`

**Constraints**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- API: `POST /oagw/v1/plugins`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

### Plugin Identification and Resolution Model

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-identification`

The system **MUST** implement the parse/classify/resolve algorithm distinguishing UUID-backed (custom, stored) plugin identifiers from named (registry-resolved, unstored) plugin identifiers, expose it for reuse by upstream and route CRUD's `plugin_ref` validation, and recognize the full named-plugin catalog (`noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, `basic`, `bearer`, `required_headers`, `timeout`, `cors`, `request_id`, `logging`, `metrics`) as valid, bindable `plugin_ref` values without requiring a stored row.

**Implements**:
- `cpt-cf-oagw-algo-plugin-identifier-parse`
- `cpt-cf-oagw-algo-plugin-resolve-ref`

**Constraints**: `cpt-cf-oagw-adr-plugin-system`

**Touches**:
- API: `GET /oagw/v1/plugins/{id}`
- DB Table: `oagw_plugin`
- Entities: `Plugin identification (plugin_ref / plugin_uuid)`

### Plugin-In-Use Deletion Protection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-in-use-protection`

The system **MUST** reject `DELETE /oagw/v1/plugins/{id}` with `409 PluginInUse` (`gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`) — including a `plugin_id` field and a `referenced_by: {upstreams: [...], routes: [...]}` body listing every referencing resource's anonymous GTS identifier — whenever the plugin is still bound by any upstream (via `plugins.items[]` or the scalar `auth_plugin_ref`/`auth_plugin_uuid` columns) or route (`plugins.items[]`), and **MUST** return `204 No Content` and delete the row when no reference exists.

**Implements**:
- `cpt-cf-oagw-flow-plugin-delete`
- `cpt-cf-oagw-algo-plugin-ref-count`

**Constraints**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `DELETE /oagw/v1/plugins/{id}`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

### GC Bookkeeping Fields

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-gc-bookkeeping`

The system **MUST** persist and expose `gc_eligible_at` and `last_used_at` on every stored custom plugin, **MUST** run the mark phase of `cpt-cf-oagw-algo-plugin-gc-mark-sweep` at plugin creation and whenever this feature's own delete path evaluates the plugin's reference count, and **MUST NOT** implement the periodic sweep job that deletes rows once `gc_eligible_at` has elapsed (deferred, see DECOMPOSITION §2.4 Out of scope).

**Implements**:
- `cpt-cf-oagw-algo-plugin-gc-mark-sweep`
- `cpt-cf-oagw-state-plugin-lifecycle`

**Constraints**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- DB Table: `oagw_plugin`
- Entities: `Plugin`

### Tenant Scoping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-tenant-scope`

The system **MUST** scope every plugin CRUD operation (`POST`, `GET` list, `GET` by id, `GET` source, `DELETE`) strictly to the caller's tenant; a plugin owned by another tenant (including an ancestor tenant) **MUST** be invisible, returning `404` (status and RFC 9457 envelope only — neither the proxy-time `RouteNotFound` identifier nor the data-plane, `503`-scoped `PluginNotFound` identifier is asserted for this management-API lookup miss) from `GET`/`DELETE`/`GET .../source` and being excluded from `GET` list results.

**Implements**:
- `cpt-cf-oagw-flow-plugin-list`
- `cpt-cf-oagw-flow-plugin-get`
- `cpt-cf-oagw-flow-plugin-get-source`
- `cpt-cf-oagw-flow-plugin-delete`

**Constraints**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `GET /oagw/v1/plugins`
- API: `GET /oagw/v1/plugins/{id}`
- API: `GET /oagw/v1/plugins/{id}/source`
- API: `DELETE /oagw/v1/plugins/{id}`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

### Plugin Immutability

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-immutability`

The system **MUST NOT** expose any operation that modifies a stored plugin's `plugin_type`, `name`, `config_schema`, or `source_code` after creation; a changed plugin **MUST** be represented as a new `POST /oagw/v1/plugins` resource followed by re-binding upstream/route references to the new identifier.

**Implements**:
- `cpt-cf-oagw-dod-plugin-crud-surface`

**Constraints**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- API: `POST /oagw/v1/plugins`
- DB Table: `oagw_plugin`
- Entities: `Plugin`

## 6. Acceptance Criteria

- [x] `POST /oagw/v1/plugins` with a valid `{plugin_type: "guard", name, config_schema, source_code}` body returns `201` with a body whose identifier matches `gts.cf.core.oagw.guard_plugin.v1~{uuid}`, where the UUID is server-generated and not equal to any client-supplied value.
- [x] `POST /oagw/v1/plugins` with a missing `source_code`, or an invalid `plugin_type` (not one of `auth`|`guard`|`transform`), returns `400` with GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`.
- [x] `POST /oagw/v1/plugins` twice with the same `name` for the same tenant returns `400` on the second call.
- [x] `GET /oagw/v1/plugins` returns only plugins owned by the caller's tenant; a plugin created under a different tenant is absent from the list.
- [x] `GET /oagw/v1/plugins/{id}` for a just-created plugin returns `200` with the stored `plugin_type`, `name`, `config_schema`, `gc_eligible_at` (non-null), and `last_used_at: null`, and omits `source_code`.
- [x] `GET /oagw/v1/plugins/{id}` for a well-formed named identifier (e.g. `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`) returns `404`.
- [x] `GET /oagw/v1/plugins/{id}` for a syntactically invalid identifier (matching neither a bare UUID nor the anonymous GTS plugin form) returns `400`.
- [x] `GET /oagw/v1/plugins/{id}` for a UUID-backed identifier belonging to a different tenant returns `404`.
- [x] `GET /oagw/v1/plugins/{id}` succeeds identically (`200 OK`, identical body) whether `{id}` is supplied as the bare UUID `id` value or as the anonymous GTS form `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` for the same plugin.
- [x] `GET /oagw/v1/plugins/{id}/source` for a just-created plugin returns `200` with `Content-Type: text/plain` and a body byte-for-byte identical to the `source_code` submitted at creation.
- [x] `DELETE /oagw/v1/plugins/{id}` for a plugin with zero upstream/route bindings returns `204`, and a subsequent `GET /oagw/v1/plugins/{id}` on the same id returns `404`.
- [x] `DELETE /oagw/v1/plugins/{id}` for a plugin bound by one upstream and one route returns `409` with GTS `type` `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`, a `plugin_id` field, and `referenced_by.upstreams`/`referenced_by.routes` each containing exactly one identifier matching the bound resources.
- [x] A plugin's `gc_eligible_at` is non-null immediately after creation (zero references), becomes `null` once an upstream or route binds it, and becomes non-null again once that binding is removed.
- [x] No route exists under `/oagw/v1/plugins` that would modify `plugin_type`, `name`, `config_schema`, or `source_code` of an existing row (no `PUT`/`PATCH` handler is registered for this resource).
- [x] All twelve named-plugin catalog tokens (`noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, `basic`, `bearer`, `required_headers`, `timeout`, `cors`, `request_id`, `logging`, `metrics`) classify as `kind: "named"` and resolve successfully via `cpt-cf-oagw-algo-plugin-resolve-ref` without any `oagw_plugin` row existing for them.
- [x] No custom Starlark plugin's `source_code` is parsed, interpreted, or executed by any code path exercised by this feature's tests, verifying the `cpt-cf-oagw-nfr-starlark-sandbox` carve-out (storage/identification only in this feature).
