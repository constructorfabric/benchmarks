# Feature: Upstream Management API

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Upstream Flow](#create-upstream-flow)
  - [List Upstreams Flow](#list-upstreams-flow)
  - [Get Upstream Flow](#get-upstream-flow)
  - [Replace Upstream Flow](#replace-upstream-flow)
  - [Delete Upstream Flow](#delete-upstream-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Create Validation Algorithm](#create-validation-algorithm)
  - [Replace Validation Algorithm](#replace-validation-algorithm)
  - [List Query Algorithm](#list-query-algorithm)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream Enabled State Machine](#upstream-enabled-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Create Upstream Endpoint](#create-upstream-endpoint)
  - [List and Get Upstream Endpoints](#list-and-get-upstream-endpoints)
  - [Replace Upstream Endpoint](#replace-upstream-endpoint)
  - [Delete Upstream Endpoint](#delete-upstream-endpoint)
  - [Enabled/Disabled Semantics](#enableddisabled-semantics)
  - [Per-Endpoint Authorization](#per-endpoint-authorization)
  - [RFC 9457 Error Responses](#rfc-9457-error-responses)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-upstream-management-api-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-upstream-management-api`

## 1. Feature Context

### 1.1 Overview

This feature exposes the five CRUD (create, read, update, delete) endpoints over the
`Upstream` resource — the configuration record that names one external service OAGW
(Outbound API Gateway) can forward requests to. It is the management surface every later
proxy request depends on for a target to resolve.

### 1.2 Purpose

Upstreams are the fundamental configuration unit of OAGW: every proxied request ultimately
targets one. This feature lets `cpt-cf-oagw-actor-platform-operator` and
`cpt-cf-oagw-actor-tenant-admin` declare, inspect, replace, and remove upstream configuration
— endpoints, protocol, alias, auth reference, and the `enabled` flag — with tenant-scoped
storage, alias-uniqueness enforcement, and immutable identity fields.

**Requirements**: `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-enable-disable`,
`cpt-cf-oagw-nfr-input-validation`, `cpt-cf-oagw-nfr-multi-tenancy`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-https-only`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, inspects, replaces, and deletes upstreams for global configuration; the only actor who can grant a descendant tenant the `oagw:upstream:bind` permission that this feature checks. |
| `cpt-cf-oagw-actor-tenant-admin` | Performs the same five operations scoped to their own tenant, including binding to an ancestor's alias when the required permission and sharing mode allow it. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-enable-disable`, `cpt-cf-oagw-fr-error-codes`
- **Design**: [DESIGN.md](../DESIGN.md) §3.3 API Contracts (`cpt-cf-oagw-interface-api`), §3.1 Domain Model (`cpt-cf-oagw-design-domain-model`)
- **Interface**: `cpt-cf-oagw-interface-management-api`
- **Schema**: [upstream.v1.schema.json](../schemas/upstream.v1.schema.json)
- **Decomposition**: `cpt-cf-oagw-feature-upstream-management-api`
- **Dependencies**: `cpt-cf-oagw-feature-resource-model-and-store` (domain types, alias derivation, and the tenant-scoped in-memory store this feature reads and writes through)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-configure-upstream`

All five flows below run behind Bearer token authentication (`toolkit-auth`, established by
the gear-foundation feature). Every step that reaches the store operates on the tenant
identifier carried by the caller's token; no flow accepts a caller-supplied `tenant_id`.

### Create Upstream Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-api-create`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A hostname-based upstream is created with an auto-derived alias.
- An IP-based upstream is created with an explicit alias.
- An `http`-scheme, port-80 endpoint is accepted because Override 2 adds `http` as a legal
  `scheme` value independent of whether the runtime later opens a plaintext connection.
- A tenant admin creates an upstream whose alias matches an ancestor's alias (a "bind"),
  holding `oagw:upstream:bind` and an ancestor sharing mode that permits it.

**Error Scenarios**:
- Body fails schema validation (missing `server.endpoints`, missing `protocol`, malformed
  `alias` pattern, an endpoint scheme outside the accepted set).
- Alias — derived or explicit — collides with an existing upstream in the same tenant.
- Alias matches an ancestor's alias but the caller lacks `oagw:upstream:bind`, or the
  ancestor's `auth.sharing` is `enforce` (blocks the override the bind would introduce) or
  `private` (blocks visibility of the ancestor upstream needed to detect the match at all).
- Caller lacks `gts.cf.core.oagw.upstream.v1~:create`.

**Steps**:
1. [ ] - `p1` - Operator sends create request with `server`, `protocol`, and optional `alias`, `tags`, `auth`, `headers`, `plugins`, `rate_limit`, `cors`, `enabled` - `inst-create-1`
2. [ ] - `p1` - API: POST /oagw/v1/upstreams (Upstream document body, no `id`/`tenant_id` in the payload) - `inst-create-2`
3. [ ] - `p1` - **IF** caller lacks `gts.cf.core.oagw.upstream.v1~:create` - `inst-create-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed, `application/problem+json`, `X-OAGW-Error-Source: gateway` - `inst-create-3a`
4. [ ] - `p1` - **ELSE** - `inst-create-4`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-upstream-api-validate-create` against the body - `inst-create-4a`
   2. [ ] - `p1` - **IF** validation fails (schema, alias pattern, or bind-permission check) - `inst-create-4b`
      1. [ ] - `p1` - **RETURN** 400 ValidationError with per-field `detail`, `X-OAGW-Error-Source: gateway` - `inst-create-4b1`
   3. [ ] - `p1` - **ELSE IF** `(tenant_id, alias)` already exists for the calling tenant - `inst-create-4c`
      1. [ ] - `p1` - **RETURN** 409 Conflict, `type` set to `gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1`, `X-OAGW-Error-Source: gateway` - `inst-create-4c1`
   4. [ ] - `p1` - **ELSE** - `inst-create-4d`
      1. [ ] - `p1` - Generate a UUID and build the anonymous GTS id `gts.cf.core.oagw.upstream.v1~{uuid}` - `inst-create-4d1`
      2. [ ] - `p1` - DB: INSERT tenant-scoped store (id, tenant_id, alias, enabled=true unless supplied, server, protocol, auth, headers, rate_limit, cors, plugins, tags) - `inst-create-4d2`
      3. [ ] - `p1` - **RETURN** 201 Created with the full stored Upstream document, including the server-generated `id` - `inst-create-4d3`

### List Upstreams Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-api-list`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- List returns only the calling tenant's own upstreams, paginated by `$top`/`$skip`.
- `$filter`, `$select`, and `$orderby` narrow, project, and order the result set.

**Error Scenarios**:
- `$top` above 100 is clamped to 100 rather than rejected.
- `$filter` does not parse as a valid OData boolean expression, so the list query algorithm
  rejects it with a validation error instead of running the lookup.
- Caller lacks `gts.cf.core.oagw.upstream.v1~:read`.

**Steps**:
1. [ ] - `p1` - Admin sends list request with optional `$filter`, `$select`, `$orderby`, `$top`, `$skip` - `inst-list-1`
2. [ ] - `p1` - API: GET /oagw/v1/upstreams?$filter=...&$select=...&$orderby=...&$top=...&$skip=... - `inst-list-2`
3. [ ] - `p1` - **IF** caller lacks `gts.cf.core.oagw.upstream.v1~:read` - `inst-list-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed, `X-OAGW-Error-Source: gateway` - `inst-list-3a`
4. [ ] - `p1` - **ELSE** - `inst-list-4`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-upstream-api-list-query` to parse and clamp query parameters - `inst-list-4a`
   2. [ ] - `p1` - **IF** `$filter` fails to parse as a valid OData boolean expression - `inst-list-4b`
      1. [ ] - `p1` - **RETURN** 400 ValidationError with per-field `detail`, `X-OAGW-Error-Source: gateway` - `inst-list-4b1`
   3. [ ] - `p1` - **ELSE** - `inst-list-4c`
      1. [ ] - `p1` - DB: SELECT tenant-scoped store WHERE tenant_id = caller's tenant, apply `$filter`, `$orderby`, `$skip`, `$top` (default 50, max 100) - `inst-list-4c1`
      2. [ ] - `p1` - Apply `$select` field projection to each result - `inst-list-4c2`
      3. [ ] - `p1` - **RETURN** 200 OK with the resulting Upstream array; ancestor upstreams reachable only via the proxy tenant-chain walk are never included - `inst-list-4c3`

### Get Upstream Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-api-get`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The calling tenant's own upstream is returned by `id`.

**Error Scenarios**:
- `id` does not exist for the calling tenant, including the case where it exists only as an
  ancestor tenant's upstream — the response is indistinguishable 404 either way.
- Caller lacks `gts.cf.core.oagw.upstream.v1~:read`.

**Steps**:
1. [ ] - `p1` - Admin requests a single upstream by id - `inst-get-1`
2. [ ] - `p1` - API: GET /oagw/v1/upstreams/{id} - `inst-get-2`
3. [ ] - `p1` - **IF** caller lacks `gts.cf.core.oagw.upstream.v1~:read` - `inst-get-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed, `X-OAGW-Error-Source: gateway` - `inst-get-3a`
4. [ ] - `p1` - **ELSE** - `inst-get-4`
   1. [ ] - `p1` - DB: SELECT tenant-scoped store WHERE id = {id} AND tenant_id = caller's tenant - `inst-get-4a`
   2. [ ] - `p1` - **IF** no row matches (absent entirely, or present only for an ancestor tenant) - `inst-get-4b`
      1. [ ] - `p1` - **RETURN** 404 Problem Details with `type` set to `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`, `X-OAGW-Error-Source: gateway` - `inst-get-4b1`
   3. [ ] - `p1` - **ELSE** - `inst-get-4c`
      1. [ ] - `p1` - **RETURN** 200 OK with the full Upstream document - `inst-get-4c1`

### Replace Upstream Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-api-replace`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A full replacement body overwrites every stored field; any optional field the request omits
  is cleared in the stored document rather than left at its previous value.
- An IP-based upstream's explicit alias is repeated unchanged in the replacement body — the
  no-op case the alias-immutability rule tolerates.
- A replacement body that omits `enabled` resets it to the schema default `true`, even when the
  stored upstream was previously `enabled: false`, because a `PUT` is a full replacement and an
  omitted field is never read from the prior stored value.

**Error Scenarios**:
- The replacement body would change the derived or explicit alias — rejected regardless of
  which direction the endpoint set moved (hostname to hostname, hostname to IP, IP to
  hostname, or IP to IP).
- `id` in the path does not resolve to a upstream owned by the calling tenant (own-tenant 404;
  ancestor-tenant 404, identical response).
- The replacement body attempts to set `id` or `tenant_id` to a different value than stored —
  rejected as validation failure since both are immutable.
- Re-validation of the ancestor-bind constraint fails after the replacement changes overrides,
  endpoints, or alias.

**Steps**:
1. [ ] - `p1` - Operator sends a complete replacement document for an existing upstream - `inst-replace-1`
2. [ ] - `p1` - API: PUT /oagw/v1/upstreams/{id} (full Upstream document, no `id`/`tenant_id` fields accepted in the body) - `inst-replace-2`
3. [ ] - `p1` - **IF** caller lacks `gts.cf.core.oagw.upstream.v1~:override` - `inst-replace-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed, `X-OAGW-Error-Source: gateway` - `inst-replace-3a`
4. [ ] - `p1` - **ELSE** - `inst-replace-4`
   1. [ ] - `p1` - DB: SELECT tenant-scoped store WHERE id = {id} AND tenant_id = caller's tenant - `inst-replace-4a`
   2. [ ] - `p1` - **IF** no row matches - `inst-replace-4b`
      1. [ ] - `p1` - **RETURN** 404 Problem Details with `type` set to `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`, `X-OAGW-Error-Source: gateway` - `inst-replace-4b1`
   3. [ ] - `p1` - **ELSE** - `inst-replace-4c`
      1. [ ] - `p1` - Run `cpt-cf-oagw-algo-upstream-api-validate-replace` against the stored row and the new body - `inst-replace-4c1`
      2. [ ] - `p1` - **IF** validation fails (schema, alias-transition rejection, or bind re-validation) - `inst-replace-4c2`
         1. [ ] - `p1` - **RETURN** 400 ValidationError, `X-OAGW-Error-Source: gateway` - `inst-replace-4c2a`
      3. [ ] - `p1` - **ELSE** - `inst-replace-4c3`
         1. [ ] - `p1` - DB: UPDATE tenant-scoped store SET every field from the new body, clearing any optional field the body omits and resetting an omitted `enabled` to the schema default `true` regardless of the previously stored value, keeping `id`/`tenant_id`/`alias` from the stored row - `inst-replace-4c3a`
         2. [ ] - `p1` - **RETURN** 200 OK with the replaced Upstream document - `inst-replace-4c3b`

### Delete Upstream Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-api-delete`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- An upstream owned by the calling tenant is removed and a subsequent GET on the same `id`
  returns 404.

**Error Scenarios**:
- `id` does not resolve to a upstream owned by the calling tenant (own-tenant absent, or
  present only for an ancestor tenant): both cases answer 404.
- Caller lacks `gts.cf.core.oagw.upstream.v1~:delete`.

**Steps**:
1. [ ] - `p1` - Operator requests deletion of an upstream by id - `inst-delete-1`
2. [ ] - `p1` - API: DELETE /oagw/v1/upstreams/{id} - `inst-delete-2`
3. [ ] - `p1` - **IF** caller lacks `gts.cf.core.oagw.upstream.v1~:delete` - `inst-delete-3`
   1. [ ] - `p1` - **RETURN** 401 AuthenticationFailed, `X-OAGW-Error-Source: gateway` - `inst-delete-3a`
4. [ ] - `p1` - **ELSE** - `inst-delete-4`
   1. [ ] - `p1` - DB: DELETE tenant-scoped store WHERE id = {id} AND tenant_id = caller's tenant - `inst-delete-4a`
   2. [ ] - `p1` - **IF** a row was removed - `inst-delete-4b`
      1. [ ] - `p1` - **RETURN** 204 No Content - `inst-delete-4b1`
   3. [ ] - `p1` - **ELSE** - `inst-delete-4c`
      1. [ ] - `p1` - **RETURN** 404 Problem Details with `type` set to `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`, `X-OAGW-Error-Source: gateway` - `inst-delete-4c1`

## 3. Processes / Business Logic (CDSL)

### Create Validation Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-upstream-api-validate-create`

**Input**: Raw create-request body, calling tenant id, calling token's granted permissions

**Output**: A normalized Upstream record ready to insert, or a list of validation errors

**Steps**:
1. [ ] - `p1` - Validate the body against the Upstream JSON Schema shape (`server.endpoints` non-empty, `protocol` one of the two supported GTS protocol identifiers, `additionalProperties` rejected) - `inst-valc-1`
2. [ ] - `p1` - **FOR EACH** endpoint in `server.endpoints` - `inst-valc-2`
   1. [ ] - `p1` - **IF** `scheme` is not one of `https`, `wss`, `wt`, `grpc`, `http`, `ws` - `inst-valc-2a`
      1. [ ] - `p1` - Add error: unsupported scheme - `inst-valc-2a1`
   2. [ ] - `p1` - Validate `host` as hostname (RFC 1123) or IPv4/IPv6 literal, `port` in `1..65535` - `inst-valc-2b`
3. [ ] - `p1` - **IF** an explicit `alias` was supplied - `inst-valc-3`
   1. [ ] - `p1` - **IF** it fails the alias pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` - `inst-valc-3a`
      1. [ ] - `p1` - Add error: invalid alias format - `inst-valc-3a1`
4. [ ] - `p1` - Compute `derived = compute_derived_alias(endpoints)` (hostname-based single or common-suffix pools; `None` for IP-based or non-derivable pools) - `inst-valc-4`
5. [ ] - `p1` - **IF** `derived` is `Some` and no explicit alias was supplied - `inst-valc-5`
   1. [ ] - `p1` - Use `derived`, normalized to ASCII lowercase with trailing dot stripped, as the effective alias - `inst-valc-5a`
6. [ ] - `p1` - **ELSE IF** `derived` is `Some` and an explicit alias was supplied - `inst-valc-6`
   1. [ ] - `p1` - **IF** the explicit alias equals `derived` (idempotent no-op) - `inst-valc-6a`
      1. [ ] - `p1` - Use `derived` as the effective alias - `inst-valc-6a1`
   2. [ ] - `p1` - **ELSE** - `inst-valc-6b`
      1. [ ] - `p1` - Add error: explicit alias conflicts with the derivable value - `inst-valc-6b1`
7. [ ] - `p1` - **ELSE IF** `derived` is `None` and no explicit alias was supplied - `inst-valc-7`
   1. [ ] - `p1` - Add error: alias required for IP-based or non-derivable endpoint sets - `inst-valc-7a`
8. [ ] - `p1` - **ELSE** - `inst-valc-8`
   1. [ ] - `p1` - Use the supplied explicit alias, normalized, as the effective alias - `inst-valc-8a`
9. [ ] - `p1` - **IF** no validation errors so far - `inst-valc-9`
   1. [ ] - `p1` - DB: SELECT tenant-scoped store WHERE tenant_id = caller's tenant AND alias = effective alias - `inst-valc-9a`
   2. [ ] - `p1` - **IF** found - `inst-valc-9b`
      1. [ ] - `p1` - **RETURN** conflict signal (409) rather than a validation error - `inst-valc-9b1`
   3. [ ] - `p1` - DB: SELECT ancestor tenant-scoped stores WHERE alias = effective alias, walking the tenant chain toward root - `inst-valc-9c`
   4. [ ] - `p1` - **IF** an ancestor upstream with the same alias is found - `inst-valc-9d`
      1. [ ] - `p1` - **IF** caller lacks `oagw:upstream:bind` - `inst-valc-9d1`
         1. [ ] - `p1` - Add error: bind permission required - `inst-valc-9d1a`
      2. [ ] - `p1` - **ELSE IF** the ancestor's `auth.sharing` is `enforce` - `inst-valc-9d2`
         1. [ ] - `p1` - Add error: ancestor enforces its configuration, override not allowed - `inst-valc-9d2a`
      3. [ ] - `p1` - **ELSE IF** the ancestor's `auth.sharing` is `private` - `inst-valc-9d3`
         1. [ ] - `p1` - Add error: ancestor configuration is private, not visible for binding - `inst-valc-9d3a`
10. [ ] - `p1` - **RETURN** normalized record when the error list is empty, otherwise the error list - `inst-valc-10`

### Replace Validation Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-upstream-api-validate-replace`

**Input**: Stored Upstream row, new request body, calling tenant id, calling token's granted permissions

**Output**: A normalized Upstream record ready to persist, or a list of validation errors

**Steps**:
1. [ ] - `p1` - **IF** the body sets `id` or `tenant_id` to a value different from the stored row - `inst-valr-1`
   1. [ ] - `p1` - Add error: `id` and `tenant_id` are immutable - `inst-valr-1a`
2. [ ] - `p1` - Validate the body against the Upstream JSON Schema shape, same as create - `inst-valr-2`
3. [ ] - `p1` - Compute `new_derived = compute_derived_alias(new body's endpoints)` and compare the stored row's endpoint shape to the new one to classify the transition as Derivable→Derivable, Derivable→Non-derivable, Non-derivable→Non-derivable, or Non-derivable→Derivable - `inst-valr-3`
4. [ ] - `p1` - **IF** the endpoint set is unchanged from the stored row - `inst-valr-4`
   1. [ ] - `p1` - **IF** the body's alias (if present) differs from the stored alias - `inst-valr-4a`
      1. [ ] - `p1` - Add error: alias override not allowed without an endpoint change - `inst-valr-4a1`
   2. [ ] - `p1` - **ELSE** - `inst-valr-4b`
      1. [ ] - `p1` - Retain the stored alias - `inst-valr-4b1`
5. [ ] - `p1` - **ELSE IF** transition is Derivable→Derivable - `inst-valr-5`
   1. [ ] - `p1` - **IF** `new_derived` equals the stored alias - `inst-valr-5a`
      1. [ ] - `p1` - Retain the stored alias - `inst-valr-5a1`
   2. [ ] - `p1` - **ELSE** - `inst-valr-5b`
      1. [ ] - `p1` - Add error: alias would change; delete and re-create instead - `inst-valr-5b1`
6. [ ] - `p1` - **ELSE IF** transition is Non-derivable→Non-derivable - `inst-valr-6`
   1. [ ] - `p1` - **IF** the body's explicit alias equals the stored alias - `inst-valr-6a`
      1. [ ] - `p1` - Retain the stored alias - `inst-valr-6a1`
   2. [ ] - `p1` - **ELSE** - `inst-valr-6b`
      1. [ ] - `p1` - Add error: a differing user-provided alias is not accepted - `inst-valr-6b1`
7. [ ] - `p1` - **ELSE** (transition is Derivable→Non-derivable or Non-derivable→Derivable) - `inst-valr-7`
   1. [ ] - `p1` - Add error: alias immutable across this transition; delete and re-create instead - `inst-valr-7a`
8. [ ] - `p1` - **IF** the new body's `auth`, `plugins`, endpoints, or alias differ from the stored row - `inst-valr-8`
   1. [ ] - `p1` - Re-run the ancestor-bind check from `cpt-cf-oagw-algo-upstream-api-validate-create` steps 9c-9d against the (possibly unchanged) effective alias - `inst-valr-8a`
9. [ ] - `p1` - **RETURN** normalized record when the error list is empty, otherwise the error list - `inst-valr-9`

### List Query Algorithm

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-api-list-query`

**Input**: Raw `$filter`, `$select`, `$orderby`, `$top`, `$skip` query string values

**Output**: A parsed, bounded query descriptor consumed by the list flow's store lookup

**Steps**:
1. [ ] - `p1` - Parse `$filter` as an OData boolean expression over Upstream fields (`alias`, `enabled`, `tags`, and nested `server`/`protocol` fields); reject unparseable expressions with a validation error - `inst-lq-1`
2. [ ] - `p1` - Parse `$select` as a comma-separated field list; empty or absent means all fields - `inst-lq-2`
3. [ ] - `p1` - Parse `$orderby` as `field [asc|desc]`; default direction is `asc` - `inst-lq-3`
4. [ ] - `p1` - **IF** `$top` is absent - `inst-lq-4`
   1. [ ] - `p1` - Use 50 - `inst-lq-4a`
5. [ ] - `p1` - **ELSE IF** `$top` exceeds 100 - `inst-lq-5`
   1. [ ] - `p1` - Clamp to 100 - `inst-lq-5a`
6. [ ] - `p1` - **IF** `$skip` is absent - `inst-lq-6`
   1. [ ] - `p1` - Use 0 - `inst-lq-6a`
7. [ ] - `p1` - **RETURN** the parsed descriptor - `inst-lq-7`

## 4. States (CDSL)

### Upstream Enabled State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-upstream-api-enabled`

**States**: enabled, disabled, ancestor-disabled

**Initial State**: enabled

**Transitions**:
1. [ ] - `p1` - **FROM** enabled **TO** disabled **WHEN** a `PUT` on the same tenant's own upstream sets `enabled: false` - `inst-state-1`
2. [ ] - `p1` - **FROM** disabled **TO** enabled **WHEN** a `PUT` on the same tenant's own upstream sets `enabled: true`, and no ancestor upstream sharing the same alias is itself disabled - `inst-state-2`
3. [ ] - `p1` - **FROM** enabled **TO** ancestor-disabled **WHEN** the ancestor upstream that this tenant's upstream binds to (same alias, reached through the tenant chain) transitions to disabled - `inst-state-3`
4. [ ] - `p1` - **FROM** ancestor-disabled **TO** ancestor-disabled **WHEN** a descendant's `PUT` sets its own `enabled: true` while the ancestor remains disabled — the write to the descendant's own `enabled` field is accepted and stored, but the state used for authorization decisions in this feature (whether the descendant's `enabled` value is honored) stays `ancestor-disabled` until the ancestor upstream is re-enabled - `inst-state-4`

## 5. Definitions of Done

### Create Upstream Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-api-create`

The system **MUST** implement `POST /oagw/v1/upstreams` so it validates the request body, derives
or validates the alias, enforces `(tenant_id, alias)` uniqueness with a `409 Conflict` on
collision, applies the ancestor-alias bind check (`oagw:upstream:bind` plus sharing-mode
rules), accepts `http`- and `ws`-scheme endpoints unconditionally at validation time, and
returns `201 Created` with a server-generated UUID `id`.

**Implements**:
- `cpt-cf-oagw-flow-upstream-api-create`
- `cpt-cf-oagw-algo-upstream-api-validate-create`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`

### List and Get Upstream Endpoints

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-api-list-get`

The system **MUST** implement `GET /oagw/v1/upstreams` with `$filter`, `$select`, `$orderby`,
`$top` (default 50, max 100), and `$skip`, and `GET /oagw/v1/upstreams/{id}`, both scoped
strictly to the calling tenant so an ancestor's upstream, or an unknown `id`, answers `404`.

**Implements**:
- `cpt-cf-oagw-flow-upstream-api-list`
- `cpt-cf-oagw-flow-upstream-api-get`
- `cpt-cf-oagw-algo-upstream-api-list-query`

**Touches**:
- API: `GET /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`

### Replace Upstream Endpoint

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-api-replace`

The system **MUST** implement `PUT /oagw/v1/upstreams/{id}` as a full-document replace that
clears any optional field the request omits, keeps `id` and `tenant_id` immutable, applies the
alias-transition table (reject any request that would change a derived or explicit alias), and
re-validates the ancestor-bind constraint when overrides, endpoints, or alias changed.

**Implements**:
- `cpt-cf-oagw-flow-upstream-api-replace`
- `cpt-cf-oagw-algo-upstream-api-validate-replace`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`

### Delete Upstream Endpoint

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-api-delete`

The system **MUST** implement `DELETE /oagw/v1/upstreams/{id}` scoped to the calling tenant,
returning `204 No Content` on success and `404` for an unknown or ancestor-owned `id`, and a
subsequent `GET` on the deleted `id` **MUST** also answer `404`.

**Implements**:
- `cpt-cf-oagw-flow-upstream-api-delete`

**Touches**:
- API: `DELETE /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`

### Enabled/Disabled Semantics

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-api-enabled`

The system **MUST** default `enabled` to `true` when a create request omits it, store the
explicit value when supplied, and reset `enabled` to the same schema default `true` whenever a
replace request omits it, even if the stored upstream was `enabled: false` beforehand — full
replacement never carries forward a previous value for an omitted field. The system **MUST**
also honor the rule that a descendant tenant cannot re-enable an upstream whose ancestor,
sharing the same alias, is disabled — the descendant's own stored `enabled` value persists as
written but has no effect while the ancestor stays disabled.

**Implements**:
- `cpt-cf-oagw-state-upstream-api-enabled`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`

### Per-Endpoint Authorization

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-api-authz`

The system **MUST** gate every one of the five endpoints on its own permission —
`gts.cf.core.oagw.upstream.v1~:create` for `POST`, `:read` for both `GET` forms,
`:override` for `PUT`, and `:delete` for `DELETE` — and additionally require
`oagw:upstream:bind` whenever a create or replace request's effective alias matches an
ancestor tenant's upstream alias.

**Implements**:
- `cpt-cf-oagw-flow-upstream-api-create`
- `cpt-cf-oagw-flow-upstream-api-list`
- `cpt-cf-oagw-flow-upstream-api-get`
- `cpt-cf-oagw-flow-upstream-api-replace`
- `cpt-cf-oagw-flow-upstream-api-delete`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams/{id}`
- API: `PUT /oagw/v1/upstreams/{id}`
- API: `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### RFC 9457 Error Responses

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-api-errors`

The system **MUST** report every error from these five endpoints as an RFC 9457
`application/problem+json` document carrying one of four GTS `type` identifiers:
`gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` for `400`,
`gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` for `401`,
`gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` for `404`, and
`gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1` for `409`. The system **MUST**
also attach `X-OAGW-Error-Source: gateway` to every one of these responses, since none of them
is a passthrough from an upstream service. The `400` and `401` identifiers come directly from
DESIGN.md's error table. DESIGN.md defines only one `404` identifier for the whole gateway, so
this management API reuses that same `route.not_found` identifier for every resource-not-found
case described below, instead of minting an upstream-specific one. DESIGN.md's error table has
no `409` row for an alias conflict; its only `409` entry is `PluginInUse`, which covers a
different resource entirely. This feature therefore introduces the `alias_conflict` identifier
above as the single literal every implementation of the create and replace flows must emit.
Separately, `cpt-cf-oagw-fr-error-codes` in PRD.md lists a closed set of error codes with no
`409` row at all, so that table is not exhaustive for this feature's conflict case, a gap that
DECOMPOSITION.md and DESIGN.md both corroborate by documenting a `409 Conflict` response
elsewhere.

**Implements**:
- `cpt-cf-oagw-flow-upstream-api-create`
- `cpt-cf-oagw-flow-upstream-api-list`
- `cpt-cf-oagw-flow-upstream-api-get`
- `cpt-cf-oagw-flow-upstream-api-replace`
- `cpt-cf-oagw-flow-upstream-api-delete`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams/{id}`
- API: `PUT /oagw/v1/upstreams/{id}`
- API: `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/upstreams` with a valid body returns `201 Created` and a server-generated `id` not present in the request.
- [ ] `POST /oagw/v1/upstreams` with an endpoint of `scheme: "http"` and `port: 80` succeeds and returns `201 Created`.
- [ ] `POST /oagw/v1/upstreams` with an alias that already exists for the calling tenant returns `409 Conflict` as `application/problem+json` with `type` set to `gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1`.
- [ ] `GET /oagw/v1/upstreams/{id}` for an id unknown to the calling tenant returns `404` as `application/problem+json` with `type` set to `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` and `X-OAGW-Error-Source: gateway`.
- [ ] `PUT /oagw/v1/upstreams/{id}` with a body that omits a previously-set `tags` array stores `tags` as cleared, and the subsequent `GET` returns `tags` as an empty array `[]`.
- [ ] `PUT /oagw/v1/upstreams/{id}` with a body that omits `enabled` on an upstream currently stored as `enabled: false` returns the replaced document with `enabled: true`, and the subsequent `GET` confirms the reset value.
- [ ] `PUT /oagw/v1/upstreams/{id}` whose endpoint change would alter the alias returns `400 ValidationError`.
- [ ] `DELETE /oagw/v1/upstreams/{id}` returns `204 No Content`, and a subsequent `GET /oagw/v1/upstreams/{id}` on the same id returns `404`.
- [ ] `GET /oagw/v1/upstreams?$top=1` returns at most one result when more than one upstream exists for the calling tenant.
- [ ] `GET /oagw/v1/upstreams?$filter=` with an unparseable expression returns `400 ValidationError` as `application/problem+json` with `X-OAGW-Error-Source: gateway`, rather than an empty or error-free result set.
- [ ] A request to any of the five endpoints without a valid Bearer token, or with a token lacking the endpoint's required permission, returns `401`.
- [ ] Creating an upstream whose alias matches an ancestor tenant's alias without `oagw:upstream:bind` returns `400 ValidationError` rather than silently binding.
