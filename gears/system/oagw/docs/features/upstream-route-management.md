# Feature: Upstream and Route Management


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Upstream](#create-upstream)
  - [Replace Upstream](#replace-upstream)
  - [Create Route](#create-route)
  - [Replace Route](#replace-route)
  - [List and Read Upstreams and Routes](#list-and-read-upstreams-and-routes)
  - [Delete Upstream or Route](#delete-upstream-or-route)
  - [Enable and Disable an Upstream or Route](#enable-and-disable-an-upstream-or-route)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Alias Derivation from Endpoints](#alias-derivation-from-endpoints)
  - [Alias Update Enforcement](#alias-update-enforcement)
  - [Upstream Model Validation](#upstream-model-validation)
  - [Route Model Validation](#route-model-validation)
  - [Sharing Modes and Ancestor Permission Gates](#sharing-modes-and-ancestor-permission-gates)
  - [Store Write and Invariant Enforcement](#store-write-and-invariant-enforcement)
  - [OData List Query Translation](#odata-list-query-translation)
  - [Resource Identifier Resolution](#resource-identifier-resolution)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream State Machine](#upstream-state-machine)
  - [Route State Machine](#route-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Upstream Model and Schema Conformance](#upstream-model-and-schema-conformance)
  - [Upstream Management Endpoints](#upstream-management-endpoints)
  - [Route Model and Schema Conformance](#route-model-and-schema-conformance)
  - [Route Management Endpoints](#route-management-endpoints)
  - [Alias Derivation and Enforcement](#alias-derivation-and-enforcement)
  - [Tenant Scoping and Isolation](#tenant-scoping-and-isolation)
  - [Sharing Modes, Ancestor Permissions and Layering](#sharing-modes-ancestor-permissions-and-layering)
  - [In-Memory Store and Logical Invariants](#in-memory-store-and-logical-invariants)
  - [OData List Queries](#odata-list-queries)
  - [Enabled and Disabled Write Semantics](#enabled-and-disabled-write-semantics)
  - [Management Error Contract](#management-error-contract)
  - [Test Layering](#test-layering)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-upstream-route-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-upstream-route-management`

<!--
=============================================================================
FEATURE SPECIFICATION
=============================================================================
PURPOSE: Define detailed implementation behavior — flows, algorithms, states,
and implementation requirements that bridge PRD and DESIGN to code.

SCOPE:
  ✓ Actor flows (user-facing interactions, step by step)
  ✓ Processes / Business Logic (incl. internal logic, validation, async jobs, etc)
  ✓ State machines (entity lifecycle)
  ✓ Implementation requirements (what to build)
  ✓ Acceptance criteria (how to verify)

NOT IN THIS DOCUMENT (see other templates):
  ✗ Requirements → PRD.md
  ✗ Architecture, components, APIs → DESIGN.md
  ✗ Why a specific approach was chosen → ADR/

CDSL PSEUDO-CODE:
  Optional. Use for complex flows or when precise behavior must be
  communicated. Skip for simple features to avoid overhead.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

This feature is the OAGW control plane: the tenant-scoped management API for upstreams and routes,
the in-memory store that holds them, and the write-side rules that make the stored configuration
trustworthy for the data plane. It registers the ten management endpoints DECOMPOSITION entry 2.2
lists — create, list, get, replace and delete for `/oagw/v1/upstreams` and for `/oagw/v1/routes` —
and implements validation of the upstream and route models against
[schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json) and
[schemas/route.v1.schema.json](../schemas/route.v1.schema.json), alias derivation and enforcement
from endpoint hostnames, immutable identity fields, enabled/disabled semantics with hierarchical
inheritance, ancestor permission gates, and config sharing modes with layering. Persistence is
in-memory (DECOMPOSITION assumption 3): the store keeps the DESIGN logical model
`cpt-cf-oagw-db-schema` and enforces its invariants, and it is the dataset the plugin feature (2.3)
scans for plugin-in-use conflicts and the data plane (2.4, 2.5) resolves configuration from.

### 1.2 Purpose

The control plane comes second in the decomposition because the data plane has nothing to resolve
until it exists: entry 2.4 resolves an alias to an upstream and matches routes against the store this
feature populates, entry 2.3 scans the upstream and route plugin bindings this feature validates and
stores, and entry 2.5 enforces the rate limits and CORS posture written here. This feature owns the
endpoint half of `cpt-cf-oagw-interface-management-api` — the ten management operations themselves,
on top of the mount root and response contract entry 2.1 wired — and the control-plane half of the
shared requirements: derivation and uniqueness of `cpt-cf-oagw-fr-alias-resolution` (entry 2.4 owns
request-time resolution), CRUD semantics of `cpt-cf-oagw-fr-enable-disable` (entry 2.4 owns
enforcement on the request path), and management-request validation of
`cpt-cf-oagw-nfr-input-validation` (entry 2.4 owns body and header validation on the proxy path). It
realizes `cpt-cf-oagw-principle-tenant-scope` as store-level tenant isolation and
`cpt-cf-oagw-principle-cred-isolation` as the rule that the stored configuration carries `cred://`
references and never secret material, and it follows the resource model of
`cpt-cf-oagw-design-domain-model` and the contracts of `cpt-cf-oagw-interface-api`.

**Requirements**:

- [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
- [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
- [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
- [x] `p2` - `cpt-cf-oagw-fr-config-layering`
- [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
- [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
- [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
- [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
- [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
- [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`
- [ ] `p1` - `cpt-cf-oagw-interface-management-api`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`, `cpt-cf-oagw-principle-cred-isolation`

**Feature-local deviations from platform baselines** (each inherited from the decomposition's
task-level assumptions or from a documented DESIGN/schema gap; none is a new decision taken here):

- Gear-relative management paths — DECOMPOSITION assumption 1. The ten endpoints are registered as
  `/oagw/v1/upstreams` and `/oagw/v1/routes` without the `/api` prefix, because the host api-gateway
  nests the gear router under its own `prefix_path`, which is empty in the graded configuration. The
  `/api/oagw/v1/...` paths in `cpt-cf-oagw-interface-api` are the absolute form behind an operator
  gateway and are not what this gear registers. Review owner: OAGW component maintainer
  (`cf-gears-oagw`).
- `http` admitted as an endpoint `scheme` — DECOMPOSITION assumption 2 and entry 2.2 scope, recorded
  against `cpt-cf-oagw-constraint-https-only`. The shipped schema declares the `scheme` enum
  `https|wss|wt|grpc`; this feature's model and validation admit `http` alongside them, because the
  graded runtime configuration sets `allow_http_upstream: true`. Whether a plaintext upstream
  connection is actually attempted remains governed by `allow_http_upstream` and is decided on the
  data plane (entry 2.4); the default posture of the constraint stays HTTPS-only. Review owner: OAGW
  component maintainer, with the security reviewer as second approver. Validation: an in-crate test
  asserts an endpoint with `scheme: http` is accepted at create time, and a second test asserts the
  recorded default `allow_http_upstream: false` still expresses the HTTPS-only posture (the
  connection-time refusal itself is entry 2.4's test).
- In-memory store in place of SeaORM and `toolkit-db` — DECOMPOSITION assumption 3, recorded against
  `cpt-cf-oagw-constraint-multi-sql` and `cpt-cf-oagw-principle-tenant-scope`. The store is built on
  the crate's existing `dashmap`/`parking_lot`/`arc-swap` dependencies, keeps `cpt-cf-oagw-db-schema`
  as the logical data model including the DESIGN table and column names, and enforces the DESIGN's
  relational invariants on write; tenant scoping is realized by tenant-keyed lookups rather than the
  DESIGN's secure-ORM wording, and no raw SQL exists in the gear. Review owner: OAGW component
  maintainer. Validation: in-crate tests assert each invariant is refused on write and that no record
  belonging to another tenant is reachable through any read path.
- Route model fields beyond the shipped route schema — `enabled` (PRD
  `cpt-cf-oagw-fr-enable-disable` and the DESIGN `Route` class), `priority` (the DESIGN route-match
  determinism invariant and the duplicate-match 409 rule of entry 2.2) and route-level `cors` (the
  DESIGN `Route` class, with per-upstream/route CORS configuration per ADR 0004). The route schema's
  root object sets no `additionalProperties: false`, so the create/replace DTO admits them; the
  recorded defaults are `enabled: true` and `priority: 0`. Review owner: OAGW component maintainer.
  Validation: in-crate tests assert the recorded defaults on a create that omits them and that a
  route `priority` participates in the duplicate-match conflict check.
- `grpc` match declared but not served — DECOMPOSITION entry 2.2 scope. The route model accepts
  `match.grpc` with `service` and `method` and an upstream `protocol` of
  `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`, and the store keeps them exactly as declared,
  but no gRPC route matching or proxying exists in this deployment (DESIGN Phase 3; entries 2.4 and
  2.6 own the proxy path). Review owner: OAGW component maintainer. Validation: an in-crate test
  asserts a `match.grpc` route is stored verbatim and that no HTTP match key is derived from it.
- Conflict-class problem `type` identifiers — the DESIGN error table in `cpt-cf-oagw-interface-api`
  enumerates a 409 identifier only for `PluginInUse` (entry 2.3) and none for this feature's 409
  classes, the `(tenant_id, alias)` collision and the duplicate route match. This feature therefore
  returns 409 through the entry-2.1 mapping layer with the conflict `type` identifier that layer
  defines in the `gts.cf.core.errors.err.v1~cf.oagw....v1` space, names the colliding key in
  `detail`, and does not extend the error table on its own. Review owner: OAGW component maintainer,
  with the API contract owner as second approver. Validation: an in-crate test asserts a 409 body is
  `application/problem+json`, carries a GTS `type` identifier from that space and names the colliding
  key in `detail`.

Coverage note: `cpt-cf-oagw-constraint-toolkit-deploy` is inherited from dependency entry 2.1 (gear
deployment) and is cited on the DoDs that sit on that entry's layer for that reason, not as a
constraint this feature adopts on its own.

**Cross-cutting concerns**:

- Security: every management operation requires Bearer authentication and the permission string of
  `cpt-cf-oagw-interface-api` (`gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}` and the
  route equivalent), tenant scoping is applied on every read and write before any other rule, and the
  `auth.config` surface carries `cred://` references only — no secret material is ever stored,
  returned or logged (`cpt-cf-oagw-principle-cred-isolation`). The management path performs no
  credential resolution; reference shape is the only check made here.
- Reliability: a management write is all-or-nothing — a rejected invariant leaves the store exactly
  as it was and returns the mapped error, so a client retry is the whole remedy. The store is
  process-local by assumption 3, so a host restart drops the stored configuration and recovery is
  re-provisioning (the DESIGN's registry provisioning path materializes entities through the same
  validation pipeline this feature implements); there is no persistence to recover from.
- Data integrity: the DESIGN's logical invariants — unique `(tenant_id, alias)`, route match
  uniqueness among enabled routes, plugin binding positions contiguous from 0, cascade delete of a
  route with its upstream — are enforced inside a single write critical section, and readers observe
  either the previous or the new snapshot, never a partial write.
- Observability: structured log lines for each management write (operation, resource type, resource
  id, tenant, principal, timestamp, outcome) are in scope; the ADR 0001 audit record contract and the
  metrics families are entry 2.7, and no metrics endpoint is exposed (DECOMPOSITION assumption 9).
- Rollback: no persistence and no migration exist, so rollback is the operational act of redeploying
  the previous executable and re-creating the previous configuration through this same API; the only
  in-gear rollback is the staged-write discard of a rejected change.
- Test layering: coverage is in-crate Rust tests only — unit tests inside `#[cfg(test)]` modules per
  layer and integration tests under the crate's `tests/` directory that boot the gear router and
  exercise the ten endpoints — and the `testing/e2e/gears/oagw/` directory is not used (DECOMPOSITION
  assumption 5).
- Compile-time gate: the endpoints exist in the host executable only when the host feature `oagw` is
  enabled and the crate is linked for inventory registration (entry 2.1's gate); this feature adds no
  gate of its own and no new gear.
- Performance: not applicable in this feature — the management path has no latency budget to state or
  measure, and the read-side cost of the store on the proxy hot path belongs to entry 2.4
  (`cpt-cf-oagw-nfr-low-latency`). This feature only guarantees that a write publishes its snapshot
  once, so no reader can observe a partially applied configuration.
- Compliance/Privacy: not applicable in this feature — resources hold routing configuration, not
  personal data; nothing is written to disk (assumption 3), so there is no retention, residency or
  subject-right surface; and no secret material enters the store, so there is no credential exposure
  surface beyond the `cred://` reference check.
- Accessibility: not applicable in this feature — no user-facing interface is authored beyond the
  `application/problem+json` error body contract of entry 2.1, whose machine-readable `type`, `title`
  and `detail` fields are the only surface an accessibility concern could attach to.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Creates, replaces, lists and deletes upstreams and routes, sets `enabled` and `tags`, and configures sharing modes; enforces configuration on descendant tenants through the ancestor permission gates. |
| `cpt-cf-oagw-actor-tenant-admin` | Performs the same operations inside its own tenant, binds to an ancestor upstream by `alias` when `oagw:upstream:bind` grants it, and sees ancestor resources as 404 on the management API. |
| `cpt-cf-oagw-actor-types-registry` | Holds the GTS type definitions the upstream and route models conform to; the resource identifiers this feature issues and accepts in path parameters are GTS identifiers in that space. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-db-schema`
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.2 and assumptions 1 to 9
- **ADRs**: [0001 Request Routing](../ADR/0001-request-routing.md) (`cpt-cf-oagw-adr-request-routing`), [0006 State Management](../ADR/0006-state-management.md) (`cpt-cf-oagw-adr-state-management`); supporting baselines: [0004 CORS](../ADR/0004-cors.md) (`cpt-cf-oagw-adr-cors`, route-level `cors` field stored here), [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md) (`cpt-cf-oagw-adr-error-source-distinction`, response header applied by entry 2.1)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` — this feature mounts its endpoints on the router that feature wires, returns errors through its canonical mapping layer, stamps `X-OAGW-Error-Source` through its header layer, and reads the gear configuration it loads
- **Resolved gear dependencies used here**: `tenant-resolver` (calling tenant and ancestor chain), `authz-resolver` (permission checks), `types-registry`; `credstore` is declared but not called on the management path
- **Platform baselines**: toolkit canonical error contract (`toolkit_canonical_errors::CanonicalError` serialized as RFC 9457 `application/problem+json` with GTS `type` identifiers in the `gts.cf.core.errors.err.v1~cf.oagw....v1` space) and the `X-OAGW-Error-Source` response header, both delivered by the entry-2.1 cross-cutting layer; toolkit Bearer authentication and the permission strings of `cpt-cf-oagw-interface-api`; `tenant-resolver` tenant hierarchy; the anonymous GTS resource identifier pattern `gts.cf.core.oagw.upstream.v1~{uuid}` and `gts.cf.core.oagw.route.v1~{uuid}` of `cpt-cf-oagw-interface-api`; the host OpenAPI registry and readiness surface entry 2.1 registers against; the `psl` public-suffix list crate and the `dashmap`/`parking_lot`/`arc-swap` store primitives already present in the crate's `Cargo.toml`

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the
end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to
actor actions. Every failure below returns through the entry-2.1 mapping layer, so each error body is
`application/problem+json` with a GTS `type` identifier and every response carries
`X-OAGW-Error-Source`.

**Use cases**: `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-usecase-configure-route`

**Referenced, not covered here**:

- `cpt-cf-oagw-usecase-proxy-request` — covered by DECOMPOSITION entry 2.4, which owns the proxy request path that resolves an alias and matches routes against the store this feature populates.
- `cpt-cf-oagw-nfr-low-latency` — covered by DECOMPOSITION entry 2.4, which owns the proxy hot path whose latency budget the management path of this feature does not share.

### Create Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-create`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor sends `POST /oagw/v1/upstreams` with `server.endpoints`, `protocol` and any of `alias`,
  `auth`, `headers`, `rate_limit`, `cors`, `plugins`, `tags` and receives `201` with the stored
  representation, including the server-generated `id`, the derived or validated `alias` and
  `enabled: true`.
- A tenant administrator whose `alias` matches an ancestor upstream creates a binding when
  `oagw:upstream:bind` is granted and the ancestor's sharing modes permit the requested overrides.
- An actor who supplies the exact derived `alias` for hostname endpoints succeeds with the same
  result as omitting it (idempotent tolerance).

**Error Scenarios**:
- `400` for an invalid endpoint (`host` not RFC 1123 and not an IP literal, `port` outside 1 to
  65535, an unknown `scheme`, an unknown `protocol` identifier), a missing explicit `alias` for an
  IP-based or non-derivable endpoint set, a supplied `alias` that differs from the derived value, an
  unknown `auth.type` plugin identifier, `allow_credentials: true` combined with a `*` origin, or a
  malformed `cred://` reference.
- `409` when another upstream of the same tenant already holds the `(tenant_id, alias)` key.
- `403` when the alias resolves to an ancestor upstream and `oagw:upstream:bind` is not granted, or
  when an ancestor field marked `sharing: enforce` would be overridden.
- `404` when the alias resolves to an ancestor upstream marked `sharing: private`, which is invisible
  to a descendant.
- `401` when the Bearer token is missing or invalid.

**Steps**:
1. [x] - `p1` - Actor sends the create request with `server.endpoints`, `protocol` and the optional fields - `inst-ucre-01`
2. [x] - `p1` - API: `POST /oagw/v1/upstreams` authenticates the Bearer token and requires the `gts.cf.core.oagw.upstream.v1~:create` permission - `inst-ucre-02`
3. [x] - `p1` - Resolve the calling tenant from the security context; a request without a resolvable tenant is rejected - `inst-ucre-03`
4. [x] - `p1` - Parse the body into the upstream create DTO, rejecting unknown properties per the schema's `additionalProperties: false` - `inst-ucre-04`
5. [x] - `p1` - Validate the body with `cpt-cf-oagw-algo-upstream-validate` - `inst-ucre-05`
6. [x] - `p1` - Derive or verify the `alias` with `cpt-cf-oagw-algo-alias-derive` - `inst-ucre-06`
7. [x] - `p1` - **IF** validation or alias enforcement fails - `inst-ucre-07`
   1. [x] - `p1` - Map the domain error through the entry-2.1 mapping layer and **RETURN** `400` with the validation problem body - `inst-ucre-08`
8. [x] - `p1` - **ELSE** - `inst-ucre-09`
   1. [x] - `p1` - Check the `(tenant_id, alias)` key against the tenant's own upstreams - `inst-ucre-10`
   2. [x] - `p1` - Apply the ancestor bind and sharing-mode gates with `cpt-cf-oagw-algo-sharing-mode-validate` - `inst-ucre-11`
9. [x] - `p1` - Store: insert the `oagw_upstream` record with its `oagw_upstream_tag` and `oagw_upstream_plugin` rows and the generated `id`, inside one atomic write that enforces `UNIQUE (tenant_id, alias)` - `inst-ucre-12`
10. [x] - `p1` - Publish the new configuration snapshot and invalidate consumer caches - `inst-ucre-13`
11. [x] - `p1` - **RETURN** `201` with the stored upstream representation, or `400`, `403`, `404` or `409` as described above - `inst-ucre-14`

### Replace Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-replace`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor sends `PUT /oagw/v1/upstreams/{id}` with a full replacement body; all fields are
  overwritten, omitted optional fields are cleared, and the response is `200` with the replaced
  representation.
- An endpoint change that recomputes to the existing `alias` (derivable to derivable, or
  non-derivable to derivable with the same derived value) is accepted.

**Error Scenarios**:
- `404` when `{id}` does not resolve to an upstream owned by the calling tenant, including an
  ancestor-owned upstream.
- `400` when the replacement is invalid, or when the endpoints would change the `alias` (including a
  hostname to IP change, which is rejected even when an explicit `alias` is supplied), or when a
  supplied `alias` differs from the stored one.
- `409` when a revalidated ancestor bind constraint or a sharing-mode gate rejects the change.
- `400` when the request attempts to change the immutable `id` or `tenant_id`.

**Steps**:
1. [x] - `p1` - Actor sends the replacement body for an existing upstream - `inst-urep-01`
2. [x] - `p1` - API: `PUT /oagw/v1/upstreams/{id}` authenticates the Bearer token and requires the `gts.cf.core.oagw.upstream.v1~:override` permission - `inst-urep-02`
3. [x] - `p1` - Resolve `{id}` with `cpt-cf-oagw-algo-resource-identity`, scoped to the calling tenant - `inst-urep-03`
4. [x] - `p1` - **IF** no upstream of this tenant carries that identifier - `inst-urep-04`
   1. [x] - `p1` - **RETURN** `404` with the not-found problem body; an ancestor-owned upstream is indistinguishable from a missing one - `inst-urep-05`
5. [x] - `p1` - Reject any attempt to change `id` or `tenant_id`, and require a supplied `alias` to equal the stored one - `inst-urep-06`
6. [x] - `p1` - Validate the replacement body with `cpt-cf-oagw-algo-upstream-validate` and enforce alias immutability with `cpt-cf-oagw-algo-alias-update-enforce` - `inst-urep-07`
7. [x] - `p1` - **IF** the endpoints, the overrides or the `alias` changed - `inst-urep-08`
   1. [x] - `p1` - Re-validate the ancestor bind constraints and sharing modes with `cpt-cf-oagw-algo-sharing-mode-validate` - `inst-urep-09`
8. [x] - `p1` - Store: replace the `oagw_upstream` record and its `oagw_upstream_tag` and `oagw_upstream_plugin` rows in one atomic write, clearing omitted optional fields - `inst-urep-10`
9. [x] - `p1` - Publish the new configuration snapshot and invalidate consumer caches - `inst-urep-11`
10. [x] - `p1` - **RETURN** `200` with the replaced upstream representation - `inst-urep-12`

### Create Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-create`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor sends `POST /oagw/v1/routes` with `upstream_id` and `match` and receives `201` with the
  stored route, including the server-generated `id` and the recorded `priority`.
- A route whose `match` carries `http` with `methods`, `path`, `query_allowlist` and
  `path_suffix_mode`, or `grpc` with `service` and `method`, is stored as declared.

**Error Scenarios**:
- `400` when the body is invalid: a missing or malformed `upstream_id`, an `upstream_id` that does not
  resolve to an upstream owned by the calling tenant (including an ancestor-owned upstream), a `match`
  that carries neither or both of `http` and `grpc`, an empty `methods` array, a method outside
  `GET|POST|PUT|DELETE|PATCH`, an empty `path`, or an invalid `rate_limit`.
- `401` when the Bearer token is missing or invalid.
- `403` when the create permission is not granted.
- `409` when an enabled route of the same upstream already holds the same `path`, `priority` and
  method combination.

**Steps**:
1. [x] - `p1` - Actor sends the create request with `upstream_id` and `match` - `inst-rcre-01`
2. [x] - `p1` - API: `POST /oagw/v1/routes` authenticates the Bearer token and requires the `gts.cf.core.oagw.route.v1~:create` permission - `inst-rcre-02`
3. [x] - `p1` - Resolve the calling tenant from the security context - `inst-rcre-03`
4. [x] - `p1` - Parse the body into the route create DTO and validate it with `cpt-cf-oagw-algo-route-validate` - `inst-rcre-04`
5. [x] - `p1` - Resolve `upstream_id` with `cpt-cf-oagw-algo-resource-identity` scoped to the calling tenant; an ancestor-owned upstream is not directly addressable and resolves as missing - `inst-rcre-05`
6. [x] - `p1` - **IF** validation fails or the upstream reference does not resolve - `inst-rcre-06`
   1. [x] - `p1` - **RETURN** `400` with the validation problem body naming the offending field - `inst-rcre-07`
7. [x] - `p1` - **ELSE** - `inst-rcre-08`
   1. [x] - `p1` - Check the match-uniqueness invariant (`path`, `priority` and method) among the upstream's enabled routes - `inst-rcre-09`
8. [x] - `p1` - Store: insert the `oagw_route` record with its `oagw_route_http_match` or `oagw_route_grpc_match`, `oagw_route_method`, `oagw_route_tag` and `oagw_route_plugin` rows in one atomic write - `inst-rcre-10`
9. [x] - `p1` - Publish the new configuration snapshot and invalidate consumer caches - `inst-rcre-11`
10. [x] - `p1` - **RETURN** `201` with the stored route representation, or `400` or `409` as described above - `inst-rcre-12`

### Replace Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-replace`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor sends `PUT /oagw/v1/routes/{id}` with a full replacement body; `match`, `plugins`,
  `rate_limit`, `tags`, `enabled` and `priority` are overwritten and the response is `200`.
- A body that echoes the existing `upstream_id` is accepted, satisfying the schema's required-field
  list without changing the reference.

**Error Scenarios**:
- `404` when `{id}` does not resolve to a route owned by the calling tenant.
- `400` when the replacement is invalid, or when the body changes `upstream_id`, which is immutable.
- `409` when the revalidated match rule collides with another enabled route of the same upstream.

**Steps**:
1. [x] - `p1` - Actor sends the replacement body for an existing route - `inst-rrep-01`
2. [x] - `p1` - API: `PUT /oagw/v1/routes/{id}` authenticates the Bearer token and requires the `gts.cf.core.oagw.route.v1~:override` permission - `inst-rrep-02`
3. [x] - `p1` - Resolve `{id}` with `cpt-cf-oagw-algo-resource-identity`, scoped to the calling tenant - `inst-rrep-03`
4. [x] - `p1` - **IF** no route of this tenant carries that identifier - `inst-rrep-04`
   1. [x] - `p1` - **RETURN** `404` with the not-found problem body - `inst-rrep-05`
5. [x] - `p1` - **ELSE** - `inst-rrep-06`
   1. [x] - `p1` - Reject any attempt to change `id`, `tenant_id` or `upstream_id`: the reference is immutable, so a supplied value must equal the stored one and a differing value is `400` - `inst-rrep-07`
6. [x] - `p1` - Validate the replacement body with `cpt-cf-oagw-algo-route-validate` - `inst-rrep-08`
7. [x] - `p1` - Re-validate the match-uniqueness invariant among the upstream's enabled routes with `cpt-cf-oagw-algo-store-invariants` - `inst-rrep-09`
8. [x] - `p1` - Store: replace the `oagw_route` record and its match, method, tag and plugin rows in one atomic write, clearing omitted optional fields - `inst-rrep-10`
9. [x] - `p1` - Publish the new configuration snapshot and invalidate consumer caches - `inst-rrep-11`
10. [x] - `p1` - **RETURN** `200` with the replaced route representation, or `400` or `409` as described above - `inst-rrep-12`

### List and Read Upstreams and Routes

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-management-list-query`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor lists upstreams or routes with `GET /oagw/v1/upstreams` or `GET /oagw/v1/routes` and the
  OData parameters `$filter`, `$select`, `$orderby`, `$top` and `$skip`, and receives `200` with the
  tenant-scoped, filtered, ordered page.
- An actor reads a single resource by its GTS identifier and receives `200` with the selected fields.

**Error Scenarios**:
- `404` when `{id}` does not resolve to a resource of the calling tenant, including an ancestor-owned
  resource.
- `400` for an unsupported `$filter` operator, an unknown `$orderby` or `$select` field, a `$top`
  above `100`, or a negative `$skip`.

**Steps**:
1. [x] - `p1` - Actor sends a list or read request for upstreams or routes - `inst-lst-01`
2. [x] - `p1` - API: `GET /oagw/v1/upstreams`, `GET /oagw/v1/routes`, `GET /oagw/v1/upstreams/{id}` or `GET /oagw/v1/routes/{id}` authenticates the Bearer token and requires the `read` permission of the matching resource type - `inst-lst-02`
3. [x] - `p1` - Resolve the calling tenant and restrict the collection to that tenant's own records before any filtering, ordering or paging - `inst-lst-03`
4. [x] - `p1` - **IF** the request targets a single identifier - `inst-lst-04`
   1. [x] - `p1` - Resolve it with `cpt-cf-oagw-algo-resource-identity`; an unresolved or foreign identifier is **RETURN**ed as `404` - `inst-lst-05`
5. [x] - `p1` - **ELSE** - `inst-lst-06`
   1. [x] - `p1` - Translate the query parameters with `cpt-cf-oagw-algo-odata-query`; an unsupported expression is **RETURN**ed as `400` - `inst-lst-07`
6. [x] - `p1` - **RETURN** `200` with the resources projected onto `$select` when given, and the full stored representation otherwise - `inst-lst-08`

### Delete Upstream or Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-route-delete`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor deletes a route with `DELETE /oagw/v1/routes/{id}` and receives `204`; the route and its
  match, method, tag and plugin rows disappear from the store.
- An actor deletes an upstream with `DELETE /oagw/v1/upstreams/{id}` and receives `204`; the upstream
  and all routes belonging to it are removed (cascade).

**Error Scenarios**:
- `404` when `{id}` does not resolve to a resource of the calling tenant, including an ancestor-owned
  resource.
- `401` when the Bearer token is missing or invalid.

**Steps**:
1. [x] - `p1` - Actor sends the delete request - `inst-del-01`
2. [x] - `p1` - API: `DELETE /oagw/v1/upstreams/{id}` or `DELETE /oagw/v1/routes/{id}` authenticates the Bearer token and requires the `delete` permission of the matching resource type - `inst-del-02`
3. [x] - `p1` - Resolve `{id}` with `cpt-cf-oagw-algo-resource-identity`, scoped to the calling tenant - `inst-del-03`
4. [x] - `p1` - **IF** the identifier does not resolve within the tenant - `inst-del-04`
   1. [x] - `p1` - **RETURN** `404` with the not-found problem body - `inst-del-05`
5. [x] - `p1` - **ELSE** - `inst-del-06`
   1. [x] - `p1` - Store: delete the record and its dependent rows in one atomic write, cascading from an upstream to the `oagw_route` records that reference it - `inst-del-07`
6. [x] - `p1` - Publish the new configuration snapshot and invalidate consumer caches - `inst-del-08`
7. [x] - `p1` - **RETURN** `204` with no body - `inst-del-09`

### Enable and Disable an Upstream or Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-enable-disable`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A created resource carries `enabled: true` by default; the owning tenant sets it to `false` with a
  replace operation and the stored value changes to `false`.
- A disabled upstream or route stays addressable and readable through the management API with
  `enabled: false`, so an operator can re-enable it or delete it.

**Error Scenarios**:
- A descendant tenant cannot re-enable an ancestor-disabled resource: the resource is not addressable
  on the management API at all and resolves as `404`, so no request can flip its `enabled` value.
- `400` when `enabled` is supplied as a non-boolean value.

**Steps**:
1. [x] - `p1` - Actor sends a create or replace request carrying `enabled` - `inst-enb-01`
2. [x] - `p1` - Apply the recorded default `enabled: true` when the field is omitted on create - `inst-enb-02`
3. [x] - `p1` - Store: persist the boolean on the owning tenant's record - `inst-enb-03`
4. [x] - `p1` - **IF** the resource is owned by an ancestor tenant and the caller is a descendant - `inst-enb-04`
   1. [x] - `p1` - Refuse the operation through the tenant-scoped lookup, which returns `404` before any field is read, so an inherited disabled state cannot be lifted - `inst-enb-05`
5. [x] - `p1` - **ELSE** - `inst-enb-06`
   1. [x] - `p1` - Accept the new `enabled` value and record it on the store snapshot the data plane resolves from - `inst-enb-07`
6. [x] - `p1` - Treat the effect of a disabled upstream (`503`) and the exclusion of a disabled route from matching as data-plane behaviour (entry 2.4), not as a management-side action - `inst-enb-08`
7. [x] - `p1` - **RETURN** the create or replace result with the effective `enabled` value - `inst-enb-09`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. These are the
reusable building blocks called by the actor flows above and reused by the features that follow.

### Alias Derivation from Endpoints

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-alias-derive`

**Input**: the request's `server.endpoints[]` (each with `scheme`, `host`, `port`) and the optional
caller-supplied `alias`.

**Output**: the validated `alias` to store, or a derivation failure that maps to `400`.

**Steps**:
1. [x] - `p1` - Normalize every `server.endpoints[].host` to ASCII lowercase and strip one trailing dot - `inst-ader-01`
2. [x] - `p1` - Validate each host per RFC 1123 (at most 253 characters, labels of 1 to 63 characters drawn from ASCII alphanumerics and hyphens, no label starting or ending with a hyphen) or accept an IPv4 or IPv6 literal, marking such endpoints as IP-based - `inst-ader-02`
3. [x] - `p1` - Validate each `port` as an integer in 1 to 65535 with the recorded default `443` when omitted, and each `scheme` against `https|wss|wt|grpc` plus the `http` admitted by assumption 2 - `inst-ader-03`
4. [x] - `p1` - Require every endpoint of the pool to share the same `protocol`, `scheme` and `port` per the DESIGN multi-endpoint pool rule - `inst-ader-04`
5. [x] - `p1` - **IF** the pool holds exactly one hostname endpoint - `inst-ader-05`
   1. [x] - `p1` - Derive the alias from that hostname, omitting the port when it is standard for the scheme (`80` for `http`, `443` for `https|wss|wt|grpc`) and appending `:port` otherwise - `inst-ader-06`
6. [x] - `p1` - **ELSE IF** the pool holds several hostname endpoints - `inst-ader-07`
   1. [x] - `p1` - Compute the longest common domain suffix of at least two labels and validate it against the public suffix list with the `psl` crate: the suffix must be a registrable domain and not a bare public suffix - `inst-ader-08`
   2. [x] - `p1` - Derive the alias from that suffix, preserving `:port` when the pool's port is non-standard - `inst-ader-09`
7. [x] - `p1` - **IF** an endpoint is IP-based, or the only common suffix is a bare public suffix such as `co.uk`, or the hostnames share no registrable suffix - `inst-ader-10`
   1. [x] - `p1` - Require the caller-supplied `alias`; when it is absent, fail with `400` - `inst-ader-11`
8. [x] - `p1` - **ELSE IF** a caller-supplied `alias` is present - `inst-ader-12`
   1. [x] - `p1` - Normalize it, tolerate it when it equals the derived value, and fail with `400` when it differs - `inst-ader-13`
9. [x] - `p1` - **RETURN** the normalized `alias`, which always matches the schema pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` - `inst-ader-14`

### Alias Update Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-alias-update-enforce`

**Input**: the stored upstream (`id`, `tenant_id`, `alias`, `server.endpoints[]`) and the
replacement body's `server.endpoints[]` and `alias`.

**Output**: the accepted replacement carrying the unchanged `alias`, or a `400` rejection.

**Steps**:
1. [x] - `p1` - Compute the derived alias of the proposed `server.endpoints[]` with `cpt-cf-oagw-algo-alias-derive` - `inst-aupd-01`
2. [x] - `p1` - **IF** the endpoints are unchanged - `inst-aupd-02`
   1. [x] - `p1` - Keep the existing `alias`; a supplied `alias` must equal it exactly, otherwise fail with `400` - `inst-aupd-03`
3. [x] - `p1` - **ELSE IF** the existing endpoints are derivable and the proposed endpoints are derivable - `inst-aupd-04`
   1. [x] - `p1` - Accept when the recomputed alias equals the existing one and fail with `400` when it differs, directing the operator to delete and re-create - `inst-aupd-05`
4. [x] - `p1` - **ELSE IF** the existing endpoints are derivable and the proposed endpoints are not - `inst-aupd-06`
   1. [x] - `p1` - Fail with `400` always, even when an explicit `alias` is supplied - `inst-aupd-07`
5. [x] - `p1` - **ELSE IF** neither set is derivable - `inst-aupd-08`
   1. [x] - `p1` - Retain the existing alias and fail with `400` when a differing alias is supplied - `inst-aupd-09`
6. [x] - `p1` - **ELSE** (non-derivable to derivable) - `inst-aupd-10`
   1. [x] - `p1` - Accept when the derived alias equals the existing one and fail with `400` otherwise - `inst-aupd-11`
7. [x] - `p1` - **RETURN** the replacement with the immutable `id`, `tenant_id` and `alias` - `inst-aupd-12`

### Upstream Model Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-upstream-validate`

**Input**: the upstream create or replace body.

**Output**: a typed upstream model with recorded defaults applied, or the first validation failure
with the offending field.

**Steps**:
1. [x] - `p1` - Require `server` and `protocol`, and reject unknown properties - `inst-uval-01`
2. [x] - `p1` - Require `server.endpoints` with at least one entry, each carrying `scheme` and `host`, an optional `port` defaulting to `443` in 1 to 65535, and `host` matching hostname, IPv4 or IPv6 - `inst-uval-02`
3. [x] - `p1` - Require `protocol` to be `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1` or `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1` - `inst-uval-03`
4. [x] - `p1` - Validate `auth.type` as a GTS identifier against the four resolvable builtin auth plugin types `noop`, `apikey`, `oauth2_client_cred` and `oauth2_client_cred_basic`, as declared by entry 2.3's builtin registry contract; `basic` and `bearer` remain catalog-only identifiers and fail with `unknown auth plugin`, resolvability of a UUID-backed custom plugin stays deferred to entry 2.3, and `auth.sharing` defaults to `private` - `inst-uval-04`
5. [x] - `p1` - Validate that any secret reference inside `auth.config` is a well-formed `cred://` reference; no credential store call is made and no secret material is read - `inst-uval-05`
6. [x] - `p1` - Validate `headers` against the schema: `request.set`, `request.add`, `request.remove`, `request.passthrough` in `none|allowlist|all` defaulting to `none`, `request.passthrough_allowlist`, and `response.set`, `response.add`, `response.remove` - `inst-uval-06`
7. [x] - `p1` - Validate `rate_limit`: `sustained` with `rate` of at least 1 and `window` in `second|minute|hour|day` defaulting to `second`, `burst.capacity` of at least 1, defaulting to `sustained.rate` when omitted, `algorithm` in `token_bucket|sliding_window` defaulting to `token_bucket`, `scope` in `global|tenant|user|ip|route` defaulting to `tenant`, `strategy` in `reject|queue|degrade` defaulting to `reject`, `cost` of at least 1 defaulting to `1`, and `sharing` defaulting to `private` - `inst-uval-07`
8. [x] - `p1` - Validate `cors`: `enabled` required, `allowed_origins` entries are `*` or a URI, `allowed_methods` drawn from `GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS` defaulting to `GET` and `POST`, `expose_headers` defaulting to empty, `allow_credentials` defaulting to `false`, `sharing` in `private|inherit|enforce` defaulting to `private`, and `allow_credentials: true` combined with a `*` origin rejected - `inst-uval-08`
9. [x] - `p1` - Validate `plugins.sharing` defaulting to `private` and every `plugins.items[]` entry as either a GTS identifier or a UUID - `inst-uval-09`
10. [x] - `p1` - Validate `tags` entries against `^[a-z0-9_-]+$` - `inst-uval-10`
11. [x] - `p1` - Apply the recorded defaults `enabled: true` and the schema defaults above, and hand the plugin bindings to the store for position validation (contiguous from 0); resolvability of a UUID-backed custom plugin is entry 2.3's registry concern - `inst-uval-11`
12. [x] - `p1` - **RETURN** the typed model, or the validation failure for the entry-2.1 mapping layer - `inst-uval-12`

### Route Model Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-route-validate`

**Input**: the route create or replace body and the calling tenant.

**Output**: a typed route model with recorded defaults applied, or the first validation failure.

**Steps**:
1. [x] - `p1` - Require `upstream_id` and `match`, and reject unknown properties at the nested objects - `inst-rval-01`
2. [x] - `p1` - Require `match` to carry exactly one of `http` or `grpc`, and record the derived `match_type` the logical model stores - `inst-rval-02`
3. [x] - `p1` - For `match.http`, require `methods` with at least one entry drawn from `GET|POST|PUT|DELETE|PATCH` and a `path` of at least one character; apply the defaults `query_allowlist: []` and `path_suffix_mode: append` from `disabled|append` - `inst-rval-03`
4. [x] - `p1` - For `match.grpc`, require `service` and `method`, each of at least one character, and store them as declared (no gRPC path is served) - `inst-rval-04`
5. [x] - `p1` - Validate `rate_limit` with the same field set and defaults as the upstream rate limit - `inst-rval-05`
6. [x] - `p1` - Validate `cors` with the same field set, defaults and `allow_credentials: true` combined with a `*` origin rejection as the upstream validator's `cors` step, so both write paths enforce the ADR 0004 invariant - `inst-rval-14`
7. [x] - `p1` - Validate `plugins.sharing` defaulting to `private` and every `plugins.items[]` entry as a GTS identifier, with positions validated by the store - `inst-rval-06`
8. [x] - `p1` - Validate `tags` entries against `^[a-z0-9_-]+$` - `inst-rval-07`
9. [x] - `p1` - Apply the recorded defaults `enabled: true` and `priority: 0` for the fields the shipped route schema does not declare (deviation record in section 1.2) - `inst-rval-08`
10. [x] - `p1` - Resolve `upstream_id` within the calling tenant; an identifier that resolves to an ancestor-owned upstream is treated as missing, and an unresolvable one fails with `400` - `inst-rval-09`
11. [x] - `p1` - **TRY** to build the typed route model - `inst-rval-10`
12. [x] - `p1` - **CATCH** a validation failure - `inst-rval-11`
   1. [x] - `p1` - Return the offending field and reason to the caller flow for the entry-2.1 mapping layer - `inst-rval-12`
13. [x] - `p1` - **RETURN** the typed route model - `inst-rval-13`

### Sharing Modes and Ancestor Permission Gates

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-sharing-mode-validate`

**Input**: the calling tenant and its permission set, the resource being written, the ancestor chain
from `tenant-resolver`, and the ancestor resources relevant to the write.

**Output**: the accepted resource with the ancestor constraint set recorded for resolution, or the
rejection (`403` or `400`) to map.

**Steps**:
1. [x] - `p1` - Resolve the ancestor chain of the calling tenant - `inst-shar-01`
2. [x] - `p1` - **IF** the write is an upstream create whose `alias` matches an ancestor upstream - `inst-shar-02`
   1. [x] - `p1` - Require `oagw:upstream:bind` and fail with `403` when it is not granted - `inst-shar-03`
3. [x] - `p1` - Treat an ancestor field with `sharing: private` as invisible to the descendant: the ancestor resource resolves as `404` on the management API and contributes no inheritable value - `inst-shar-04`
4. [x] - `p1` - **IF** the request overrides a field whose ancestor value carries `sharing: enforce` - `inst-shar-05`
   1. [x] - `p1` - Fail the write with `400`, since `enforce` blocks descendant overrides - `inst-shar-06`
5. [x] - `p1` - Permit an auth override only when `auth.sharing` is `inherit` and the caller holds `oagw:upstream:override_auth`, and require the replacement `auth.config` to reference credentials by `cred://` - `inst-shar-07`
6. [x] - `p1` - Accept a descendant `rate_limit` only when `oagw:upstream:override_rate` is granted and the resulting effective limit is not weaker than an enforced ancestor value, because `min(ancestor.enforced, descendant)` still applies - `inst-shar-08`
7. [x] - `p1` - Accept descendant `plugins.items[]` only as an append to the inherited chain when `oagw:upstream:add_plugins` is granted, and never allow an enforced ancestor plugin to be removed - `inst-shar-09`
8. [x] - `p1` - Apply tag semantics as add-only union: request `tags` are tenant-local additions and never remove an inherited tag, including in a binding-style create that resolves to an existing upstream definition - `inst-shar-10`
9. [x] - `p1` - Leave the read-time effective merge (upstream < route < tenant) to entry 2.4; this algorithm records and enforces the write-side constraints only - `inst-shar-11`
10. [x] - `p1` - **RETURN** the accepted resource and the ancestor constraint set - `inst-shar-12`

### Store Write and Invariant Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-store-invariants`

**Input**: a validated change (insert, replace or delete) for one tenant's upstream or route, with
its dependent rows.

**Output**: the committed record, or the invariant violation that maps to `409` or `400`.

**Steps**:
1. [x] - `p1` - Take the store's write lock for the affected tenant key - `inst-store-01`
2. [x] - `p1` - Check `UNIQUE (tenant_id, alias)` for an upstream write inside the same critical section as the write - `inst-store-02`
3. [x] - `p1` - Check route match determinism for an upstream: no two enabled routes may share the same `path`, `priority` and method combination - `inst-store-03`
4. [x] - `p1` - Check plugin binding positions are contiguous from 0 on every written `oagw_upstream_plugin` or `oagw_route_plugin` row set, storing `plugin_ref` always and `plugin_uuid` only for UUID-backed plugins - `inst-store-04`
5. [x] - `p1` - Apply the whole change as one atomic unit over the upstream or route record and its dependent rows, so a rejected invariant leaves no partial write behind - `inst-store-05`
6. [x] - `p1` - Stamp the record's `created_at` on insert and refresh it on replace, because the list contract orders on it - `inst-store-06`
7. [x] - `p1` - Publish the new immutable configuration snapshot so a reader sees the previous or the new state and never an intermediate one - `inst-store-07`
8. [x] - `p1` - Invalidate the consumer caches after a successful write per `cpt-cf-oagw-adr-state-management`, so the data plane does not resolve from a stale snapshot - `inst-store-08`
9. [x] - `p1` - **CATCH** an invariant violation - `inst-store-09`
   1. [x] - `p1` - Discard the staged change, release the lock, and return the violation to the calling flow for the entry-2.1 mapping layer - `inst-store-10`
10. [x] - `p1` - **RETURN** the committed record - `inst-store-11`

### OData List Query Translation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-odata-query`

**Input**: the query parameters `$filter`, `$select`, `$orderby`, `$top` and `$skip`, and the
tenant-scoped collection.

**Output**: the projected and ordered page, or a `400` for an unsupported expression.

**Steps**:
1. [x] - `p1` - Scope the collection to the calling tenant before any filtering, ordering or paging - `inst-oq-01`
2. [x] - `p1` - Parse `$filter` over the model's fields, supporting the comparison form the DESIGN documents (for example `alias eq 'api.openai.com'` for upstreams and `upstream_id eq '{uuid}'` for routes) - `inst-oq-02`
3. [x] - `p1` - Reject an unsupported operator or an unknown field with `400` - `inst-oq-03`
4. [x] - `p1` - Parse `$orderby` over the model's fields with an optional direction, including `created_at desc` - `inst-oq-04`
5. [x] - `p1` - Parse `$select` as a field list and reject an unknown field with `400` - `inst-oq-05`
6. [x] - `p1` - Parse `$top` with the recorded default `50` and maximum `100`, and `$skip` as a non-negative offset; a value outside those bounds is `400` - `inst-oq-06`
7. [x] - `p1` - Apply filter, then ordering, then offset and limit, so paging is stable for a given tenant - `inst-oq-07`
8. [x] - `p1` - Project the page onto the `$select` fields when given - `inst-oq-08`
9. [x] - `p1` - Never include credential material or resolved secret values in a projected response - `inst-oq-09`
10. [x] - `p1` - **RETURN** the page - `inst-oq-10`

### Resource Identifier Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-resource-identity`

**Input**: a path `{id}` parameter, the expected resource type, and the calling tenant.

**Output**: the stored record, or the `400` / `404` outcome to map.

**Steps**:
1. [x] - `p1` - Parse `{id}` as an anonymous GTS identifier of the expected form, `gts.cf.core.oagw.upstream.v1~{uuid}` for an upstream and `gts.cf.core.oagw.route.v1~{uuid}` for a route, extracting the instance part - `inst-rid-01`
2. [x] - `p1` - Accept a bare UUID by inferring the type from the endpoint path - `inst-rid-02`
3. [x] - `p1` - Fail with `400` when the identifier does not parse or when the type prefix does not match the endpoint's resource type - `inst-rid-03`
4. [x] - `p1` - Look the record up by its `id` inside the calling tenant's key space only - `inst-rid-04`
5. [x] - `p1` - **IF** no record of this tenant carries the identifier, including a record owned by an ancestor - `inst-rid-05`
   1. [x] - `p1` - **RETURN** not found, so an ancestor resource is indistinguishable from a missing one - `inst-rid-06`
6. [x] - `p1` - **ELSE** - `inst-rid-07`
   1. [x] - `p1` - **RETURN** the record with its `id`, `tenant_id` and `alias` intact - `inst-rid-08`
7. [x] - `p1` - Treat `id` and `tenant_id` as immutable in every subsequent write - `inst-rid-09`

## 4. States (CDSL)

Optional: Include when entities have explicit lifecycle states.

Both resources this feature owns have an explicit lifecycle. The store holds no other state machine:
the plugin definitions belong to entry 2.3 and the runtime states of the proxy path (circuit breaker,
token buckets) belong to entries 2.4 and 2.5.

### Upstream State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-upstream-lifecycle`

**States**: `absent`, `active`, `disabled`

**Initial State**: `absent`

**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `active` **WHEN** a create passes validation, alias derivation, the `(tenant_id, alias)` uniqueness check and the bind and sharing-mode gates - `inst-ust-01`
2. [x] - `p1` - **FROM** `active` **TO** `disabled` **WHEN** the owning tenant's replace sets `enabled: false` - `inst-ust-02`
3. [x] - `p1` - **FROM** `disabled` **TO** `active` **WHEN** the owning tenant's replace sets `enabled: true`; a descendant cannot drive this transition because an ancestor resource is not addressable on the management API - `inst-ust-03`
4. [x] - `p1` - **FROM** `active` **TO** `absent` **WHEN** a delete removes the upstream and cascades its routes - `inst-ust-04`
5. [x] - `p1` - **FROM** `disabled` **TO** `absent` **WHEN** a delete removes the upstream - `inst-ust-05`

**Closed transition set**: the transitions above are the only ones possible. A replace that leaves
`enabled` unchanged causes no transition, a failed validation or a rejected invariant leaves the
resource in its current state, and no state is re-entered on its own or skipped. The effect of a
`disabled` upstream on the request path is entry 2.4's behaviour; this machine tracks only the stored
value.

### Route State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-route-lifecycle`

**States**: `absent`, `active`, `disabled`

**Initial State**: `absent`

**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `active` **WHEN** a create passes route validation, resolves `upstream_id` inside the calling tenant and passes the match-uniqueness check - `inst-rst-01`
2. [x] - `p1` - **FROM** `active` **TO** `disabled` **WHEN** the owning tenant's replace sets `enabled: false` - `inst-rst-02`
3. [x] - `p1` - **FROM** `disabled` **TO** `active` **WHEN** the owning tenant's replace sets `enabled: true` and the revalidated match rule does not collide with another enabled route of the same upstream - `inst-rst-03`
4. [x] - `p1` - **FROM** `active` **TO** `absent` **WHEN** a delete removes the route - `inst-rst-04`
5. [x] - `p1` - **FROM** `disabled` **TO** `absent` **WHEN** a delete removes the route, or the owning upstream is deleted and the removal cascades - `inst-rst-05`

**Closed transition set**: the transitions above are the only ones possible. A `disabled` route keeps
its record but its match key leaves the uniqueness set, so another route may take the same `path`,
`priority` and method combination while it is disabled; re-entering `active` then requires the
uniqueness check again and fails with `409` on a collision. A failed validation leaves the state
unchanged, and exclusion of a disabled route from matching is entry 2.4's behaviour.

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Upstream Model and Schema Conformance

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-model`

The system **MUST** define the `Upstream` model and its `ServerConfig`/`Endpoint`, `HeadersConfig`,
`RateLimitConfig`, `CorsConfig`, `PluginsConfig` and `Tag` parts with the exact field names, enums and
recorded defaults of [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json) — required
`server` and `protocol`, `enabled` defaulting to `true`, `alias` matching
`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`, `tags` matching `^[a-z0-9_-]+$`, `server.endpoints` with at least
one entry of `scheme`, `host` and `port` defaulting to `443`, the `protocol` GTS enum, and the `auth`,
`headers`, `plugins`, `rate_limit` and `cors` field sets with their schema defaults — and **MUST**
reject unknown properties. The model **MUST** treat `id`, `tenant_id` and `alias` as immutable after
creation and **MUST** carry no secret material, only `cred://` references.

**Implements**:
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-upstream-replace`
- `cpt-cf-oagw-algo-upstream-validate`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-cred-isolation`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema` (logical model only — in-memory store, DECOMPOSITION assumption 3)
- Entities: `Upstream`, `ServerConfig`, `Endpoint`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`, `Tag`

### Upstream Management Endpoints

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-upstream-endpoints`

The system **MUST** register `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`,
`GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}` and `DELETE /oagw/v1/upstreams/{id}`
under the gear mount root with the status codes `201` create, `200` get/list/replace and `204`
delete, **MUST** authenticate each call and require the
`gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}` permission of
`cpt-cf-oagw-interface-api`, **MUST** return every failure through the entry-2.1 mapping layer as
`application/problem+json` with `X-OAGW-Error-Source: gateway`, and **MUST** register the operations
and schemas in the host OpenAPI registry (platform baseline, section 1.4).

**Implements**:
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-upstream-replace`
- `cpt-cf-oagw-flow-management-list-query`
- `cpt-cf-oagw-flow-upstream-route-delete`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema` (logical model only, DECOMPOSITION assumption 3)
- Entities: `Upstream`

### Route Model and Schema Conformance

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-model`

The system **MUST** define the `Route` and `MatchConfig` models with the exact field names of
[schemas/route.v1.schema.json](../schemas/route.v1.schema.json) — required `upstream_id` and `match`,
`match` carrying exactly one of `http` (`methods`, `path`, `query_allowlist`, `path_suffix_mode`) and
`grpc` (`service`, `method`), `plugins.sharing` with `plugins.items[]`, and `rate_limit` — together
with the `enabled`, `priority` and `cors` fields the DESIGN domain model declares and the shipped
route schema omits (deviation record in section 1.2), with recorded defaults `enabled: true` and
`priority: 0`. The model **MUST** treat `id`, `tenant_id` and `upstream_id` as immutable and **MUST**
derive the stored `match_type` from which member of `match` is present.

**Implements**:
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-route-replace`
- `cpt-cf-oagw-algo-route-validate`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema` (logical model only, DECOMPOSITION assumption 3)
- Entities: `Route`, `MatchConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`, `Tag`

### Route Management Endpoints

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-route-endpoints`

The system **MUST** register `POST /oagw/v1/routes`, `GET /oagw/v1/routes`,
`GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}` and `DELETE /oagw/v1/routes/{id}` with the
status codes `201` create, `200` get/list/replace and `204` delete, **MUST** authenticate each call
and require the `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` permission of
`cpt-cf-oagw-interface-api`, **MUST** reject a body that changes `upstream_id` with `400` while
accepting a body that echoes the stored value, and **MUST** return every failure through the
entry-2.1 mapping layer with `X-OAGW-Error-Source: gateway`.

**Implements**:
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-route-replace`
- `cpt-cf-oagw-flow-management-list-query`
- `cpt-cf-oagw-flow-upstream-route-delete`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema` (logical model only, DECOMPOSITION assumption 3)
- Entities: `Route`

### Alias Derivation and Enforcement

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-derivation`

The system **MUST** derive the upstream `alias` from `server.endpoints[]` exactly as
`cpt-cf-oagw-algo-alias-derive` specifies — a single hostname, `hostname:port` for a non-standard
port, the longest common domain suffix of at least two labels for a multi-host pool validated against
the public suffix list with the `psl` crate so a bare public suffix such as `co.uk` is never accepted,
`suffix:port` preserved for a non-standard port, and lowercase normalization with trailing dots
stripped — **MUST** require an explicit `alias` for IP-based or non-derivable endpoints, **MUST**
reject a supplied `alias` that differs from the derived value with `400` while tolerating the exact
derived value, and **MUST** keep `alias`, `id` and `tenant_id` immutable on every write.

**Implements**:
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-upstream-replace`
- `cpt-cf-oagw-algo-alias-derive`
- `cpt-cf-oagw-algo-alias-update-enforce`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema` (`UNIQUE (tenant_id, alias)`)
- Entities: `Upstream`

### Tenant Scoping and Isolation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tenant-scoping`

The system **MUST** scope every management read and write to the calling tenant resolved from the
security context, **MUST** key every record by `tenant_id` in the store, **MUST** present an
ancestor-owned resource as `404` on get, list, replace and delete, **MUST** treat an ancestor-owned
upstream as an unresolvable `upstream_id` for route creation (`400`), and **MUST** leave ancestor
resources visible to the data plane only through the tenant-hierarchy walk that entry 2.4 performs.

**Implements**:
- `cpt-cf-oagw-flow-management-list-query`
- `cpt-cf-oagw-flow-upstream-route-delete`
- `cpt-cf-oagw-algo-resource-identity`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: every management endpoint under `/oagw/v1/upstreams` and `/oagw/v1/routes`
- DB: `cpt-cf-oagw-db-schema` (tenant-keyed lookups)
- Entities: `Upstream`, `Route`

### Sharing Modes, Ancestor Permissions and Layering

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sharing-and-permissions`

The system **MUST** enforce the sharing modes `private|inherit|enforce` of
`cpt-cf-oagw-fr-hierarchical-config` on the write path — `enforce` blocking a descendant override and
`private` hiding the ancestor resource behind `404` — **MUST** require `oagw:upstream:bind` for a
create whose `alias` matches an ancestor upstream, `oagw:upstream:override_auth` for an auth override,
`oagw:upstream:override_rate` for an own rate limit and `oagw:upstream:add_plugins` for appending
plugins, **MUST** apply the rate-limit inheritance rule `min(ancestor.enforced, descendant)` when
accepting a descendant limit, **MUST** append descendant plugins to the inherited chain without
removing an enforced ancestor plugin, and **MUST** apply add-only union semantics to `tags` so a
descendant can never remove an inherited tag, honouring the layering priority of
`cpt-cf-oagw-fr-config-layering` (upstream < route < tenant) in the write-side constraints it records.

**Implements**:
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-upstream-replace`
- `cpt-cf-oagw-algo-sharing-mode-validate`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema` (recorded ancestor constraint set)
- Entities: `Upstream`, `Route`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`

### In-Memory Store and Logical Invariants

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-store-invariants`

The system **MUST** implement the store on the crate's existing `dashmap`/`parking_lot`/`arc-swap`
dependency set with no `toolkit-db` dependency and no SQL, **MUST** keep `cpt-cf-oagw-db-schema` as
the logical model with the DESIGN table and column names, **MUST** enforce `UNIQUE (tenant_id, alias)`,
route match determinism (no two enabled routes of one upstream sharing `path`, `priority` and method)
and plugin binding positions contiguous from 0 inside a single write critical section, **MUST** apply
a multi-row change atomically, **MUST** cascade the delete of an upstream to its routes, **MUST** stamp
`created_at` for the list contract's ordering, and **MUST** publish a new immutable snapshot and
invalidate consumer caches after each successful write per `cpt-cf-oagw-adr-state-management`.

**Implements**:
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-upstream-route-delete`
- `cpt-cf-oagw-algo-store-invariants`
- `cpt-cf-oagw-state-upstream-lifecycle`
- `cpt-cf-oagw-state-route-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: every management write endpoint
- DB: `cpt-cf-oagw-db-schema` (logical model only, DECOMPOSITION assumption 3)
- Entities: `Upstream`, `Route`, `ServerConfig`, `MatchConfig`, `PluginsConfig`, `Tag`

### OData List Queries

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-odata-list`

The system **MUST** support `$filter`, `$select`, `$orderby`, `$top` and `$skip` on
`GET /oagw/v1/upstreams` and `GET /oagw/v1/routes` with `$top` defaulting to `50` and capped at `100`,
**MUST** apply the tenant scope before filtering, ordering and paging, **MUST** reject an unsupported
`$filter` operator, an unknown `$orderby` or `$select` field, an out-of-range `$top` and a negative
`$skip` with `400`, and **MUST** project responses onto the `$select` fields when the parameter is
present.

**Implements**:
- `cpt-cf-oagw-flow-management-list-query`
- `cpt-cf-oagw-algo-odata-query`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `GET /oagw/v1/upstreams`, `GET /oagw/v1/routes`
- DB: `cpt-cf-oagw-db-schema` (tenant-scoped reads)
- Entities: `Upstream`, `Route`

### Enabled and Disabled Write Semantics

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-enable-disable`

The system **MUST** create every upstream and route with `enabled` defaulting to `true`, **MUST**
store the boolean set by the owning tenant's replace operation, **MUST NOT** allow a descendant to
re-enable an ancestor-disabled resource — an ancestor resource is not addressable on the management
API, so the inherited disabled state cannot be lifted from here — and **MUST** leave the enforcement
of the stored `enabled` value to the data plane (entry 2.4 owns the `503` and the match-exclusion
behaviour of `cpt-cf-oagw-fr-enable-disable`).

**Implements**:
- `cpt-cf-oagw-flow-enable-disable`
- `cpt-cf-oagw-state-upstream-lifecycle`
- `cpt-cf-oagw-state-route-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`, `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema`
- Entities: `Upstream`, `Route`

### Management Error Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-contract`

The system **MUST** return every management failure as a canonical `application/problem+json` body
produced by the entry-2.1 mapping layer — `400` for validation failures including an unresolvable
`upstream_id`, `403` for a missing permission or bind gate, `404` for an out-of-tenant or unknown
resource, `409` for a `(tenant_id, alias)` collision and a duplicate route match — with the GTS `type`
identifier, the standard problem fields and the OAGW extension fields of `cpt-cf-oagw-interface-api`,
and **MUST NOT** emit a management error in any other body format or include credential material in
any error body or log line.

**Implements**:
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-management-list-query`
- `cpt-cf-oagw-algo-store-invariants`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Principles**: `cpt-cf-oagw-principle-rfc9457`

**Touches**:
- API: every management endpoint's failure response (`application/problem+json`)
- Entities: domain error type exposed by entry 2.1

### Test Layering

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-in-crate-test-coverage`

The system **MUST** cover this feature with in-crate Rust tests only — unit tests inside
`#[cfg(test)]` modules per layer for the alias derivation and update rules, the upstream and route
validators, the sharing-mode and permission gates, the store invariants and the OData translation, and
integration tests under the crate's `tests/` directory that boot the gear router and exercise the ten
endpoints including the `201`/`200`/`204` statuses, the `409` conflicts, the `404` tenant scoping and
the error contract — and **MUST NOT** add an e2e suite under `testing/e2e/gears/oagw/`
(DECOMPOSITION assumption 5).

**Implements**:
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-algo-alias-derive`
- `cpt-cf-oagw-algo-store-invariants`
- `cpt-cf-oagw-algo-odata-query`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: the ten management endpoints (asserted by the integration tests)
- Entities: `Upstream`, `Route`

## 6. Acceptance Criteria

- [x] `POST /oagw/v1/upstreams` with a single hostname endpoint and no `alias` returns `201` with the derived `alias` and `enabled: true`.
- [x] A multi-host pool with a registrable common suffix derives the suffix as `alias`; a pool whose only common suffix is a bare public suffix such as `co.uk` is rejected with `400` unless an explicit `alias` is supplied.
- [x] An IP-based endpoint set without an explicit `alias` returns `400`; a supplied `alias` that differs from the derived value returns `400`, and the exact derived value is tolerated.
- [x] An endpoint with `scheme: http` is accepted at create time and the stored model keeps it verbatim.
- [x] A second upstream with the same `alias` in the same tenant returns `409` with an `application/problem+json` body carrying a GTS `type` identifier and the colliding key in `detail`.
- [x] `POST /oagw/v1/routes` with an `upstream_id` that does not resolve inside the calling tenant returns `400`; a duplicate `path`, `priority` and method combination among the upstream's enabled routes returns `409`.
- [x] A route whose `match` carries neither or both of `http` and `grpc` returns `400`; a `match.grpc` route is stored verbatim with its `service` and `method`.
- [x] `PUT /oagw/v1/upstreams/{id}` that would change the derived `alias` returns `400`, including a hostname to IP change with an explicit `alias`; `PUT /oagw/v1/routes/{id}` that changes `upstream_id` returns `400`.
- [x] `DELETE /oagw/v1/upstreams/{id}` returns `204` and removes the upstream together with its routes; `DELETE /oagw/v1/routes/{id}` returns `204`.
- [x] A resource owned by an ancestor tenant returns `404` on get, replace and delete, and its identifier does not resolve as a route's `upstream_id` for a descendant.
- [x] `GET /oagw/v1/upstreams` and `GET /oagw/v1/routes` honour `$filter`, `$select`, `$orderby`, `$top` and `$skip`, default `$top` to `50`, cap it at `100`, and return `400` for an unsupported expression.
- [x] A create that overrides an ancestor field marked `sharing: enforce` is rejected, a `private` ancestor resource resolves as `404`, and the four ancestor permission gates are enforced.
- [x] A descendant `rate_limit` is accepted only when it is not weaker than an enforced ancestor value, descendant plugins are appended, and inherited `tags` cannot be removed.
- [x] A created route carries `priority: 0` and a created resource carries `enabled: true` when the fields are omitted, and both are stored when supplied.
- [x] A management failure returns `application/problem+json` with the GTS `type` identifier, `title`, `status`, `detail` and `instance`, and every management response carries `X-OAGW-Error-Source`.
- [x] No response body, error body or log line contains secret material or anything other than a `cred://` reference for credentials.
- [x] The store refuses a write that would break `UNIQUE (tenant_id, alias)`, route match determinism or contiguous plugin binding positions, and a reader never observes a partially applied write.
- [x] The in-crate unit and integration tests pass with the host feature set used by the graded configuration, and no test artifact is added under `testing/e2e/gears/oagw/`.
