# Feature: Resource Model and Store

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Upstream Flow](#create-upstream-flow)
  - [Create Route Flow](#create-route-flow)
  - [Update Upstream Endpoints Flow](#update-upstream-endpoints-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Upstream Schema Validation](#upstream-schema-validation)
  - [Route Schema Validation](#route-schema-validation)
  - [Alias Derivation](#alias-derivation)
  - [Alias Update Transition Check](#alias-update-transition-check)
  - [Tenant-Scoped Store Write Invariants](#tenant-scoped-store-write-invariants)
  - [Tenant Hierarchy Walk](#tenant-hierarchy-walk)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream and Route Enabled-State Lifecycle](#upstream-and-route-enabled-state-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Domain Types Matching the Supplied Schemas](#domain-types-matching-the-supplied-schemas)
  - [Schema-Level Validation Rejects Malformed Requests](#schema-level-validation-rejects-malformed-requests)
  - [Alias Derivation and Normalization](#alias-derivation-and-normalization)
  - [Alias Immutability Across Updates](#alias-immutability-across-updates)
  - [Upstream Alias Uniqueness in the Tenant-Scoped Store](#upstream-alias-uniqueness-in-the-tenant-scoped-store)
  - [Route Match Determinism in the Tenant-Scoped Store](#route-match-determinism-in-the-tenant-scoped-store)
  - [Contiguous Plugin Binding Positions in the Tenant-Scoped Store](#contiguous-plugin-binding-positions-in-the-tenant-scoped-store)
  - [Anonymous GTS Resource Identifier Generation](#anonymous-gts-resource-identifier-generation)
  - [Tenant Hierarchy Walk Primitive](#tenant-hierarchy-walk-primitive)
  - [Enabled/Disabled Lifecycle Enforcement](#enableddisabled-lifecycle-enforcement)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)
  - [7.1 Upstream Field Reference](#71-upstream-field-reference)
  - [7.2 Route Field Reference](#72-route-field-reference)
  - [7.3 Shared Nested Configuration Shapes](#73-shared-nested-configuration-shapes)
  - [7.4 Schema Reconciliation Notes](#74-schema-reconciliation-notes)
  - [7.5 Resource Identification Pattern](#75-resource-identification-pattern)
  - [7.6 Out of Scope](#76-out-of-scope)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-resource-model-and-store-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-resource-model-and-store`

## 1. Feature Context

### 1.1 Overview

This feature defines the `Upstream`, `Route`, and `Plugin` domain types, their request/response
shapes, schema-level validation, alias derivation, and the tenant-scoped in-memory store that
every later CRUD and proxy feature reads and writes through.

### 1.2 Purpose

`gears/system/oagw/oagw/src/lib.rs` is currently empty, so no domain type, validation rule, or
storage primitive exists yet. Every later feature — upstream management, route management,
plugin management, and the proxy data plane — depends on this feature for its data shapes and
its persistence layer. This feature realizes `upstream.v1.schema.json` and `route.v1.schema.json`
field for field, implements the alias derivation and immutability rules those schemas describe
only in prose, and builds the tenant-scoped store (no database is configured for `oagw`) that
enforces the invariants `DESIGN.md` §3.6 documents for a relational schema.

**Requirements**: `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-nfr-multi-tenancy`,
`cpt-cf-oagw-nfr-input-validation`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates and replaces upstreams and routes through the management API; every write passes through this feature's validation and store logic before persistence. |
| `cpt-cf-oagw-actor-tenant-admin` | Same create/replace paths as the platform operator, scoped to their own tenant; sees 404 for any resource owned by a different tenant. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-design-domain-model` (§3.1 Domain Model)
- **Schemas**: [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json), [schemas/route.v1.schema.json](../schemas/route.v1.schema.json)
- **ADRs**: [0001 Request Routing](../ADR/0001-request-routing.md), [0004 CORS](../ADR/0004-cors.md)
- **Decomposition**: `cpt-cf-oagw-feature-resource-model-and-store`
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` (base router mount, shared RFC 9457
  error model, and inbound Bearer authentication gate this feature's handlers build on)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-usecase-configure-route`

### Create Upstream Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-resource-model-create-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A hostname-endpoint upstream is created with its alias auto-derived from the endpoint host.
- An IP-endpoint upstream is created after the operator supplies an explicit alias.

**Error Scenarios**:
- The request body fails schema-level validation (missing required field, bad enum value, or an
  unknown property rejected by `additionalProperties: false`).
- The endpoint set is IP-based, or otherwise non-derivable, and no explicit alias is supplied.
- The operator supplies an alias that differs from the value hostname derivation would produce.
- A second upstream in the same tenant is created with an alias that already exists.

**Steps**:
1. [ ] - `p1` - Operator submits `server.endpoints`, `protocol`, and optionally `alias`, `auth`, `headers`, `rate_limit`, `cors`, `plugins`, and `tags` - `inst-create-up-1`
2. [ ] - `p1` - API: POST /oagw/v1/upstreams (body validated against `upstream.v1.schema.json`) - `inst-create-up-2`
3. [ ] - `p1` - Process: run `cpt-cf-oagw-algo-resource-model-upstream-validate` against the request body - `inst-create-up-3`
4. [ ] - `p1` - **IF** schema validation fails - `inst-create-up-4`
   1. [ ] - `p1` - **RETURN** 400 ValidationError with the failing field paths - `inst-create-up-4a`
5. [ ] - `p1` - **ELSE** - `inst-create-up-5`
   1. [ ] - `p1` - Process: run `cpt-cf-oagw-algo-resource-model-alias-derivation` against `server.endpoints` and the supplied `alias` (if any) - `inst-create-up-5a`
6. [ ] - `p1` - **IF** alias derivation rejects the request (missing explicit alias for a non-derivable endpoint set, or a supplied alias that differs from the derived value) - `inst-create-up-6`
   1. [ ] - `p1` - **RETURN** 400 ValidationError naming the violated alias rule - `inst-create-up-6a`
7. [ ] - `p1` - **ELSE** - `inst-create-up-7`
   1. [ ] - `p1` - Store: run `cpt-cf-oagw-algo-resource-model-store-write-invariants` to check `(tenant_id, alias)` uniqueness within the calling tenant's upstream partition - `inst-create-up-7a`
8. [ ] - `p1` - **IF** an upstream with the same `(tenant_id, alias)` already exists - `inst-create-up-8`
   1. [ ] - `p1` - **RETURN** 409 Conflict - `inst-create-up-8a`
9. [ ] - `p1` - **ELSE** - `inst-create-up-9`
   1. [ ] - `p1` - Store: assign `id = gts.cf.core.oagw.upstream.v1~{new uuid}`, set `tenant_id` from the authenticated principal, insert the `Upstream` record with `enabled` defaulted to `true` - `inst-create-up-9a`
   2. [ ] - `p1` - **RETURN** 201 Created with the persisted `Upstream`, including the resolved alias - `inst-create-up-9b`

### Create Route Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-resource-model-create-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A route is created referencing a tenant-owned upstream, with a match pattern that does not
  collide with any enabled route already registered under that upstream.

**Error Scenarios**:
- The request body fails schema-level validation, including a `match` block naming neither
  `http` nor `grpc`, or naming both.
- `upstream_id` does not resolve to an upstream owned by the calling tenant.
- The new route's method/path pair collides with an enabled route already under the upstream.

**Steps**:
1. [ ] - `p1` - Operator submits `upstream_id` and a `match` block (`http` or `grpc`), optionally `plugins`, `rate_limit`, and `tags` - `inst-create-rt-1`
2. [ ] - `p1` - API: POST /oagw/v1/routes (body validated against `route.v1.schema.json`) - `inst-create-rt-2`
3. [ ] - `p1` - Process: run `cpt-cf-oagw-algo-resource-model-route-validate` against the request body - `inst-create-rt-3`
4. [ ] - `p1` - **IF** schema validation fails - `inst-create-rt-4`
   1. [ ] - `p1` - **RETURN** 400 ValidationError with the failing field paths - `inst-create-rt-4a`
5. [ ] - `p1` - **ELSE** - `inst-create-rt-5`
   1. [ ] - `p1` - Store: look up `upstream_id` within the calling tenant's upstream partition - `inst-create-rt-5a`
6. [ ] - `p1` - **IF** no tenant-owned upstream matches `upstream_id` - `inst-create-rt-6`
   1. [ ] - `p1` - **RETURN** 400 ValidationError ("upstream not found") - `inst-create-rt-6a`
7. [ ] - `p1` - **ELSE** - `inst-create-rt-7`
   1. [ ] - `p1` - Store: run `cpt-cf-oagw-algo-resource-model-store-write-invariants` to check route match determinism against the target upstream's existing enabled routes - `inst-create-rt-7a`
8. [ ] - `p1` - **IF** an enabled route already shares `(method, path)` under the same upstream - `inst-create-rt-8`
   1. [ ] - `p1` - **RETURN** 409 Conflict - `inst-create-rt-8a`
9. [ ] - `p1` - **ELSE** - `inst-create-rt-9`
   1. [ ] - `p1` - Store: assign `id = gts.cf.core.oagw.route.v1~{new uuid}`, set `tenant_id` from the authenticated principal, insert the `Route` record with `enabled` defaulted to `true` - `inst-create-rt-9a`
   2. [ ] - `p1` - **RETURN** 201 Created with the persisted `Route` - `inst-create-rt-9b`

### Update Upstream Endpoints Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-resource-model-update-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- A full replacement of a tenant-owned upstream succeeds because the endpoint change does not
  alter the alias that was already stored.

**Error Scenarios**:
- The replacement body carries `id` or `tenant_id` (both immutable and rejected if present and
  differing from the stored values).
- The endpoint change would alter the derived alias, or supplies a differing alias for a
  non-derivable endpoint set; both are rejected per the alias-update transition rules.

**Steps**:
1. [ ] - `p1` - Operator submits a full replacement body for an existing upstream, omitting `id` and `tenant_id` - `inst-update-up-1`
2. [ ] - `p1` - API: PUT /oagw/v1/upstreams/{id} - `inst-update-up-2`
3. [ ] - `p1` - Store: load the existing `Upstream` by `(tenant_id, id)` - `inst-update-up-3`
4. [ ] - `p1` - **IF** no tenant-owned upstream matches `id` - `inst-update-up-4`
   1. [ ] - `p1` - **RETURN** 404 Not Found - `inst-update-up-4a`
5. [ ] - `p1` - **ELSE** - `inst-update-up-5`
   1. [ ] - `p1` - Process: run `cpt-cf-oagw-algo-resource-model-upstream-validate` against the new body - `inst-update-up-5a`
6. [ ] - `p1` - **IF** schema validation fails - `inst-update-up-6`
   1. [ ] - `p1` - **RETURN** 400 ValidationError - `inst-update-up-6a`
7. [ ] - `p1` - **ELSE** - `inst-update-up-7`
   1. [ ] - `p1` - Process: run `cpt-cf-oagw-algo-resource-model-alias-update-transition` with the stored endpoints/alias and the new endpoints/alias - `inst-update-up-7a`
8. [ ] - `p1` - **IF** the transition check rejects the change - `inst-update-up-8`
   1. [ ] - `p1` - **RETURN** 400 ValidationError instructing the operator to delete and re-create the upstream instead - `inst-update-up-8a`
9. [ ] - `p1` - **ELSE** - `inst-update-up-9`
   1. [ ] - `p1` - Store: replace every field of the `Upstream` record, keeping `id`, `tenant_id`, and `alias` unchanged - `inst-update-up-9a`
   2. [ ] - `p1` - **RETURN** 200 OK with the replaced `Upstream` - `inst-update-up-9b`

## 3. Processes / Business Logic (CDSL)

### Upstream Schema Validation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-resource-model-upstream-validate`

**Input**: Raw create/update request body for an `Upstream`

**Output**: A validated `Upstream` DTO, or a list of field-level validation errors

**Steps**:
1. [ ] - `p1` - Reject any top-level key not in `{id, enabled, alias, tags, server, protocol, auth, headers, plugins, rate_limit, cors}` (`additionalProperties: false`) - `inst-upval-1`
2. [ ] - `p1` - Verify the required keys `server` and `protocol` are present - `inst-upval-2`
3. [ ] - `p1` - **IF** `alias` is present - `inst-upval-3`
   1. [ ] - `p1` - Verify it matches `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` - `inst-upval-3a`
4. [ ] - `p1` - **FOR EACH** entry in `tags` - `inst-upval-4`
   1. [ ] - `p1` - Verify it matches `^[a-z0-9_-]+$` - `inst-upval-4a`
5. [ ] - `p1` - Verify `server.endpoints` has at least one item and `server` carries no key beyond `endpoints` - `inst-upval-5`
6. [ ] - `p1` - **FOR EACH** endpoint in `server.endpoints` - `inst-upval-6`
   1. [ ] - `p1` - Verify `scheme` is one of `"https"`, `"wss"`, `"wt"`, `"grpc"` (the supplied schema's four values), plus `"http"` (added as a fifth accepted value by Override 2), plus `"ws"` (tolerated separately as the plaintext counterpart of `"wss"`); reject any other value - `inst-upval-6a`
   2. [ ] - `p1` - Verify `host` matches the hostname, IPv4, or IPv6 format; reject any other string shape - `inst-upval-6b`
   3. [ ] - `p1` - Verify `port` (default `443` if omitted) is an integer between `1` and `65535` - `inst-upval-6c`
   4. [ ] - `p1` - Reject any endpoint key beyond `scheme`, `host`, `port` (`additionalProperties: false`) - `inst-upval-6d`
7. [ ] - `p1` - Verify `protocol` equals `"gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"` or `"gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"` - `inst-upval-7`
8. [ ] - `p1` - **IF** `auth` is present and `auth.sharing` is present - `inst-upval-8`
   1. [ ] - `p1` - Verify it is one of `"private"`, `"inherit"`, `"enforce"` (default `"private"`) - `inst-upval-8a`
9. [ ] - `p1` - **IF** `headers` is present - `inst-upval-9`
   1. [ ] - `p1` - Reject any key in `headers`, `headers.request`, or `headers.response` beyond the declared property set at each level (`additionalProperties: false` at every nesting level) - `inst-upval-9a`
   2. [ ] - `p1` - **IF** `headers.request.passthrough` is present, verify it is one of `"none"`, `"allowlist"`, `"all"` (default `"none"`) - `inst-upval-9b`
10. [ ] - `p1` - **IF** `plugins` is present - `inst-upval-10`
    1. [ ] - `p1` - **IF** `plugins.sharing` is present, verify it is one of `"private"`, `"inherit"`, `"enforce"` (default `"private"`) - `inst-upval-10a`
    2. [ ] - `p1` - **IF** `plugins.items` is present, verify every entry is either a `gts-identifier` string or a plain UUID string - `inst-upval-10b`
11. [ ] - `p1` - **IF** `rate_limit` is present - `inst-upval-11`
    1. [ ] - `p1` - Reject any key in `rate_limit` beyond `sharing`, `algorithm`, `sustained`, `burst`, `scope`, `strategy`, `cost` (`additionalProperties: false`) - `inst-upval-11a`
    2. [ ] - `p1` - **IF** `sharing` is present, verify it is one of `"private"`, `"inherit"`, `"enforce"` (default `"private"`) - `inst-upval-11b`
    3. [ ] - `p1` - Verify `sustained` is present with `sustained.rate >= 1`; verify `sustained.window` (default `"second"`) is one of `"second"`, `"minute"`, `"hour"`, `"day"` - `inst-upval-11c`
    4. [ ] - `p1` - Verify `algorithm` (default `"token_bucket"`) is one of `"token_bucket"`, `"sliding_window"`; `scope` (default `"tenant"`) is one of `"global"`, `"tenant"`, `"user"`, `"ip"`, `"route"`; `strategy` (default `"reject"`) is one of `"reject"`, `"queue"`, `"degrade"`; `cost` (default `1`) is an integer `>= 1` - `inst-upval-11d`
    5. [ ] - `p1` - **IF** `burst` is present, verify `burst.capacity` is an integer `>= 1`; it defaults to `sustained.rate` when the field is omitted - `inst-upval-11e`
12. [ ] - `p1` - **IF** `cors` is present - `inst-upval-12`
    1. [ ] - `p1` - Verify `enabled` is present (required inside `cors`) - `inst-upval-12a`
    2. [ ] - `p1` - **IF** `sharing` is present, verify it is one of `"private"`, `"inherit"`, `"enforce"` (default `"private"`) - `inst-upval-12b`
    3. [ ] - `p1` - **IF** `allow_credentials` is `true` - `inst-upval-12c`
       1. [ ] - `p1` - Verify `allowed_origins` does not contain the literal `"*"` - `inst-upval-12c-i`
13. [ ] - `p1` - **RETURN** the validated `Upstream` DTO, or the accumulated field errors - `inst-upval-13`

### Route Schema Validation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-resource-model-route-validate`

**Input**: Raw create/update request body for a `Route`

**Output**: A validated `Route` DTO, or a list of field-level validation errors

**Steps**:
1. [ ] - `p1` - Verify the required keys `upstream_id` and `match` are present - `inst-rtval-1`
2. [ ] - `p1` - Reject any key inside `match` beyond `http` and `grpc` (`additionalProperties: false`) - `inst-rtval-2`
3. [ ] - `p1` - **IF** `match` contains neither `http` nor `grpc`, or contains both - `inst-rtval-3`
   1. [ ] - `p1` - **RETURN** 400 ValidationError ("match must contain exactly one of http or grpc", the schema's `oneOf` constraint) - `inst-rtval-3a`
4. [ ] - `p1` - **IF** `match.http` is present - `inst-rtval-4`
   1. [ ] - `p1` - Verify `methods` is a non-empty array whose entries are each one of `"GET"`, `"POST"`, `"PUT"`, `"DELETE"`, `"PATCH"` - `inst-rtval-4a`
   2. [ ] - `p1` - Verify `path` has length `>= 1` - `inst-rtval-4b`
   3. [ ] - `p1` - **IF** `query_allowlist` is present, verify it is an array of strings (default `[]`, meaning no query parameters are forwarded) - `inst-rtval-4c`
   4. [ ] - `p1` - **IF** `path_suffix_mode` is present, verify it is `"disabled"` or `"append"` (default `"append"`) - `inst-rtval-4d`
   5. [ ] - `p1` - Reject any key in `match.http` beyond `methods`, `path`, `query_allowlist`, `path_suffix_mode` - `inst-rtval-4e`
5. [ ] - `p1` - **IF** `match.grpc` is present - `inst-rtval-5`
   1. [ ] - `p1` - Verify `service` and `method` each have length `>= 1`; reject any other key in `match.grpc` - `inst-rtval-5a`
   2. [ ] - `p1` - Note: this shape validates and round-trips through the store; no gRPC dispatch code path exists in this build (Scope Reality) - `inst-rtval-5b`
6. [ ] - `p1` - **FOR EACH** entry in `tags` - `inst-rtval-6`
   1. [ ] - `p1` - Verify it matches `^[a-z0-9_-]+$` - `inst-rtval-6a`
7. [ ] - `p1` - **IF** `plugins.items` is present - `inst-rtval-7`
   1. [ ] - `p1` - Verify every entry is a string in the `gts-identifier` format; unlike `Upstream.plugins.items`, a bare UUID is not an accepted alternative here - `inst-rtval-7a`
8. [ ] - `p1` - **IF** `rate_limit` is present, apply the same validation as `cpt-cf-oagw-algo-resource-model-upstream-validate` step `inst-upval-11`, including its `additionalProperties: false` check and `burst.capacity` rule (the two schemas define an identical `rate_limit` shape, so this route path inherits the fix at its source) - `inst-rtval-8`
9. [ ] - `p1` - **RETURN** the validated `Route` DTO, or the accumulated field errors - `inst-rtval-9`

### Alias Derivation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-resource-model-alias-derivation`

**Input**: A validated `server.endpoints` list and an optional user-supplied `alias` string

**Output**: An accepted, normalized alias string, or a validation rejection naming the violated
rule

**Steps**:
1. [ ] - `p1` - **FOR EACH** endpoint, classify its `host` as hostname, IPv4, or IPv6 - `inst-aliasderive-1`
2. [ ] - `p1` - **IF** every endpoint's host is an IP address, or the endpoint set mixes IP and hostname hosts - `inst-aliasderive-2`
   1. [ ] - `p1` - Mark the endpoint set non-derivable - `inst-aliasderive-2a`
3. [ ] - `p1` - **ELSE IF** the endpoint set has exactly one distinct hostname - `inst-aliasderive-3`
   1. [ ] - `p1` - Set `derived = hostname`, appending `:port` unless `port` equals the standard port for the endpoint's `scheme` (`80` for `http`/`ws`; `443` for `https`/`wss`/`wt`/`grpc`) - `inst-aliasderive-3a`
4. [ ] - `p1` - **ELSE** (the endpoint set has more than one distinct hostname) - `inst-aliasderive-4`
   1. [ ] - `p1` - Compute the longest common domain suffix shared by every hostname - `inst-aliasderive-4a`
   2. [ ] - `p1` - Validate the computed suffix against the public suffix list (PSL, the registry of domain suffixes that are not themselves individually registrable, for example `co.uk`) - `inst-aliasderive-4b`
   3. [ ] - `p1` - **IF** no common suffix exists, the suffix has fewer than two labels, or the suffix is itself a bare PSL entry - `inst-aliasderive-4c`
      1. [ ] - `p1` - Mark the endpoint set non-derivable - `inst-aliasderive-4c-i`
   4. [ ] - `p1` - **ELSE** - `inst-aliasderive-4d`
      1. [ ] - `p1` - Set `derived = suffix`, appending `:port` unless every endpoint shares the same port and that port is the scheme's standard port - `inst-aliasderive-4d-i`
5. [ ] - `p1` - **IF** the endpoint set is non-derivable - `inst-aliasderive-5`
   1. [ ] - `p1` - **IF** no `alias` was supplied - `inst-aliasderive-5a`
      1. [ ] - `p1` - **RETURN** rejection ("explicit alias required for IP-based or non-derivable endpoints") - `inst-aliasderive-5a-i`
   2. [ ] - `p1` - **ELSE** - `inst-aliasderive-5b`
      1. [ ] - `p1` - **RETURN** the supplied `alias`, normalized (ASCII-lowercased, trailing dot stripped) - `inst-aliasderive-5b-i`
6. [ ] - `p1` - **ELSE** (a `derived` value exists) - `inst-aliasderive-6`
   1. [ ] - `p1` - **IF** an `alias` was supplied - `inst-aliasderive-6a`
      1. [ ] - `p1` - Normalize both `derived` and the supplied `alias` (ASCII-lowercase, trailing dot stripped) - `inst-aliasderive-6a-i`
      2. [ ] - `p1` - **IF** they differ - `inst-aliasderive-6a-ii`
         1. [ ] - `p1` - **RETURN** rejection ("user-provided alias must match the derived value") - `inst-aliasderive-6a-ii-a`
      3. [ ] - `p1` - **ELSE** - `inst-aliasderive-6a-iii`
         1. [ ] - `p1` - **RETURN** the normalized `derived` value (accepted as an idempotent no-op) - `inst-aliasderive-6a-iii-a`
   2. [ ] - `p1` - **ELSE** - `inst-aliasderive-6b`
      1. [ ] - `p1` - **RETURN** the normalized `derived` value - `inst-aliasderive-6b-i`

### Alias Update Transition Check

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-resource-model-alias-update-transition`

**Input**: The stored `Upstream`'s current `server.endpoints` and `alias`, plus the proposed new
`server.endpoints` and optional new `alias` from a replacement request

**Output**: Accept (the alias always stays equal to the stored value) or reject with a message
directing the operator to delete and re-create

**Steps**:
1. [ ] - `p1` - Run `cpt-cf-oagw-algo-resource-model-alias-derivation`'s classification step against the stored endpoints to get `existing_derived` (a value, or non-derivable) - `inst-aliastrans-1`
2. [ ] - `p1` - Run the same classification against the proposed endpoints to get `new_derived` - `inst-aliastrans-2`
3. [ ] - `p1` - **IF** the proposed endpoint set is identical to the stored endpoint set (no endpoint change at all) - `inst-aliastrans-3`
   1. [ ] - `p1` - **IF** the proposed `alias` is omitted, or equals the stored `alias` after normalization - `inst-aliastrans-3a`
      1. [ ] - `p1` - **RETURN** accept, alias unchanged (exact-match tolerated as a no-op) - `inst-aliastrans-3a-i`
   2. [ ] - `p1` - **ELSE** - `inst-aliastrans-3b`
      1. [ ] - `p1` - **RETURN** reject ("alias override not allowed when endpoints are unchanged") - `inst-aliastrans-3b-i`
4. [ ] - `p1` - **ELSE IF** `existing_derived` and `new_derived` both exist (Derivable → Derivable) - `inst-aliastrans-4`
   1. [ ] - `p1` - **IF** `new_derived` equals the stored `alias` - `inst-aliastrans-4a`
      1. [ ] - `p1` - **RETURN** accept, alias unchanged (recomputed alias equals the existing one) - `inst-aliastrans-4a-i`
   2. [ ] - `p1` - **ELSE** - `inst-aliastrans-4b`
      1. [ ] - `p1` - **RETURN** reject ("endpoint change would alter the derived alias; delete and re-create") - `inst-aliastrans-4b-i`
5. [ ] - `p1` - **ELSE IF** `existing_derived` exists and `new_derived` does not (Derivable → Non-derivable, hostname → IP) - `inst-aliastrans-5`
   1. [ ] - `p1` - **RETURN** reject always, even when the request supplies an explicit `alias` - `inst-aliastrans-5a`
6. [ ] - `p1` - **ELSE IF** neither `existing_derived` nor `new_derived` exists (Non-derivable → Non-derivable, IP → IP) - `inst-aliastrans-6`
   1. [ ] - `p1` - **IF** the proposed `alias` is omitted, or equals the stored `alias` after normalization - `inst-aliastrans-6a`
      1. [ ] - `p1` - **RETURN** accept, existing alias retained - `inst-aliastrans-6a-i`
   2. [ ] - `p1` - **ELSE** - `inst-aliastrans-6b`
      1. [ ] - `p1` - **RETURN** reject ("a differing user-provided alias is not accepted") - `inst-aliastrans-6b-i`
7. [ ] - `p1` - **ELSE** (`existing_derived` does not exist, `new_derived` does — Non-derivable → Derivable, IP → hostname) - `inst-aliastrans-7`
   1. [ ] - `p1` - **IF** `new_derived` equals the stored `alias` - `inst-aliastrans-7a`
      1. [ ] - `p1` - **RETURN** accept, alias unchanged (derived alias coincidentally equals the existing one) - `inst-aliastrans-7a-i`
   2. [ ] - `p1` - **ELSE** - `inst-aliastrans-7b`
      1. [ ] - `p1` - **RETURN** reject ("delete and re-create") - `inst-aliastrans-7b-i`

### Tenant-Scoped Store Write Invariants

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-resource-model-store-write-invariants`

**Input**: A validated `Upstream` or `Route` DTO, the calling tenant's `tenant_id`, and the
operation kind (create or replace)

**Output**: Accept (persist, with `id`/`tenant_id` assigned or preserved) or a conflict/validation
rejection

**Steps**:
1. [ ] - `p1` - **IF** the entity is an `Upstream` - `inst-storeinv-1`
   1. [ ] - `p1` - Store: scan the tenant's upstream partition for a record sharing `(tenant_id, alias)`, excluding the record being replaced - `inst-storeinv-1a`
   2. [ ] - `p1` - **IF** a match is found - `inst-storeinv-1b`
      1. [ ] - `p1` - **RETURN** 409 Conflict - `inst-storeinv-1b-i`
2. [ ] - `p1` - **IF** the entity is a `Route` **AND** its `match` carries `http` - `inst-storeinv-2`
   1. [ ] - `p1` - Store: for each method in the new route's `match.http.methods`, scan the target upstream's enabled routes for one sharing that `method` and the same `match.http.path`, excluding the route being replaced - `inst-storeinv-2a`
   2. [ ] - `p1` - **IF** any overlapping `(method, path)` pair is found among enabled routes - `inst-storeinv-2b`
      1. [ ] - `p1` - **RETURN** 409 Conflict ("route match is ambiguous with an existing enabled route") - `inst-storeinv-2b-i`
3. [ ] - `p1` - **ELSE IF** the entity is a `Route` **AND** its `match` carries `grpc` - `inst-storeinv-3`
   1. [ ] - `p1` - Store: run no uniqueness scan for this route. Two enabled routes sharing one `(service, method)` pair under the same upstream are both accepted in this build, because no gRPC dispatch path ever reads a route's match to observe the collision (Scope Reality) - `inst-storeinv-3a`
4. [ ] - `p1` - **IF** the entity carries `plugins.items` - `inst-storeinv-4`
   1. [ ] - `p1` - Store: verify the binding positions to be written form a contiguous sequence starting at `0` - `inst-storeinv-4a`
   2. [ ] - `p1` - **IF** the positions are not contiguous from `0` - `inst-storeinv-4b`
      1. [ ] - `p1` - **RETURN** 400 ValidationError ("plugin binding positions must be contiguous from 0") - `inst-storeinv-4b-i`
5. [ ] - `p1` - **IF** the operation is a create - `inst-storeinv-5`
   1. [ ] - `p1` - Store: assign `id = gts.cf.core.oagw.{upstream|route}.v1~{new uuid}` and `tenant_id` from the calling tenant - `inst-storeinv-5a`
6. [ ] - `p1` - **ELSE** (replace) - `inst-storeinv-6`
   1. [ ] - `p1` - Store: keep the existing `id` and `tenant_id`; reject the write if the request body supplied a different value for either - `inst-storeinv-6a`
7. [ ] - `p1` - **RETURN** accept - `inst-storeinv-7`

### Tenant Hierarchy Walk

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-resource-model-tenant-hierarchy-walk`

**Input**: A starting `tenant_id` and a caller-supplied lookup function (for example, "find an
enabled upstream by alias" or "find enabled routes for an upstream")

**Output**: The first match found while walking from the starting tenant toward the root tenant,
paired with the tenant that produced it, or no match

**Steps**:
1. [ ] - `p1` - Set `current_tenant = tenant_id` - `inst-tenwalk-1`
2. [ ] - `p1` - **WHILE** `current_tenant` is defined - `inst-tenwalk-2`
   1. [ ] - `p1` - Apply the lookup function against `current_tenant`'s partition of the store - `inst-tenwalk-2a`
   2. [ ] - `p1` - **IF** the lookup function returns a match - `inst-tenwalk-2b`
      1. [ ] - `p1` - **RETURN** the match paired with `current_tenant` (the closest match wins; a descendant's own resource shadows an ancestor's) - `inst-tenwalk-2b-i`
   3. [ ] - `p1` - **ELSE** - `inst-tenwalk-2c`
      1. [ ] - `p1` - Set `current_tenant` to the parent tenant of `current_tenant` in the tenant hierarchy - `inst-tenwalk-2c-i`
3. [ ] - `p1` - **RETURN** no match (the walk reached the root tenant with nothing found) - `inst-tenwalk-3`

## 4. States (CDSL)

### Upstream and Route Enabled-State Lifecycle

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-resource-model-enabled-lifecycle`

**States**: Enabled, Disabled

**Initial State**: Enabled

**Transitions**:
1. [ ] - `p1` - **FROM** Enabled **TO** Disabled **WHEN** an operator replaces the resource with `enabled: false` - `inst-lifecycle-1`
2. [ ] - `p1` - **FROM** Disabled **TO** Enabled **WHEN** an operator replaces the resource with `enabled: true` and no ancestor tenant's upstream sharing the same alias is itself `Disabled` - `inst-lifecycle-2`
3. [ ] - `p1` - **FROM** Disabled **TO** Disabled **WHEN** an operator attempts to set `enabled: true` while an ancestor tenant's upstream sharing the same alias is `Disabled`; the stored field is not changed, and the resource's effective state stays `Disabled` for every descendant - `inst-lifecycle-3`

## 5. Definitions of Done

### Domain Types Matching the Supplied Schemas

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-domain-types`

The system **MUST** define `Upstream`, `Route`, and `Plugin` domain types, together with the
nested `ServerConfig`/`Endpoint`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`,
`CorsConfig`, and `PluginsConfig` shapes, whose fields match `upstream.v1.schema.json` and
`route.v1.schema.json` as enumerated in §7. Where DESIGN.md's domain model and the supplied
schema disagree (`tenant_id` on both entities, `Route.enabled`, `Route.cors`, and
`Route.priority`), the reconciliation documented in §7.4 governs, and no field is invented
beyond what §7 enumerates.

**Implements**:
- `cpt-cf-oagw-flow-resource-model-create-upstream`
- `cpt-cf-oagw-flow-resource-model-create-route`

**Touches**:
- API (forward reference; this feature owns no endpoints — realized by
  `cpt-cf-oagw-feature-upstream-management-api` and `cpt-cf-oagw-feature-route-management-api`):
  `POST /oagw/v1/upstreams`, `POST /oagw/v1/routes`
- Entities: `Upstream`, `Route`, `Plugin`, `ServerConfig`, `Endpoint`

### Schema-Level Validation Rejects Malformed Requests

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-schema-validation`

The system **MUST** enforce every required-field, enum, pattern, range, and
`additionalProperties: false` rule the two schemas declare, plus the `oneOf` constraint on
`Route.match`. It **MUST** also accept two values beyond the schema's own `"https"`, `"wss"`,
`"wt"`, `"grpc"`: `scheme: "http"`, added as a fifth accepted value by Override 2, and
`scheme: "ws"`, tolerated separately as the plaintext counterpart of `"wss"`. Acceptance of a
scheme value at create time is independent of whether OAGW later opens a plaintext connection,
which `allow_http_upstream` governs elsewhere.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-upstream-validate`
- `cpt-cf-oagw-algo-resource-model-route-validate`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:
- API (forward reference; this feature owns no endpoints — realized by
  `cpt-cf-oagw-feature-upstream-management-api` and `cpt-cf-oagw-feature-route-management-api`):
  `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`, `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- Entities: `Upstream`, `Route`, `Endpoint`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`

### Alias Derivation and Normalization

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-alias-derivation`

The system **MUST** derive an upstream's alias from hostname-based endpoints (single hostname,
or a PSL-validated common registrable suffix across multiple hostnames), **MUST** require an
explicit alias for IP-based or otherwise non-derivable endpoint sets, and **MUST** normalize
every alias to ASCII lowercase with a trailing dot stripped before storing or comparing it.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-alias-derivation`
- `cpt-cf-oagw-flow-resource-model-create-upstream`

**Touches**:
- API (forward reference; this feature owns no endpoints — realized by
  `cpt-cf-oagw-feature-upstream-management-api`): `POST /oagw/v1/upstreams`
- Entities: `Upstream`, `Endpoint`

### Alias Immutability Across Updates

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-alias-immutability`

The system **MUST** enforce the alias-update transition table (DESIGN.md:426-432): any endpoint
replacement that would change the stored alias is rejected regardless of whether the caller
supplies a matching or differing alias, and only an exact-match alias (recomputed or
user-supplied) is ever accepted on a replace.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-alias-update-transition`
- `cpt-cf-oagw-flow-resource-model-update-upstream`

**Touches**:
- API (forward reference; this feature owns no endpoints — realized by
  `cpt-cf-oagw-feature-upstream-management-api`): `PUT /oagw/v1/upstreams/{id}`
- Entities: `Upstream`

### Upstream Alias Uniqueness in the Tenant-Scoped Store

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-upstream-alias-uniqueness`

The system **MUST** provide a tenant-scoped in-memory store, since no database is configured
for `oagw`, that rejects a second upstream sharing `(tenant_id, alias)` within one tenant. This
realizes the uniqueness invariant `cpt-cf-oagw-db-schema` documents as a relational schema,
with no SQL migration involved.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-store-write-invariants`
- `cpt-cf-oagw-flow-resource-model-create-upstream`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- Store: `cpt-cf-oagw-db-schema` (realized as an in-memory, tenant-partitioned store)
- API (forward reference; this feature owns no endpoints — realized by
  `cpt-cf-oagw-feature-upstream-management-api`): `POST /oagw/v1/upstreams`
- Entities: `Upstream`

### Route Match Determinism in the Tenant-Scoped Store

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-route-match-determinism`

The system **MUST** reject a new or replaced route whose `(method, path)` pair matches an
already-enabled route under the same upstream. A disabled route sharing that same pair does
not block the write, realizing the invariant `cpt-cf-oagw-db-schema` documents as a relational
schema, with no SQL migration involved.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-store-write-invariants`
- `cpt-cf-oagw-flow-resource-model-create-route`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- Store: `cpt-cf-oagw-db-schema` (realized as an in-memory, tenant-partitioned store)
- API (forward reference; this feature owns no endpoints — realized by
  `cpt-cf-oagw-feature-route-management-api`): `POST /oagw/v1/routes`
- Entities: `Route`

### Contiguous Plugin Binding Positions in the Tenant-Scoped Store

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-plugin-binding-positions`

The system **MUST** reject a plugin-binding write whose positions are not contiguous from
zero, for example `0` and `2` with `1` skipped, so every stored plugin chain stays a dense,
ordered sequence.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-store-write-invariants`

**Touches**:
- Entities: `Plugin`

### Anonymous GTS Resource Identifier Generation

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-gts-identifiers`

The system **MUST** generate every resource ID using the anonymous GTS pattern
`gts.cf.core.oagw.{type}.v1~{uuid}`, where `{type}` is `upstream`, `route`, or the plugin's
`{kind}_plugin` value for `Plugin`.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-store-write-invariants`
- `cpt-cf-oagw-flow-resource-model-create-upstream`
- `cpt-cf-oagw-flow-resource-model-create-route`

**Touches**:
- Entities: `Upstream`, `Route`, `Plugin`

### Tenant Hierarchy Walk Primitive

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-tenant-hierarchy-walk`

The system **MUST** provide a reusable descendant-to-root tenant hierarchy walk primitive that
a caller-supplied lookup function drives, so alias resolution and route matching in later
features share one walk implementation.

**Implements**:
- `cpt-cf-oagw-algo-resource-model-tenant-hierarchy-walk`

**Touches**:
- Entities: `Upstream`, `Route`

### Enabled/Disabled Lifecycle Enforcement

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-resource-model-enabled-lifecycle`

The system **MUST** enforce the enabled/disabled lifecycle described in §4, including the rule
that a descendant tenant cannot re-enable a resource an ancestor tenant has disabled.

**Implements**:
- `cpt-cf-oagw-state-resource-model-enabled-lifecycle`

**Touches**:
- Entities: `Upstream`, `Route`

## 6. Acceptance Criteria

- [ ] An upstream created with a single hostname endpoint on the scheme's standard port derives
  its alias automatically as that hostname, with no port suffix.
- [ ] An upstream created with only IP-address endpoints and no explicit `alias` is rejected
  with 400 ValidationError.
- [ ] An upstream created with hostname endpoints and a user-supplied `alias` that differs from
  the derivable value is rejected with 400 ValidationError.
- [ ] An upstream endpoint declared with `scheme: "http"` and `port: 80` is accepted at
  create time, independent of whether `allow_http_upstream` later permits an actual plaintext
  connection.
- [ ] A second upstream created in the same tenant with an `alias` identical to an existing
  upstream's `alias` is rejected with 409 Conflict.
- [ ] A route whose `match` block names both `http` and `grpc`, or neither, is rejected with
  400 ValidationError (the schema's `oneOf` constraint).
- [ ] A route created with `match.grpc` validates and round-trips (a subsequent read returns the
  same `service` and `method`), even though no gRPC proxy dispatch path exists in this build.
- [ ] Two enabled routes under the same upstream sharing the same method and path are rejected
  with 409 Conflict; a disabled route sharing the same pair does not conflict.
- [ ] Plugin bindings written with non-contiguous positions (for example, `0` and `2`, skipping
  `1`) are rejected with 400 ValidationError.
- [ ] Resolving an alias supplied in mixed case (for example, `Api.OpenAI.COM`) returns the same
  upstream as the stored, normalized lowercase alias.
- [ ] The tenant hierarchy walk returns a descendant tenant's upstream in preference to an
  ancestor tenant's upstream that shares the same alias.

## 7. Additional Context (optional)

### 7.1 Upstream Field Reference

| Field | Type / Enum | Required | Notes |
|---|---|---|---|
| `id` | UUID string | No (read-only) | Server-generated; immutable after create. |
| `enabled` | boolean, default `true` | No | Governs §4's enabled/disabled lifecycle. |
| `alias` | string, pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` | No (derived or required per §7.4) | See §7.3 for derivation. |
| `tags` | array of string, pattern `^[a-z0-9_-]+$` | No | Add-only union across tenant hierarchy (per `cpt-cf-oagw-fr-hierarchical-config`, not built in this feature). |
| `server` | object, `additionalProperties: false`, requires `endpoints` | Yes | See `ServerConfig`/`Endpoint` below. |
| `protocol` | `"gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"` or `"gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"` | Yes | GTS-formatted enum, quoted verbatim. |
| `auth` | object: `type` (gts-identifier string), `sharing` (`"private"`\|`"inherit"`\|`"enforce"`, default `"private"`), `config` (object) | No | No `additionalProperties: false` at this level — the schema permits keys beyond `type`/`sharing`/`config`. |
| `headers` | `HeadersConfig` (see §7.3) | No | |
| `plugins` | object: `sharing` (`"private"`\|`"inherit"`\|`"enforce"`, default `"private"`), `items` (array; each entry is a `gts-identifier` string **or** a UUID string) | No | The UUID alternative distinguishes custom (Starlark) plugins from builtin ones. |
| `rate_limit` | `RateLimitConfig` (see §7.3) | No | |
| `cors` | `CorsConfig` (see §7.3) | No | |

`ServerConfig`: object, `additionalProperties: false`, requires `endpoints` (array, `minItems: 1`).

`Endpoint`: object, `additionalProperties: false`, requires `scheme` and `host`.
- `scheme`: schema enum is exactly `"https"`, `"wss"`, `"wt"`, `"grpc"` (default `"https"`). This
  build adds `"http"` as a fifth accepted value (Override 2), a deliberate addition beyond the
  supplied enum, not a value the supplied schema already contains. It separately tolerates
  `"ws"` as the plaintext counterpart of `"wss"`, likewise a deliberate extension rather than
  part of the supplied enum. Whether OAGW opens an actual plaintext connection for `"http"` or
  `"ws"` is governed solely by the `allow_http_upstream` configuration flag; accepting the
  scheme value at create time is a separate, earlier question that this feature owns.
- `host`: string matching hostname, IPv4, or IPv6 format.
- `port`: integer, `1`–`65535`, default `443`.

### 7.2 Route Field Reference

| Field | Type / Enum | Required | Notes |
|---|---|---|---|
| `id` | UUID string | No (read-only) | Server-generated; immutable after create. |
| `tags` | array of string, pattern `^[a-z0-9_-]+$` | No | |
| `upstream_id` | UUID string | Yes | Immutable after create; not present in the update DTO. |
| `match` | object, `additionalProperties: false`; `oneOf` requires exactly one of `http`/`grpc` | Yes | See below. |
| `plugins` | object: `sharing` (`"private"`\|`"inherit"`\|`"enforce"`, default `"private"`), `items` (array of `gts-identifier` strings, default `[]`) | No | No UUID alternative here, unlike `Upstream.plugins.items`. |
| `rate_limit` | Identical shape to `Upstream.rate_limit` (see §7.3) | No | |

`match.http` (`additionalProperties: false`, requires `methods` and `path`):
- `methods`: array, `minItems: 1`, each entry one of `"GET"`, `"POST"`, `"PUT"`, `"DELETE"`, `"PATCH"`.
- `path`: string, `minLength: 1`.
- `query_allowlist`: array of strings, default `[]` (empty allowlist means no query parameter is forwarded).
- `path_suffix_mode`: `"disabled"` or `"append"`, default `"append"`.

`match.grpc` (`additionalProperties: false`, requires `service` and `method`):
- `service`: string, `minLength: 1` (fully qualified gRPC service name).
- `method`: string, `minLength: 1` (RPC method name).
- This shape validates and round-trips through the store and the management API. No gRPC
  proxy dispatch code path exists in this build (Scope Reality) — a request naming a route with
  `match.grpc` at proxy time is out of scope for this feature and the proxy features that follow
  it, not a defect of this feature's validation.

### 7.3 Shared Nested Configuration Shapes

`HeadersConfig` (object, `additionalProperties: false`):
- `request` (object, `additionalProperties: false`): `set` (map of string→string), `add` (map
  of string→string), `remove` (array of strings), `passthrough` (`"none"`\|`"allowlist"`\|`"all"`,
  default `"none"`), `passthrough_allowlist` (array of strings).
- `response` (object, `additionalProperties: false`): `set`, `add`, `remove` — same shapes as
  above, with no `passthrough` field (response headers are always transformed explicitly, never
  passed through wholesale).

`RateLimitConfig` (object, `additionalProperties: false`, requires `sustained`; identical on
`Upstream` and `Route`):
- `sharing`: `"private"`\|`"inherit"`\|`"enforce"`, default `"private"`.
- `algorithm`: `"token_bucket"`\|`"sliding_window"`, default `"token_bucket"`.
- `sustained` (object, requires `rate`): `rate` (integer `>= 1`), `window`
  (`"second"`\|`"minute"`\|`"hour"`\|`"day"`, default `"second"`).
- `burst` (object): `capacity` (integer `>= 1`; defaults to `sustained.rate` when the object or
  the field is omitted).
- `scope`: `"global"`\|`"tenant"`\|`"user"`\|`"ip"`\|`"route"`, default `"tenant"`.
- `strategy`: `"reject"`\|`"queue"`\|`"degrade"`, default `"reject"`.
- `cost`: integer `>= 1`, default `1`.

`CorsConfig` (object, `additionalProperties: false`, requires `enabled`; identical shape on
`Upstream` and the (unreferenced, see §7.4) `Route` definition):
- `sharing`: `"private"`\|`"inherit"`\|`"enforce"`, default `"private"`.
- `enabled`: boolean, default `false`.
- `allowed_origins`: array of strings, each either the literal `"*"` or a URI.
- `allowed_methods`: array, each one of `"GET"`, `"POST"`, `"PUT"`, `"PATCH"`, `"DELETE"`,
  `"HEAD"`, `"OPTIONS"`, default `["GET", "POST"]`.
- `expose_headers`: array of strings, default `[]`.
- `allow_credentials`: boolean, default `false`. When `true`, `allowed_origins` **MUST NOT**
  contain `"*"` (a conditional `if`/`then` constraint in both schemas).

`PluginsConfig`: the `sharing`/`items` shape described per-entity in §7.1 and §7.2; the item
type differs between `Upstream` (gts-identifier or UUID) and `Route` (gts-identifier only).

### 7.4 Schema Reconciliation Notes

The supplied schemas are the field-for-field source of truth this feature's domain types
follow. Four places where DESIGN.md's domain model (§3.1) and the schemas diverge are resolved
as follows, so the downstream implementer does not have to guess:

- **`tenant_id` on `Upstream` and `Route`**: neither schema declares a `tenant_id` property (and
  `Upstream`'s top-level object has `additionalProperties: false`, so a client-supplied
  `tenant_id` would be rejected outright). `tenant_id` is a store-managed attribute the write
  path assigns from the authenticated principal's tenant context, never accepted from the
  request body, and immutable once set — consistent with `cpt-cf-oagw-nfr-multi-tenancy`.
- **`Route.enabled`**: `route.v1.schema.json`'s top-level object does not declare
  `additionalProperties: false` and does not list `enabled` among its properties. Because
  `cpt-cf-oagw-fr-enable-disable` requires an `enabled` boolean (default `true`) on both
  upstreams and routes, the `Route` domain type carries it as a first-class field; the absence
  of a top-level `additionalProperties: false` restriction means the wire schema does not reject
  it, even though it is not one of the schema's explicitly enumerated properties.
- **`Route.cors`**: `route.v1.schema.json` defines a `cors` shape under `definitions` (identical
  to `Upstream`'s), but no property in the schema's `properties` block references it via `$ref`.
  DESIGN.md's domain model and ADR 0004 both describe `Route.cors` as a real, per-route field.
  This feature treats the schema's unreferenced `cors` definition as the intended shape for a
  `Route.cors` field, accepted under the same absent-`additionalProperties: false` reasoning as
  `Route.enabled` above, and validated identically to `Upstream.cors` (§7.3).
- **`Route.priority`**: DESIGN.md's class diagram and its route-match-determinism invariant
  (§3.6) both reference a numeric `priority` field, but neither schema declares one, and no `$ref`
  target exists for it. Because no FR requires a distinct priority field and the schema is
  silent on it, this feature does **not** add a `priority` field. Route match determinism
  (§5, `cpt-cf-oagw-dod-resource-model-route-match-determinism`) is instead enforced over
  `(upstream_id, method, path)`: no two enabled routes under the same upstream may share a
  method and path. `DESIGN.md`'s `(path_prefix, priority)` phrasing is a documented deviation
  this feature does not carry forward. `DECOMPOSITION.md` §2.4 repeats a similar phrase, "same
  path + priority + method", for the downstream route-management-api feature that depends on
  this one. Read that phrase the same way, as `(path, method)`, because the schema still
  exposes no client-settable `priority` field for that later feature to accept.

Two further, smaller notes: `Route.match_type`, shown in DESIGN.md's class diagram, is not a
separate stored field — it is derived from which key (`http` or `grpc`) is present under
`match`. And `Upstream.plugins.items` accepts either a `gts-identifier` string or a bare UUID
string, while `Route.plugins.items` accepts only a `gts-identifier` string; a custom plugin
bound to a route must be expressed as its full `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`
identifier, not a bare UUID.

### 7.5 Resource Identification Pattern

All three entity types use anonymous GTS identifiers of the form
`gts.cf.core.oagw.{type}.v1~{uuid}`:
- `Upstream`: `gts.cf.core.oagw.upstream.v1~{uuid}`.
- `Route`: `gts.cf.core.oagw.route.v1~{uuid}`.
- `Plugin`: `gts.cf.core.oagw.{kind}_plugin.v1~{uuid}`, where `{kind}` is `auth`, `guard`, or
  `transform` depending on the plugin's category.

### 7.6 Out of Scope

- gRPC request dispatch: `protocol` and `match.grpc` validate and round-trip through the store
  and the management API, but no gRPC proxy code path exists in this build (Scope Reality).
- SQL persistence: no database is configured for `oagw`; the store described in §3 and §5 is
  in-memory and tenant-partitioned, not a set of `toolkit-db`/SeaORM migrations
  (`cpt-cf-oagw-constraint-multi-sql`).
- SSRF (Server-Side Request Forgery) policy enforcement, credential injection, rate-limit
  counter evaluation, and CORS request-time handling are out of scope for this feature; it
  defines and validates the configuration fields those later features read, but does not
  execute any of that runtime behavior itself.
