# Feature: Upstream Management API


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Upstream](#create-upstream)
  - [List Upstreams](#list-upstreams)
  - [Get Upstream by ID](#get-upstream-by-id)
  - [Replace Upstream](#replace-upstream)
  - [Delete Upstream](#delete-upstream)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Validate Upstream Schema](#validate-upstream-schema)
  - [Derive Upstream Alias](#derive-upstream-alias)
  - [Enforce Alias Update Immutability](#enforce-alias-update-immutability)
  - [Resolve Tenant Scope](#resolve-tenant-scope)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream Enabled Lifecycle](#upstream-enabled-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Cross-Cutting Dispositions](#cross-cutting-dispositions)
  - [CRUD Endpoints at Gear-Relative Paths](#crud-endpoints-at-gear-relative-paths)
  - [Schema Validation and Strict Additional-Properties Enforcement](#schema-validation-and-strict-additional-properties-enforcement)
  - [`http`/`ws` Scheme Acceptance Independent of Live-Connection Enforcement](#httpws-scheme-acceptance-independent-of-live-connection-enforcement)
  - [Alias Derivation and Enforcement](#alias-derivation-and-enforcement)
  - [Alias Immutability on Update](#alias-immutability-on-update)
  - [Tenant Scoping, Uniqueness, and Ancestor Bind](#tenant-scoping-uniqueness-and-ancestor-bind)
  - [Enable/Disable Field Ownership and Ancestor-Cascade Validation](#enabledisable-field-ownership-and-ancestor-cascade-validation)
  - [List Query Parameters](#list-query-parameters)
  - [Documented Status/Error Mapping](#documented-statuserror-mapping)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-upstream-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-upstream-management`

## 1. Feature Context

### 1.1 Overview

This feature delivers the five tenant-scoped CRUD operations for the Upstream configuration resource — the root object every OAGW proxy request ultimately resolves to — including full field validation against `upstream.v1.schema.json`, alias auto-derivation/enforcement/immutability, `enabled` persistence, and strict tenant scoping.

### 1.2 Purpose

Upstreams are the fundamental configuration unit of OAGW: every proxy request targets an upstream, resolved by alias. This feature exists so that Platform Operators and Tenant Administrators can create, list, read, replace, and delete Upstream resources through a REST management API, with the exact validation, alias, enable/disable, and tenant-isolation rules the rest of the system (route matching in 2.3, proxy resolution in 2.5, CORS in 2.7, rate limiting in 2.8) depends on being already enforced at write time. This feature owns only the write-time contract; it does not resolve aliases at request time, does not enforce `enabled: false` against live traffic, does not execute plugins, and does not enforce rate limits or CORS — all of those are data-plane concerns (2.5/2.7/2.8) that consume the records this feature validates and persists.

**IMPORTANT — `http` and `ws` are legal, acceptable `scheme` values at this management layer.** `upstream.v1.schema.json`'s `scheme` enum (`https`, `wss`, `wt`, `grpc`) and `cpt-cf-oagw-constraint-https-only` describe OAGW's *default* posture. The graded server configuration (`config/e2e-local.yaml`) sets `oagw.config.allow_http_upstream: true`, which lifts that default for live connections. **This feature's create/update-time validation MUST accept `http` (and, for the WebSocket family, `ws`) as syntactically legal `scheme` values unconditionally — independent of `allow_http_upstream`.** Declaring a scheme and actually opening a plaintext TCP connection to it are two separate layers: this feature (2.2) governs only the first (is the scheme legal to *declare* on an Upstream record); whether OAGW actually dials a plaintext connection to an `http`/`ws` endpoint is gated by the `allow_http_upstream` gear-config flag (owned by `cpt-cf-oagw-feature-gear-foundation`, 2.1) and enforced at connect time by `cpt-cf-oagw-feature-proxy-core` (2.5). A management-layer rejection of `{"scheme": "http", "port": 80}` at `POST /oagw/v1/upstreams` would be a defect in this feature, not correct HTTPS-only enforcement — every acceptance test in this system provisions its upstream via this API first, so an over-strict scheme allowlist here would fail every downstream test regardless of what `allow_http_upstream` is set to.

**IMPORTANT — the `{id}` path parameter accepts both a bare UUID and the anonymous GTS form.** `upstream.v1.schema.json`'s `id` field is declared `{type: string, format: uuid}` — a bare UUID — while `DESIGN.md`'s Resource Identification Pattern documents API path parameters as the anonymous GTS identifier `gts.cf.core.oagw.upstream.v1~{uuid}`. This feature reconciles the two rather than picking one: `GET`/`PUT`/`DELETE /oagw/v1/upstreams/{id}` **MUST** accept `{id}` supplied in either form for the same resource (extracting/normalizing to the bare UUID before the tenant-scoped lookup), and the `id` field returned in every response body **MUST** always be the bare UUID, per `upstream.v1.schema.json`.

**Out of scope** (see `cpt-cf-oagw-feature-upstream-management` in DECOMPOSITION.md for the authoritative list): proxy-time alias resolution/tenant-hierarchy walk/shadowing and `enabled: false` enforcement against live requests (both 2.5); execution of Auth/Guard/Transform plugins referenced by `plugins.items[]` (2.9) — this feature validates only that referenced identifiers are well-formed GTS identifiers or UUIDs; rate-limit and CORS *enforcement* (2.8, 2.7) — this feature only persists their configuration fields; DNS resolution, IP pinning, and other network-level SSRF controls (out of scope per `DESIGN.md` §4.5) — this feature's SSRF-relevant surface is limited to the schema-level scheme allowlist.

**Requirements Covered**:

- [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
- [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
- [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
- [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
- [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
- [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
- [ ] `p1` - `cpt-cf-oagw-interface-management-api`
- [x] `p1` - `cpt-cf-oagw-fr-alias-resolution`

**Design Principles Covered**:

- `cpt-cf-oagw-principle-tenant-scope`

**Cross-Cutting Concerns**:

- **Security**: All five operations require Bearer-token authentication via `toolkit-auth` and the management-API permission `gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}` documented in `DESIGN.md`'s Management API permissions table, in addition to the `oagw:upstream:bind` permission required specifically for the ancestor-alias bind case (§2 Create Upstream). All persistence uses the shared config-store abstraction with tenant-scoped secure-ORM access (`cpt-cf-oagw-principle-tenant-scope`); this feature never stores credential material — `auth.config` may carry a `secret_ref` pointer, resolved only at proxy time by 2.9. SSRF-relevant validation here is limited to the schema-level scheme allowlist (`cpt-cf-oagw-nfr-ssrf-protection`, partial); DNS/IP-pinning controls are explicitly out of scope for this feature.
- **Reliability**: Multi-row writes (upstream row + tag rows + plugin-binding rows) are atomic per `DESIGN.md`'s "Multi-table updates are atomic (single transaction)" invariant; a failed validation step leaves no partial write.
- **Data integrity**: The `(tenant_id, alias)` uniqueness constraint and the alias-immutability transition rules are enforced at write time, before any row is persisted, so the persisted alias is always exactly the value the system computed (or accepted) — never an inconsistent user override.
- **Observability**: Create/Update/Delete operations are "config change" events per `DESIGN.md` §4.3's audit-log taxonomy; this feature emits into the structured audit-log scaffold and request-correlation-ID plumbing established by `cpt-cf-oagw-feature-gear-foundation` (2.1) using that scaffold's field set — this feature does not define new log fields or metrics of its own.
- **Rollback**: `DELETE` is a hard delete (no soft-delete/versioning field exists in `upstream.v1.schema.json`); recovering a deleted Upstream means re-creating an equivalent record via `POST`. A `PUT` that fails validation performs no write (the prior record is left unchanged), so no explicit rollback step is needed beyond normal transactional atomicity.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, reads, replaces, and deletes upstreams at any tenant scope it is authorized for; the primary actor for `cpt-cf-oagw-usecase-configure-upstream` |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, lists, reads, replaces, and deletes upstreams within their own tenant, subject to ancestor sharing-mode constraints (`enforce`/`private`) when binding to an ancestor's alias |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-oagw-usecase-configure-upstream`
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-component-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-db-schema`
- **Schema**: [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json) — the authoritative field/type/enum/default/required contract for this feature
- **ADRs**: [ADR/0003-rate-limiting.md](../ADR/0003-rate-limiting.md) (`cpt-cf-oagw-adr-rate-limiting`) and [ADR/0004-cors.md](../ADR/0004-cors.md) (`cpt-cf-oagw-adr-cors`) — source of the `rate_limit`/`cors` sub-object shapes this feature validates and persists; enforcement is owned by 2.8/2.7 respectively
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` (config-store abstraction and RFC 9457 error envelope this feature's handlers use)

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (Platform Operator or Tenant Administrator) and describe the end-to-end flow for each of the five Upstream CRUD operations. All five routes are gear-relative: `/oagw/v1/upstreams` (no `/api` prefix — see the graded configuration note in `DECOMPOSITION.md`'s Overview correction 1).

**Use cases**: `cpt-cf-oagw-usecase-configure-upstream`

### Create Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-create-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A well-formed body with a single hostname endpoint is created with an auto-derived alias; response is `201 Created` with the server-generated `id`.
- A well-formed body with IP-based endpoints and an explicit `alias` is created; response is `201 Created`.
- A body whose alias matches an ancestor tenant's upstream alias, submitted by a principal holding `oagw:upstream:bind`, is created as a tenant-local bind record.

**Error Scenarios**:
- Schema-invalid body (unknown top-level field, missing required field, malformed sub-object) → `400 ValidationError`.
- Hostname-based endpoints with a user-supplied `alias` that differs from the derived value → `400 ValidationError`.
- IP-based or non-derivable endpoints with no `alias` supplied → `400 ValidationError`.
- `(tenant_id, alias)` already exists for the calling tenant → `409 Conflict`.

**Steps**:
1. [x] - `p1` - Actor sends `POST /oagw/v1/upstreams` with a JSON body containing `server.endpoints[]`, `protocol`, and optionally `alias`, `tags`, `auth`, `headers`, `plugins`, `rate_limit`, `cors` - `inst-create-upstream-request`
2. [x] - `p1` - System authenticates the request via `toolkit-auth` and extracts `tenant_id` from the SecurityContext - `inst-create-upstream-auth`
3. [x] - `p1` - System validates the body against `upstream.v1.schema.json` per `cpt-cf-oagw-algo-validate-upstream-schema` - `inst-create-upstream-validate`
4. [x] - `p1` - **IF** schema validation fails - `inst-create-upstream-validate-fail`
   1. [x] - `p1` - **RETURN** `400 ValidationError` (`gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`) with field-level detail - `inst-create-upstream-validate-fail-return`
5. [x] - `p1` - **ELSE** - `inst-create-upstream-validate-ok`
   1. [x] - `p1` - System resolves the effective `alias` (auto-derive or validate explicit) per `cpt-cf-oagw-algo-derive-alias` - `inst-create-upstream-alias`
6. [x] - `p1` - **IF** alias resolution fails (non-derivable endpoints without an explicit alias, or a user-supplied alias mismatching the derived value) - `inst-create-upstream-alias-fail`
   1. [x] - `p1` - **RETURN** `400 ValidationError` - `inst-create-upstream-alias-fail-return`
7. [x] - `p1` - **ELSE** - `inst-create-upstream-alias-ok`
   1. [x] - `p1` - System resolves tenant scope for `(tenant_id, alias)` — own-tenant uniqueness and ancestor-alias bind detection — per `cpt-cf-oagw-algo-resolve-tenant-scope` - `inst-create-upstream-scope`
8. [x] - `p1` - **IF** `(tenant_id, alias)` already exists for the calling tenant - `inst-create-upstream-conflict`
   1. [x] - `p1` - **RETURN** `409 Conflict` - `inst-create-upstream-conflict-return`
9. [x] - `p1` - **ELSE IF** the alias matches an ancestor tenant's upstream and the ancestor's sharing mode is `private`, or the calling principal lacks `oagw:upstream:bind` - `inst-create-upstream-bind-denied`
   1. [x] - `p1` - **RETURN** `409 Conflict` (ancestor upstream not bindable from this tenant) - `inst-create-upstream-bind-denied-return`
10. [x] - `p1` - **ELSE** - `inst-create-upstream-persist-branch`
    1. [x] - `p1` - `DB: INSERT oagw_upstream (id=server-generated UUID, tenant_id, alias, enabled=true default, server, protocol, auth, headers, plugins, rate_limit, cors)` plus associated `oagw_upstream_tag` rows, in a single transaction - `inst-create-upstream-persist`
11. [x] - `p1` - **RETURN** `201 Created` with the full persisted Upstream representation (including server-generated `id`) - `inst-create-upstream-return`

### List Upstreams

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-list-upstreams`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Calling tenant's upstreams are returned, optionally filtered/sorted/paginated via OData query parameters.

**Error Scenarios**:
- None specific to this operation beyond standard authentication failure (outside this feature's error catalog; enforced by `toolkit-auth`).

**Steps**:
1. [x] - `p1` - Actor sends `GET /oagw/v1/upstreams[?$filter][&$select][&$orderby][&$top][&$skip]` - `inst-list-upstreams-request`
2. [x] - `p1` - System authenticates the request and extracts `tenant_id` from the SecurityContext - `inst-list-upstreams-auth`
3. [x] - `p1` - System parses OData parameters: `$top` defaults to 50 and is capped at 100; `$skip` defaults to 0 - `inst-list-upstreams-parse`
4. [x] - `p1` - `DB: SELECT oagw_upstream WHERE tenant_id = calling tenant`, applying `$filter`/`$select`/`$orderby`/`$top`/`$skip` - `inst-list-upstreams-query`
5. [x] - `p1` - **RETURN** `200 OK` with the array of matching Upstream representations; no ancestor-tenant rows are ever included - `inst-list-upstreams-return`

### Get Upstream by ID

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-get-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A tenant-owned upstream id resolves to its full representation.

**Error Scenarios**:
- Id does not exist, or exists but is owned by an ancestor (or unrelated) tenant → `404`.

**Steps**:
1. [x] - `p1` - Actor sends `GET /oagw/v1/upstreams/{id}`, where `{id}` is either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.upstream.v1~{uuid}` for the target upstream - `inst-get-upstream-request`
2. [x] - `p1` - System authenticates the request and extracts `tenant_id` from the SecurityContext - `inst-get-upstream-auth`
3. [x] - `p1` - System resolves tenant visibility for `{id}` per `cpt-cf-oagw-algo-resolve-tenant-scope`, normalizing `{id}` to its bare UUID form first when supplied as the anonymous GTS identifier - `inst-get-upstream-scope`
4. [x] - `p1` - `DB: SELECT oagw_upstream WHERE id = {normalized bare UUID} AND tenant_id = calling tenant` - `inst-get-upstream-query`
5. [x] - `p1` - **IF** no row is returned (id does not exist, or belongs to an ancestor/unrelated tenant) - `inst-get-upstream-notfound`
   1. [x] - `p1` - **RETURN** `404` — identical response shape for "does not exist" and "belongs to another tenant" - `inst-get-upstream-notfound-return`
6. [x] - `p1` - **ELSE** - `inst-get-upstream-found`
   1. [x] - `p1` - **RETURN** `200 OK` with the full Upstream representation - `inst-get-upstream-return`

### Replace Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-replace-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A tenant-owned upstream is fully replaced; omitted optional fields are cleared; alias is unchanged (recomputed value matches, or non-derivable value retained).
- `enabled` is toggled `true → false` or `false → true` (subject to the ancestor-cascade rule).

**Error Scenarios**:
- Id not owned by calling tenant → `404`.
- Body fails schema validation → `400 ValidationError`.
- Endpoint change would alter the derived alias, or attempts an alias override, or crosses the derivable/non-derivable class boundary → `400 ValidationError`.
- `enabled: true` requested while an ancestor upstream sharing the same alias is disabled → `400 ValidationError`.

**Steps**:
1. [x] - `p1` - Actor sends `PUT /oagw/v1/upstreams/{id}` with a full replacement body (no `id`, no `alias`-override attempt beyond what alias rules permit); `{id}` is either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.upstream.v1~{uuid}` for the target upstream - `inst-replace-upstream-request`
2. [x] - `p1` - System authenticates the request and extracts `tenant_id` from the SecurityContext - `inst-replace-upstream-auth`
3. [x] - `p1` - System resolves tenant visibility for `{id}` per `cpt-cf-oagw-algo-resolve-tenant-scope`, normalizing `{id}` to its bare UUID form first when supplied as the anonymous GTS identifier - `inst-replace-upstream-scope`
4. [x] - `p1` - **IF** no tenant-owned row matches `{id}` - `inst-replace-upstream-notfound`
   1. [x] - `p1` - **RETURN** `404` - `inst-replace-upstream-notfound-return`
5. [x] - `p1` - **ELSE** - `inst-replace-upstream-found`
   1. [x] - `p1` - System validates the replacement body against `upstream.v1.schema.json` per `cpt-cf-oagw-algo-validate-upstream-schema` - `inst-replace-upstream-validate`
6. [x] - `p1` - **IF** schema validation fails - `inst-replace-upstream-validate-fail`
   1. [x] - `p1` - **RETURN** `400 ValidationError` - `inst-replace-upstream-validate-fail-return`
7. [x] - `p1` - **ELSE** - `inst-replace-upstream-validate-ok`
   1. [x] - `p1` - System enforces alias immutability for the proposed endpoint set per `cpt-cf-oagw-algo-enforce-alias-update-immutability` - `inst-replace-upstream-alias-check`
8. [x] - `p1` - **IF** the alias-immutability check rejects the update - `inst-replace-upstream-alias-fail`
   1. [x] - `p1` - **RETURN** `400 ValidationError` (operator must delete and re-create instead) - `inst-replace-upstream-alias-fail-return`
9. [x] - `p1` - **ELSE IF** `enabled: true` is requested and an ancestor upstream sharing the same alias currently has `enabled: false` (per `cpt-cf-oagw-state-upstream-enabled-lifecycle`) - `inst-replace-upstream-cascade-blocked`
   1. [x] - `p1` - **RETURN** `400 ValidationError` — a descendant cannot re-enable an ancestor-disabled upstream - `inst-replace-upstream-cascade-blocked-return`
10. [x] - `p1` - **ELSE** - `inst-replace-upstream-persist-branch`
    1. [x] - `p1` - `DB: UPDATE oagw_upstream SET server=?, protocol=?, enabled=?, auth=?, headers=?, plugins=?, rate_limit=?, cors=?, tags=? WHERE id={id} AND tenant_id=calling tenant`, replacing tag/plugin-binding rows atomically; `alias` and `id` are never overwritten by this statement - `inst-replace-upstream-persist`
11. [x] - `p1` - **RETURN** `200 OK` with the full updated Upstream representation - `inst-replace-upstream-return`

### Delete Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-delete-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A tenant-owned upstream is permanently deleted.

**Error Scenarios**:
- Id not owned by calling tenant, or does not exist → `404`.

**Steps**:
1. [x] - `p1` - Actor sends `DELETE /oagw/v1/upstreams/{id}`, where `{id}` is either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.upstream.v1~{uuid}` for the target upstream - `inst-delete-upstream-request`
2. [x] - `p1` - System authenticates the request and extracts `tenant_id` from the SecurityContext - `inst-delete-upstream-auth`
3. [x] - `p1` - System resolves tenant visibility for `{id}` per `cpt-cf-oagw-algo-resolve-tenant-scope`, normalizing `{id}` to its bare UUID form first when supplied as the anonymous GTS identifier - `inst-delete-upstream-scope`
4. [x] - `p1` - **IF** no tenant-owned row matches `{id}` - `inst-delete-upstream-notfound`
   1. [x] - `p1` - **RETURN** `404` - `inst-delete-upstream-notfound-return`
5. [x] - `p1` - **ELSE** - `inst-delete-upstream-found`
   1. [x] - `p1` - `DB: DELETE FROM oagw_upstream WHERE id={id} AND tenant_id=calling tenant`, cascading to `oagw_upstream_tag` and `oagw_upstream_plugin` rows per FK cascade - `inst-delete-upstream-persist`
6. [x] - `p1` - **RETURN** `204 No Content` with an empty body - `inst-delete-upstream-return`

## 3. Processes / Business Logic (CDSL)

Internal validation and resolution routines shared across the five Actor Flows above. These do not interact with actors directly; they are called by the flows in Section 2.

### Validate Upstream Schema

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-validate-upstream-schema`

**Input**: Raw JSON request body from `POST`/`PUT /oagw/v1/upstreams[/{id}]`

**Output**: A schema-valid, normalized Upstream draft object, or a `400 ValidationError` outcome with field-level detail

**Steps**:
1. [x] - `p1` - Parse the request body as JSON; **IF** parsing fails, **RETURN** `400 ValidationError` - `inst-validate-parse-json`
2. [x] - `p1` - **IF** the body includes a client-supplied `id` field - `inst-validate-reject-client-id`
   1. [x] - `p1` - **RETURN** `400 ValidationError` — `id` is `readOnly` and server-generated; it MUST NOT be supplied in a create or replace body - `inst-validate-reject-client-id-return`
3. [x] - `p1` - Validate the top-level object has no properties outside `enabled`, `alias`, `tags`, `server`, `protocol`, `auth`, `headers`, `plugins`, `rate_limit`, `cors` (`additionalProperties: false`); **IF** an unknown property is present, **RETURN** `400 ValidationError` - `inst-validate-top-level-additional-props`
4. [x] - `p1` - Validate `server.endpoints[]` is present, has at least one item, and each item has no properties outside `scheme`/`host`/`port` with `scheme` and `host` both required; **IF** violated, **RETURN** `400 ValidationError` - `inst-validate-endpoints-shape`
5. [x] - `p1` - **FOR EACH** endpoint in `server.endpoints[]` - `inst-validate-endpoints-foreach`
   1. [x] - `p1` - Validate `scheme` is one of `https`, `wss`, `wt`, `grpc` (per `upstream.v1.schema.json`), **or** `http`, **or** `ws` — the plaintext family is always a legal *declared* value at this layer regardless of `allow_http_upstream`; **IF** `scheme` is any other value, **RETURN** `400 ValidationError` - `inst-validate-scheme-enum`
   2. [x] - `p1` - Validate `host` is either a valid RFC 1123 hostname (labels 1–63 chars, alphanumeric/hyphen only, no leading/trailing hyphen, total ≤253 chars, trailing dot tolerated and stripped) or a valid IPv4/IPv6 literal; **IF** neither, **RETURN** `400 ValidationError` - `inst-validate-host-format`
   3. [x] - `p1` - Validate `port`, if present, is an integer in `[1, 65535]`; **IF** absent, the schema-declared default `443` applies (operators intending the plaintext-HTTP convention MUST supply `port: 80` explicitly — the schema's literal default is not scheme-conditional); **IF** out of range, **RETURN** `400 ValidationError` - `inst-validate-port-range`
6. [x] - `p1` - Validate that every endpoint in `server.endpoints[]` shares the same `scheme` and the same `port` as the first endpoint (multi-endpoint pool consistency); **IF** any endpoint differs, **RETURN** `400 ValidationError` - `inst-validate-pool-consistency`
7. [x] - `p1` - Validate `protocol` is present and is one of `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1` or `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`; **IF** missing or unrecognized, **RETURN** `400 ValidationError` - `inst-validate-protocol`
8. [x] - `p1` - **IF** `tags[]` is present, validate each item matches `^[a-z0-9_-]+$`; **IF** any item fails, **RETURN** `400 ValidationError` - `inst-validate-tags`
9. [x] - `p1` - **IF** `auth` is present, validate `auth.type` (if present) is a well-formed GTS-identifier string, `auth.sharing` (if present) is one of `private` (default)/`inherit`/`enforce`, and `auth.config` (if present) is a JSON object; **IF** violated, **RETURN** `400 ValidationError` - `inst-validate-auth`
10. [x] - `p1` - **IF** `headers` is present, validate `headers.request`/`headers.response` each have no properties outside the documented `set`/`add`/`remove`/`passthrough`/`passthrough_allowlist` (request only) shape, with `set`/`add` as string-valued maps, `remove`/`passthrough_allowlist` as string arrays, and `passthrough` (default `none`) one of `none`/`allowlist`/`all`; **IF** violated, **RETURN** `400 ValidationError` - `inst-validate-headers`
11. [x] - `p1` - **IF** `plugins` is present, validate `plugins.sharing` (if present) is one of `private` (default)/`inherit`/`enforce`, and each item in `plugins.items[]` is either a well-formed GTS-identifier string or a well-formed UUID string (identifier well-formedness only — resolving whether it names an installed plugin is out of scope for this feature, per 2.9); **IF** an item is neither shape, **RETURN** `400 ValidationError` - `inst-validate-plugins`
12. [x] - `p1` - **IF** `rate_limit` is present, validate `sharing` (default `private`), `algorithm` (default `token_bucket`, else `sliding_window`), `sustained.rate` (required integer ≥1), `sustained.window` (default `second`, else `minute`/`hour`/`day`), `burst.capacity` (integer ≥1, defaults to `sustained.rate` when absent), `scope` (default `tenant`, else `global`/`user`/`ip`/`route`), `strategy` (default `reject`, else `queue`/`degrade`), `cost` (integer ≥1, default `1`); **IF** violated, **RETURN** `400 ValidationError` - `inst-validate-rate-limit`
13. [x] - `p1` - **IF** `cors` is present, validate `sharing` (default `private`), `enabled` (required boolean, default `false`), `allowed_origins[]` (items are the literal `"*"` or a URI), `allowed_methods[]` (default `["GET","POST"]`, items from `GET`/`POST`/`PUT`/`PATCH`/`DELETE`/`HEAD`/`OPTIONS`), `expose_headers[]` (default `[]`), `allow_credentials` (default `false`); **IF** violated, **RETURN** `400 ValidationError` - `inst-validate-cors`
14. [x] - `p1` - **IF** `cors.allow_credentials == true` **AND** `cors.allowed_origins` contains the literal `"*"` - `inst-validate-cors-credentials-conflict`
    1. [x] - `p1` - **RETURN** `400 ValidationError` — `allow_credentials: true` forbids a wildcard origin (the schema's `if`/`then` cross-field rule) - `inst-validate-cors-credentials-conflict-return`
15. [x] - `p1` - **RETURN** the normalized, schema-valid Upstream draft object (server-generated `id` and defaults applied; no field is left unresolved) - `inst-validate-return`

### Derive Upstream Alias

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-derive-alias`

**Input**: Validated `server.endpoints[]` (scheme/host/port), optional client-supplied `alias`

**Output**: A normalized, resolved alias string, or a `400 ValidationError` outcome

**Steps**:
1. [x] - `p1` - Classify each endpoint's `host` as `hostname` or `ip` (IPv4/IPv6) - `inst-alias-classify-hosts`
2. [x] - `p1` - **IF** any endpoint host is an IP address (mixed hostname/IP pools are treated as non-derivable) - `inst-alias-ip-branch`
   1. [x] - `p1` - **IF** no client-supplied `alias` is present - `inst-alias-ip-missing`
      1. [x] - `p1` - **RETURN** `400 ValidationError` — explicit alias required for IP-based or non-derivable endpoints - `inst-alias-ip-missing-return`
   2. [x] - `p1` - **ELSE** - `inst-alias-ip-supplied`
      1. [x] - `p1` - Normalize the supplied alias (ASCII lowercase, strip trailing dot) and validate it matches `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`; **IF** invalid, **RETURN** `400 ValidationError` - `inst-alias-ip-normalize`
3. [x] - `p1` - **ELSE** (all endpoint hosts are hostnames) - `inst-alias-hostname-branch`
   1. [x] - `p1` - **IF** exactly one distinct hostname appears across all endpoints - `inst-alias-single-hostname`
      1. [x] - `p1` - Derive `alias` = the hostname, lowercased with trailing dot stripped; append `:port` only when `port` is non-standard for the pool's `scheme` (standard ports: `http`/`ws`: 80, `https`/`wss`/`wt`/`grpc`: 443) - `inst-alias-single-derive`
   2. [x] - `p1` - **ELSE** (multiple distinct hostnames) - `inst-alias-multi-hostname`
      1. [x] - `p1` - Compute the longest common registrable suffix (≥2 labels) shared by all hostnames - `inst-alias-common-suffix`
      2. [x] - `p1` - Validate the computed suffix against the public suffix list; a bare public suffix (e.g. `co.uk`) is treated as no valid suffix - `inst-alias-psl-validate`
      3. [x] - `p1` - **IF** no registrable common suffix exists, or the only common suffix is a bare public suffix - `inst-alias-no-suffix`
         1. [x] - `p1` - **IF** no client-supplied `alias` is present - `inst-alias-no-suffix-missing`
            1. [x] - `p1` - **RETURN** `400 ValidationError` — non-derivable, explicit alias required - `inst-alias-no-suffix-missing-return`
         2. [x] - `p1` - **ELSE** - `inst-alias-no-suffix-supplied`
            1. [x] - `p1` - Normalize the supplied alias per the pattern/lowercase/trailing-dot rules; **IF** invalid, **RETURN** `400 ValidationError` - `inst-alias-no-suffix-normalize`
      4. [x] - `p1` - **ELSE** - `inst-alias-suffix-ok`
         1. [x] - `p1` - Derive `alias` = the common suffix, lowercased; append `:port` only when `port` is non-standard for the pool's `scheme` - `inst-alias-suffix-derive`
4. [x] - `p1` - **IF** an alias was derived (single-hostname or multi-hostname common-suffix path) **AND** the client also supplied an explicit `alias` - `inst-alias-derived-vs-supplied`
   1. [x] - `p1` - **IF** the normalized supplied alias equals the derived alias - `inst-alias-match`
      1. [x] - `p1` - Accept as an idempotent no-op; use the derived value - `inst-alias-match-accept`
   2. [x] - `p1` - **ELSE** - `inst-alias-mismatch`
      1. [x] - `p1` - **RETURN** `400 ValidationError` — hostname-based endpoints always auto-derive; a differing user-supplied alias is rejected - `inst-alias-mismatch-return`
5. [x] - `p1` - **RETURN** the resolved, normalized alias string - `inst-alias-return`

### Enforce Alias Update Immutability

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-enforce-alias-update-immutability`

**Input**: Existing persisted Upstream's `alias` and endpoint-derivability class, proposed `server.endpoints[]` (and optional `alias`) from a `PUT` body

**Output**: The accepted (unchanged) alias to persist, or a `400 ValidationError` outcome

**Steps**:
1. [x] - `p1` - Classify the existing persisted endpoint set's derivability (`derivable` / `non-derivable`) using the classification rules of `cpt-cf-oagw-algo-derive-alias` - `inst-alias-update-classify-old`
2. [x] - `p1` - Classify the proposed endpoint set's derivability the same way - `inst-alias-update-classify-new`
3. [x] - `p1` - **IF** the proposed endpoint set is `derivable` - `inst-alias-update-new-derivable`
   1. [x] - `p1` - Compute `recomputed_alias` from the proposed endpoints per `cpt-cf-oagw-algo-derive-alias`'s derivation rules - `inst-alias-update-recompute`
   2. [x] - `p1` - **IF** the `PUT` body also supplies an explicit `alias` that (normalized) differs from `recomputed_alias` - `inst-alias-update-override-attempt`
      1. [x] - `p1` - **RETURN** `400 ValidationError` — alias override not allowed on update - `inst-alias-update-override-attempt-return`
   3. [x] - `p1` - **IF** `recomputed_alias` equals the existing persisted alias - `inst-alias-update-recompute-match`
      1. [x] - `p1` - Accept — alias unchanged - `inst-alias-update-recompute-accept`
   4. [x] - `p1` - **ELSE** - `inst-alias-update-recompute-mismatch`
      1. [x] - `p1` - **RETURN** `400 ValidationError` — an endpoint change that would alter the derived alias is rejected; the operator must delete and re-create the upstream - `inst-alias-update-recompute-return`
4. [x] - `p1` - **ELSE** (the proposed endpoint set is `non-derivable`) - `inst-alias-update-new-nonderivable`
   1. [x] - `p1` - **IF** the existing (old) endpoint set was `derivable` (a hostname → IP transition) - `inst-alias-update-hostname-to-ip`
      1. [x] - `p1` - **RETURN** `400 ValidationError` — rejected always, even when an explicit alias matching the prior value is supplied - `inst-alias-update-hostname-to-ip-return`
   2. [x] - `p1` - **ELSE** (old and new endpoint sets are both `non-derivable`, e.g. IP → IP) - `inst-alias-update-ip-to-ip`
      1. [x] - `p1` - **IF** the `PUT` body omits `alias`, or supplies an `alias` that (normalized) equals the existing persisted alias - `inst-alias-update-ip-retain`
         1. [x] - `p1` - Accept — existing alias retained - `inst-alias-update-ip-retain-accept`
      2. [x] - `p1` - **ELSE** - `inst-alias-update-ip-diff`
         1. [x] - `p1` - **RETURN** `400 ValidationError` — a differing user-supplied alias is not accepted on update - `inst-alias-update-ip-diff-return`
5. [x] - `p1` - **RETURN** the accepted alias value (always equal to the existing persisted alias in every non-rejected branch) for persistence - `inst-alias-update-return`

### Resolve Tenant Scope

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-resolve-tenant-scope`

**Input**: Operation type (`create`/`get`/`list`/`replace`/`delete`), calling `tenant_id`, resolved `alias` (for `create`), target resource `id` (for `get`/`replace`/`delete`) — accepted as either the bare UUID or the anonymous GTS form `gts.cf.core.oagw.upstream.v1~{uuid}` and normalized to the bare UUID before lookup — calling principal's permissions

**Output**: A scope-resolution outcome — `proceed`, `bind` (create only), `conflict` (create only), or `not_found` (get/replace/delete only)

**Steps**:
1. [x] - `p1` - **IF** operation is `create` - `inst-scope-create-branch`
   1. [x] - `p1` - `DB: SELECT oagw_upstream WHERE tenant_id = calling tenant AND alias = resolved alias` - `inst-scope-create-own-lookup`
   2. [x] - `p1` - **IF** a row exists for the calling tenant with this alias - `inst-scope-create-own-conflict`
      1. [x] - `p1` - **RETURN** `conflict` (`(tenant_id, alias)` uniqueness violated) - `inst-scope-create-own-conflict-return`
   3. [x] - `p1` - **ELSE** - `inst-scope-create-walk-branch`
      1. [x] - `p1` - Walk the tenant hierarchy from the calling tenant's immediate parent to the root, searching for an upstream with the same alias - `inst-scope-create-ancestor-walk`
   4. [x] - `p1` - **IF** no ancestor upstream shares this alias - `inst-scope-create-no-ancestor`
      1. [x] - `p1` - **RETURN** `proceed` (ordinary tenant-scoped create) - `inst-scope-create-no-ancestor-return`
   5. [x] - `p1` - **ELSE** (an ancestor upstream shares this alias) - `inst-scope-create-ancestor-found`
      1. [x] - `p1` - **IF** the ancestor's sharing mode is `private`, or the calling principal lacks `oagw:upstream:bind` - `inst-scope-create-bind-blocked`
         1. [x] - `p1` - **RETURN** `conflict` (ancestor upstream is not bindable from the calling tenant) - `inst-scope-create-bind-blocked-return`
      2. [x] - `p1` - **ELSE** - `inst-scope-create-bind-allowed`
         1. [x] - `p1` - **RETURN** `bind` (create proceeds as a tenant-local bind record referencing the shared alias) - `inst-scope-create-bind-allowed-return`
2. [x] - `p1` - **ELSE** (operation is `get`, `list`, `replace`, or `delete`) - `inst-scope-visibility-branch`
   1. [x] - `p1` - Restrict every query predicate to `tenant_id = calling tenant` only — never traverse to ancestor tenants - `inst-scope-visibility-restrict`
   2. [x] - `p1` - **IF** the operation targets a specific `id` and that id exists but belongs to a different (including ancestor) tenant, or does not exist at all - `inst-scope-visibility-foreign`
      1. [x] - `p1` - **RETURN** `not_found` — ancestor/foreign resources are invisible via the management API, identically to a non-existent id - `inst-scope-visibility-foreign-return`
   3. [x] - `p1` - **ELSE** - `inst-scope-visibility-own`
      1. [x] - `p1` - **RETURN** `proceed` against the tenant-owned row(s) - `inst-scope-visibility-own-return`

## 4. States (CDSL)

### Upstream Enabled Lifecycle

- [x] `p2` - **ID**: `cpt-cf-oagw-state-upstream-enabled-lifecycle`

**States**: Enabled, Disabled

**Initial State**: Enabled (schema default `enabled: true`)

**Transitions**:
1. [x] - `p1` - **FROM** Enabled **TO** Disabled **WHEN** the owning tenant submits `PUT` with `enabled: false` on a tenant-owned upstream - `inst-state-enabled-to-disabled`
2. [x] - `p1` - **FROM** Disabled **TO** Enabled **WHEN** the owning tenant submits `PUT` with `enabled: true` **AND** no ancestor upstream sharing the same alias currently has `enabled: false` - `inst-state-disabled-to-enabled`
3. [x] - `p1` - **FROM** Disabled **TO** Disabled **WHEN** the owning tenant submits `PUT` with `enabled: true` **WHILE** an ancestor upstream sharing the same alias currently has `enabled: false` — the write is rejected with `400 ValidationError`; a descendant cannot re-enable an ancestor-disabled upstream - `inst-state-disabled-reenable-blocked`

Proxy-time enforcement of the Disabled state against live requests (returning `503` to callers) is out of scope for this feature — it belongs to `cpt-cf-oagw-feature-proxy-core` (2.5), which reads the `enabled` value this feature persists.

## 5. Definitions of Done

**Note on identifier-slug naming**: unlike the other eight feature files in this decomposition, the `cpt-*` slugs in this file's Definitions of Done (`cpt-cf-oagw-dod-crud-endpoints`, `-dod-schema-validation`, `-dod-tenant-scoping`, `-dod-enable-disable`, `-dod-error-mapping`) and Processes section (`cpt-cf-oagw-algo-validate-upstream-schema`) omit the `upstream-` feature-name segment other files include in their equivalents. This is a recorded, accepted naming asymmetry — these slugs are stable and verified collision-free across all nine feature files, and implementation source markers (`@cpt-begin`/`@cpt-end`) will reference them, so they are intentionally left unrenamed rather than churned for a cosmetic fix.

### Cross-Cutting Dispositions

The following review domains are explicitly dispositioned for this feature, rather than left silent:

- **Performance**: Not applicable as a dedicated budget — this feature's five operations are low-rate, latency-insensitive control-plane CRUD against the config-store, not the request-forwarding hot path; per-request latency budgets are owned by `cpt-cf-oagw-feature-proxy-core` (2.5).
- **UX/Accessibility**: Not applicable — this is a machine-to-machine REST management API with no user interface; there is no visual or interactive surface for accessibility criteria to apply to.
- **Compliance/Privacy**: Not applicable because no personal or regulated data is stored — an Upstream record holds infrastructure configuration (endpoints, protocol, headers, rate-limit/CORS settings) and, at most, a `secret_ref` pointer to credential material held and governed by `cred_store`, never the credential itself.
- **Resilience/Recovery**: Not applicable beyond the transactional-atomicity guarantee already stated under Reliability/Rollback above — this feature performs no outbound network calls and owns no retry, failover, or circuit-breaker behavior of its own; those concerns belong to the data-plane features that consume the records this feature persists.
- **Data Privacy**: Not applicable for the same reason as Compliance/Privacy above — no end-user personal data flows through or is retained by this feature's CRUD surface.
- **External Integrations**: Not applicable as a live-integration concern — this feature only validates and persists identifiers/configuration that *reference* other systems (credential store via `secret_ref`, plugins via `plugins.items[]`); it never itself calls out to an upstream, `cred_store`, or a plugin at write time (see `cpt-cf-oagw-dod-http-scheme-acceptance`'s no-live-connection guarantee).
- **Config/Health**: Not applicable — this feature introduces no gear-configuration flags or health-check endpoints of its own; it only reads the `allow_http_upstream` flag owned by `cpt-cf-oagw-feature-gear-foundation` (2.1) by reference, and enforces none of it itself.

### CRUD Endpoints at Gear-Relative Paths

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-crud-endpoints`

The system **MUST** expose all five operations at the gear-relative paths `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}` (no `/api` prefix), returning `201`/`200`/`200`/`200`/`204` on success respectively.

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-list-upstreams`
- `cpt-cf-oagw-flow-get-upstream`
- `cpt-cf-oagw-flow-replace-upstream`
- `cpt-cf-oagw-flow-delete-upstream`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams/{id}`
- API: `PUT /oagw/v1/upstreams/{id}`
- API: `DELETE /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`

### Schema Validation and Strict Additional-Properties Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-schema-validation`

The system **MUST** validate every create/replace request body against `upstream.v1.schema.json` in full: `additionalProperties: false` at the top level and in every nested sub-object (`server`, each endpoint, `headers.request`, `headers.response`, `rate_limit`, `cors`), all required fields, all enums, and all defaults (`enabled: true`, endpoint `scheme: https`/`port: 443`, `auth.sharing: private`, `headers.request.passthrough: none`, `plugins.sharing: private`, `rate_limit.sharing: private`/`algorithm: token_bucket`/`sustained.window: second`/`scope: tenant`/`strategy: reject`/`cost: 1`, `cors.sharing: private`/`enabled: false`/`allowed_methods: [GET, POST]`/`expose_headers: []`/`allow_credentials: false`). The system **MUST** reject a `cors` configuration combining `allow_credentials: true` with a wildcard (`"*"`) origin.

**Implements**:
- `cpt-cf-oagw-algo-validate-upstream-schema`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`

### `http`/`ws` Scheme Acceptance Independent of Live-Connection Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-http-scheme-acceptance`

The system **MUST** accept `scheme: http` and `scheme: ws` as syntactically legal values on every endpoint at create/replace time, unconditionally — this acceptance does **NOT** depend on the `allow_http_upstream` gear-config flag. The system **MUST NOT** open any live network connection as part of validating or persisting an Upstream record; live-connection gating by `allow_http_upstream` is exclusively `cpt-cf-oagw-feature-proxy-core`'s (2.5) responsibility at request-forwarding time.

**Implements**:
- `cpt-cf-oagw-algo-validate-upstream-schema`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Endpoint`

### Alias Derivation and Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-derivation`

The system **MUST** auto-derive `alias` for hostname-based endpoints (single hostname, or multiple hostnames sharing a PSL-validated registrable common suffix of ≥2 labels), **MUST** require and validate an explicit `alias` for IP-based or non-derivable endpoints, **MUST** reject a user-supplied alias that differs from the auto-derived value for hostname-based endpoints, and **MUST** normalize every alias to ASCII lowercase with a trailing dot stripped.

**Implements**:
- `cpt-cf-oagw-algo-derive-alias`
- `cpt-cf-oagw-flow-create-upstream`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- Entities: `Upstream`

### Alias Immutability on Update

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-immutability`

The system **MUST** treat `alias` as immutable once set: an endpoint change on `PUT` that would alter the derived alias is rejected; a derivable ↔ non-derivable class transition (hostname → IP or IP → hostname with a differing/no matching value) is rejected; a differing user-supplied alias with unchanged endpoints is rejected; only the case where the recomputed/retained alias exactly equals the existing persisted alias succeeds.

**Implements**:
- `cpt-cf-oagw-algo-enforce-alias-update-immutability`
- `cpt-cf-oagw-flow-replace-upstream`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### Tenant Scoping, Uniqueness, and Ancestor Bind

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tenant-scoping`

The system **MUST** enforce `(tenant_id, alias)` uniqueness with `409 Conflict` on collision, **MUST** treat a create whose alias matches an ancestor tenant's upstream alias as a bind request gated by the `oagw:upstream:bind` permission and the ancestor's sharing mode (`private` blocks the bind), and **MUST** make ancestor (or otherwise foreign-tenant) upstreams invisible (`404`) through `GET`/`PUT`/`DELETE`/list on the management API.

**Implements**:
- `cpt-cf-oagw-algo-resolve-tenant-scope`
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-list-upstreams`
- `cpt-cf-oagw-flow-get-upstream`
- `cpt-cf-oagw-flow-replace-upstream`
- `cpt-cf-oagw-flow-delete-upstream`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams/{id}`
- API: `PUT /oagw/v1/upstreams/{id}`
- API: `DELETE /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`

### Enable/Disable Field Ownership and Ancestor-Cascade Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-enable-disable`

The system **MUST** persist the `enabled` boolean (default `true`) on every Upstream, and **MUST** reject (at write time, `400 ValidationError`) a `PUT` that sets `enabled: true` while an ancestor upstream sharing the same alias has `enabled: false`. Enforcement of `enabled: false` against live proxy traffic is explicitly out of scope for this feature.

**Implements**:
- `cpt-cf-oagw-state-upstream-enabled-lifecycle`
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-replace-upstream`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### List Query Parameters

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-list-query-params`

The system **MUST** support the OData query parameters `$filter`, `$select`, `$orderby`, `$top` (default 50, max 100), and `$skip` on `GET /oagw/v1/upstreams`.

**Implements**:
- `cpt-cf-oagw-flow-list-upstreams`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `GET /oagw/v1/upstreams`
- Entities: `Upstream`

### Documented Status/Error Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-mapping`

The system **MUST** return, for every response this feature produces: `201 Created` (successful `POST`), `200 OK` (successful `GET`/`PUT`), `204 No Content` (successful `DELETE`), `400 ValidationError` (`gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`) for every schema/alias/pool/cross-field validation failure, `404` for a missing or ancestor-owned resource on `GET`/`PUT`/`DELETE`, and `409 Conflict` for an `(tenant_id, alias)` collision or a blocked ancestor bind. All error bodies **MUST** be rendered as `application/problem+json` via the RFC 9457 envelope owned by `cpt-cf-oagw-feature-gear-foundation`, with `X-OAGW-Error-Source: gateway`. `RouteNotFound` (`gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`) is the only `404`-class GTS identifier documented in `DESIGN.md`'s Error Response Format table; since PRD/DESIGN define no dedicated `404` identifier for a missing/ancestor-invisible Upstream, and this feature introduces no new GTS identifiers, the `404` responses in this feature carry that same envelope and status code without asserting a more specific `type`. Likewise, PRD's `cpt-cf-oagw-usecase-configure-upstream` documents the alias-conflict alternative flow only as "Return 409 Conflict" with no dedicated GTS `type`; the sole `409` entry in the Error Response Format table, `PluginInUse`, is scoped to plugin-management (2.4) and **MUST NOT** be reused for alias conflicts. Acceptance verification for `404` and `409` in this feature is therefore on HTTP status code and RFC 9457 envelope shape, consistent with what PRD/DESIGN actually document.

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-list-upstreams`
- `cpt-cf-oagw-flow-get-upstream`
- `cpt-cf-oagw-flow-replace-upstream`
- `cpt-cf-oagw-flow-delete-upstream`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams/{id}`
- API: `PUT /oagw/v1/upstreams/{id}`
- API: `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

## 6. Acceptance Criteria

- [x] `POST /oagw/v1/upstreams` with a single HTTPS hostname endpoint (standard port 443) and no explicit `alias` returns `201 Created` with a server-generated `id` and `alias` auto-derived to the hostname.
- [x] The same request repeated with an explicit `alias` equal to the correctly-derived value succeeds (`201 Created`, idempotent no-op on the derivation).
- [x] `POST` with an explicit `alias` that differs from the auto-derivable value for a hostname-based endpoint returns `400 ValidationError`.
- [x] `POST` with two IP-based endpoints and no `alias` field returns `400 ValidationError`; the identical request with an explicit `alias` returns `201 Created`.
- [x] `POST` with two hostname endpoints sharing a registrable common suffix (e.g., `us.vendor.com`, `eu.vendor.com`) and no explicit alias returns `201 Created` with `alias` derived as the common suffix (`vendor.com`).
- [x] `POST` with two hostname endpoints whose only common suffix is a bare public suffix (e.g., `foo.co.uk`, `bar.co.uk`) and no explicit alias returns `400 ValidationError`.
- [x] **`POST /oagw/v1/upstreams` with `{"server": {"endpoints": [{"scheme": "http", "host": "example-plaintext.internal", "port": 80}]}, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "alias": "example-plaintext-svc"}` returns `201 Created` under the graded configuration (`oagw.config.allow_http_upstream: true`), confirming the management-layer scheme allowlist accepts `http` independent of live-connection enforcement.**
- [x] `POST` with an endpoint pool containing endpoints of differing `scheme` (e.g., one `https`, one `wss`) returns `400 ValidationError`.
- [x] `POST` with an endpoint pool containing endpoints of differing `port` returns `400 ValidationError`.
- [x] `POST` omitting the required `protocol` field returns `400 ValidationError`.
- [x] `POST` with `cors.enabled: true`, `cors.allow_credentials: true`, and `cors.allowed_origins: ["*"]` returns `400 ValidationError`.
- [x] `POST` with an unrecognized top-level property (e.g., `"foo": "bar"`) returns `400 ValidationError` (`additionalProperties: false`).
- [x] `POST` with a client-supplied `id` field in the request body returns `400 ValidationError`.
- [x] `POST` with `plugins.items` containing a value that is neither a well-formed GTS identifier nor a well-formed UUID returns `400 ValidationError`.
- [x] `POST` with `rate_limit.sustained.rate` present and `rate_limit.sustained.window` omitted persists/returns `sustained.window: "second"` (schema default applied).
- [x] Two `POST`s from the same tenant that resolve to the same `(tenant_id, alias)` return `201 Created` then `409 Conflict` on the second.
- [x] `GET /oagw/v1/upstreams/{id}` for an upstream owned by an ancestor tenant returns `404`.
- [x] `GET /oagw/v1/upstreams/{id}` for a non-existent id returns `404` with the same response shape as the ancestor-owned case.
- [x] `GET /oagw/v1/upstreams` never returns rows belonging to an ancestor tenant, defaults `$top` to 50, and caps it at 100.
- [x] `PUT /oagw/v1/upstreams/{id}` that changes a hostname-based upstream's endpoint host (causing the recomputed alias to differ) returns `400 ValidationError`, and the persisted alias remains unchanged.
- [x] `PUT` that transitions an upstream's endpoints from hostname-based to IP-based is rejected with `400 ValidationError`, even when an explicit `alias` matching the prior derived value is supplied.
- [x] `PUT` that changes IP-based endpoints while supplying an `alias` different from the currently persisted one returns `400 ValidationError`.
- [x] `PUT` that resubmits the same endpoints unchanged succeeds (`200 OK`) and preserves the existing alias.
- [x] `PUT` setting `enabled: false` on a tenant-owned upstream succeeds (`200 OK`); a descendant-tenant upstream sharing the same alias then cannot be set `enabled: true` while the ancestor copy remains disabled (`400 ValidationError`).
- [x] `DELETE /oagw/v1/upstreams/{id}` for a tenant-owned upstream returns `204 No Content`, and a subsequent `GET` for that id returns `404`.
- [x] `DELETE` for an ancestor-owned or non-existent id returns `404`.
- [x] `GET /oagw/v1/upstreams/{id}` succeeds identically (`200 OK`, identical body) whether `{id}` is supplied as the bare UUID `id` value or as the anonymous GTS form `gts.cf.core.oagw.upstream.v1~{uuid}` for the same upstream.
