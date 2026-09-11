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
  - [Schema Validation](#schema-validation)
  - [Alias Derivation](#alias-derivation)
  - [Alias Normalization](#alias-normalization)
  - [Alias Uniqueness Check](#alias-uniqueness-check)
  - [Alias Immutability Check](#alias-immutability-check)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream State Machine](#upstream-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Create Upstream Endpoint](#create-upstream-endpoint)
  - [List Upstreams Endpoint](#list-upstreams-endpoint)
  - [Get Upstream Endpoint](#get-upstream-endpoint)
  - [Replace Upstream Endpoint](#replace-upstream-endpoint)
  - [Delete Upstream Endpoint](#delete-upstream-endpoint)
  - [Field-Level Schema Validation](#field-level-schema-validation)
  - [Alias Derivation and Normalization](#alias-derivation-and-normalization)
  - [Per-Tenant Alias Uniqueness](#per-tenant-alias-uniqueness)
  - [Alias Immutability](#alias-immutability)
  - [Enable/Disable and Ancestor-Disable Propagation](#enabledisable-and-ancestor-disable-propagation)
  - [Problem-Document Error Responses](#problem-document-error-responses)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-upstream-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-upstream-management`
## 1. Feature Context

### 1.1 Overview

This feature gives operators and tenant administrators CRUD control over upstream definitions at `/oagw/v1/upstreams`, the root configuration object every proxy request resolves to. It fixes alias derivation, normalization, per-tenant uniqueness, and immutability so aliases are a stable routing key.

### 1.2 Purpose

Upstreams are the fundamental configuration unit for outbound proxying: every proxy request ultimately targets one. This feature builds the tenant-scoped surface that creates, lists, retrieves, replaces, and deletes upstream definitions, validating every field against `upstream.v1.schema.json` while accepting the documented `http`/`ws` scheme extension. It also fixes alias derivation and enforces the `enabled` flag together with ancestor-disable propagation so later features can trust the alias as a stable routing key.

**Requirements**: `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-enable-disable`, `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-nfr-multi-tenancy`, `cpt-cf-oagw-nfr-input-validation`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

Authentication and authorization for every endpoint in this feature are enforced by the platform's API
gateway ahead of this gear, per the boundary owned by `cpt-cf-oagw-feature-gear-foundation`; this
feature specifies no permission check of its own.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, lists, replaces, and deletes upstream definitions that apply globally or to a tenant subtree. |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, lists, replaces, and deletes upstream definitions scoped to their own tenant. |

Both actors follow identical steps for every flow in this feature; only
`cpt-cf-oagw-actor-platform-operator` is named in the **Actor** field below to avoid repeating
identical flows twice.

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` — this feature requires the mounted gear router, the RFC 9457 error model, and the resolved gear configuration section established there before it can expose any upstream CRUD endpoint.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor and describe the end-to-end flow of managing an upstream definition through the REST API.

**Use cases**: `cpt-cf-oagw-usecase-configure-upstream`

### Create Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-create-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Request validates, alias derives or validates cleanly, and the upstream is persisted with a generated identifier.

**Error Scenarios**:
- Field-level schema validation fails and no record is persisted.
- The derived or supplied alias already exists for the calling tenant.
- The derived or supplied alias matches an ancestor tenant's disabled upstream while the new record
  would be enabled.

**Steps**:
1. [ ] - `p1` - Operator sends `POST /oagw/v1/upstreams` with server endpoints, protocol, and optional auth/headers/plugins/rate-limit/CORS fields - `inst-create-upstream-01`
2. [ ] - `p1` - System validates the request body field by field against `upstream.v1.schema.json`, including the `http`/`ws` scheme extension - `inst-create-upstream-02`
3. [ ] - `p1` - **IF** validation fails - `inst-create-upstream-03`
   1. [ ] - `p1` - **RETURN** `400 ValidationError` problem document listing every failing field - `inst-create-upstream-04`
4. [ ] - `p1` - **ELSE** - `inst-create-upstream-05`
   1. [ ] - `p1` - System derives the alias from hostname endpoints, or accepts the explicit alias supplied for non-derivable endpoints - `inst-create-upstream-06`
5. [ ] - `p1` - System normalizes the derived or supplied alias to ASCII lowercase with a trailing dot stripped - `inst-create-upstream-07`
6. [ ] - `p1` - System checks the normalized alias for uniqueness within the calling tenant - `inst-create-upstream-08`
7. [ ] - `p1` - **IF** the alias already exists for this tenant - `inst-create-upstream-09`
   1. [ ] - `p1` - **RETURN** `409` conflict problem document naming the alias field - `inst-create-upstream-10`
8. [ ] - `p1` - **ELSE** - `inst-create-upstream-11`
   1. [ ] - `p1` - **IF** the derived or supplied alias matches an ancestor tenant's disabled upstream and the new record would be enabled - `inst-create-upstream-14`
      1. [ ] - `p1` - **RETURN** `400 ValidationError` problem document rejecting the create under a disabled ancestor alias - `inst-create-upstream-15`
   2. [ ] - `p1` - **ELSE** - `inst-create-upstream-16`
      1. [ ] - `p1` - System persists a new upstream record, defaulting `enabled` to `true` - `inst-create-upstream-12`
9. [ ] - `p1` - **RETURN** `201 Created` with the persisted upstream representation, including its generated identifier and resolved alias - `inst-create-upstream-13`

### List Upstreams

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-list-upstreams`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- Query parameters validate and a filtered, paginated page of the tenant's own upstreams is returned.

**Error Scenarios**:
- An unsupported field is referenced in `$filter`/`$select`, or `$top`/`$skip` are out of the allowed range.

**Steps**:
1. [ ] - `p1` - Operator sends `GET /oagw/v1/upstreams` with optional `$filter`, `$select`, `$orderby`, `$top`, `$skip` query parameters - `inst-list-upstreams-01`
2. [ ] - `p1` - **IF** any query parameter is malformed, references an undeclared field, or `$top` exceeds 100 - `inst-list-upstreams-02`
   1. [ ] - `p1` - **RETURN** `400 ValidationError` problem document naming the offending parameter - `inst-list-upstreams-03`
3. [ ] - `p1` - **ELSE** - `inst-list-upstreams-04`
   1. [ ] - `p1` - System retrieves only the calling tenant's own upstream records, applying `$filter`, `$select`, `$orderby`, `$skip`, and `$top` (default 50) - `inst-list-upstreams-05`
4. [ ] - `p1` - **RETURN** `200` with the filtered, paginated list of upstream representations - `inst-list-upstreams-06`

### Get Upstream by ID

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-get-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The requested identifier belongs to the calling tenant and its representation is returned.

**Error Scenarios**:
- The identifier does not exist, or belongs to another tenant.

**Steps**:
1. [ ] - `p1` - Operator sends `GET /oagw/v1/upstreams/{id}` - `inst-get-upstream-01`
2. [ ] - `p1` - **IF** no upstream with that identifier exists for the calling tenant - `inst-get-upstream-02`
   1. [ ] - `p1` - **RETURN** `404` problem document - `inst-get-upstream-03`
3. [ ] - `p1` - **ELSE** - `inst-get-upstream-04`
   1. [ ] - `p1` - **RETURN** `200` with the upstream representation - `inst-get-upstream-05`

### Replace Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-replace-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The full replacement body validates, the alias is unchanged, and every field of the stored record is overwritten.

**Error Scenarios**:
- The identifier does not exist for the calling tenant.
- Schema validation fails.
- The endpoint change would alter the derived alias.
- The request attempts to re-enable an ancestor-disabled upstream.

**Steps**:
1. [ ] - `p1` - Operator sends `PUT /oagw/v1/upstreams/{id}` with a complete upstream representation - `inst-replace-upstream-01`
2. [ ] - `p1` - **IF** no upstream with that identifier exists for the calling tenant - `inst-replace-upstream-02`
   1. [ ] - `p1` - **RETURN** `404` problem document - `inst-replace-upstream-03`
3. [ ] - `p1` - **ELSE** - `inst-replace-upstream-04`
   1. [ ] - `p1` - System validates the request body field by field against `upstream.v1.schema.json` - `inst-replace-upstream-05`
4. [ ] - `p1` - **IF** validation fails - `inst-replace-upstream-06`
   1. [ ] - `p1` - **RETURN** `400 ValidationError` problem document listing every failing field - `inst-replace-upstream-07`
5. [ ] - `p1` - **ELSE** - `inst-replace-upstream-08`
   1. [ ] - `p1` - System checks alias immutability against the stored upstream's endpoints and alias - `inst-replace-upstream-09`
6. [ ] - `p1` - **IF** the immutability check requires delete-and-recreate - `inst-replace-upstream-10`
   1. [ ] - `p1` - **RETURN** `400 ValidationError` problem document rejecting the alias-changing update - `inst-replace-upstream-11`
7. [ ] - `p1` - **ELSE** - `inst-replace-upstream-12`
   1. [ ] - `p1` - **IF** the request sets `enabled: true` while an ancestor tenant's upstream sharing the same alias remains disabled - `inst-replace-upstream-13`
      1. [ ] - `p1` - **RETURN** `400 ValidationError` problem document rejecting the re-enable attempt - `inst-replace-upstream-14`
   2. [ ] - `p1` - **ELSE** - `inst-replace-upstream-15`
      1. [ ] - `p1` - System overwrites every field of the stored upstream with the request body, clearing any omitted optional field - `inst-replace-upstream-16`
8. [ ] - `p1` - **RETURN** `200` with the replaced upstream representation - `inst-replace-upstream-17`

### Delete Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-delete-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The identifier belongs to the calling tenant and its record is removed.

**Error Scenarios**:
- The identifier does not exist for the calling tenant.

**Steps**:
1. [ ] - `p1` - Operator sends `DELETE /oagw/v1/upstreams/{id}` - `inst-delete-upstream-01`
2. [ ] - `p1` - **IF** no upstream with that identifier exists for the calling tenant - `inst-delete-upstream-02`
   1. [ ] - `p1` - **RETURN** `404` problem document - `inst-delete-upstream-03`
3. [ ] - `p1` - **ELSE** - `inst-delete-upstream-04`
   1. [ ] - `p1` - System removes the upstream record and its own plugin bindings from the in-process control-plane store - `inst-delete-upstream-05`
4. [ ] - `p1` - **RETURN** `204 No Content` - `inst-delete-upstream-06`

## 3. Processes / Business Logic (CDSL)

Internal validation and derivation routines invoked by the create and replace flows above; they do not interact with actors directly.

### Schema Validation

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-schema-validation`

**Input**: candidate upstream request body submitted on create or replace.

**Output**: a validated, defaulted upstream object, or a list of field-level validation errors.

**Steps**:
1. [ ] - `p1` - Parse the request body as JSON and reject a non-object payload - `inst-schema-validate-01`
2. [ ] - `p1` - Validate that the required top-level fields `server` and `protocol` are present - `inst-schema-validate-02`
3. [ ] - `p1` - Validate `server.endpoints` is a non-empty array whose entries each declare `scheme` and `host` - `inst-schema-validate-03`
4. [ ] - `p1` - **FOR EACH** endpoint in `server.endpoints` - `inst-schema-validate-04`
   1. [ ] - `p1` - Validate `scheme` is one of `https`, `wss`, `wt`, `grpc`, `http`, `ws`, accepting `http`/`ws` as the documented extension beyond the checked-in enum - `inst-schema-validate-05`
   2. [ ] - `p1` - Validate `host` matches the hostname, IPv4, or IPv4 address format - `inst-schema-validate-06`
   3. [ ] - `p1` - **IF** `port` is omitted - `inst-schema-validate-07`
      1. [ ] - `p1` - Default `port` to 80 for `http`/`ws`, or 443 for `https`/`wss`/`wt`/`grpc` - `inst-schema-validate-08`
5. [ ] - `p1` - Validate `auth`, `headers`, `plugins`, `rate_limit`, and `cors` against their declared shapes, enums, and defaults - `inst-schema-validate-09`
6. [ ] - `p1` - Reject the request when any object in the payload carries a property outside its schema's declared set (`additionalProperties: false`) - `inst-schema-validate-10`
7. [ ] - `p1` - **TRY** - `inst-schema-validate-11`
   1. [ ] - `p1` - Assemble the validated, defaulted upstream object from the checked fields - `inst-schema-validate-12`
8. [ ] - `p1` - **CATCH** a field validation failure - `inst-schema-validate-13`
   1. [ ] - `p1` - Collect every field-level error into a single problem document - `inst-schema-validate-14`
9. [ ] - `p1` - **RETURN** the validated upstream object, or the collected field errors - `inst-schema-validate-15`

### Alias Derivation

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-alias-derivation`

**Input**: the validated `server.endpoints` list from a create or replace request, and any user-supplied `alias`.

**Output**: a derived alias string, an accepted explicit alias, or a validation rejection.

**Steps**:
1. [ ] - `p1` - **IF** every endpoint host in the request is a hostname rather than an IP address - `inst-alias-derive-01`
   1. [ ] - `p1` - **IF** exactly one distinct hostname is present across all endpoints - `inst-alias-derive-02`
      1. [ ] - `p1` - Derive the alias as that single hostname - `inst-alias-derive-03`
   2. [ ] - `p1` - **ELSE** - `inst-alias-derive-04`
      1. [ ] - `p1` - Compute the longest common hostname suffix shared across all endpoint hosts - `inst-alias-derive-05`
      2. [ ] - `p1` - **IF** the common suffix has at least two labels - `inst-alias-derive-06`
         1. [ ] - `p1` - Derive the alias as that common suffix - `inst-alias-derive-07`
      3. [ ] - `p1` - **ELSE** - `inst-alias-derive-08`
         1. [ ] - `p1` - Mark derivation as not possible - `inst-alias-derive-09`
2. [ ] - `p1` - **ELSE** - `inst-alias-derive-10`
   1. [ ] - `p1` - Mark derivation as not possible, since IP-based or mixed endpoints require an explicit alias - `inst-alias-derive-11`
3. [ ] - `p1` - **IF** derivation succeeded and the shared endpoint port is non-standard for its scheme - `inst-alias-derive-12`
   1. [ ] - `p1` - Append `:{port}` to the derived alias - `inst-alias-derive-13`
4. [ ] - `p1` - **IF** derivation succeeded and a user-supplied `alias` is present and differs from the derived value - `inst-alias-derive-14`
   1. [ ] - `p1` - **RETURN** a validation rejection for the diverging alias - `inst-alias-derive-15`
5. [ ] - `p1` - **IF** derivation is not possible and no user-supplied `alias` is present - `inst-alias-derive-16`
   1. [ ] - `p1` - **RETURN** a validation rejection requiring an explicit alias - `inst-alias-derive-17`
6. [ ] - `p1` - **RETURN** the derived alias, or the accepted user-supplied alias for non-derivable endpoints - `inst-alias-derive-18`

### Alias Normalization

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-alias-normalization`

**Input**: a derived or user-supplied alias string.

**Output**: the normalized alias string used for storage and uniqueness checking.

**Steps**:
1. [ ] - `p1` - Strip a single trailing dot from the alias when present - `inst-alias-normalize-01`
2. [ ] - `p1` - Convert every ASCII character in the alias to lowercase - `inst-alias-normalize-02`
3. [ ] - `p1` - **RETURN** the normalized alias - `inst-alias-normalize-03`

### Alias Uniqueness Check

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-alias-uniqueness-check`

**Input**: the normalized alias and the calling `tenant_id`.

**Output**: a uniqueness confirmation, or a conflict rejection.

**Steps**:
1. [ ] - `p1` - Search the in-process control-plane store for an existing upstream sharing this `tenant_id` and normalized alias - `inst-alias-unique-01`
2. [ ] - `p1` - **IF** a match exists and its identifier differs from the upstream currently being created or replaced - `inst-alias-unique-02`
   1. [ ] - `p1` - **RETURN** a `409` conflict rejection naming the alias field - `inst-alias-unique-03`
3. [ ] - `p1` - **ELSE** - `inst-alias-unique-04`
   1. [ ] - `p1` - **RETURN** a uniqueness confirmation - `inst-alias-unique-05`

### Alias Immutability Check

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-alias-immutability-check`

**Input**: the stored upstream record and its incoming full-replace request.

**Output**: an immutability confirmation, or a rejection requiring delete-and-recreate.

**Steps**:
1. [ ] - `p1` - Recompute the derived alias for the incoming request's endpoints using alias derivation - `inst-alias-immutable-01`
2. [ ] - `p1` - **IF** the incoming request supplies an explicit `alias` value - `inst-alias-immutable-02`
   1. [ ] - `p1` - **IF** the supplied alias equals the recomputed derived alias, or the stored upstream's endpoints are non-derivable and the supplied alias equals the stored alias - `inst-alias-immutable-03`
      1. [ ] - `p1` - **RETURN** an immutability confirmation as an idempotent no-op - `inst-alias-immutable-04`
   2. [ ] - `p1` - **ELSE** - `inst-alias-immutable-05`
      1. [ ] - `p1` - **RETURN** a rejection requiring delete-and-recreate - `inst-alias-immutable-06`
3. [ ] - `p1` - **ELSE** - `inst-alias-immutable-07`
   1. [ ] - `p1` - **IF** the recomputed derived alias equals the stored alias - `inst-alias-immutable-08`
      1. [ ] - `p1` - **RETURN** an immutability confirmation - `inst-alias-immutable-09`
   2. [ ] - `p1` - **ELSE** - `inst-alias-immutable-10`
      1. [ ] - `p1` - **RETURN** a rejection requiring delete-and-recreate - `inst-alias-immutable-11`

## 4. States (CDSL)

### Upstream State Machine

- [ ] `p1` - **ID**: `cpt-cf-oagw-state-upstream-lifecycle`

**States**: Enabled, Disabled

**Initial State**: Enabled

**Transitions**:
1. [ ] - `p1` - **FROM** Enabled **TO** Disabled **WHEN** an operator submits a full replace with `enabled: false` - `inst-state-upstream-01`
2. [ ] - `p1` - **FROM** Disabled **TO** Enabled **WHEN** an operator submits a full replace with `enabled: true` and no ancestor tenant's upstream sharing the same alias is disabled - `inst-state-upstream-02`
3. [ ] - `p1` - **FROM** Disabled **TO** Disabled **WHEN** an operator submits `enabled: true` while an ancestor tenant's upstream sharing the same alias remains disabled, so the re-enable request is rejected - `inst-state-upstream-03`

## 5. Definitions of Done

**Constraints**: `cpt-cf-oagw-constraint-https-only`

### Create Upstream Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-create-upstream-endpoint`

The system **MUST** expose `POST /oagw/v1/upstreams` that validates the request, derives or validates the alias, checks uniqueness, and persists the record.

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`

### List Upstreams Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-list-upstreams-endpoint`

The system **MUST** expose `GET /oagw/v1/upstreams` supporting `$filter`, `$select`, `$orderby`, `$top`, and `$skip`, returning only the calling tenant's own records.

**Implements**:
- `cpt-cf-oagw-flow-list-upstreams`

**Touches**:
- API: `GET /oagw/v1/upstreams`
- Entities: `Upstream`

### Get Upstream Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-get-upstream-endpoint`

The system **MUST** expose `GET /oagw/v1/upstreams/{id}` returning `404` when the identifier is absent or owned by another tenant.

**Implements**:
- `cpt-cf-oagw-flow-get-upstream`

**Touches**:
- API: `GET /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### Replace Upstream Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-replace-upstream-endpoint`

The system **MUST** expose `PUT /oagw/v1/upstreams/{id}` performing a full-field replacement that clears any omitted optional field.

**Implements**:
- `cpt-cf-oagw-flow-replace-upstream`

**Touches**:
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`

### Delete Upstream Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-delete-upstream-endpoint`

The system **MUST** expose `DELETE /oagw/v1/upstreams/{id}` returning `204` on success and `404` when the identifier is not owned by the calling tenant.

**Implements**:
- `cpt-cf-oagw-flow-delete-upstream`

**Touches**:
- API: `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### Field-Level Schema Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-schema-validation`

The system **MUST** validate every create and replace request field by field against `upstream.v1.schema.json`, enforcing required fields, enums, formats, defaults, and `additionalProperties: false`, while accepting `http` and `ws` as an extension beyond the checked-in `scheme` enum, with default port 80 for `http` and `ws`, and 443 for the TLS family (`https`, `wss`, `wt`, `grpc`).

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-replace-upstream`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `ServerConfig`, `Endpoint`, `AuthConfig`, `HeadersConfig`, `PluginsConfig`, `RateLimitConfig`, `CorsConfig`

### Alias Derivation and Normalization

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-derivation-normalization`

The system **MUST** derive the alias from hostname endpoints, normalize it to ASCII lowercase with trailing dots stripped, and reject a user-supplied alias diverging from the derived value.

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- Entities: `Upstream`, `Endpoint`

### Per-Tenant Alias Uniqueness

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-uniqueness`

The system **MUST** enforce alias uniqueness scoped to `(tenant_id, alias)`, rejecting a create or replace whose alias collides with a different upstream owned by the same tenant.

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-replace-upstream`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### Alias Immutability

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-immutability`

The system **MUST** reject any replace request whose endpoint change would alter the derived alias, requiring the operator to delete and re-create the upstream instead.

**Implements**:
- `cpt-cf-oagw-flow-replace-upstream`

**Touches**:
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`, `Endpoint`

### Enable/Disable and Ancestor-Disable Propagation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-enable-disable`

The system **MUST** support the `enabled` flag defaulting to `true`, and reject a create or replace request that would enable an upstream while an ancestor tenant's same-alias upstream remains disabled.

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-replace-upstream`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### Problem-Document Error Responses

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-responses`

The system **MUST** return the correct status code for every success and failure path — `200`, `201`, `204`, `400`, `404`, `409` — with `application/problem+json` bodies for every failure.

**Implements**:
- `cpt-cf-oagw-flow-create-upstream`
- `cpt-cf-oagw-flow-list-upstreams`
- `cpt-cf-oagw-flow-get-upstream`
- `cpt-cf-oagw-flow-replace-upstream`
- `cpt-cf-oagw-flow-delete-upstream`

**Touches**:
- API: `POST /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams`
- API: `GET /oagw/v1/upstreams/{id}`
- API: `PUT /oagw/v1/upstreams/{id}`
- API: `DELETE /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

## 6. Acceptance Criteria

- [ ] `POST /oagw/v1/upstreams` with body `{"server":{"endpoints":[{"scheme":"http","host":"internal.example.com","port":80}]},"protocol":"gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"}` returns `201 Created` with `alias` equal to `internal.example.com`.
- [ ] `POST /oagw/v1/upstreams` with an endpoint `scheme` outside `https`, `wss`, `wt`, `grpc`, `http`, `ws` returns `400` with an `application/problem+json` body whose `status` field is `400`.
- [ ] `POST /oagw/v1/upstreams` omitting the required `server` field returns `400` with a problem document naming `server` as missing.
- [ ] `POST /oagw/v1/upstreams` omitting the required `protocol` field returns `400` with a problem document naming `protocol` as missing.
- [ ] `POST /oagw/v1/upstreams` with an unknown top-level property returns `400`, enforcing `additionalProperties: false` from `upstream.v1.schema.json`.
- [ ] `POST /oagw/v1/upstreams` with endpoints `us.vendor.com` and `eu.vendor.com`, both on port 443, returns `201` with `alias` equal to `vendor.com`.
- [ ] `POST /oagw/v1/upstreams` with a single hostname endpoint and a user-supplied `alias` that diverges from the derived value returns `400 ValidationError`.
- [ ] `POST /oagw/v1/upstreams` with two IP-address endpoints and no `alias` field returns `400 ValidationError` requiring an explicit alias.
- [ ] `POST /oagw/v1/upstreams` whose derived alias already exists for the same `tenant_id` returns `409` with an `application/problem+json` body.
- [ ] `GET /oagw/v1/upstreams?$top=10&$skip=0` returns `200` with at most 10 upstream representations, all owned by the calling tenant.
- [ ] `GET /oagw/v1/upstreams/{id}` for an identifier owned by a different tenant returns `404`.
- [ ] `PUT /oagw/v1/upstreams/{id}` changing a hostname endpoint such that the derived alias would change returns `400 ValidationError` and leaves the stored record unmodified.
- [ ] `PUT /oagw/v1/upstreams/{id}` setting `enabled: true` while an ancestor tenant's same-alias upstream has `enabled: false` returns `400 ValidationError`.
- [ ] `POST /oagw/v1/upstreams` deriving or supplying an alias that matches an ancestor tenant's disabled upstream, with no `enabled` field (defaulting to `true`), returns `400 ValidationError` and persists no record.
- [ ] `DELETE /oagw/v1/upstreams/{id}` on an existing tenant-owned upstream returns `204 No Content`, and a subsequent `GET` on the same `id` returns `404`.
